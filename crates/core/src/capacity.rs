//! Resource pools as a person writes them: the one definition of a pool's
//! [`Unit`] (how an amount is parsed, stored and shown) and of a pool's
//! [`Capacity`], behind `.hephconfig`'s `resources:`, `--resource name=n`,
//! `-j`, `HEPH_RESOURCE_<NAME>` and a target's `resources = {…}`, so `50%`,
//! `0.5` or `24G` cannot mean two things depending on which surface it was
//! typed into. Also the one check of a pool name's syntax
//! (`hplugin::provider::PoolName` delegates here).
//!
//! Pools are generic: `cpu` and `memory` are pools like any other, except that
//! they exist without being declared, have a fixed unit (`cpu` and `bytes`)
//! and a host default (the parallelism, the host's memory) that a percentage
//! is taken of.

/// Micro-CPUs in one CPU: how the `cpu` unit stores an amount.
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

/// How a pool's amounts are written, stored and shown. Every amount is stored
/// as a `u64` in the unit's base: a count, micro-CPUs, bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Unit {
    /// A whole number (`2`). What a pool written without a unit has, unless
    /// it is a builtin.
    Count,
    /// CPUs as a decimal in steps of 0.000001 (`0.5`, `2`), stored in
    /// micro-CPUs. The builtin `cpu` pool's unit.
    Cpu,
    /// Bytes, as a whole number or a size (`24G`). The builtin `memory`
    /// pool's unit.
    Bytes,
}

impl Unit {
    /// Every unit, in the order their names are listed in an error.
    pub const ALL: [Self; 3] = [Self::Count, Self::Cpu, Self::Bytes];

    /// The unit's name in `.hephconfig` (`unit: cpu`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Cpu => "cpu",
            Self::Bytes => "bytes",
        }
    }

    /// The unit named `s`, `None` when there is none.
    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|u| u.name() == s)
    }

    /// The fixed unit of a builtin pool (`cpu`, `memory`), `None` for any
    /// other name.
    pub fn of_builtin(pool: &str) -> Option<Self> {
        match pool {
            CPU => Some(Self::Cpu),
            MEMORY => Some(Self::Bytes),
            _ => None,
        }
    }

    /// The unit of a pool written without one: a builtin's own, else
    /// [`Count`](Self::Count).
    pub fn default_for(pool: &str) -> Self {
        Self::of_builtin(pool).unwrap_or(Self::Count)
    }

    /// Parse an amount written in this unit into the unit's base.
    ///
    /// - `count`: ASCII digits.
    /// - `cpu`: ASCII digits, optionally `.` and more digits; rounded to the
    ///   nearest 0.000001. A non-zero amount below that step is refused.
    /// - `bytes`: a memory size ([`parse_memory_size`](crate::units::parse_memory_size)).
    ///
    /// No sign, no whitespace, no exponent. Errors describe the value, not
    /// where it came from; the caller names the pool and the surface.
    pub fn parse(self, s: &str) -> anyhow::Result<u64> {
        match self {
            Self::Count => whole(s).ok_or_else(|| {
                anyhow::anyhow!("{s:?} is not a count: write a whole number such as 2")
            }),
            Self::Cpu => parse_cpu(s),
            Self::Bytes => crate::units::parse_memory_size(s),
        }
    }

    /// How an amount is shown to a target (`${resource:<pool>}`) and to a
    /// person: `cpu` in whole CPUs rounded up (at least 1 when the amount is
    /// above 0), `bytes` and `count` as the integer.
    pub fn render(self, amount: u64) -> String {
        match self {
            Self::Cpu => amount.div_ceil(MICROS_PER_CPU).to_string(),
            Self::Count | Self::Bytes => amount.to_string(),
        }
    }

    /// An amount written back in this unit exactly, so that
    /// [`parse`](Self::parse) returns it: `cpu` as a decimal (`0.00001`),
    /// the others as the integer. For a value that crosses a boundary as text.
    pub fn canonical(self, amount: u64) -> String {
        match self {
            Self::Cpu => {
                let (int, frac) = (amount / MICROS_PER_CPU, amount % MICROS_PER_CPU);
                if frac == 0 {
                    int.to_string()
                } else {
                    let frac = format!("{frac:06}");
                    format!("{int}.{}", frac.trim_end_matches('0'))
                }
            }
            Self::Count | Self::Bytes => amount.to_string(),
        }
    }
}

impl std::fmt::Display for Unit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// `s` as micro-CPUs: `<digits>[.<digits>]`, rounded half up to the micro.
fn parse_cpu(s: &str) -> anyhow::Result<u64> {
    let bad = || anyhow::anyhow!("{s:?} is not a number of CPUs: write e.g. 2 or 0.5");
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (s, None),
    };
    let int = whole(int).ok_or_else(bad)?;
    let mut micros = int
        .checked_mul(MICROS_PER_CPU)
        .ok_or_else(|| anyhow::anyhow!("{s:?} CPUs is too large"))?;
    if let Some(frac) = frac {
        if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        let mut digits = frac.bytes().map(|b| u64::from(b - b'0'));
        let mut part = 0u64;
        for _ in 0..6 {
            part = part * 10 + digits.next().unwrap_or(0);
        }
        // Half up on the seventh digit.
        if digits.next().is_some_and(|d| d >= 5) {
            part += 1;
        }
        micros = micros
            .checked_add(part)
            .ok_or_else(|| anyhow::anyhow!("{s:?} CPUs is too large"))?;
        if micros == 0 && frac.bytes().any(|b| b != b'0') {
            anyhow::bail!("{s:?} CPUs is below the smallest step, 0.000001; use 0 for none");
        }
    }
    Ok(micros)
}

