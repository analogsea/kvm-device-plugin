//! Command-line configuration and its validation.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use crate::endpoint;

/// The resource name containers request unless configured otherwise.
pub const DEFAULT_RESOURCE_NAME: &str = "devices.analogsea.ca/kvm";
/// The largest pool, which keeps every device list well under gRPC's default 4 MiB message limit.
pub const MAX_SLOTS: u32 = 10_000;

/// The command line.
#[derive(Debug, Parser)]
#[command(
    name = "kvm-device-plugin",
    version,
    about = "A Kubernetes device plugin that exposes the host's /dev/kvm to containers.",
    after_help = "Each container that requests one unit of the resource holds one slot and receives the host's \
                  /dev/kvm read-write. Every slot grants the same device, so --slots caps how many containers \
                  on the node can hold KVM access at once; it is not a VM, CPU or memory limit."
)]
pub struct Cli {
    /// Extended resource name that containers request.
    #[arg(long, value_name = "DOMAIN/NAME", default_value = DEFAULT_RESOURCE_NAME)]
    pub resource_name: String,
    /// How many containers on this node can hold the resource at once (1 to 10000).
    #[arg(long, value_name = "COUNT", default_value_t = 1000)]
    pub slots: u32,
    /// Kubelet's device-plugin directory, which holds kubelet.sock.
    #[arg(long, value_name = "DIR", default_value = "/var/lib/kubelet/device-plugins")]
    pub device_plugin_dir: PathBuf,
    /// The KVM device this plugin opens to check health. Containers always receive the host's /dev/kvm.
    #[arg(long, value_name = "PATH", default_value = "/dev/kvm")]
    pub kvm_path: PathBuf,
}

/// Validated configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub resource_name: ResourceName,
    pub slots: u32,
    pub device_plugin_dir: PathBuf,
    pub kvm_path: PathBuf,
    pub timing: Timing,
}

/// Deadlines and intervals. Only tests change them.
#[derive(Debug, Clone)]
pub struct Timing {
    /// Bounds connecting to kubelet and its Register call, including kubelet's dial back to the plugin.
    pub register_timeout: Duration,
    /// The first delay after a failed generation; it doubles up to `backoff_max`.
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    /// How often to check the plugin and kubelet sockets.
    pub poll_interval: Duration,
    /// How often to probe KVM.
    pub health_interval: Duration,
    /// How long one probe may take before KVM counts as unhealthy.
    pub probe_timeout: Duration,
    /// How long after registering kubelet may take to start watching devices.
    pub watch_grace: Duration,
    /// How long open connections may take to close on shutdown.
    pub shutdown_grace: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            register_timeout: Duration::from_secs(10),
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            poll_interval: Duration::from_secs(1),
            health_interval: Duration::from_secs(10),
            probe_timeout: Duration::from_secs(5),
            watch_grace: Duration::from_secs(5),
            shutdown_grace: Duration::from_secs(5),
        }
    }
}

impl TryFrom<Cli> for Config {
    type Error = ConfigError;

    fn try_from(cli: Cli) -> Result<Self, ConfigError> {
        let resource_name = ResourceName::new(cli.resource_name)?;
        if !(1..=MAX_SLOTS).contains(&cli.slots) {
            return Err(ConfigError::Slots(cli.slots));
        }
        match cli.device_plugin_dir.to_str() {
            Some("") => return Err(ConfigError::EmptyDevicePluginDir),
            Some(_) => {},
            None => return Err(ConfigError::NonUtf8DevicePluginDir),
        }
        let longest = cli.device_plugin_dir.as_os_str().len()
            + 1
            + endpoint::max_basename_len(resource_name.socket_stem().len());
        if longest >= endpoint::SUN_PATH_LEN {
            return Err(ConfigError::SocketPathTooLong {
                longest,
                limit: endpoint::SUN_PATH_LEN - 1,
            });
        }
        Ok(Self {
            resource_name,
            slots: cli.slots,
            device_plugin_dir: cli.device_plugin_dir,
            kvm_path: cli.kvm_path,
            timing: Timing::default(),
        })
    }
}

