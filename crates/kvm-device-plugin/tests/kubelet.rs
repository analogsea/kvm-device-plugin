//! The plugin against a fake kubelet over real Unix sockets.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use kvm_device_plugin::config::{Cli, Config, Timing};
use kvm_device_plugin::endpoint;
use kvm_device_plugin::health::Health;
use kvm_device_plugin::proto::v1beta1::device_plugin_client::DevicePluginClient;
use kvm_device_plugin::proto::v1beta1::registration_server::{Registration, RegistrationServer};
use kvm_device_plugin::proto::v1beta1::{
    AllocateRequest, ContainerAllocateRequest, DevicePluginOptions, Empty, ListAndWatchResponse,
    RegisterRequest,
};
use kvm_device_plugin::supervisor::{self, RunError};
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Request, Response, Status, Streaming};

const RESOURCE: &str = "example.com/kvm";
const STEM: &str = "example.com_kvm";
const SLOTS: u32 = 3;
const REGISTER_TIMEOUT: Duration = Duration::from_millis(300);
const WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
enum Answer {
    Accept,
    Refuse,
    Hang,
}

/// One registration as kubelet would see it. Like kubelet, the fake connects back to the plugin, reads its
/// options and starts watching its devices before answering, unless it refuses outright.
struct Registered {
    request: RegisterRequest,
    deadline: Option<Duration>,
    client: Option<DevicePluginClient<Channel>>,
    devices: Option<Streaming<ListAndWatchResponse>>,
}

struct FakeKubelet {
    answer: Arc<Mutex<Answer>>,
    registrations: mpsc::UnboundedReceiver<Registered>,
    stop: CancellationToken,
}

struct Registrar {
    dir: PathBuf,
    answer: Arc<Mutex<Answer>>,
    registrations: mpsc::UnboundedSender<Registered>,
}

#[tonic::async_trait]
impl Registration for Registrar {
    async fn register(&self, request: Request<RegisterRequest>) -> Result<Response<Empty>, Status> {
        let deadline =
            request.metadata().get("grpc-timeout").map(|v| grpc_timeout(v.to_str().unwrap()));
        let request = request.into_inner();
        let answer = *self.answer.lock().unwrap();
        let mut registered = Registered { request, deadline, client: None, devices: None };
        if !matches!(answer, Answer::Refuse) {
            let mut client = DevicePluginClient::new(
                connect(&self.dir.join(&registered.request.endpoint)).await?,
            );
            client.get_device_plugin_options(Empty {}).await?;
            registered.devices = Some(client.list_and_watch(Empty {}).await?.into_inner());
            registered.client = Some(client);
        }
        let _ = self.registrations.send(registered);
        match answer {
            Answer::Accept => Ok(Response::new(Empty {})),
            Answer::Refuse => Err(Status::invalid_argument("synthetic refusal")),
            Answer::Hang => std::future::pending().await,
        }
    }
}

/// Decodes a gRPC timeout header such as `300000u`.
fn grpc_timeout(value: &str) -> Duration {
    let (amount, unit) = value.split_at(value.len() - 1);
    let amount: u64 = amount.parse().unwrap();
    match unit {
        "H" => Duration::from_secs(amount * 3600),
        "M" => Duration::from_secs(amount * 60),
        "S" => Duration::from_secs(amount),
        "m" => Duration::from_millis(amount),
        "u" => Duration::from_micros(amount),
        "n" => Duration::from_nanos(amount),
        _ => panic!("bad grpc-timeout {value}"),
    }
}

impl FakeKubelet {
    fn start(dir: &Path, answer: Answer) -> Self {
        let answer = Arc::new(Mutex::new(answer));
        let (tx, registrations) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        let listener = UnixListener::bind(dir.join("kubelet.sock")).unwrap();
        let registrar =
            Registrar { dir: dir.into(), answer: Arc::clone(&answer), registrations: tx };
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

    async fn registration(&mut self) -> Registered {
        tokio::time::timeout(WAIT, self.registrations.recv()).await.unwrap().unwrap()
    }
}

impl Drop for FakeKubelet {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct Plugin {
    stop: CancellationToken,
    task: JoinHandle<Result<(), RunError>>,
}

impl Plugin {
    fn start(dir: &Path) -> Self {
        Self::start_with(dir, Duration::from_millis(20))
    }

