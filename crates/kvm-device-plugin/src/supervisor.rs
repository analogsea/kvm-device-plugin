//! The plugin's lifecycle. Each generation listens on a fresh socket, registers it with kubelet and serves it
//! until it stops; failed generations are retried with exponential backoff.

use std::fmt;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hyper_util::rt::TokioIo;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Endpoint, Server};

use crate::config::Config;
use crate::endpoint;
use crate::health::{HealthHandle, Monitor, Probe};
use crate::plugin::{self, Service};
use crate::pool::Pool;
use crate::proto::v1beta1::RegisterRequest;
use crate::proto::v1beta1::device_plugin_server::DevicePluginServer;
use crate::proto::v1beta1::registration_client::RegistrationClient;
use crate::proto::{KUBELET_SOCKET, VERSION};

/// Why the plugin stopped without being asked to.
#[derive(Debug)]
pub enum RunError {
    /// The socket-name nonce could not be generated.
    Nonce(io::Error),
    /// The health monitor ended unexpectedly.
    MonitorEnded,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Nonce(err) => write!(f, "cannot generate a socket name: {err}"),
            Self::MonitorEnded => write!(f, "the KVM health monitor ended unexpectedly"),
        }
    }
}

impl std::error::Error for RunError {}

/// Runs the plugin until `stop` is cancelled. KVM is checked once before the first registration, so kubelet's
/// first device list usually reflects a real check.
pub async fn run(
    config: Config,
    probe: Arc<dyn Probe>,
    stop: CancellationToken,
) -> Result<(), RunError> {
    let nonce = endpoint::nonce().map_err(RunError::Nonce)?;
    let timing = &config.timing;
    let (mut monitor, health) = Monitor::new(probe, timing.health_interval, timing.probe_timeout);
    if !monitor.check(&stop).await {
        return Ok(());
    }
    let monitor_stop = CancellationToken::new();
    let mut monitor_task = tokio::spawn(monitor.run(monitor_stop.clone()));
    let supervisor = Supervisor { config, nonce, health, stop };
    let result = tokio::select! {
        () = supervisor.run() => Ok(()),
        _ = &mut monitor_task => Err(RunError::MonitorEnded),
    };
    monitor_stop.cancel();
    if !monitor_task.is_finished() {
        let _ = monitor_task.await;
    }
    result
}

struct Supervisor {
    config: Config,
    nonce: u64,
    health: HealthHandle,
    stop: CancellationToken,
}

/// How a generation ended.
enum Outcome {
    Stopped,
    Failed(String),
}

impl Supervisor {
    async fn run(&self) {
        let timing = &self.config.timing;
        let mut backoff = Backoff::new(timing.backoff_initial, timing.backoff_max);
        let mut generation: u32 = 0;
        loop {
            let outcome = self.generation(generation, &mut backoff).await;
            generation = generation.wrapping_add(1);
            match outcome {
                Outcome::Stopped => return,
                Outcome::Failed(reason) => {
                    let delay = backoff.next_delay();
                    tracing::warn!("{reason}; retrying in {delay:?}");
                    tokio::select! {
                        () = self.stop.cancelled() => return,
                        () = tokio::time::sleep(delay) => {},
                    }
                },
            }
        }
    }

    async fn generation(&self, generation: u32, backoff: &mut Backoff) -> Outcome {
        if self.stop.is_cancelled() {
            return Outcome::Stopped;
        }
        let dir = &self.config.device_plugin_dir;
        self.sweep().await;
        let name = endpoint::basename(&self.stem(), self.nonce, generation);
        let path = dir.join(&name);
        let (listener, identity) = match listen(&path) {
            Ok(bound) => bound,
            Err(err) => {
                return Outcome::Failed(format!("cannot listen on {}: {err}", path.display()));
            },
        };
        let token = self.stop.child_token();
        let service =
            Service::new(Pool::new(self.config.slots), self.health.clone(), token.clone());
        let server = tokio::spawn(Server::builder().serve_with_incoming_shutdown(
            DevicePluginServer::new(service),
            UnixListenerStream::new(listener),
            token.clone().cancelled_owned(),
        ));
        let outcome = tokio::select! {
            () = self.stop.cancelled() => Outcome::Stopped,
            registered = self.register(&name) => match registered {
                Ok(()) => {
                    tracing::info!("registered {} with kubelet as {name}", self.config.resource_name);
                    backoff.reset();
                    self.stop.cancelled().await;
                    Outcome::Stopped
                },
                Err(reason) => Outcome::Failed(reason),
            },
        };
        self.teardown(&token, server, &path, identity).await;
        outcome
    }