/// Why the configuration is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    ResourceName { name: String, reason: &'static str },
    Slots(u32),
    EmptyDevicePluginDir,
    NonUtf8DevicePluginDir,
    SocketPathTooLong { longest: usize, limit: usize },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResourceName { name, reason } => {
                write!(f, "invalid --resource-name {name:?}: {reason}")
            },
            Self::Slots(slots) => {
                write!(f, "invalid --slots {slots}: must be between 1 and {MAX_SLOTS}")
            },
            Self::EmptyDevicePluginDir => write!(f, "--device-plugin-dir must not be empty"),
            Self::NonUtf8DevicePluginDir => write!(f, "--device-plugin-dir must be valid UTF-8"),
            Self::SocketPathTooLong { longest, limit } => write!(
                f,
                "plugin socket paths could reach {longest} bytes, over the {limit}-byte Unix socket limit; \
                 shorten --device-plugin-dir or --resource-name"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// A Kubernetes extended resource name, validated as kubelet validates device-plugin registrations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceName(String);

impl ResourceName {
    pub fn new(name: String) -> Result<Self, ConfigError> {
        match extended_resource_name_error(&name) {
            None => Ok(Self(name)),
            Some(reason) => Err(ConfigError::ResourceName { name, reason }),
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name with `/` replaced by `_`, for socket file names. Domains cannot contain `_`, so this is injective.
    #[must_use]
    pub fn socket_stem(&self) -> String {
        self.0.replace('/', "_")
    }
}

impl fmt::Display for ResourceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Mirrors kubelet's `IsExtendedResourceName`: not a native resource, and `requests.<name>` is a qualified name.
fn extended_resource_name_error(name: &str) -> Option<&'static str> {
    let Some((domain, short)) = name.split_once('/') else {
        return Some("must have the form <domain>/<name>");
    };
    if name.contains("kubernetes.io/") {
        return Some("the kubernetes.io/ namespace is reserved for native resources");
    }
    if name.starts_with("requests.") {
        return Some("must not start with \"requests.\"");
    }
    if short.contains('/') {
        return Some("must contain exactly one \"/\"");
    }
    let prefix = format!("requests.{domain}");
    if prefix.len() > 253 {
        return Some("the domain must be at most 244 characters");
    }
    if !is_dns1123_subdomain(&prefix) {
        return Some("the domain must be a lowercase DNS subdomain");
    }
    if short.is_empty() || short.len() > 63 {
        return Some("the part after \"/\" must be 1 to 63 characters");
    }
    if !is_qualified_name_part(short) {
        return Some(
            "the part after \"/\" must consist of alphanumerics, '-', '_' or '.', \
             and start and end with an alphanumeric",
        );
    }
    None
}

/// `[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*`
fn is_dns1123_subdomain(name: &str) -> bool {
    name.split('.').all(|label| {
        let bytes = label.as_bytes();
        let alnum = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        bytes.first().is_some_and(alnum)
            && bytes.last().is_some_and(alnum)
            && bytes.iter().all(|b| alnum(b) || *b == b'-')
    })
}

/// `([A-Za-z0-9][-A-Za-z0-9_.]*)?[A-Za-z0-9]`
fn is_qualified_name_part(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(resource_name: &str, slots: u32, dir: &str) -> Cli {
        Cli {
            resource_name: resource_name.into(),
            slots,
            device_plugin_dir: dir.into(),
            kvm_path: "/dev/kvm".into(),
        }
    }

    #[test]
    fn resource_names_follow_kubelet_rules() {
        let max_domain = format!("{}.io", "a".repeat(241));
        let long_domain = format!("{}.io", "a".repeat(242));
        let valid = [
            DEFAULT_RESOURCE_NAME.to_owned(),
            "k8s.io/kvm".to_owned(),
            "example.com/Kvm_1.x-y".to_owned(),
            format!("example.com/{}", "a".repeat(63)),
            format!("{max_domain}/kvm"),
        ];
        for name in valid {
            assert!(extended_resource_name_error(&name).is_none(), "{name}");
        }
        let invalid = [
            "kvm".to_owned(),
            "kubernetes.io/kvm".to_owned(),
            "foo.kubernetes.io/kvm".to_owned(),
            "requests.example.com/kvm".to_owned(),
            "example.com/a/b".to_owned(),
            "/kvm".to_owned(),
            "Example.com/kvm".to_owned(),
            "example..com/kvm".to_owned(),
            "-example.com/kvm".to_owned(),
            "example_com/kvm".to_owned(),
            "example.com/".to_owned(),
            "example.com/-kvm".to_owned(),
            "example.com/kvm.".to_owned(),
            "example.com/k vm".to_owned(),
            format!("example.com/{}", "a".repeat(64)),
            format!("{long_domain}/kvm"),
        ];
        for name in invalid {
            assert!(extended_resource_name_error(&name).is_some(), "{name}");
        }
    }

    #[test]
    fn slots_are_bounded() {
        for slots in [1, MAX_SLOTS] {
            assert!(Config::try_from(cli(DEFAULT_RESOURCE_NAME, slots, "/d")).is_ok());
        }
        for slots in [0, MAX_SLOTS + 1] {
            let err = Config::try_from(cli(DEFAULT_RESOURCE_NAME, slots, "/d")).unwrap_err();
            assert_eq!(err, ConfigError::Slots(slots));
        }
    }

    #[test]
    fn socket_paths_must_fit_sun_path() {
        let stem = DEFAULT_RESOURCE_NAME.len();
        let fits = endpoint::SUN_PATH_LEN - 2 - endpoint::max_basename_len(stem);
        let dir = |len| "d".repeat(len);
        assert!(Config::try_from(cli(DEFAULT_RESOURCE_NAME, 1, &dir(fits))).is_ok());
        let err = Config::try_from(cli(DEFAULT_RESOURCE_NAME, 1, &dir(fits + 1))).unwrap_err();
        assert!(matches!(err, ConfigError::SocketPathTooLong { .. }), "{err}");
        assert!(matches!(
            Config::try_from(cli(DEFAULT_RESOURCE_NAME, 1, "")),
            Err(ConfigError::EmptyDevicePluginDir)
        ));
        let mut non_utf8 = cli(DEFAULT_RESOURCE_NAME, 1, "");
        non_utf8.device_plugin_dir =
            <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"/d\xff").into();
        assert_eq!(Config::try_from(non_utf8).unwrap_err(), ConfigError::NonUtf8DevicePluginDir);
    }

    #[test]
    fn socket_stem_replaces_the_slash() {
        let name = ResourceName::new(DEFAULT_RESOURCE_NAME.into()).unwrap();
        assert_eq!(name.socket_stem(), "devices.analogsea.ca_kvm");
    }
}
