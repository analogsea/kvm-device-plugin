use std::process::ExitCode;

use clap::Parser;
use kvm_device_plugin::config::{Cli, Config};

fn main() -> ExitCode {
    let _config = match Config::try_from(Cli::parse()) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("kvm-device-plugin: {err}");
            return ExitCode::from(2);
        },
    };
    eprintln!("kvm-device-plugin: the runtime is not implemented yet");
    ExitCode::FAILURE
}
