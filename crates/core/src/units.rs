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

/// Format a byte count the way a person reads one (`512 B`, `3.4 KiB`,
/// `1.2 GiB`).
///
/// Binary units throughout, and labelled as such. Three copies of this existed —
/// in the TUI, in the engine's stall diagnostics, and in `heph tool scratch` —
/// and the engine's divided by binary powers while labelling the result `GB`,
/// which is simply wrong: `1 << 30` bytes is a gibibyte. One definition means
/// one answer, and `heph tool gc` reporting a different number than the TUI for
/// the same bytes is the kind of discrepancy nobody can explain later.
///
/// No decimal in the bytes band: `1023 B`, not `1023.0 B`. A fraction of a byte
/// is not a thing.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut unit = UNITS[0];
    for u in UNITS {
        unit = u;
        v /= 1024.0;
        if v < 1024.0 {
            break;
        }
    }
    format!("{v:.1} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_reads_like_a_person_wrote_it() {
        assert_eq!(human_bytes(0), "0 B");
        // No decimal in the bytes band — a fraction of a byte is not a thing.
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(10 * 1024 * 1024 * 1024), "10.0 GiB");
        // Binary divisors get binary labels. The engine's old copy divided by
        // `1 << 30` and called it "GB".
        assert_eq!(human_bytes(1 << 30), "1.0 GiB");
        // Saturates at TiB rather than inventing PiB/EiB — nothing this
        // formats (a cache, a build artifact) reaches a pebibyte, and an
        // absurd-looking number is better than a unit nobody recognises.
        assert_eq!(human_bytes(u64::MAX), "16777216.0 TiB");
    }

    /// Every size this formats must parse back to roughly itself, or the two
    /// halves of this module disagree about what a unit is.
    #[test]
    fn the_two_halves_agree_on_units() {
        for n in [1024u64, 1536, 5 * 1024 * 1024, 3 * 1024 * 1024 * 1024] {
            let rendered = human_bytes(n);
            let (num, unit) = rendered.split_once(' ').expect("has a unit");
            let reparsed = parse_size(&format!("1{unit}")).expect("unit parses");
            // Compare in f64 rather than casting back: the rendered value is
            // rounded to one decimal, so the point is that the *unit* matches,
            // not that the bytes survive a lossy round trip.
            let scaled = num.parse::<f64>().expect("number") * reparsed as f64;
            assert!(
                (scaled - n as f64).abs() < 1.0,
                "{rendered} did not round-trip: {scaled} vs {n}"
            );
        }
    }

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
