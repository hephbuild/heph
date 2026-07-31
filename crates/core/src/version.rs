pub const VERSION: &str = match option_env!("HEPH_BUILD_VERSION") {
    Some(val) => val,
    None => "v0.0.0-dev",
};

/// Width of the release-flavour slot CI patches into an already-compiled
/// binary (`scripts/patch-flavour.sh`, run from the "Build" job in
/// `.github/workflows/heph.yml`) so `heph version` and self-upgrade
/// (`hselfupdate`) can tell std and debug flavours apart without a second
/// compile — both flavours of a release share one compile (see the
/// strip-after-build note on `[profile.release]` in the root `Cargo.toml`), so
/// a compile-time constant (`option_env!`, like [`VERSION`] above) would be
/// identical for both and couldn't distinguish them. `strip` doesn't touch
/// this slot: it's ordinary `.rodata`, not a debug section or a symbol-table
/// entry.
const FLAVOUR_SLOT_LEN: usize = 32;

/// Distinctive marker the slot is initialized with, so the patch script can
/// find (and verify the uniqueness of) it inside the compiled binary. An
/// unpatched slot — a local `cargo build`, or any test binary — still starts
/// with this marker, which [`parse_flavour_slot`] reads back as `""` (the
/// std/no-flavour default).
const FLAVOUR_SLOT_MARKER: &[u8] = b"HEPH_FLAVOUR_SLOT_V1";

const _: () = assert!(
    FLAVOUR_SLOT_MARKER.len() < FLAVOUR_SLOT_LEN,
    "the flavour slot marker must fit within the slot, with room for a nul"
);

#[expect(
    clippy::indexing_slicing,
    reason = "const-evaluated at compile time: `i` is loop-bounded by \
              FLAVOUR_SLOT_MARKER.len(), which a `const_assert` below keeps under \
              FLAVOUR_SLOT_LEN — an out-of-bounds index fails the build, it can't panic at runtime"
)]
const fn flavour_slot_init() -> [u8; FLAVOUR_SLOT_LEN] {
    let mut buf = [0u8; FLAVOUR_SLOT_LEN];
    let mut i = 0;
    while i < FLAVOUR_SLOT_MARKER.len() {
        buf[i] = FLAVOUR_SLOT_MARKER[i];
        i += 1;
    }
    buf
}

#[used]
static FLAVOUR_SLOT: [u8; FLAVOUR_SLOT_LEN] = flavour_slot_init();

/// Read a flavour slot's content: empty if it's still the unpatched marker
/// (nothing was ever stamped, so treat it as std/no-flavour) or is entirely
/// zeroed (explicitly stamped as std), otherwise the bytes up to the first
/// nul, as UTF-8. Split out from [`flavour`] so tests can exercise arbitrary
/// slot contents without patching the actual compiled binary.
fn parse_flavour_slot(slot: &[u8; FLAVOUR_SLOT_LEN]) -> &str {
    if slot.starts_with(FLAVOUR_SLOT_MARKER) {
        return "";
    }
    let end = slot.iter().position(|&b| b == 0).unwrap_or(slot.len());
    slot.get(..end)
        .and_then(|s| std::str::from_utf8(s).ok())
        .unwrap_or("")
}

/// This binary's release flavour: `""` for the default "std" build, or a name
/// like `"debug"`. See [`FLAVOUR_SLOT_LEN`] for why this is a post-build patch
/// rather than a compile-time constant.
///
/// Returns an owned `String`, not `&'static str`: reading [`FLAVOUR_SLOT`]
/// must go through [`std::ptr::read_volatile`], which yields the bytes by
/// value rather than a reference into `'static` memory. That volatile read is
/// not optional — verified empirically under this project's release profile
/// (thin LTO, `opt-level = 3`): a plain load of an immutable `static` with a
/// fully known initializer is exactly what LLVM is entitled to (and does)
/// constant-fold to that initializer, silently ignoring whatever
/// `scripts/patch-flavour.sh` wrote into the compiled file afterward.
pub fn flavour() -> String {
    // SAFETY: `FLAVOUR_SLOT` is a valid, always-initialized `'static` array;
    // reading it by volatile load is always sound, it just forces an actual
    // memory access instead of the compile-time-known value.
    let slot = unsafe { std::ptr::read_volatile(&raw const FLAVOUR_SLOT) };
    parse_flavour_slot(&slot).to_string()
}

