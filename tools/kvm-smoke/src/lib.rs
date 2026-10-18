//! The guest program and how to judge its exits. The guest writes 0x2a to a port on x86-64 or to an unbacked address
//! on arm64, which only a vCPU really executing guest code can do. Linux-only code in `main.rs` runs it.

/// The exit status when `/dev/kvm` cannot be opened: 5 if its owner, group and mode deny access (`EACCES`), 6 if the
/// device cgroup does (`EPERM`), and 3 for anything else, such as a missing device.
#[must_use]
pub fn open_failure_status(errno: Option<i32>) -> u8 {
    match errno {
        Some(libc::EACCES) => 5,
        Some(libc::EPERM) => 6,
        _ => 3,
    }
}

/// The value the guest writes.
pub const MAGIC: u8 = 0x2a;
/// Where the guest's code is loaded, in guest-physical memory starting at 0.
pub const CODE_ADDRESS: u64 = 0x1000;
/// The guest's memory size; 2 MiB covers hosts with any page size.
pub const MEMORY_SIZE: usize = 0x20_0000;
/// The x86-64 guest's I/O port.
pub const PORT: u16 = 0x10;
/// The arm64 guest's MMIO address, outside its memory.
pub const MMIO_ADDRESS: u64 = 0x1000_0000;

