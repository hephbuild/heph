use std::collections::BTreeMap;

/// Resolved build variant: the concrete set of Go toolchain factors a target
/// compiles/links under. Produced by resolving a [`VariantRef`] (`v`/`vp` addr
/// args) against the `variants` provider_state (see
/// [`crate::plugingo::variant`]).
///
/// **Cache correctness:** every field here affects build output, so every field
/// must be reflected in the compile/golist/lint cache keys (the `Go*Def::hash`
/// functions). GOOS/GOARCH/GOEXPERIMENT/CGO are set as process env in the driver
/// `run()` (never as hashed config keys), so they are hashed via those `Def`s;
/// gcflags land on the `go tool compile` command line; ldflags land on the link
/// script (hashed transitively by the exec driver's run-script hash).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Factors {
    pub goos: String,
    pub goarch: String,
    /// `-tags` build tags (file selection). Sorted for determinism.
    pub build_tags: Vec<String>,
    /// `GOEXPERIMENT` values. Sorted for determinism.
    pub goexperiment: Vec<String>,
    /// Flags passed verbatim to `go tool compile` (the `-gcflags` equivalent).
    /// Order preserved — flag semantics can be order-sensitive.
    pub gcflags: Vec<String>,
    /// Flags passed verbatim to `go tool link` (the `-ldflags` equivalent).
    /// Order preserved.
    pub ldflags: Vec<String>,
    /// Build with the race detector (`-race` on `go list`, `go tool compile` and
    /// `go tool link`). Not a variant field: it is selected by asking for a
    /// `*_race` target, and rides the addr as `race=1` (see [`VariantRef::race`]).
    ///
    /// Race instrumentation is a whole-program property — every archive in the
    /// link, stdlib included, must be built with it — so it keys the archive
    /// cache exactly like `goos`/`goarch` do.
    pub race: bool,
}

/// Whether a build for `goos` with race mode `race` needs cgo.
///
/// Only race builds ever do, and only off darwin. Go's `runtime/race` — the
/// package that pulls in the prebuilt TSan runtime — is a **cgo** package
/// everywhere except darwin, where `race_darwin_<arch>.go` supplies the
/// syso-derived symbol information directly and no C toolchain is needed. So a
/// darwin race build stays as hermetic as an ordinary one (`CGO_ENABLED=0`,
/// internal linking), while a linux race build enables cgo and links externally.
///
/// This is an implementation split, not a semantic one: `test_race` behaves
/// identically on all three supported targets.
pub fn cgo_required(goos: &str, race: bool) -> bool {
    race && goos != "darwin"
}

/// `CGO_ENABLED` value for these coordinates — see [`cgo_required`].
pub fn cgo_enabled_value(goos: &str, race: bool) -> &'static str {
    if cgo_required(goos, race) { "1" } else { "0" }
}

/// `go tool link -buildmode=` value for these coordinates.
///
/// `pie` everywhere, except a race build on linux: the race runtime needs a
/// fixed address layout, so `go build` itself refuses `-buildmode=pie` with
/// `-race` there ("-buildmode=pie not supported when -race is enabled on
/// linux/amd64"). `go tool link` does *not* enforce that — it accepts the
/// combination and emits a binary that dies at startup with "ThreadSanitizer
/// failed to allocate", so the choice has to be made here. darwin supports
/// pie+race and keeps it.
pub fn link_buildmode(goos: &str, race: bool) -> &'static str {
    if race && goos == "linux" {
        "exe"
    } else {
        "pie"
    }
}

impl Factors {
    /// `-buildmode=` value for linking under these factors. See [`link_buildmode`].
    pub fn link_buildmode(&self) -> &'static str {
        link_buildmode(&self.goos, self.race)
    }

    /// `CGO_ENABLED` value this factor set builds under. See [`cgo_required`].
    pub fn cgo_enabled_value(&self) -> &'static str {
        cgo_enabled_value(&self.goos, self.race)
    }

    /// `GOEXPERIMENT` env value (`"a,b"`), or `None` when no experiments are set.
    pub fn goexperiment_env(&self) -> Option<String> {
        if self.goexperiment.is_empty() {
            None
        } else {
            Some(self.goexperiment.join(","))
        }
    }
}

