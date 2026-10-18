//! A Kubernetes device plugin that exposes the host's `/dev/kvm` to containers. The library exists for the binary
//! and its tests; it is not a stable API.

pub mod config;
pub mod endpoint;
pub mod health;
mod kvm;
pub mod plugin;
pub mod pool;
pub mod privileges;
pub mod proto;
pub mod supervisor;
