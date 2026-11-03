//! Resource-pool capacities as a person writes them: the one parser behind
//! `.hephconfig`'s `resources:`, `--resource name=n`, `-j` and
//! `HEPH_RESOURCE_<NAME>`, so `50%` or `24G` cannot mean two things depending
//! on which surface it was typed into. Also the one check of a pool name's
//! syntax (`hplugin::provider::PoolName` delegates here).

/// Micro-CPUs in one CPU: the `cpu` pool's unit.
pub const MICROS_PER_CPU: u64 = 1_000_000;

/// The CPU pool's name.
pub const CPU: &str = "cpu";
/// The memory pool's name.
pub const MEMORY: &str = "memory";

/// The longest a pool name may be.
pub const MAX_POOL_NAME_LEN: usize = 64;

/// Whether `s` is a well-formed pool name: `[a-z][a-z0-9_]{0,63}`.
pub fn is_pool_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && s.len() <= MAX_POOL_NAME_LEN
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// A pool's capacity as written, before the engine's default is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capacity {
    /// An amount in the pool's unit: micro-CPUs for `cpu`, bytes for
    /// `memory`, a count for a custom pool. `0` means "the pool exists; the
    /// run sets its capacity" (config only).
    Amount(u64),
    /// A percentage (> 0) of the engine's default capacity; `cpu` and
    /// `memory` only, the pools that have one.
    Percent(u64),
}

/// Whether 0 is an accepted capacity: a config file may declare a pool the
/// run sets (`gpu: 0`), a per-run override must give it a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zero {
    Allowed,
    Refused,
}

impl Capacity {
    /// Parse `s` as `pool`'s capacity.
    ///
    /// - `N%`: ASCII digits then one `%`, N ≥ 1, `cpu` and `memory` only.
    ///   Over 100 is an overcommit by choice and accepted.
    /// - `cpu`: whole CPUs, ASCII digits, at most `u32::MAX`.
    /// - `memory`: a memory size ([`parse_memory_size`](crate::units::parse_memory_size)).
    /// - a custom pool: a count, ASCII digits, at most `u32::MAX`.
    ///
    /// Errors describe the value, not where it came from; the caller names
    /// the flag, variable or config key.
    pub fn parse(pool: &str, s: &str, zero: Zero) -> anyhow::Result<Self> {
        let has_base = pool == CPU || pool == MEMORY;
        if let Some(digits) = s.strip_suffix('%') {
            if !has_base {
                anyhow::bail!(
                    "{s:?} is a percentage, but pool {pool} has no default to take it of: give it a count"
                );
            }
            let pct = whole(digits)
                .ok_or_else(|| anyhow::anyhow!("{s:?} is not a percentage: write e.g. 50%"))?;
            if pct == 0 {
                anyhow::bail!("{s:?} leaves pool {pool} empty: a percentage is at least 1%");
            }
            return Ok(Self::Percent(pct));
        }
        let amount = if pool == MEMORY {
            crate::units::parse_memory_size(s)?
        } else {
            let n = whole(s)
                .filter(|&n| n <= u64::from(u32::MAX))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{s:?} is not a capacity: write a whole number from 0 to {}{}",
                        u32::MAX,
                        if has_base {
                            ", or a percentage such as 50%"
                        } else {
                            ""
                        }
                    )
                })?;
            if pool == CPU {
                n.saturating_mul(MICROS_PER_CPU)
            } else {
                n
            }
        };
        if amount == 0 && zero == Zero::Refused {
            anyhow::bail!("{s:?} is not a capacity for a run: it is at least 1");
        }
        Ok(Self::Amount(amount))
    }

    /// The capacity in the pool's unit, given the engine's default for it
    /// (only read for a percentage). `cpu` rounds to the nearest micro-CPU and
    /// is at least 1; `memory` floors to bytes. Saturates at `u64::MAX`.
    pub fn resolve(self, pool: &str, default: impl FnOnce() -> u64) -> u64 {
        match self {
            Self::Amount(n) => n,
            Self::Percent(pct) => {
                let scaled = u128::from(default()) * u128::from(pct);
                let n = if pool == CPU {
                    ((scaled + 50) / 100).max(1)
                } else {
                    scaled / 100
                };
                u64::try_from(n).unwrap_or(u64::MAX)
            }
        }
    }
}

/// `s` as a whole number when it is only ASCII digits.
fn whole(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_names() {
        for ok in ["cpu", "gpu", "a", "docker_2", &"a".repeat(64)] {
            assert!(is_pool_name(ok), "{ok}");
        }
        for bad in ["", "Cpu", "cpu ", "1a", "_a", "a-b", &"a".repeat(65)] {
            assert!(!is_pool_name(bad), "{bad}");
        }
    }

    #[test]
    fn capacity_parse_and_resolve() {
        use Capacity::*;
        let p = |pool, s| Capacity::parse(pool, s, Zero::Refused);
        assert_eq!(p("cpu", "4").unwrap(), Amount(4 * MICROS_PER_CPU));
        assert_eq!(p("memory", "32G").unwrap(), Amount(32 << 30));
        assert_eq!(p("gpu", "2").unwrap(), Amount(2));
        assert_eq!(p("cpu", "50%").unwrap(), Percent(50));
        assert!(p("cpu", "0").is_err());
        assert!(p("gpu", "0").is_err());
        assert_eq!(
            Capacity::parse("gpu", "0", Zero::Allowed).unwrap(),
            Amount(0)
        );
        assert!(p("cpu", "4294967296").is_err());
        assert!(p("cpu", "1.5").is_err());
        // cpu rounds to nearest, at least one micro; memory floors.
        assert_eq!(Percent(50).resolve("cpu", || 3), 2);
        assert_eq!(Percent(1).resolve("cpu", || 1), 1);
        assert_eq!(Percent(75).resolve("memory", || 3), 2);
        assert_eq!(Percent(u64::MAX).resolve("memory", || u64::MAX), u64::MAX);
    }
}
