//! The device-plugin gRPC service kubelet calls.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::Stream;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

use crate::health::{Health, HealthHandle};
use crate::pool::Pool;
use crate::proto::v1beta1::device_plugin_server::DevicePlugin;
use crate::proto::v1beta1::{
    AllocateRequest, AllocateResponse, ContainerAllocateResponse, Device, DevicePluginOptions,
    DeviceSpec, Empty, ListAndWatchResponse, PreStartContainerRequest, PreStartContainerResponse,
    PreferredAllocationRequest, PreferredAllocationResponse,
};
use crate::proto::{HEALTHY, UNHEALTHY};

/// The device every allocation exposes, at the same path inside the container.
pub const KVM_DEVICE: &str = "/dev/kvm";

type DeviceStream = Pin<Box<dyn Stream<Item = Result<ListAndWatchResponse, Status>> + Send>>;

/// Serves one generation of the plugin. Cancelling `stop` ends every device stream, so the server can shut down.
#[derive(Debug, Clone)]
pub struct Service {
    pool: Pool,
    health: HealthHandle,
    watchers: Watchers,
    stop: CancellationToken,
}

impl Service {
    #[must_use]
    pub const fn new(
        pool: Pool,
        health: HealthHandle,
        watchers: Watchers,
        stop: CancellationToken,
    ) -> Self {
        Self { pool, health, watchers, stop }
    }
}

/// Counts open device streams.
#[derive(Debug, Clone, Default)]
pub struct Watchers(Arc<AtomicUsize>);

impl Watchers {
    #[must_use]
    pub fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    fn watch(&self) -> WatchGuard {
        self.0.fetch_add(1, Ordering::SeqCst);
        WatchGuard(Arc::clone(&self.0))
    }
}

/// Owned by a device stream, so the count drops however the stream ends, including when the client goes away.
#[derive(Debug)]
struct WatchGuard(Arc<AtomicUsize>);

impl Drop for WatchGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Neither optional call is needed: the plugin has nothing to prepare and no allocation preference.
#[must_use]
pub const fn options() -> DevicePluginOptions {
    DevicePluginOptions { pre_start_required: false, get_preferred_allocation_available: false }
}