/// The guest's code for an architecture.
#[must_use]
pub fn guest_code(arch: Arch) -> Vec<u8> {
    match arch {
        // mov al, 0x2a; out 0x10, al; hlt
        Arch::X86_64 => vec![0xb0, MAGIC, 0xe6, 0x10, 0xf4],
        // movz x0, #0x2a; movz x1, #0x1000, lsl #16; str x0, [x1]; b .
        Arch::Aarch64 => [0xd280_0540_u32, 0xd2a2_0001, 0xf900_0020, 0x1400_0000]
            .iter()
            .flat_map(|instruction| instruction.to_le_bytes())
            .collect(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

/// How a `KVM_RUN` ended, from the start of the vCPU's `kvm_run` area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    PortOut { port: u16, data: Vec<u8> },
    Halt,
    MmioWrite { address: u64, data: Vec<u8> },
    Other(u32),
}

const EXIT_IO: u32 = 2;
const EXIT_HLT: u32 = 5;
const EXIT_MMIO: u32 = 6;
const IO_OUT: u8 = 1;
const EXIT_REASON: usize = 8;
const EXIT_DATA: usize = 32;

/// Decodes an exit from the `kvm_run` area. Port data lives elsewhere in the area, at an offset the exit gives.
#[must_use]
pub fn decode(run: &[u8]) -> Exit {
    let u16_at = |at: usize| u16::from_ne_bytes([run[at], run[at + 1]]);
    let u32_at = |at: usize| u32::from_ne_bytes(run[at..at + 4].try_into().unwrap());
    let u64_at = |at: usize| u64::from_ne_bytes(run[at..at + 8].try_into().unwrap());
    let reason = u32_at(EXIT_REASON);
    match reason {
        EXIT_HLT => Exit::Halt,
        // struct { u8 direction, size; u16 port; u32 count; u64 data_offset; }
        EXIT_IO if run[EXIT_DATA] == IO_OUT => {
            let size = usize::from(run[EXIT_DATA + 1]) * u32_at(EXIT_DATA + 4) as usize;
            let offset = usize::try_from(u64_at(EXIT_DATA + 8)).unwrap_or(usize::MAX);
            match run.get(offset..offset.saturating_add(size)) {
                Some(data) => Exit::PortOut { port: u16_at(EXIT_DATA + 2), data: data.to_vec() },
                None => Exit::Other(reason),
            }
        },
        // struct { u64 phys_addr; u8 data[8]; u32 len; u8 is_write; }
        EXIT_MMIO if run[EXIT_DATA + 20] != 0 => {
            let len = (u32_at(EXIT_DATA + 16) as usize).min(8);
            let data = run[EXIT_DATA + 8..EXIT_DATA + 8 + len].to_vec();
            Exit::MmioWrite { address: u64_at(EXIT_DATA), data }
        },
        other => Exit::Other(other),
    }
}

/// What the guest did, if the exits so far prove it ran; `Err` describes the first unexpected exit.
pub enum Verdict {
    /// The guest proved it ran.
    Ran,
    /// Run the vCPU again.
    Continue,
}

/// Judges the next exit: x86-64 must write the magic byte to the port and then halt; arm64 must write it to the
/// MMIO address.
pub fn judge(arch: Arch, wrote: &mut bool, exit: &Exit) -> Result<Verdict, String> {
    let magic = |data: &[u8]| data.first() == Some(&MAGIC) && data[1..].iter().all(|b| *b == 0);
    match (arch, exit, *wrote) {
        (Arch::X86_64, Exit::PortOut { port: PORT, data }, false)
            if data.len() == 1 && magic(data) =>
        {
            *wrote = true;
            Ok(Verdict::Continue)
        },
        (Arch::X86_64, Exit::Halt, true) => Ok(Verdict::Ran),
        // `str x0` writes all 8 bytes.
        (Arch::Aarch64, Exit::MmioWrite { address: MMIO_ADDRESS, data }, false)
            if data.len() == 8 && magic(data) =>
        {
            *wrote = true;
            Ok(Verdict::Ran)
        },
        _ => Err(format!("unexpected exit {exit:?}")),
    }
}

/// The `KVM_{GET,SET}_ONE_REG` ID of an arm64 core register at `offset` bytes into `struct kvm_regs`.
#[must_use]
pub const fn arm64_core_register(offset: u64) -> u64 {
    const ARM64: u64 = 0x6000_0000_0000_0000;
    const SIZE_U64: u64 = 0x0030_0000_0000_0000;
    const CORE: u64 = 0x0010 << 16;
    ARM64 | SIZE_U64 | CORE | (offset / 4)
}

/// `struct user_pt_regs`: `regs[31]`, `sp`, `pc`, `pstate`.
pub const ARM64_PC_OFFSET: u64 = 32 * 8;

#[cfg(test)]
mod tests {
    use super::*;

    fn run_area(reason: u32, payload: &[(usize, &[u8])]) -> Vec<u8> {
        let mut run = vec![0; 4096];
        run[EXIT_REASON..EXIT_REASON + 4].copy_from_slice(&reason.to_ne_bytes());
        for (at, bytes) in payload {
            run[*at..*at + bytes.len()].copy_from_slice(bytes);
        }
        run
    }

    fn port_out(port: u16, data: &[u8]) -> Vec<u8> {
        let header =
            [&[IO_OUT, 1][..], &port.to_ne_bytes(), &1u32.to_ne_bytes(), &512u64.to_ne_bytes()]
                .concat();
        run_area(EXIT_IO, &[(EXIT_DATA, &header), (512, data)])
    }

    fn mmio_write(address: u64, data: u64) -> Vec<u8> {
        let header =
            [&address.to_ne_bytes()[..], &data.to_ne_bytes(), &8u32.to_ne_bytes(), &[1]].concat();
        run_area(EXIT_MMIO, &[(EXIT_DATA, &header)])
    }

    #[test]
    fn guest_code_matches_its_assembly() {
        assert_eq!(guest_code(Arch::X86_64), [0xb0, 0x2a, 0xe6, 0x10, 0xf4]);
        let arm = guest_code(Arch::Aarch64);
        assert_eq!(arm.len(), 16);
        assert_eq!(&arm[..4], &[0x40, 0x05, 0x80, 0xd2], "movz x0, #0x2a");
        // movz x1, #imm16, lsl #16 loads the MMIO address.
        let movz = u32::from_le_bytes(arm[4..8].try_into().unwrap());
        assert_eq!(u64::from((movz >> 5) & 0xffff) << 16, MMIO_ADDRESS);
    }

    #[test]
    fn decodes_port_mmio_and_halt_exits() {
        assert_eq!(
            decode(&port_out(PORT, &[MAGIC])),
            Exit::PortOut { port: PORT, data: vec![MAGIC] }
        );
        assert_eq!(decode(&run_area(EXIT_HLT, &[])), Exit::Halt);
        let mmio =
            Exit::MmioWrite { address: MMIO_ADDRESS, data: [MAGIC, 0, 0, 0, 0, 0, 0, 0].to_vec() };
        assert_eq!(decode(&mmio_write(MMIO_ADDRESS, u64::from(MAGIC))), mmio);
        assert_eq!(decode(&run_area(9, &[])), Exit::Other(9));
    }

    #[test]
    fn only_the_exact_guest_behaviour_passes() {
        let run = |arch, exits: &[Vec<u8>]| -> Result<bool, String> {
            let mut wrote = false;
            for exit in exits {
                if let Verdict::Ran = judge(arch, &mut wrote, &decode(exit))? {
                    return Ok(true);
                }
            }
            Ok(false)
        };
        let halt = run_area(EXIT_HLT, &[]);
        assert_eq!(run(Arch::X86_64, &[port_out(PORT, &[MAGIC]), halt.clone()]), Ok(true));
        assert!(run(Arch::X86_64, std::slice::from_ref(&halt)).is_err(), "halting without writing");
        assert!(run(Arch::X86_64, &[port_out(PORT, &[0x2b])]).is_err());
        assert!(run(Arch::X86_64, &[port_out(PORT + 1, &[MAGIC])]).is_err());
        assert_eq!(run(Arch::Aarch64, &[mmio_write(MMIO_ADDRESS, u64::from(MAGIC))]), Ok(true));
        assert!(run(Arch::Aarch64, &[mmio_write(MMIO_ADDRESS, 0x12a)]).is_err());
        assert!(run(Arch::Aarch64, &[mmio_write(MMIO_ADDRESS + 8, u64::from(MAGIC))]).is_err());
        assert!(run(Arch::Aarch64, &[halt]).is_err());
    }

    #[test]
    fn permission_failures_are_told_apart_from_missing_devices() {
        assert_eq!(open_failure_status(Some(libc::EACCES)), 5);
        assert_eq!(open_failure_status(Some(libc::EPERM)), 6);
        assert_eq!(open_failure_status(Some(libc::ENOENT)), 3);
        assert_eq!(open_failure_status(Some(libc::ENXIO)), 3);
        assert_eq!(open_failure_status(None), 3);
    }

    #[test]
    fn arm64_register_ids_match_the_kernel_encoding() {
        // KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM_CORE | KVM_REG_ARM_CORE_REG(regs.pc)
        assert_eq!(arm64_core_register(ARM64_PC_OFFSET), 0x6030_0000_0010_0040);
    }
}
