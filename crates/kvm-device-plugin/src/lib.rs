//! A Kubernetes device plugin that exposes the host's `/dev/kvm` to containers. The library exists for the binary
//! and its tests; it is not a stable API.

pub mod config;
pub mod endpoint;
pub mod pool;
pub mod proto;