#[tonic::async_trait]
impl DevicePlugin for Service {
    async fn get_device_plugin_options(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<DevicePluginOptions>, Status> {
        Ok(Response::new(options()))
    }

    type ListAndWatchStream = DeviceStream;

    async fn list_and_watch(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<DeviceStream>, Status> {
        let guard = self.watchers.watch();
        let stream = watch_devices(self.pool, self.health.subscribe(), self.stop.clone(), guard);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_preferred_allocation(
        &self,
        _request: Request<PreferredAllocationRequest>,
    ) -> Result<Response<PreferredAllocationResponse>, Status> {
        Err(Status::unimplemented("this plugin has no allocation preference"))
    }

    async fn allocate(
        &self,
        request: Request<AllocateRequest>,
    ) -> Result<Response<AllocateResponse>, Status> {
        let request = request.into_inner();
        validate(self.pool, &request).map_err(Status::invalid_argument)?;
        if let Health::Unhealthy(reason) = self.health.current() {
            self.health.recheck();
            return Err(Status::failed_precondition(format!("KVM is unhealthy: {reason}")));
        }
        Ok(Response::new(allocation(request.container_requests.len())))
    }

    async fn pre_start_container(
        &self,
        _request: Request<PreStartContainerRequest>,
    ) -> Result<Response<PreStartContainerResponse>, Status> {
        Ok(Response::new(PreStartContainerResponse {}))
    }
}

/// Sends the device list now and whenever KVM becomes healthy or unhealthy, until `stop` is cancelled or the
/// health monitor ends. The client dropping the stream simply drops it; nothing else waits on it.
fn watch_devices(
    pool: Pool,
    health: watch::Receiver<Health>,
    stop: CancellationToken,
    guard: WatchGuard,
) -> impl Stream<Item = Result<ListAndWatchResponse, Status>> + Send + 'static {
    futures_util::stream::unfold(
        (health, stop, guard, true),
        move |(mut health, stop, guard, first)| async move {
            if !first {
                tokio::select! {
                    biased;
                    () = stop.cancelled() => return None,
                    changed = health.changed() => changed.ok()?,
                }
            }
            if stop.is_cancelled() {
                return None;
            }
            let healthy = health.borrow_and_update().is_healthy();
            Some((Ok(devices(pool, healthy)), (health, stop, guard, false)))
        },
    )
}

fn devices(pool: Pool, healthy: bool) -> ListAndWatchResponse {
    let health = if healthy { HEALTHY } else { UNHEALTHY };
    let devices =
        pool.ids().map(|id| Device { id, health: health.into(), topology: None }).collect();
    ListAndWatchResponse { devices }
}

/// Kubelet sends one container per request, each naming distinct pool IDs; anything else is a caller bug.
fn validate(pool: Pool, request: &AllocateRequest) -> Result<(), String> {
    if request.container_requests.is_empty() {
        return Err("the request names no containers".into());
    }
    let mut seen = HashSet::new();
    for (index, container) in request.container_requests.iter().enumerate() {
        if container.devices_ids.is_empty() {
            return Err(format!("container request {index} names no devices"));
        }
        for id in &container.devices_ids {
            let Some(slot) = pool.slot(id) else {
                let shown: String = id.chars().take(64).collect();
                return Err(format!("unknown device ID {shown:?}"));
            };
            if !seen.insert(slot) {
                return Err(format!("device ID {id} is requested more than once"));
            }
        }
    }
    Ok(())
}

/// One response per container, each granting read-write access to the host's KVM device and nothing else.
fn allocation(containers: usize) -> AllocateResponse {
    let response = ContainerAllocateResponse {
        devices: vec![DeviceSpec {
            container_path: KVM_DEVICE.into(),
            host_path: KVM_DEVICE.into(),
            permissions: "rw".into(),
        }],
        ..ContainerAllocateResponse::default()
    };
    AllocateResponse { container_responses: vec![response; containers] }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use futures_util::StreamExt;

    use super::*;
    use crate::health::Monitor;
    use crate::proto::v1beta1::ContainerAllocateRequest;

    const SLOTS: u32 = 4;

    /// A service whose KVM health follows `healthy`, and the monitor that publishes it.
    fn service(healthy: &Arc<AtomicBool>) -> (Service, Monitor, CancellationToken) {
        let flag = Arc::clone(healthy);
        let probe = move || {
            if flag.load(Ordering::SeqCst) {
                Health::Healthy
            } else {
                Health::Unhealthy("synthetic".into())
            }
        };
        let interval = Duration::from_secs(3600);
        let (monitor, handle) = Monitor::new(Arc::new(probe), interval, interval);
        let stop = CancellationToken::new();
        let service = Service::new(Pool::new(SLOTS), handle, Watchers::default(), stop.clone());
        (service, monitor, stop)
    }

    async fn healthy_service() -> (Service, Monitor, CancellationToken) {
        let (service, mut monitor, stop) = service(&Arc::new(AtomicBool::new(true)));
        monitor.check(&stop).await;
        (service, monitor, stop)
    }

    fn request(containers: &[&[&str]]) -> Request<AllocateRequest> {
        let container_requests = containers
            .iter()
            .map(|ids| ContainerAllocateRequest {
                devices_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            })
            .collect();
        Request::new(AllocateRequest { container_requests })
    }

    #[tokio::test]
    async fn allocate_grants_kvm_once_per_container() {
        let (service, _monitor, _stop) = healthy_service().await;
        let expected = ContainerAllocateResponse {
            devices: vec![DeviceSpec {
                container_path: "/dev/kvm".into(),
                host_path: "/dev/kvm".into(),
                permissions: "rw".into(),
            }],
            ..ContainerAllocateResponse::default()
        };
        for (containers, count) in
            [(&[&["kvm-0"][..]][..], 1), (&[&["kvm-1", "kvm-3"][..], &["kvm-2"][..]][..], 2)]
        {
            let response = service.allocate(request(containers)).await.unwrap().into_inner();
            assert_eq!(response.container_responses, vec![expected.clone(); count]);
        }
    }

    #[tokio::test]
    async fn allocate_rejects_malformed_requests() {
        let (service, _monitor, _stop) = healthy_service().await;
        let cases: [&[&[&str]]; 6] = [
            &[],
            &[&[]],
            &[&["kvm-4"]],
            &[&["gpu-0"]],
            &[&["kvm-1", "kvm-1"]],
            &[&["kvm-1"], &["kvm-1"]],
        ];
        for containers in cases {
            let status = service.allocate(request(containers)).await.unwrap_err();
            assert_eq!(status.code(), tonic::Code::InvalidArgument, "{containers:?}");
        }
    }

    // Real time: paused time would reach the monitor's hourly check and pass without the recheck.
    #[tokio::test]
    async fn allocate_refuses_while_unhealthy_and_asks_for_a_recheck() {
        let healthy = Arc::new(AtomicBool::new(false));
        let (service, mut monitor, stop) = service(&healthy);
        monitor.check(&stop).await;
        let status = service.allocate(request(&[&["kvm-0"]])).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("synthetic"));

        healthy.store(true, Ordering::SeqCst);
        let run = tokio::spawn(monitor.run(stop.clone()));
        tokio::time::timeout(Duration::from_secs(5), async {
            while !service.health.current().is_healthy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the failed allocation should trigger an immediate recheck");
        stop.cancel();
        run.await.unwrap();
    }

    #[tokio::test]
    async fn options_request_no_optional_calls() {
        let (service, _monitor, _stop) = healthy_service().await;
        let options = service.get_device_plugin_options(Request::new(Empty {})).await.unwrap();
        assert_eq!(options.into_inner(), DevicePluginOptions::default());
        assert!(service.pre_start_container(Request::new(Default::default())).await.is_ok());
    }

    #[tokio::test]
    async fn watchers_see_the_fixed_pool_and_every_health_change_until_stopped() {
        let healthy = Arc::new(AtomicBool::new(true));
        let (service, mut monitor, stop) = service(&healthy);
        monitor.check(&stop).await;
        let watch = || async { service.list_and_watch(Request::new(Empty {})).await.unwrap() };
        let mut first = watch().await.into_inner();
        let mut second = watch().await.into_inner();
        assert_eq!(service.watchers.count(), 2);
        let all = |health: &str| {
            (0..SLOTS).map(|i| (format!("kvm-{i}"), health.to_owned())).collect::<Vec<_>>()
        };
        let next = |response: Option<Result<ListAndWatchResponse, Status>>| {
            let response = response.unwrap().unwrap();
            assert!(response.devices.iter().all(|device| device.topology.is_none()));
            response
                .devices
                .into_iter()
                .map(|device| (device.id, device.health))
                .collect::<Vec<_>>()
        };
        let healthy_list = all(HEALTHY);
        let unhealthy_list = all(UNHEALTHY);
        for stream in [&mut first, &mut second] {
            assert_eq!(next(stream.next().await), healthy_list);
        }

        service.allocate(request(&[&["kvm-0"]])).await.unwrap();
        healthy.store(false, Ordering::SeqCst);
        monitor.check(&stop).await;
        for stream in [&mut first, &mut second] {
            assert_eq!(next(stream.next().await), unhealthy_list);
        }

        drop(second);
        assert_eq!(service.watchers.count(), 1);
        healthy.store(true, Ordering::SeqCst);
        monitor.check(&stop).await;
        assert_eq!(next(first.next().await), healthy_list);

        stop.cancel();
        let ended = tokio::time::timeout(Duration::from_secs(5), first.next()).await.unwrap();
        assert!(ended.is_none());
        drop(first);
        assert!(watch().await.into_inner().next().await.is_none());
        assert_eq!(service.watchers.count(), 0);
    }

    #[tokio::test]
    async fn watchers_end_when_the_monitor_ends() {
        let (service, monitor, _stop) = healthy_service().await;
        let mut stream = service.list_and_watch(Request::new(Empty {})).await.unwrap().into_inner();
        stream.next().await.unwrap().unwrap();
        drop(monitor);
        assert!(stream.next().await.is_none());
    }
}
