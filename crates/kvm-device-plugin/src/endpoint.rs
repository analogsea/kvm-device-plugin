//! Plugin socket names. Each generation binds a fresh name, so kubelet never mistakes a new registration for the
//! connection it is still tearing down, and overlapping plugin instances never share a socket.

use std::io::{self, Read};

/// The longest Unix socket path, including its terminating NUL.
pub const SUN_PATH_LEN: usize =
    size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path);

const NONCE_DIGITS: usize = 16;
const SUFFIX: &str = ".sock";

/// `<stem>-<nonce as 16 hex digits>-<generation>.sock`
#[must_use]
pub fn basename(stem: &str, nonce: u64, generation: u64) -> String {
    format!("{stem}-{nonce:016x}-{generation}{SUFFIX}")
}

/// The longest basename for a stem of `stem_len` bytes.
#[must_use]
pub const fn max_basename_len(stem_len: usize) -> usize {
    stem_len + 1 + NONCE_DIGITS + 1 + u64::MAX.ilog10() as usize + 1 + SUFFIX.len()
}

/// The nonce and generation of a socket `basename` produced for `stem`, or `None` if `name` is anything else.
#[must_use]
pub fn parse(stem: &str, name: &str) -> Option<(u64, u64)> {
    let rest = name.strip_prefix(stem)?.strip_prefix('-')?.strip_suffix(SUFFIX)?;
    let (nonce, generation) = rest.split_once('-')?;
    let lower_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    if nonce.len() != NONCE_DIGITS || !nonce.bytes().all(lower_hex) {
        return None;
    }
    let generation_value: u64 = generation.parse().ok()?;
    if generation_value.to_string() != generation {
        return None;
    }
    Some((u64::from_str_radix(nonce, 16).ok()?, generation_value))
}

/// A random value distinguishing this process's sockets from other instances'. It needs uniqueness, not secrecy.
pub fn nonce() -> io::Result<u64> {
    let mut bytes = [0; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(u64::from_ne_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEM: &str = "example.com_kvm";

    #[test]
    fn basenames_round_trip() {
        for (nonce, generation) in [(0, 0), (0xdead_beef, 7), (u64::MAX, u64::MAX)] {
            let name = basename(STEM, nonce, generation);
            assert!(name.len() <= max_basename_len(STEM.len()));
            assert_eq!(parse(STEM, &name), Some((nonce, generation)), "{name}");
        }
        assert_eq!(basename(STEM, u64::MAX, u64::MAX).len(), max_basename_len(STEM.len()));
    }

    #[test]
    fn parse_rejects_other_names() {
        let nonce = "0123456789abcdef";
        for name in [
            "kubelet.sock".to_owned(),
            format!("{STEM}.sock"),
            format!("{STEM}-{nonce}-1"),
            format!("{STEM}-{nonce}-01.sock"),
            format!("{STEM}-{nonce}-+1.sock"),
            format!("{STEM}-{nonce}-.sock"),
            format!("{STEM}-0123456789ABCDEF-1.sock"),
            format!("{STEM}-0123456789abcde-1.sock"),
            format!("{STEM}x-{nonce}-1.sock"),
            // Another resource whose stem extends ours.
            format!("{STEM}-{nonce}-{nonce}-1.sock"),
        ] {
            assert_eq!(parse(STEM, &name), None, "{name}");
        }
    }
}
