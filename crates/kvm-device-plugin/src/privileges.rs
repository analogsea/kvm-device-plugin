//! Dropping capabilities. The plugin runs privileged only so that the node's `/dev/kvm` is in its device cgroup, and
//! container runtimes then give it every capability. It needs none: as root it owns the device, kubelet's socket and
//! the device-plugin directory. Capabilities belong to each thread, so this must run before any thread starts.

#[cfg(target_os = "linux")]
pub use linux::drop_capabilities;

/// Elsewhere there are no Linux capabilities to drop.
#[cfg(not(target_os = "linux"))]
pub fn drop_capabilities() -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;

    const CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    const CAP_SETPCAP: u32 = 8;

    #[repr(C)]
    struct Header {
        version: u32,
        pid: libc::c_int,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    /// Empties the calling thread's bounding set if it may, its ambient, inheritable, permitted and effective sets,
    /// and sets `no_new_privs`, then checks that no capability remains. Without `CAP_SETPCAP` the bounding set stays,
    /// but with nothing permitted and no new privileges it can never be used.
    pub fn drop_capabilities() -> Result<(), String> {
        if holds(CAP_SETPCAP).map_err(|err| format!("cannot read capabilities: {err}"))? {
            let mut cap: u32 = 0;
            while prctl(libc::PR_CAPBSET_READ, cap.into(), 0)
                .map_err(|err| format!("cannot read the bounding set: {err}"))?
                .is_some()
            {
                prctl(libc::PR_CAPBSET_DROP, cap.into(), 0).map_err(|err| {
                    format!("cannot drop capability {cap} from the bounding set: {err}")
                })?;
                cap += 1;
            }
        }
        prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL as _, 0)
            .map_err(|err| format!("cannot clear the ambient set: {err}"))?;
        capset(&[Data::default(); 2]).map_err(|err| format!("cannot drop capabilities: {err}"))?;
        prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0)
            .map_err(|err| format!("cannot set no_new_privs: {err}"))?;
        let left = capget().map_err(|err| format!("cannot read capabilities: {err}"))?;
        if left.iter().any(|data| data.effective | data.permitted | data.inheritable != 0) {
            return Err("capabilities remain after dropping them".into());
        }
        Ok(())
    }

    fn holds(cap: u32) -> io::Result<bool> {
        let data = capget()?;
        Ok(data[(cap / 32) as usize].effective & (1 << (cap % 32)) != 0)
    }

    /// `Ok(None)` when the kernel does not know the capability, which ends a walk of the bounding set.
    fn prctl(
        option: libc::c_int,
        arg: libc::c_ulong,
        arg3: libc::c_ulong,
    ) -> io::Result<Option<libc::c_int>> {
        // SAFETY: these prctl options take integer arguments and never dereference them.
        let result =
            unsafe { libc::prctl(option, arg, arg3, 0 as libc::c_ulong, 0 as libc::c_ulong) };
        match result {
            0.. => Ok(Some(result)),
            _ => match io::Error::last_os_error() {
                err if option == libc::PR_CAPBSET_READ
                    && err.raw_os_error() == Some(libc::EINVAL) =>
                {
                    Ok(None)
                },
                err => Err(err),
            },
        }
    }

    fn capget() -> io::Result<[Data; 2]> {
        let mut header = Header { version: CAPABILITY_VERSION_3, pid: 0 };
        let mut data = [Data::default(); 2];
        // SAFETY: version 3 reads two Data structs, which `data` holds; pid 0 is the calling thread.
        let result = unsafe { libc::syscall(libc::SYS_capget, &raw mut header, data.as_mut_ptr()) };
        if result < 0 { Err(io::Error::last_os_error()) } else { Ok(data) }
    }

    fn capset(data: &[Data; 2]) -> io::Result<()> {
        let mut header = Header { version: CAPABILITY_VERSION_3, pid: 0 };
        // SAFETY: version 3 writes two Data structs, which `data` holds; pid 0 is the calling thread.
        let result = unsafe { libc::syscall(libc::SYS_capset, &raw mut header, data.as_ptr()) };
        if result < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn status(field: &str) -> String {
            let status = std::fs::read_to_string("/proc/thread-self/status").expect("status");
            let line = status.lines().find(|line| line.starts_with(field)).expect(field);
            line[field.len()..].trim().to_owned()
        }

        // Each test runs on its own thread, and capabilities and no_new_privs belong to the thread, so dropping them
        // here leaves other tests alone. With CAP_SETPCAP, as root usually has, it also empties the bounding set.
        #[test]
        fn drops_every_capability_and_new_privileges() {
            let could_drop_bounding = holds(CAP_SETPCAP).expect("capget");
            drop_capabilities().expect("drop");
            for field in ["CapInh:", "CapPrm:", "CapEff:", "CapAmb:"] {
                assert_eq!(u64::from_str_radix(&status(field), 16), Ok(0), "{field}");
            }
            assert_eq!(status("NoNewPrivs:"), "1");
            if could_drop_bounding {
                assert_eq!(u64::from_str_radix(&status("CapBnd:"), 16), Ok(0));
            }
            drop_capabilities().expect("dropping again");
        }
    }
}