    fn start_with(dir: &Path, backoff: Duration) -> Self {
        let cli = Cli {
            resource_name: RESOURCE.into(),
            slots: SLOTS,
            device_plugin_dir: dir.into(),
            kvm_path: "/dev/kvm".into(),
        };
        let mut config = Config::try_from(cli).unwrap();
        config.timing = Timing {
            register_timeout: REGISTER_TIMEOUT,
            backoff_initial: backoff,
            backoff_max: backoff * 5,
            poll_interval: Duration::from_millis(20),
            health_interval: Duration::from_secs(3600),
            probe_timeout: Duration::from_secs(1),
            watch_grace: Duration::from_secs(1),
            shutdown_grace: Duration::from_secs(1),
        };
        let stop = CancellationToken::new();
        let task =
            tokio::spawn(supervisor::run(config, Arc::new(|| Health::Healthy), stop.clone()));
        Self { stop, task }
    }

    /// Stops the plugin and returns how long it took.
    async fn stop(self) -> Duration {
        let started = Instant::now();
        self.stop.cancel();
        tokio::time::timeout(WAIT, self.task).await.unwrap().unwrap().unwrap();
        started.elapsed()
    }
}

async fn connect(path: &Path) -> Result<Channel, Status> {
    let uri = format!("unix:{}", path.display());
    Endpoint::from_shared(uri)
        .unwrap()
        .connect()
        .await
        .map_err(|err| Status::unavailable(format!("cannot dial the plugin: {err}")))
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

    let Registered { request, deadline, client, devices } = kubelet.registration().await;
    assert_eq!(request.version, "v1beta1");
    assert_eq!(request.resource_name, RESOURCE);
    assert_eq!(request.options, Some(DevicePluginOptions::default()));
    assert!(endpoint::parse(STEM, &request.endpoint).is_some(), "{}", request.endpoint);
    assert!(deadline.is_some_and(|d| d <= REGISTER_TIMEOUT), "{deadline:?}");

    let (mut client, mut devices) = (client.unwrap(), devices.unwrap());
    let list = devices.next().await.unwrap().unwrap();
    let ids: Vec<_> = list.devices.iter().map(|d| (d.id.as_str(), d.health.as_str())).collect();
    assert_eq!(ids, [("kvm-0", "Healthy"), ("kvm-1", "Healthy"), ("kvm-2", "Healthy")]);
    let request = AllocateRequest {
        container_requests: vec![ContainerAllocateRequest { devices_ids: vec!["kvm-1".into()] }],
    };
    let allocation = client.allocate(request).await.unwrap().into_inner();
    assert_eq!(allocation.container_responses.len(), 1);
    let device = &allocation.container_responses[0].devices[0];
    let paths = (&*device.host_path, &*device.container_path, &*device.permissions);
    assert_eq!(paths, ("/dev/kvm", "/dev/kvm", "rw"));

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
    assert!(!dir.exists(), "the plugin must not create kubelet's directory");

    std::fs::create_dir(&dir).unwrap();
    let mut kubelet = FakeKubelet::start(&dir, Answer::Refuse);
    let refused = kubelet.registration().await.request;
    kubelet.answer(Answer::Accept);
    let mut accepted = kubelet.registration().await.request;
    while accepted.endpoint == refused.endpoint {
        accepted = kubelet.registration().await.request;
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
    let started = Instant::now();
    let second = kubelet.registration().await;
    assert!(started.elapsed() >= REGISTER_TIMEOUT / 2);
    assert_ne!(first.request.endpoint, second.request.endpoint);
    assert!(!dir.path().join(&first.request.endpoint).exists());
    assert!(plugin.stop().await < Duration::from_millis(200));
    assert_eq!(plugin_sockets(dir.path()), Vec::<String>::new());
}

#[tokio::test]
async fn stopping_interrupts_backoff() {
    let dir = directory();
    let mut kubelet = FakeKubelet::start(dir.path(), Answer::Refuse);
    let plugin = Plugin::start_with(dir.path(), Duration::from_secs(60));
    kubelet.registration().await;
    assert!(plugin.stop().await < Duration::from_millis(200));
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
    let mut expected = vec![live, not_a_socket, other_resource, registration.request.endpoint];
    expected.sort();
    assert_eq!(plugin_sockets(dir.path()), expected);
    plugin.stop().await;
}
