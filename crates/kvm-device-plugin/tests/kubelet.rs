//! The plugin against a fake kubelet over real Unix sockets.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use hyper_util::rt::TokioIo;
use kvm_device_plugin::config::{Cli, Config, Timing};
use kvm_device_plugin::endpoint;
use kvm_device_plugin::health::Health;
use kvm_device_plugin::proto::v1beta1::device_plugin_client::DevicePluginClient;
use kvm_device_plugin::proto::v1beta1::registration_server::{Registration, RegistrationServer};
use kvm_device_plugin::proto::v1beta1::{
    AllocateRequest, ContainerAllocateRequest, DevicePluginOptions, Empty, RegisterRequest,
};
use kvm_device_plugin::supervisor::{self, RunError};
use tempfile::TempDir;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Request, Response, Status};

const RESOURCE: &str = "example.com/kvm";
const STEM: &str = "example.com_kvm";
const SLOTS: u32 = 3;
const WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
enum Answer {
    Accept,
    Refuse,
    Hang,
}

/// Records registrations and answers them as told.
struct FakeKubelet {
    answer: Arc<Mutex<Answer>>,
    registrations: mpsc::UnboundedReceiver<RegisterRequest>,
    stop: CancellationToken,
}

struct Registrar {
    answer: Arc<Mutex<Answer>>,
    registrations: mpsc::UnboundedSender<RegisterRequest>,
}

#[tonic::async_trait]
impl Registration for Registrar {
    async fn register(&self, request: Request<RegisterRequest>) -> Result<Response<Empty>, Status> {
        let _ = self.registrations.send(request.into_inner());
        let answer = *self.answer.lock().unwrap();
        match answer {
            Answer::Accept => Ok(Response::new(Empty {})),
            Answer::Refuse => Err(Status::invalid_argument("synthetic refusal")),
            Answer::Hang => std::future::pending().await,
        }
    }
}

impl FakeKubelet {
    fn start(dir: &Path, answer: Answer) -> Self {
        let answer = Arc::new(Mutex::new(answer));
        let (tx, registrations) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        let listener = UnixListener::bind(dir.join("kubelet.sock")).unwrap();
        let registrar = Registrar { answer: Arc::clone(&answer), registrations: tx };
        tokio::spawn(Server::builder().serve_with_incoming_shutdown(
            RegistrationServer::new(registrar),
            UnixListenerStream::new(listener),
            stop.clone().cancelled_owned(),
        ));
        Self { answer, registrations, stop }
    }

    fn answer(&self, answer: Answer) {
        *self.answer.lock().unwrap() = answer;
    }

    async fn registration(&mut self) -> RegisterRequest {
        tokio::time::timeout(WAIT, self.registrations.recv()).await.unwrap().unwrap()
    }
}

impl Drop for FakeKubelet {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct Plugin {
    dir: PathBuf,
    stop: CancellationToken,
    task: JoinHandle<Result<(), RunError>>,
}

impl Plugin {
    fn start(dir: &Path) -> Self {
        let cli = Cli {
            resource_name: RESOURCE.into(),
            slots: SLOTS,
            device_plugin_dir: dir.into(),
            kvm_path: "/dev/kvm".into(),
        };
        let mut config = Config::try_from(cli).unwrap();
        config.timing = Timing {
            register_timeout: Duration::from_millis(300),
            backoff_initial: Duration::from_millis(20),
            backoff_max: Duration::from_millis(100),
            poll_interval: Duration::from_millis(20),
            health_interval: Duration::from_secs(3600),
            probe_timeout: Duration::from_secs(1),
            watch_grace: Duration::from_secs(1),
            shutdown_grace: Duration::from_secs(1),
        };
        let stop = CancellationToken::new();
        let task =
            tokio::spawn(supervisor::run(config, Arc::new(|| Health::Healthy), stop.clone()));
        Self { dir: dir.into(), stop, task }
    }

    async fn client(&self, endpoint: &str) -> DevicePluginClient<Channel> {
        DevicePluginClient::new(connect(self.dir.join(endpoint)).await)
    }

