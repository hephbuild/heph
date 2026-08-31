//! Human-readable unit parsing, shared by anything that takes a size from a
//! person: `heph tool gc`'s caps, a `scratch` declaration's `max_size`, and a
//! config file. One definition, so `500MB` cannot come to mean two things
//! depending on which surface it was typed into.

use anyhow::Context as _;

/// Parse a human size (`50GiB`, `500MB`, `1024`) into bytes.
///
/// Accepts the binary units people actually type and treats the `B`-less forms
/// as the same thing, because `50G` meaning something different from `50GiB`
/// would be a trap and nobody wants decimal gigabytes for a disk cap.
pub fn parse_size(s: &str) -> anyhow::Result<u64> {
    let t = s.trim();
    let digits = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(digits);
    let n: u64 = num
        .parse()
        .with_context(|| format!("{s:?} does not start with a number"))?;
    let mult = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024u64 * 1024 * 1024 * 1024,
        other => anyhow::bail!("unknown size unit {other:?} in {s:?}; try 50GiB, 500MiB, 1024"),
    };
    n.checked_mul(mult)
        .ok_or_else(|| anyhow::anyhow!("size {s:?} overflows"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_accepts_the_units_people_type() {
        assert_eq!(parse_size("1024").expect("plain"), 1024);
        assert_eq!(parse_size("1KiB").expect("kib"), 1024);
        // Binary throughout: `50GB` meaning something different from `50GiB`
        // would be a trap for a disk cap.
        assert_eq!(
            parse_size("2GB").expect("gb"),
            parse_size("2GiB").expect("gib")
        );
        assert_eq!(parse_size("50GiB").expect("gib"), 50 * 1024 * 1024 * 1024);
        assert_eq!(parse_size(" 8 MiB ").expect("spaces"), 8 * 1024 * 1024);
    }

    #[test]
    fn parse_size_rejects_nonsense_with_a_usable_message() {
        let err = parse_size("lots").expect_err("not a number");
        assert!(format!("{err:#}").contains("number"));
        let err = parse_size("5 parsecs").expect_err("bad unit");
        let msg = format!("{err:#}");
        assert!(msg.contains("parsecs"), "{msg}");
        assert!(msg.contains("50GiB"), "must suggest a valid form: {msg}");
    }

    #[test]
    fn parse_size_does_not_overflow_silently() {
        assert!(parse_size("99999999999999999999TiB").is_err());
    }
}