/// The variant coordinate carried on a target address: the variant `name` (`v`
/// arg) plus the `pkg` that defines it (`vp` arg; `""` = workspace root).
///
/// User-facing targets are addressed with `v` only; the provider resolves the
/// closest ancestor package defining that variant to fill in `pkg`. Every
/// internal / dependency target the provider emits carries the full `{v, vp}`
/// pair so the variant resolves identically no matter which subtree the target
/// lives in (see [`crate::plugingo::variant::resolve`]). `vp` threads verbatim
/// down the whole dependency graph so the entire binary shares one factor set.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VariantRef {
    pub name: String,
    pub pkg: String,
    /// Race-detector build (see [`Factors::race`]). Part of the coordinate, not
    /// of the variant: a `test_race` target threads `race=1` onto every
    /// dependency address so the whole graph — first-party, thirdparty and
    /// stdlib alike — resolves to race-instrumented archives, distinct from the
    /// ordinary ones under the same variant name.
    pub race: bool,
}

impl VariantRef {
    pub fn new(name: impl Into<String>, pkg: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            pkg: pkg.into(),
            race: false,
        }
    }

    /// This coordinate with the race flag set to `race`.
    pub fn with_race(mut self, race: bool) -> Self {
        self.race = race;
        self
    }

    /// Encode as addr args `{v, vp}`, plus `race=1` for a race build. `vp` is
    /// always emitted (even empty, for the root package) so a present `vp` key
    /// unambiguously marks a fully-resolved internal address vs. a bare user
    /// `@v=` address.
    ///
    /// `race` is emitted **only when set**, so every ordinary target's address —
    /// and therefore its cache key — is byte-identical to what it was before
    /// race mode existed.
    pub fn to_args(&self) -> BTreeMap<String, String> {
        let mut args = BTreeMap::new();
        args.insert("v".to_string(), self.name.clone());
        args.insert("vp".to_string(), self.pkg.clone());
        if self.race {
            args.insert(RACE_ARG.to_string(), "1".to_string());
        }
        args
    }
}

/// Addr arg carrying the race flag down the dependency graph. `race=1` is the
/// only accepted value; the key is absent on an ordinary build.
pub const RACE_ARG: &str = "race";

/// Read the `race` addr arg. Absent → `false`; `1` → `true`; anything else is an
/// error, so a typo (`race=true`, `race=yes`) fails loudly instead of silently
/// building without instrumentation and reporting a clean run.
pub fn race_from_args(args: &BTreeMap<String, String>) -> anyhow::Result<bool> {
    match args.get(RACE_ARG) {
        None => Ok(false),
        Some(v) if v == "1" => Ok(true),
        Some(other) => anyhow::bail!(
            "invalid `{RACE_ARG}` addr arg `{other}` (the only accepted value is `1`); \
             ask for a race build with the `test_race`/`xtest_race` target rather than \
             setting `{RACE_ARG}` by hand"
        ),
    }
}

pub fn current_goos() -> String {
    // Go's GOOS naming matches the canonical (Go/OCI) convention.
    hcore::htplatform::os().to_string()
}

