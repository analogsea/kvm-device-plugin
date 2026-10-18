//! `kvm-smoke`: runs a few instructions in a KVM guest and checks what the guest wrote. Exits 0 if the guest ran; 5 or
//! 6 if the device's permissions or the device cgroup denied opening `/dev/kvm`; 3 if opening failed otherwise, such
//! as for a missing device; 4 if KVM refused to set up the guest; 1 if the guest did not behave as expected; and 2 for
//! invalid arguments. With `--every SECONDS` it runs a new guest, opening `/dev/kvm` again, every `SECONDS` until one
//! fails, and then exits with that run's status.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let every = match kvm_smoke::interval(&args) {
        Ok(every) => every,
        Err(message) => {
            eprintln!("kvm-smoke: {message}");
            return ExitCode::from(2);
        },
    };
    loop {
        if let Err(status) = once() {
            return status;
        }
        match every {
            Some(every) => std::thread::sleep(every),
            None => return ExitCode::SUCCESS,
        }
    }
}

fn once() -> Result<(), ExitCode> {
    match run() {
        Ok(()) => {
            println!("kvm-smoke: the guest ran and wrote {:#x}", kvm_smoke::MAGIC);
            Ok(())
        },
        Err(Failure::NoKvm(reason, errno)) => {
            eprintln!("kvm-smoke: cannot open /dev/kvm: {reason}");
            Err(ExitCode::from(kvm_smoke::open_failure_status(errno)))
        },
        Err(Failure::Setup(reason)) => {
            eprintln!("kvm-smoke: cannot set up the guest: {reason}");
            Err(ExitCode::from(4))
        },
        Err(Failure::Guest(reason)) => {
            eprintln!("kvm-smoke: the guest did not run as expected: {reason}");
            Err(ExitCode::FAILURE)
        },
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum Failure {
    NoKvm(String, Option<i32>),
    Setup(String),
    Guest(String),
}

#[cfg(not(target_os = "linux"))]
fn run() -> Result<(), Failure> {
    Err(Failure::NoKvm("KVM requires Linux".into(), None))
}

#[cfg(target_os = "linux")]
fn run() -> Result<(), Failure> {
    linux::run()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::OpenOptions;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::ptr;

    use kvm_smoke::{Arch, CODE_ADDRESS, MEMORY_SIZE, Verdict, decode, guest_code, judge};

    use super::Failure;

    const GET_API_VERSION: u32 = 0xae00;
    const CREATE_VM: u32 = 0xae01;
    const CHECK_EXTENSION: u32 = 0xae03;
    const GET_VCPU_MMAP_SIZE: u32 = 0xae04;
    const CREATE_VCPU: u32 = 0xae41;
    const SET_USER_MEMORY_REGION: u32 = 0x4020_ae46;
    const RUN: u32 = 0xae80;
    const CAP_ARM_VM_IPA_SIZE: usize = 165;

    #[repr(C)]
    struct MemoryRegion {
        slot: u32,
        flags: u32,
        guest_phys_addr: u64,
        memory_size: u64,
        userspace_addr: u64,
    }

    const _: () =
        assert!(size_of::<MemoryRegion>() == 32, "SET_USER_MEMORY_REGION encodes 32 bytes");

    const ARCH: Arch = if cfg!(target_arch = "aarch64") { Arch::Aarch64 } else { Arch::X86_64 };

    pub fn run() -> Result<(), Failure> {
        let kvm =
            OpenOptions::new().read(true).write(true).open("/dev/kvm").map_err(|err| {
                Failure::NoKvm(format!("{err}; {}", identity()), err.raw_os_error())
            })?;
        let kvm = kvm.as_raw_fd();
        let guest = |what: &str, err: io::Error| Failure::Setup(format!("{what}: {err}"));
        let version =
            ioctl(kvm, GET_API_VERSION, 0).map_err(|e| guest("KVM_GET_API_VERSION", e))?;
        if version != 12 {
            return Err(Failure::Setup(format!("unsupported KVM API version {version}")));
        }
        let vm_type = match ARCH {
            Arch::Aarch64 => ioctl(kvm, CHECK_EXTENSION, CAP_ARM_VM_IPA_SIZE).unwrap_or(0).max(0),
            Arch::X86_64 => 0,
        };
        let vm = owned(ioctl(kvm, CREATE_VM, vm_type as usize))
            .map_err(|e| guest("KVM_CREATE_VM", e))?;
        if ARCH == Arch::Aarch64 {
            println!(
                "kvm-smoke: created the VM with IPA size {vm_type} (0 is the default, 40 bits)"
            );
        }

        let memory = map(None, MEMORY_SIZE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS)
            .map_err(|e| guest("allocating guest memory", e))?;
        let code = guest_code(ARCH);
        // SAFETY: the mapping is MEMORY_SIZE bytes, far more than CODE_ADDRESS plus the code's length.
        unsafe {
            ptr::copy_nonoverlapping(
                code.as_ptr(),
                memory.address.add(CODE_ADDRESS as usize),
                code.len(),
            )
        };
        let mut region = MemoryRegion {
            slot: 0,
            flags: 0,
            guest_phys_addr: 0,
            memory_size: MEMORY_SIZE as u64,
            userspace_addr: memory.address as u64,
        };
        ioctl_with(vm.as_raw_fd(), SET_USER_MEMORY_REGION, &mut region)
            .map_err(|e| guest("KVM_SET_USER_MEMORY_REGION", e))?;

        let vcpu = owned(ioctl(vm.as_raw_fd(), CREATE_VCPU, 0))
            .map_err(|e| guest("KVM_CREATE_VCPU", e))?;
        let run_size =
            ioctl(kvm, GET_VCPU_MMAP_SIZE, 0).map_err(|e| guest("KVM_GET_VCPU_MMAP_SIZE", e))?;
        let run_size =
            usize::try_from(run_size).map_err(|_| Failure::Setup("bad kvm_run size".into()))?;
        let run_area = map(Some(vcpu.as_raw_fd()), run_size, libc::MAP_SHARED)
            .map_err(|e| guest("mapping kvm_run", e))?;
        arch::prepare(vm.as_raw_fd(), vcpu.as_raw_fd())
            .map_err(|e| guest("preparing the vCPU", e))?;

        let mut wrote = false;
        for _ in 0..4 {
            match ioctl(vcpu.as_raw_fd(), RUN, 0) {
                Ok(_) => {},
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(guest("KVM_RUN", err)),
            }
            // SAFETY: the kernel keeps run_size bytes mapped at run_area while the vCPU exists.
            let area = unsafe { std::slice::from_raw_parts(run_area.address, run_size) };
            match judge(ARCH, &mut wrote, &decode(area)).map_err(Failure::Guest)? {
                Verdict::Ran => return Ok(()),
                Verdict::Continue => {},
            }
        }
        Err(Failure::Guest("the guest did not finish".into()))
    }

    /// Who this process is and what the device allows, to explain a refused open.
    fn identity() -> String {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: these calls only read the process's credentials.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let mut groups = vec![0; 64];
        // SAFETY: the buffer holds as many groups as its length says.
        let count = unsafe { libc::getgroups(64, groups.as_mut_ptr()) };
        groups.truncate(usize::try_from(count).unwrap_or(0));
        let device = match std::fs::metadata("/dev/kvm") {
            Ok(metadata) => format!(
                "/dev/kvm is mode {:o}, owner {}:{}",
                metadata.mode() & 0o7777,
                metadata.uid(),
                metadata.gid()
            ),
            Err(err) => format!("/dev/kvm: {err}"),
        };
        format!("running as {uid}:{gid}, groups {groups:?}; {device}")
    }

    fn owned(fd: io::Result<i32>) -> io::Result<OwnedFd> {
        // SAFETY: KVM returned a new file descriptor that nothing else owns.
        fd.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Memory mapped for one run, unmapped when dropped so that repeated runs don't accumulate it.
    struct Mapping {
        address: *mut u8,
        len: usize,
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: the mapping came from mmap with this length, and nothing uses it after the run that owns it.
            unsafe { libc::munmap(self.address.cast(), self.len) };
        }
    }

    fn map(fd: Option<RawFd>, len: usize, flags: i32) -> io::Result<Mapping> {
        let protection = libc::PROT_READ | libc::PROT_WRITE;
        // SAFETY: a fresh mapping that aliases nothing.
        let address =
            unsafe { libc::mmap(ptr::null_mut(), len, protection, flags, fd.unwrap_or(-1), 0) };
        if address == libc::MAP_FAILED {
            Err(io::Error::last_os_error())
        } else {
            Ok(Mapping { address: address.cast(), len })
        }
    }

    pub fn ioctl(fd: RawFd, request: u32, arg: usize) -> io::Result<i32> {
        // SAFETY: these KVM ioctls take an integer argument and never dereference it.
        check(unsafe { libc::ioctl(fd, request as _, arg) })
    }

    pub fn ioctl_with<T>(fd: RawFd, request: u32, arg: &mut T) -> io::Result<i32> {
        // SAFETY: each caller passes the struct that its ioctl's number encodes, so the kernel stays within it.
        check(unsafe { libc::ioctl(fd, request as _, ptr::from_mut(arg)) })
    }

    fn check(result: i32) -> io::Result<i32> {
        if result < 0 { Err(io::Error::last_os_error()) } else { Ok(result) }
    }

    #[cfg(target_arch = "x86_64")]
    mod arch {
        use std::io;
        use std::os::fd::RawFd;

        use kvm_smoke::CODE_ADDRESS;

        use super::ioctl_with;

        const GET_REGS: u32 = 0x8090_ae81;
        const SET_REGS: u32 = 0x4090_ae82;
        const GET_SREGS: u32 = 0x8138_ae83;
        const SET_SREGS: u32 = 0x4138_ae84;

        /// Starts the vCPU in real mode at the code: CS's base and selector 0, RIP at the code, RFLAGS' reserved bit.
        pub fn prepare(_vm: RawFd, vcpu: RawFd) -> io::Result<()> {
            // struct kvm_sregs, 312 bytes, begins with CS: u64 base, u32 limit, u16 selector.
            let mut sregs = [0u8; 312];
            ioctl_with(vcpu, GET_SREGS, &mut sregs)?;
            sregs[..8].fill(0);
            sregs[12..14].fill(0);
            ioctl_with(vcpu, SET_SREGS, &mut sregs)?;
            // struct kvm_regs: 16 general registers, then RIP and RFLAGS.
            let mut regs = [0u64; 18];
            ioctl_with(vcpu, GET_REGS, &mut regs)?;
            regs[16] = CODE_ADDRESS;
            regs[17] = 0x2;
            ioctl_with(vcpu, SET_REGS, &mut regs)?;
            Ok(())
        }
    }

    #[cfg(target_arch = "aarch64")]
    mod arch {
        use std::io;
        use std::os::fd::RawFd;

        use kvm_smoke::{ARM64_PC_OFFSET, CODE_ADDRESS, arm64_core_register};

        use super::ioctl_with;

        const _: () = assert!(size_of::<VcpuInit>() == 32, "the vCPU init ioctls encode 32 bytes");
        const _: () = assert!(size_of::<OneReg>() == 16, "SET_ONE_REG encodes 16 bytes");

        const ARM_PREFERRED_TARGET: u32 = 0x8020_aeaf;
        const ARM_VCPU_INIT: u32 = 0x4020_aeae;
        const SET_ONE_REG: u32 = 0x4010_aeac;

        /// struct kvm_vcpu_init
        #[repr(C)]
        #[derive(Default)]
        struct VcpuInit {
            target: u32,
            features: [u32; 7],
        }

        /// struct kvm_one_reg
        #[repr(C)]
        struct OneReg {
            id: u64,
            address: u64,
        }

        /// Initialises the vCPU as the host prefers, which starts it at EL1 with the MMU off, and points PC at the code.
        pub fn prepare(vm: RawFd, vcpu: RawFd) -> io::Result<()> {
            let mut init = VcpuInit::default();
            ioctl_with(vm, ARM_PREFERRED_TARGET, &mut init)?;
            ioctl_with(vcpu, ARM_VCPU_INIT, &mut init)?;
            let pc = CODE_ADDRESS;
            let mut reg =
                OneReg { id: arm64_core_register(ARM64_PC_OFFSET), address: &raw const pc as u64 };
            ioctl_with(vcpu, SET_ONE_REG, &mut reg)?;
            Ok(())
        }
    }
}