/// A pool's capacity as written, before the engine's default is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capacity {
    /// An amount in the pool's unit's base (a count, micro-CPUs, bytes). `0`
    /// means "the pool exists; the run sets its capacity" (config only).
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
    /// Parse `s` as the capacity of `pool`, whose unit is `unit`.
    ///
    /// - `N%`: ASCII digits then one `%`, N ≥ 1, only for a pool with a host
    ///   default (`cpu`, `memory`). Over 100 is an overcommit by choice.
    /// - anything else: an amount in `unit` ([`Unit::parse`]).
    pub fn parse(pool: &str, unit: Unit, s: &str, zero: Zero) -> anyhow::Result<Self> {
        if let Some(digits) = s.strip_suffix('%') {
            if Unit::of_builtin(pool).is_none() {
                anyhow::bail!(
                    "{s:?} is a percentage, but pool {pool} has no default to take it of: give it an amount"
                );
            }
            let pct = whole(digits)
                .ok_or_else(|| anyhow::anyhow!("{s:?} is not a percentage: write e.g. 50%"))?;
            if pct == 0 {
                anyhow::bail!("{s:?} leaves pool {pool} empty: a percentage is at least 1%");
            }
            return Ok(Self::Percent(pct));
        }
        let amount = unit.parse(s)?;
        if amount == 0 && zero == Zero::Refused {
            anyhow::bail!("{s:?} is not a capacity for a run: it is above 0");
        }
        Ok(Self::Amount(amount))
    }

    /// The capacity in the unit's base, given the engine's default for the
    /// pool (only read for a percentage). `cpu` rounds to the nearest
    /// micro-CPU and is at least 1; every other unit floors. Saturates at
    /// `u64::MAX`.
    pub fn resolve(self, unit: Unit, default: impl FnOnce() -> u64) -> u64 {
        match self {
            Self::Amount(n) => n,
            Self::Percent(pct) => {
                let scaled = u128::from(default()) * u128::from(pct);
                let n = if unit == Unit::Cpu {
                    ((scaled + 50) / 100).max(1)
                } else {
                    scaled / 100
                };
                u64::try_from(n).unwrap_or(u64::MAX)
            }
        }
    }
}

/// One pool as the config (with the run's overrides) declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolSpec {
    pub unit: Unit,
    /// `None`: no value was written. A custom pool is then **unbounded**
    /// (every request is granted at once); a builtin keeps its host default.
    pub capacity: Option<Capacity>,
}

