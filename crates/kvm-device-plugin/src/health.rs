//! KVM health: a probe, and a monitor that runs it periodically and publishes changes.

use std::future::{self, Future};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;

/// Whether KVM is usable from this process, and why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Unhealthy(String),
}

impl Health {
    #[must_use]
    pub const fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy)
    }
}

/// A pending health check.
pub type Check = Pin<Box<dyn Future<Output = Health> + Send>>;

/// A health check. It may run indefinitely; the monitor bounds how long it waits.
pub trait Probe: Send + Sync + 'static {
    fn check(&self) -> Check;
}

/// An immediate check, for tests.
impl<F: Fn() -> Health + Send + Sync + 'static> Probe for F {
    fn check(&self) -> Check {
        Box::pin(future::ready(self()))
    }
}

/// Checks on a blocking thread that a KVM device can be opened and can create a VM.
#[derive(Debug)]
pub struct KvmProbe(pub PathBuf);

impl Probe for KvmProbe {
    fn check(&self) -> Check {
        let path = self.0.clone();
        let task = tokio::task::spawn_blocking(move || crate::kvm::probe(&path));
        Box::pin(async move {
            task.await.unwrap_or_else(|_| Health::Unhealthy("the KVM check did not finish".into()))
        })
    }
}

/// Read access to the monitor's latest result.
#[derive(Debug, Clone)]
pub struct HealthHandle {
    rx: watch::Receiver<Health>,
    recheck: Arc<Notify>,
}

impl HealthHandle {
    #[must_use]
    pub fn current(&self) -> Health {
        self.rx.borrow().clone()
    }

    /// A receiver that is notified when KVM becomes healthy or unhealthy, but not when only the reason changes.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Health> {
        self.rx.clone()
    }

    /// Asks the monitor to check again now rather than at its next interval.
    pub fn recheck(&self) {
        self.recheck.notify_one();
    }
}

/// Runs the probe periodically. At most one check runs at a time: a check that outlives its timeout counts as
/// unhealthy, and later checks wait for it rather than piling up blocked threads. Rechecks requested while a check
/// runs coalesce into one.
pub struct Monitor {
    probe: Arc<dyn Probe>,
    interval: Duration,
    timeout: Duration,
    tx: watch::Sender<Health>,
    recheck: Arc<Notify>,
    running: Option<Check>,
}

impl Monitor {
    /// A monitor that reports KVM as unhealthy until its first check.
    pub fn new(
        probe: Arc<dyn Probe>,
        interval: Duration,
        timeout: Duration,
    ) -> (Self, HealthHandle) {
        let (tx, rx) = watch::channel(Health::Unhealthy("KVM has not been checked yet".into()));
        let recheck = Arc::new(Notify::new());
        let handle = HealthHandle { rx, recheck: Arc::clone(&recheck) };
        (Self { probe, interval, timeout, tx, recheck, running: None }, handle)
    }

    /// Checks once and publishes the result. Returns `false` if cancelled first.
    pub async fn check(&mut self, stop: &CancellationToken) -> bool {
        let mut task = self.running.take().unwrap_or_else(|| self.probe.check());
        let health = tokio::select! {
            biased;
            () = stop.cancelled() => {
                self.running = Some(task);
                return false;
            },
            health = &mut task => health,
            () = tokio::time::sleep(self.timeout) => {
                self.running = Some(task);
                Health::Unhealthy(format!("the KVM check took longer than {:?}", self.timeout))
            },
        };
        self.publish(health);
        true
    }

    /// Checks at every interval, or sooner on request, until cancelled. A check that timed out still publishes
    /// its result as soon as it finishes, since that is the newest evidence.
    pub async fn run(mut self, stop: CancellationToken) {
        loop {
            let running = &mut self.running;
            let late = tokio::select! {
                biased;
                () = stop.cancelled() => return,
                health = async { running.as_mut().expect("guarded by the condition").await },
                    if running.is_some() => Some(health),
                () = tokio::time::sleep(self.interval) => None,
                () = self.recheck.notified() => None,
            };
            if let Some(health) = late {
                self.running = None;
                self.publish(health);
            } else if !self.check(&stop).await {
                return;
            }
        }
    }