    /// Registers the plugin socket `name`. The deadline also bounds kubelet's dial back to the plugin, which it
    /// makes before answering.
    async fn register(&self, name: &str) -> Result<(), String> {
        let kubelet = self.config.device_plugin_dir.join(KUBELET_SOCKET);
        let deadline = self.config.timing.register_timeout;
        let request = RegisterRequest {
            version: VERSION.into(),
            endpoint: name.into(),
            resource_name: self.config.resource_name.to_string(),
            options: Some(plugin::options()),
        };
        let attempt = async {
            let channel = Endpoint::from_static("http://kubelet")
                .timeout(deadline)
                .connect_with_connector(tower::service_fn(move |_| {
                    let kubelet = kubelet.clone();
                    async move { UnixStream::connect(kubelet).await.map(TokioIo::new) }
                }))
                .await
                .map_err(|err| format!("cannot connect to kubelet: {}", chain(&err)))?;
            RegistrationClient::new(channel)
                .register(request)
                .await
                .map_err(|status| format!("kubelet refused registration: {}", status.message()))?;
            Ok(())
        };
        tokio::time::timeout(deadline, attempt)
            .await
            .map_err(|_| format!("kubelet did not answer registration within {deadline:?}"))?
    }

    /// Stops a generation's server and removes its socket, unless something else has replaced it.
    async fn teardown(
        &self,
        token: &CancellationToken,
        mut server: JoinHandle<Result<(), tonic::transport::Error>>,
        path: &Path,
        identity: (u64, u64),
    ) {
        token.cancel();
        match tokio::time::timeout(self.config.timing.shutdown_grace, &mut server).await {
            Ok(Ok(Ok(()))) => {},
            Ok(Ok(Err(err))) => tracing::warn!("the plugin server failed: {}", chain(&err)),
            Ok(Err(err)) => tracing::warn!("the plugin server task failed: {err}"),
            Err(_) => {
                tracing::warn!(
                    "plugin connections were still open after the shutdown grace period"
                );
                server.abort();
            },
        }
        if file_identity(path).is_ok_and(|current| current == identity)
            && let Err(err) = std::fs::remove_file(path)
            && err.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!("cannot remove {}: {err}", path.display());
        }
    }

    /// Removes sockets left by earlier instances of this plugin: those named for this resource that refuse
    /// connections. Live sockets belong to running instances, which kubelet handles alongside this one.
    async fn sweep(&self) {
        let stem = self.stem();
        let Ok(entries) = std::fs::read_dir(&self.config.device_plugin_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_ours =
                entry.file_name().to_str().is_some_and(|n| endpoint::parse(&stem, n).is_some());
            let is_socket =
                std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_socket());
            if !is_ours || !is_socket {
                continue;
            }
            let probe =
                tokio::time::timeout(Duration::from_secs(1), UnixStream::connect(&path)).await;
            match probe {
                Ok(Err(err)) if err.kind() == io::ErrorKind::ConnectionRefused => {
                    match std::fs::remove_file(&path) {
                        Ok(()) => tracing::info!("removed stale socket {}", path.display()),
                        Err(err) if err.kind() == io::ErrorKind::NotFound => {},
                        Err(err) => tracing::warn!("cannot remove {}: {err}", path.display()),
                    }
                },
                Ok(Err(err)) if err.kind() == io::ErrorKind::NotFound => {},
                Ok(Err(err)) => tracing::warn!("cannot check {}: {err}", path.display()),
                Ok(Ok(_)) | Err(_) => {},
            }
        }
    }

    fn stem(&self) -> String {
        self.config.resource_name.socket_stem()
    }
}

fn listen(path: &PathBuf) -> io::Result<(UnixListener, (u64, u64))> {
    let listener = UnixListener::bind(path)?;
    Ok((listener, file_identity(path)?))
}

fn file_identity(path: &Path) -> io::Result<(u64, u64)> {
    let metadata = std::fs::symlink_metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// An error and its sources, since transport errors alone say little.
fn chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text = format!("{text}: {cause}");
        source = cause.source();
    }
    text
}

/// Delays that double from `initial` up to `max`.
#[derive(Debug)]
struct Backoff {
    initial: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    const fn new(initial: Duration, max: Duration) -> Self {
        Self { initial, max, next: initial }
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(self.max);
        delay
    }

    const fn reset(&mut self) {
        self.next = self.initial;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_its_cap_and_resets() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(5));
        let delays: Vec<_> = (0..5).map(|_| backoff.next_delay().as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 5, 5]);
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    }
}
