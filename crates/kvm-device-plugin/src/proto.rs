//! Kubernetes device-plugin API v1beta1 bindings, generated from the vendored proto.

#[allow(clippy::all, clippy::pedantic)]
pub mod v1beta1;

/// The only API version kubelet accepts from device plugins.
pub const VERSION: &str = "v1beta1";
/// The health value of a usable device.
pub const HEALTHY: &str = "Healthy";
/// The health value of an unusable device.
pub const UNHEALTHY: &str = "Unhealthy";
/// Kubelet's registration socket, within the device-plugin directory.
pub const KUBELET_SOCKET: &str = "kubelet.sock";
