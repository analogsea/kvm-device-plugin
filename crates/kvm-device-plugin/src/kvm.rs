//! The KVM device check: the device must open read-write, speak KVM API version 12 and create a VM.

#[cfg(target_os = "linux")]
pub use linux::probe;

#[cfg(not(target_os = "linux"))]
pub fn probe(_path: &std::path::Path) -> crate::health::Health {
    crate::health::Health::Unhealthy("KVM requires Linux".into())
}

/// The machine type for the test VM. On arm64, type 0 requests a 40-bit IPA space, which hosts with a smaller limit
/// reject, so request the host's limit as QEMU does. Elsewhere, type 0 is the default VM.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn vm_type(arm64: bool, ipa_limit: libc::c_int) -> libc::c_ulong {
    match libc::c_ulong::try_from(ipa_limit) {
        Ok(limit) if arm64 && limit > 0 => limit & 0xff,
        _ => 0,
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    use std::path::Path;

    use crate::health::Health;

    const KVM_GET_API_VERSION: u32 = 0xae00;
    const KVM_CREATE_VM: u32 = 0xae01;
    const KVM_CHECK_EXTENSION: u32 = 0xae03;
    const KVM_CAP_ARM_VM_IPA_SIZE: libc::c_ulong = 165;
    const KVM_API_VERSION: libc::c_int = 12;

    pub fn probe(path: &Path) -> Health {
        match check(path) {
            Ok(()) => Health::Healthy,
            Err(reason) => Health::Unhealthy(reason),
        }
    }

    fn check(path: &Path) -> Result<(), String> {
        let path_text = path.display();
        let metadata =
            fs::metadata(path).map_err(|err| format!("cannot stat {path_text}: {err}"))?;
        if !metadata.file_type().is_char_device() {
            return Err(format!("{path_text} is not a character device"));
        }
        let kvm = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|err| format!("cannot open {path_text} read-write: {err}"))?;
        let version = ioctl(&kvm, KVM_GET_API_VERSION, 0)
            .map_err(|err| format!("{path_text} is not a KVM device: {err}"))?;
        if version != KVM_API_VERSION {
            return Err(format!("{path_text} has unsupported KVM API version {version}"));
        }
        let arm64 = cfg!(target_arch = "aarch64");
        let ipa_limit = if arm64 {
            ioctl(&kvm, KVM_CHECK_EXTENSION, KVM_CAP_ARM_VM_IPA_SIZE).unwrap_or(0)
        } else {
            0
        };
        let vm = ioctl(&kvm, KVM_CREATE_VM, super::vm_type(arm64, ipa_limit))
            .map_err(|err| format!("{path_text} cannot create a VM: {err}"))?;
        // SAFETY: KVM_CREATE_VM returned a new file descriptor that nothing else owns.
        drop(unsafe { OwnedFd::from_raw_fd(vm) });
        Ok(())
    }

    fn ioctl(file: &File, request: u32, arg: libc::c_ulong) -> io::Result<libc::c_int> {
        // SAFETY: these KVM ioctls take an integer argument and never dereference it.
        let result = unsafe { libc::ioctl(file.as_raw_fd(), request as _, arg) };
        if result < 0 { Err(io::Error::last_os_error()) } else { Ok(result) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn reason(path: &str) -> String {
            match probe(Path::new(path)) {
                Health::Unhealthy(reason) => reason,
                Health::Healthy => panic!("{path} is not KVM"),
            }
        }

        #[test]
        fn non_kvm_paths_are_unhealthy_with_a_reason() {
            assert!(reason("/nonexistent/kvm").starts_with("cannot stat"));
            assert!(reason(env!("CARGO_MANIFEST_PATH")).ends_with("is not a character device"));
            assert!(reason("/").ends_with("is not a character device"));
            assert!(reason("/dev/null").contains("is not a KVM device"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm64_requests_the_host_ipa_limit() {
        assert_eq!(vm_type(true, 36), 36);
        assert_eq!(vm_type(true, 0), 0);
        assert_eq!(vm_type(true, -1), 0);
        assert_eq!(vm_type(false, 36), 0);
    }
}