impl PoolSpec {
    /// The pool `pool` as written: an optional unit and an optional value.
    /// A missing unit is the pool's [default](Unit::default_for); a builtin's
    /// unit cannot be changed. Errors describe the value; the caller names
    /// where it was written.
    pub fn from_written(
        pool: &str,
        unit: Option<Unit>,
        value: Option<&str>,
    ) -> anyhow::Result<Self> {
        let unit = match (unit, Unit::of_builtin(pool)) {
            (Some(u), Some(fixed)) if u != fixed => {
                anyhow::bail!("pool {pool} is builtin and its unit is {fixed}, not {u}")
            }
            (Some(u), _) => u,
            (None, _) => Unit::default_for(pool),
        };
        let capacity = value
            .map(|v| Capacity::parse(pool, unit, v, Zero::Allowed))
            .transpose()?;
        Ok(Self { unit, capacity })
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
    fn unit_names_round_trip() {
        for u in Unit::ALL {
            assert_eq!(Unit::from_name(u.name()), Some(u));
        }
        for bad in ["", "Count", "cpus", "byte", "memory"] {
            assert_eq!(Unit::from_name(bad), None, "{bad}");
        }
        assert_eq!(Unit::default_for("cpu"), Unit::Cpu);
        assert_eq!(Unit::default_for("memory"), Unit::Bytes);
        assert_eq!(Unit::default_for("gpu"), Unit::Count);
    }

    #[test]
    fn count_parse_render_table() {
        for (s, want) in [("0", 0), ("2", 2), ("18446744073709551615", u64::MAX)] {
            assert_eq!(Unit::Count.parse(s).unwrap(), want, "{s}");
            assert_eq!(Unit::Count.render(want), s);
            assert_eq!(Unit::Count.canonical(want), s);
        }
        for bad in [
            "",
            "-1",
            "+1",
            "1.5",
            "1.0",
            "2G",
            " 2",
            "2 ",
            "1_000",
            "1e3",
            "18446744073709551616",
            "\u{FF12}",
        ] {
            assert!(Unit::Count.parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn cpu_parse_render_table() {
        const C: u64 = MICROS_PER_CPU;
        let ok: &[(&str, u64)] = &[
            ("0", 0),
            ("0.0", 0),
            ("2", 2 * C),
            ("0.5", C / 2),
            ("1.5", 3 * C / 2),
            ("0.000001", 1),
            ("0.00001", 10),
            ("0.0000005", 1), // half up
            ("0.3333333333333333", 333_333),
            ("0.9999995", C),
            ("4294967296", 4_294_967_296 * C),
        ];
        for (s, want) in ok {
            assert_eq!(
                Unit::Cpu.parse(s).unwrap_or_else(|e| panic!("{s}: {e:#}")),
                *want,
                "{s}"
            );
        }
        for bad in [
            "",
            ".5",
            "5.",
            "-1",
            "+1",
            "1e3",
            "1,5",
            " 1",
            "1 ",
            "0.0000001",
            "0.0000004999",
            "18446744073709.551616",
            "two",
        ] {
            assert!(Unit::Cpu.parse(bad).is_err(), "{bad:?}");
        }
        // Rendered in whole CPUs rounded up, at least 1 above 0.
        for (amount, shown) in [
            (0, "0"),
            (1, "1"),
            (10, "1"),
            (C, "1"),
            (C + 1, "2"),
            (3 * C, "3"),
        ] {
            assert_eq!(Unit::Cpu.render(amount), shown, "{amount}");
        }
        // Canonical text parses back to the same amount.
        for amount in [0, 1, 10, C / 2, C, 3 * C / 2, 123_456_789] {
            let text = Unit::Cpu.canonical(amount);
            assert_eq!(Unit::Cpu.parse(&text).unwrap(), amount, "{text}");
        }
        assert_eq!(Unit::Cpu.canonical(10), "0.00001");
        assert_eq!(Unit::Cpu.canonical(2 * C), "2");
    }

    #[test]
    fn bytes_parse_render_table() {
        for (s, want) in [
            ("4096", 4096),
            ("4G", 4 << 30),
            ("512M", 512 << 20),
            ("0", 0),
        ] {
            assert_eq!(Unit::Bytes.parse(s).unwrap(), want, "{s}");
            assert_eq!(Unit::Bytes.render(want), want.to_string());
            assert_eq!(
                Unit::Bytes.parse(&Unit::Bytes.canonical(want)).unwrap(),
                want
            );
        }
        for bad in ["", "4 G", "4.5G", "-1", "G"] {
            assert!(Unit::Bytes.parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn capacity_parse_and_resolve() {
        use Capacity::*;
        let p = |pool, s| Capacity::parse(pool, Unit::default_for(pool), s, Zero::Refused);
        assert_eq!(p("cpu", "4").unwrap(), Amount(4 * MICROS_PER_CPU));
        assert_eq!(p("cpu", "1.5").unwrap(), Amount(1_500_000));
        assert_eq!(p("memory", "32G").unwrap(), Amount(32 << 30));
        assert_eq!(p("gpu", "2").unwrap(), Amount(2));
        assert_eq!(p("cpu", "50%").unwrap(), Percent(50));
        assert!(p("cpu", "0").is_err());
        assert!(p("gpu", "0").is_err());
        assert!(p("gpu", "50%").is_err(), "a custom pool has no default");
        assert_eq!(
            Capacity::parse("gpu", Unit::Count, "0", Zero::Allowed).unwrap(),
            Amount(0)
        );
        // A custom pool may carry any unit.
        assert_eq!(
            Capacity::parse("slots", Unit::Cpu, "0.5", Zero::Refused).unwrap(),
            Amount(500_000)
        );
        // cpu rounds to nearest, at least one micro; bytes floors.
        assert_eq!(Percent(50).resolve(Unit::Cpu, || 3), 2);
        assert_eq!(Percent(1).resolve(Unit::Cpu, || 1), 1);
        assert_eq!(Percent(75).resolve(Unit::Bytes, || 3), 2);
        assert_eq!(
            Percent(u64::MAX).resolve(Unit::Bytes, || u64::MAX),
            u64::MAX
        );
    }

    #[test]
    fn pool_spec_from_written() {
        let spec = PoolSpec::from_written("gpu", None, Some("2")).unwrap();
        assert_eq!(
            spec,
            PoolSpec {
                unit: Unit::Count,
                capacity: Some(Capacity::Amount(2))
            }
        );
        let spec = PoolSpec::from_written("gpu", Some(Unit::Cpu), Some("0.5")).unwrap();
        assert_eq!(spec.capacity, Some(Capacity::Amount(500_000)));
        assert_eq!(
            PoolSpec::from_written("gpu", None, None).unwrap().capacity,
            None
        );
        assert!(PoolSpec::from_written("cpu", Some(Unit::Count), Some("2")).is_err());
        assert!(PoolSpec::from_written("memory", Some(Unit::Cpu), None).is_err());
        assert_eq!(
            PoolSpec::from_written("memory", Some(Unit::Bytes), Some("1G"))
                .unwrap()
                .capacity,
            Some(Capacity::Amount(1 << 30))
        );
    }
}
