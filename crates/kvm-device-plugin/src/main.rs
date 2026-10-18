use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use kvm_device_plugin::config::{Cli, Config};
use kvm_device_plugin::health::KvmProbe;
use kvm_device_plugin::{privileges, supervisor};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

fn main() -> ExitCode {
    let config = match Config::try_from(Cli::parse()) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("kvm-device-plugin: {err}");
            return ExitCode::from(2);
        },
    };
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_target(false).init();
    // Before the runtime starts any thread, so that every thread starts without them.
    if let Err(err) = privileges::drop_capabilities() {
        tracing::error!("{err}");
        return ExitCode::FAILURE;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!("cannot start the async runtime: {err}");
            return ExitCode::FAILURE;
        },
    };
    let grace = config.timing.shutdown_grace;
    let result = runtime.block_on(async {
        let stop = CancellationToken::new();
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        let canceller = stop.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = terminate.recv() => {},
                _ = interrupt.recv() => {},
            }
            tracing::info!("stopping");
            canceller.cancel();
        });
        tracing::info!(
            "starting: resource {}, {} slots, kubelet directory {}, KVM device {}",
            config.resource_name,
            config.slots,
            config.device_plugin_dir.display(),
            config.kvm_path.display()
        );
        let probe = Arc::new(KvmProbe(config.kvm_path.clone()));
        supervisor::run(config, probe, stop).await.map_err(Box::<dyn std::error::Error>::from)
    });
    // A KVM check blocked in the kernel must not hold the process open.
    runtime.shutdown_timeout(grace);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err}");
            ExitCode::FAILURE
        },
    }
}