pub fn current_goarch() -> String {
    // Go's GOARCH naming matches the canonical (Go/OCI) convention.
    hcore::htplatform::arch().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_variant_ref_to_args() {
        let vref = VariantRef::new("release", "app");
        let args = vref.to_args();
        assert_eq!(args.get("v").map(String::as_str), Some("release"));
        assert_eq!(args.get("vp").map(String::as_str), Some("app"));
    }

    #[test]
    fn test_variant_ref_root_pkg_emits_empty_vp() {
        let vref = VariantRef::new("release", "");
        let args = vref.to_args();
        assert_eq!(args.get("vp").map(String::as_str), Some(""));
    }

    // ---- race ----

    /// The load-bearing compatibility property: an ordinary target's addr args
    /// are byte-identical to what they were before race mode existed, so no
    /// cached archive is invalidated by this feature landing.
    #[test]
    fn non_race_variant_ref_emits_no_race_arg() {
        let args = VariantRef::new("dev", "app").to_args();
        assert_eq!(args.keys().collect::<Vec<_>>(), vec!["v", "vp"]);
        assert!(!args.contains_key(RACE_ARG));
    }

    #[test]
    fn race_variant_ref_emits_race_arg() {
        let args = VariantRef::new("dev", "app").with_race(true).to_args();
        assert_eq!(args.get(RACE_ARG).map(String::as_str), Some("1"));
        // The variant coordinate itself is untouched — race is orthogonal.
        assert_eq!(args.get("v").map(String::as_str), Some("dev"));
        assert_eq!(args.get("vp").map(String::as_str), Some("app"));
    }

    #[test]
    fn race_from_args_reads_the_flag() {
        assert!(!race_from_args(&BTreeMap::new()).unwrap());
        assert!(race_from_args(&VariantRef::new("d", "").with_race(true).to_args()).unwrap());
    }

    /// A typo must fail loudly. Silently treating `race=true` as "no race" would
    /// build without instrumentation and report a clean run — the one failure
    /// mode of this feature that a user cannot detect.
    #[test]
    fn race_from_args_rejects_anything_but_one() {
        for bad in ["true", "yes", "0", ""] {
            let args = BTreeMap::from([(RACE_ARG.to_string(), bad.to_string())]);
            let err = race_from_args(&args).expect_err("must reject `{bad}`");
            assert!(
                err.to_string().contains("test_race"),
                "the error should point at the supported way in: {err}"
            );
        }
    }

    /// The platform split. darwin's `runtime/race` is pure Go plus a syso, so a
    /// darwin race build needs no C toolchain; everywhere else it is a cgo
    /// package. Nothing but a race build ever enables cgo.
    #[test]
    fn cgo_only_for_a_non_darwin_race_build() {
        assert!(cgo_required("linux", true));
        assert!(!cgo_required("darwin", true));
        assert!(!cgo_required("linux", false));
        assert!(!cgo_required("darwin", false));
        assert_eq!(cgo_enabled_value("linux", true), "1");
        assert_eq!(cgo_enabled_value("darwin", true), "0");
        assert_eq!(cgo_enabled_value("linux", false), "0");
    }

    /// `go build` refuses `-buildmode=pie` with `-race` on linux, but `go tool
    /// link` accepts it and emits a binary that dies at startup with
    /// "ThreadSanitizer failed to allocate" — so the choice has to be made here.
    #[test]
    fn linux_race_links_non_pie() {
        assert_eq!(link_buildmode("linux", true), "exe");
        assert_eq!(link_buildmode("darwin", true), "pie");
        assert_eq!(link_buildmode("linux", false), "pie");
        assert_eq!(link_buildmode("darwin", false), "pie");
    }

    #[test]
    fn factors_expose_the_same_rules() {
        let f = Factors {
            goos: "linux".into(),
            goarch: "arm64".into(),
            race: true,
            ..Default::default()
        };
        assert_eq!(f.cgo_enabled_value(), "1");
        assert_eq!(f.link_buildmode(), "exe");
    }

    /// Race participates in the factor identity, so two otherwise-identical
    /// factor sets are different cache keys.
    #[test]
    fn race_changes_factor_identity() {
        let base = Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            ..Default::default()
        };
        let racy = Factors {
            race: true,
            ..base.clone()
        };
        assert_ne!(base, racy);
    }

    #[test]
    fn test_goexperiment_env() {
        let mut f = Factors::default();
        assert_eq!(f.goexperiment_env(), None);
        f.goexperiment = vec!["arenas".into(), "loopvar".into()];
        assert_eq!(f.goexperiment_env(), Some("arenas,loopvar".to_string()));
    }
}