    /// Stops the plugin and returns how long it took.
    async fn stop(self) -> Duration {
        let started = Instant::now();
        self.stop.cancel();
        tokio::time::timeout(WAIT, self.task).await.unwrap().unwrap().unwrap();
        started.elapsed()
    }
}

async fn connect(path: PathBuf) -> Channel {
    Endpoint::from_static("http://plugin")
        .connect_with_connector(tower::service_fn(move |_| {
            let path = path.clone();
            async move { UnixStream::connect(path).await.map(TokioIo::new) }
        }))
        .await
        .unwrap()
}

/// A short directory, since macOS's per-user temporary directories leave little room for socket names.
fn directory() -> TempDir {
    tempfile::Builder::new().prefix("kdp").tempdir_in("/tmp").unwrap()
}

fn plugin_sockets(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != "kubelet.sock")
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn registers_serves_and_stops_cleanly() {
    let dir = directory();
    let mut kubelet = FakeKubelet::start(dir.path(), Answer::Accept);
    let plugin = Plugin::start(dir.path());

    let registration = kubelet.registration().await;
    assert_eq!(registration.version, "v1beta1");
    assert_eq!(registration.resource_name, RESOURCE);
    assert_eq!(registration.options, Some(DevicePluginOptions::default()));
    assert!(endpoint::parse(STEM, &registration.endpoint).is_some(), "{}", registration.endpoint);

    let mut client = plugin.client(&registration.endpoint).await;
    let options = client.get_device_plugin_options(Empty {}).await.unwrap().into_inner();
    assert_eq!(options, DevicePluginOptions::default());
    let mut devices = client.list_and_watch(Empty {}).await.unwrap().into_inner();
    let list = devices.next().await.unwrap().unwrap();
    let ids: Vec<_> = list.devices.iter().map(|d| (d.id.as_str(), d.health.as_str())).collect();
    assert_eq!(ids, [("kvm-0", "Healthy"), ("kvm-1", "Healthy"), ("kvm-2", "Healthy")]);
    let request = AllocateRequest {
        container_requests: vec![ContainerAllocateRequest { devices_ids: vec!["kvm-1".into()] }],
    };
    let allocation = client.allocate(request).await.unwrap().into_inner();
    assert_eq!(allocation.container_responses.len(), 1);
    let device = &allocation.container_responses[0].devices[0];
    assert_eq!(
        (&*device.host_path, &*device.container_path, &*device.permissions),
        ("/dev/kvm", "/dev/kvm", "rw")
    );

    // Stopping with a device stream open ends the stream and removes the socket promptly.
    assert!(plugin.stop().await < Duration::from_millis(500));
    let ended = tokio::time::timeout(WAIT, devices.next()).await.unwrap();
    assert!(matches!(ended, None | Some(Err(_))), "{ended:?}");
    assert_eq!(plugin_sockets(dir.path()), Vec::<String>::new());
}

#[tokio::test]
async fn retries_until_kubelet_appears_and_accepts() {
    let root = directory();
    let dir = root.path().join("plugins");
    let plugin = Plugin::start(&dir);
    tokio::time::sleep(Duration::from_millis(100)).await;

    std::fs::create_dir(&dir).unwrap();
    let mut kubelet = FakeKubelet::start(&dir, Answer::Refuse);
    let refused = kubelet.registration().await;
    kubelet.answer(Answer::Accept);
    let mut accepted = kubelet.registration().await;
    while accepted.endpoint == refused.endpoint {
        accepted = kubelet.registration().await;
    }
    let (_, refused_generation) = endpoint::parse(STEM, &refused.endpoint).unwrap();
    let (_, accepted_generation) = endpoint::parse(STEM, &accepted.endpoint).unwrap();
    assert!(accepted_generation > refused_generation);
    // Refused generations leave no socket behind.
    assert_eq!(plugin_sockets(&dir), [accepted.endpoint]);
    plugin.stop().await;
}

#[tokio::test]
async fn a_hung_registration_times_out_and_cannot_delay_stopping() {
    let dir = directory();
    let mut kubelet = FakeKubelet::start(dir.path(), Answer::Hang);
    let plugin = Plugin::start(dir.path());
    let first = kubelet.registration().await;
    let second = kubelet.registration().await;
    assert_ne!(first.endpoint, second.endpoint);
    assert!(plugin.stop().await < Duration::from_millis(200));
    assert_eq!(plugin_sockets(dir.path()), Vec::<String>::new());
}

#[tokio::test]
async fn sweeps_only_stale_sockets_of_its_own_resource() {
    let dir = directory();
    let socket = |name: &str| dir.path().join(name);
    let nonce = 0x0123_4567_89ab_cdef;
    let stale = endpoint::basename(STEM, nonce, 0);
    let live = endpoint::basename(STEM, nonce, 1);
    let not_a_socket = endpoint::basename(STEM, nonce, 2);
    let other_resource = endpoint::basename("example.com_other", nonce, 0);
    drop(std::os::unix::net::UnixListener::bind(socket(&stale)).unwrap());
    let _live = std::os::unix::net::UnixListener::bind(socket(&live)).unwrap();
    std::fs::write(socket(&not_a_socket), "").unwrap();
    drop(std::os::unix::net::UnixListener::bind(socket(&other_resource)).unwrap());

    let mut kubelet = FakeKubelet::start(dir.path(), Answer::Accept);
    let plugin = Plugin::start(dir.path());
    let registration = kubelet.registration().await;
    let mut expected = vec![live, not_a_socket, other_resource, registration.endpoint];
    expected.sort();
    assert_eq!(plugin_sockets(dir.path()), expected);
    plugin.stop().await;
}
