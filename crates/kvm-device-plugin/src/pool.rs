//! The fixed pool of device IDs. Every ID stands for one share of the same `/dev/kvm`.

const PREFIX: &str = "kvm-";

/// IDs `kvm-0` to `kvm-<slots - 1>`. They depend only on the slot count, so they survive restarts, and raising the
/// count keeps every existing ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pool {
    slots: u32,
}

impl Pool {
    #[must_use]
    pub const fn new(slots: u32) -> Self {
        Self { slots }
    }

    pub fn ids(&self) -> impl Iterator<Item = String> + use<> {
        (0..self.slots).map(|slot| format!("{PREFIX}{slot}"))
    }

    /// The slot of a pool ID in canonical form, or `None` if `id` is not in the pool.
    #[must_use]
    pub fn slot(&self, id: &str) -> Option<u32> {
        let digits = id.strip_prefix(PREFIX)?;
        if !digits.bytes().all(|b| b.is_ascii_digit())
            || (digits.len() > 1 && digits.starts_with('0'))
        {
            return None;
        }
        digits.parse().ok().filter(|slot| *slot < self.slots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_stable_and_recognized() {
        let ids: Vec<_> = Pool::new(12).ids().collect();
        assert_eq!(ids.first().map(String::as_str), Some("kvm-0"));
        assert_eq!(ids.last().map(String::as_str), Some("kvm-11"));
        assert_eq!(ids.iter().collect::<std::collections::HashSet<_>>().len(), 12);
        assert!(Pool::new(20).ids().take(12).eq(ids.iter().cloned()));
        for (slot, id) in ids.iter().enumerate() {
            assert_eq!(Pool::new(12).slot(id), Some(u32::try_from(slot).unwrap()));
        }
    }

    #[test]
    fn slot_rejects_ids_outside_the_pool() {
        let pool = Pool::new(12);
        for id in [
            "kvm-12",
            "kvm-01",
            "kvm-+1",
            "kvm--1",
            "kvm-",
            "kvm-1 ",
            "KVM-1",
            "1",
            "",
            "kvm-99999999999",
        ] {
            assert_eq!(pool.slot(id), None, "{id:?}");
        }
    }
}