    fn publish(&self, health: Health) {
        self.tx.send_if_modified(|current| {
            if *current == health {
                return false;
            }
            match &health {
                Health::Healthy => tracing::info!("KVM is healthy"),
                Health::Unhealthy(reason) => tracing::warn!("KVM is unhealthy: {reason}"),
            }
            let changed = current.is_healthy() != health.is_healthy();
            *current = health;
            changed
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const INTERVAL: Duration = Duration::from_secs(10);

    fn unhealthy(reason: &str) -> Health {
        Health::Unhealthy(reason.into())
    }

    #[tokio::test(start_paused = true)]
    async fn notifies_only_when_healthiness_changes() {
        let script = Mutex::new(vec![
            Health::Healthy,
            unhealthy("b"),
            unhealthy("a"),
            Health::Healthy,
            Health::Healthy,
        ]);
        let probe = move || script.lock().unwrap().pop().unwrap_or(Health::Healthy);
        let (mut monitor, handle) = Monitor::new(Arc::new(probe), INTERVAL, INTERVAL);
        let mut rx = handle.subscribe();
        let stop = CancellationToken::new();
        assert!(monitor.check(&stop).await);
        assert!(rx.has_changed().unwrap() && rx.borrow_and_update().is_healthy());
        monitor.check(&stop).await;
        assert!(!rx.has_changed().unwrap());
        monitor.check(&stop).await;
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), unhealthy("a"));
        monitor.check(&stop).await;
        assert!(!rx.has_changed().unwrap(), "a new reason alone must not notify");
        assert_eq!(handle.current(), unhealthy("b"));
        monitor.check(&stop).await;
        assert!(rx.has_changed().unwrap() && rx.borrow_and_update().is_healthy());
    }

    #[tokio::test(start_paused = true)]
    async fn runs_at_each_interval_and_on_request_until_cancelled() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let probe = move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Health::Healthy
        };
        let (monitor, handle) = Monitor::new(Arc::new(probe), INTERVAL, INTERVAL);
        let stop = CancellationToken::new();
        let task = tokio::spawn(monitor.run(stop.clone()));
        tokio::time::sleep(INTERVAL * 2 + INTERVAL / 2).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        handle.recheck();
        tokio::time::sleep(INTERVAL / 4).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        stop.cancel();
        task.await.unwrap();
    }

    /// A probe whose checks finish only when the test sends their result, counting calls.
    #[derive(Default)]
    struct Manual {
        calls: AtomicUsize,
        pending: Mutex<Vec<tokio::sync::oneshot::Sender<Health>>>,
    }

    impl Probe for Arc<Manual> {
        fn check(&self) -> Check {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.pending.lock().unwrap().push(tx);
            Box::pin(async { rx.await.unwrap_or(Health::Unhealthy("abandoned".into())) })
        }
    }

    impl Manual {
        fn finish(&self, health: Health) {
            let tx = self.pending.lock().unwrap().pop().expect("a check is running");
            tx.send(health).unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_check_times_out_once_without_stacking() {
        let probe = Arc::new(Manual::default());
        let timeout = Duration::from_secs(1);
        let (monitor, handle) = Monitor::new(Arc::new(Arc::clone(&probe)), INTERVAL, timeout);
        let mut rx = handle.subscribe();
        let stop = CancellationToken::new();
        let task = tokio::spawn(monitor.run(stop.clone()));
        tokio::time::sleep(INTERVAL * 5).await;
        assert_eq!(
            probe.calls.load(Ordering::SeqCst),
            1,
            "later checks must wait for the hung one"
        );
        assert!(
            matches!(handle.current(), Health::Unhealthy(reason) if reason.contains("longer than"))
        );
        assert!(!rx.has_changed().unwrap(), "repeated timeouts must not notify again");

        probe.finish(Health::Healthy);
        tokio::time::sleep(INTERVAL / 10).await;
        assert!(rx.has_changed().unwrap() && rx.borrow_and_update().is_healthy());
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        tokio::time::sleep(INTERVAL).await;
        assert_eq!(probe.calls.load(Ordering::SeqCst), 2, "checks resume after the late result");

        stop.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn rechecks_coalesce_while_a_check_runs() {
        let probe = Arc::new(Manual::default());
        let (monitor, handle) = Monitor::new(Arc::new(Arc::clone(&probe)), INTERVAL, INTERVAL * 10);
        let stop = CancellationToken::new();
        let task = tokio::spawn(monitor.run(stop.clone()));
        handle.recheck();
        tokio::time::sleep(INTERVAL / 10).await;
        for _ in 0..10 {
            handle.recheck();
        }
        probe.finish(Health::Healthy);
        tokio::time::sleep(INTERVAL / 10).await;
        assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
        probe.finish(Health::Healthy);
        tokio::time::sleep(INTERVAL / 2).await;
        assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
        stop.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_check_cannot_block_shutdown() {
        let probe = Arc::new(Manual::default());
        let (mut monitor, _handle) = Monitor::new(Arc::new(Arc::clone(&probe)), INTERVAL, INTERVAL);
        let stop = CancellationToken::new();
        let canceller = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(INTERVAL / 2).await;
            canceller.cancel();
        });
        assert!(!monitor.check(&stop).await);
        monitor.run(stop).await;
    }
}