/// [`VERSION`] with the release flavour appended as semver build metadata
/// (`v1.2.3+debug`) when this binary was stamped with one — what `heph
/// version` and diagnostic banners should show a human. [`VERSION`] itself
/// stays plain: other consumers (self-upgrade's version-pin compare, the
/// default govet target address, telemetry) need the bare tag, not a value
/// that varies by flavour.
pub fn reported() -> String {
    match flavour().as_str() {
        "" => VERSION.to_string(),
        f => format!("{VERSION}+{f}"),
    }
}

/// A [semver](https://semver.org/) version decomposed into its segments.
///
/// `major`/`minor`/`patch` are the numeric core. `pre_release` is the dot-joined
/// pre-release identifiers (the `-` segment), `build` is the build metadata (the
/// `+` segment) — both `None` when absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemVer<'a> {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre_release: Option<&'a str>,
    pub build: Option<&'a str>,
}

/// Parse a semver string into its segments, tolerating an optional leading `v`
/// (heph stamps versions as e.g. `v0.0.0-dev`). Returns `None` if the numeric
/// core is malformed, so callers can simply omit the segments rather than report
/// junk.
pub fn parse(version: &str) -> Option<SemVer<'_>> {
    let version = version.strip_prefix('v').unwrap_or(version);

    // Split off build metadata (`+`) first — it can itself contain `-`, so it
    // must be peeled before the pre-release `-`.
    let (rest, build) = match version.split_once('+') {
        Some((rest, build)) => (rest, Some(build)),
        None => (version, None),
    };
    let (core, pre_release) = match rest.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (rest, None),
    };

    let mut nums = core.split('.');
    let major = nums.next()?.parse().ok()?;
    let minor = nums.next()?.parse().ok()?;
    let patch = nums.next()?.parse().ok()?;
    if nums.next().is_some() {
        return None;
    }

    Some(SemVer {
        major,
        minor,
        patch,
        pre_release: pre_release.filter(|s| !s.is_empty()),
        build: build.filter(|s| !s.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_version() {
        assert_eq!(
            parse("v1.2.3-alpha.1+build.7"),
            Some(SemVer {
                major: 1,
                minor: 2,
                patch: 3,
                pre_release: Some("alpha.1"),
                build: Some("build.7"),
            })
        );
    }

    #[test]
    fn parses_without_v_prefix() {
        assert_eq!(
            parse("10.20.30"),
            Some(SemVer {
                major: 10,
                minor: 20,
                patch: 30,
                pre_release: None,
                build: None,
            })
        );
    }

    #[test]
    fn parses_dev_default() {
        assert_eq!(
            parse(VERSION),
            Some(SemVer {
                major: 0,
                minor: 0,
                patch: 0,
                pre_release: Some("dev"),
                build: None,
            })
        );
    }

    #[test]
    fn build_metadata_may_contain_hyphen() {
        let v = parse("v1.0.0+exp-sha.5114f85").unwrap();
        assert_eq!(v.pre_release, None);
        assert_eq!(v.build, Some("exp-sha.5114f85"));
    }

    #[test]
    fn pre_release_only() {
        let v = parse("2.3.4-rc.1").unwrap();
        assert_eq!(v.pre_release, Some("rc.1"));
        assert_eq!(v.build, None);
    }

    #[test]
    fn rejects_malformed_core() {
        assert_eq!(parse("1.2"), None);
        assert_eq!(parse("1.2.3.4"), None);
        assert_eq!(parse("v1.x.3"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn flavour_is_empty_when_never_patched() {
        // Test binaries are never touched by `scripts/patch-flavour.sh`.
        assert_eq!(flavour(), "");
        assert_eq!(reported(), VERSION);
    }

    #[test]
    fn parse_flavour_slot_unpatched_marker_is_empty() {
        assert_eq!(parse_flavour_slot(&flavour_slot_init()), "");
    }

    #[test]
    fn parse_flavour_slot_reads_patched_value() {
        let mut slot = [0u8; FLAVOUR_SLOT_LEN];
        slot[..b"debug".len()].copy_from_slice(b"debug");
        assert_eq!(parse_flavour_slot(&slot), "debug");
    }

    #[test]
    fn parse_flavour_slot_all_zero_is_empty() {
        // What CI's patch script writes for the "std" flavour: an explicit,
        // rather than merely unpatched, empty slot.
        assert_eq!(parse_flavour_slot(&[0u8; FLAVOUR_SLOT_LEN]), "");
    }
}
