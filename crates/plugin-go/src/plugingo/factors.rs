use std::collections::BTreeMap;

/// Link buildmode — the `go build -buildmode=` equivalent, narrowed to the two
/// modes that make sense for an executable heph links itself.
///
/// [`BuildMode::Exe`] is the default, matching what plain `go build` produces:
/// on Linux the binary is **statically linked** with no `PT_INTERP`, so it runs
/// in a `FROM scratch` image. [`BuildMode::Pie`] asks for a position-independent
/// executable, which on Linux always carries an interpreter
/// (`/lib/ld-linux-<arch>.so.1`) even with cgo disabled and internal linking —
/// that is Go's behaviour, not a heph choice.
///
/// Uniform across the supported targets: on darwin/arm64 the Go linker upgrades
/// `exe` to PIE unconditionally (`cmd/link`: "on these platforms, everything is
/// PIE"), so both modes link and both produce a PIE Mach-O. The knob is honored
/// everywhere; darwin simply has one possible answer.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum BuildMode {
    #[default]
    Exe,
    Pie,
}

impl BuildMode {
    /// Accepted spellings, for schema docs and "allowed: [...]" errors.
    pub const NAMES: &'static [&'static str] = &["exe", "pie"];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "exe" => Some(Self::Exe),
            "pie" => Some(Self::Pie),
            _ => None,
        }
    }

    /// The `-buildmode=` value handed to `go tool link`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exe => "exe",
            Self::Pie => "pie",
        }
    }

    /// Whether `go tool compile` / `go tool asm` need `-shared`.
    ///
    /// Mirrors cmd/go (`internal/work/init.go`): `-buildmode=pie` adds `-shared`
    /// to the codegen flags, `-buildmode=exe` does not. The two **must** agree —
    /// a `-shared` archive fails a non-PIE internal link with `cannot handle
    /// R_ARM64_TLS_IE (sym runtime.load_g) when linking internally`. The reverse
    /// (plain archives into a PIE link) is fine, which is why the std library —
    /// built by `go install std`, i.e. under cmd/go's own defaults — links into
    /// either mode.
    pub fn needs_shared(self) -> bool {
        matches!(self, Self::Pie)
    }

    /// The mode a build with these coordinates must actually link in: `self`,
    /// except a **race** build on linux, which is forced to [`Self::Exe`].
    ///
    /// The race runtime needs a fixed address layout, and `go build` refuses
    /// `-buildmode=pie` with `-race` on linux outright ("-buildmode=pie not
    /// supported when -race is enabled on linux/amd64"). `go tool link` — which
    /// is what this plugin calls — does **not** enforce that: it accepts the
    /// combination and emits a binary that dies at startup with
    /// "ThreadSanitizer failed to allocate", so the choice has to be made here.
    ///
    /// darwin supports pie+race, so a variant asking for `pie` there keeps it.
    ///
    /// Applied once, at variant resolution, so `Factors::buildmode` is already
    /// the effective mode everywhere downstream — which keeps `needs_shared` and
    /// the link's `-buildmode=` agreeing without either having to know about race.
    pub fn for_race(self, goos: &str, race: bool) -> Self {
        if race && goos == "linux" {
            Self::Exe
        } else {
            self
        }
    }
}

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
/// script (hashed transitively by the exec driver's run-script hash); `buildmode`
/// lands on both — `-shared` on the compile (hashed by `GoCompileDef`) and
/// `-buildmode=` on the link script.
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
    /// Link buildmode. Drives both `go tool link -buildmode=` and whether the
    /// compile steps get `-shared` — the two are coupled, see
    /// [`BuildMode::needs_shared`]. A race build on linux overrides whatever the
    /// variant declared, see [`BuildMode::for_race`].
    pub buildmode: BuildMode,
    /// Build with the race detector (`-race` on `go list`, `go tool compile` and
    /// `go tool link`). Not a variant field: it is selected by asking for a
    /// `*_race` target, and rides the addr as `race=1` (see [`VariantRef::race`]).
    ///
    /// Race instrumentation is a whole-program property — every archive in the
    /// link, stdlib included, must be built with it — so it keys the archive
    /// cache exactly like `goos`/`goarch` do.
    pub race: bool,
}

impl Factors {
    /// A legible identifier for everything in this variant *except* `goos` and
    /// `goarch`, which callers carry as their own addr args.
    ///
    /// This is what makes the shared Go caches "one per module per variant"
    /// (`plugingo::gocache`): every driver that wants the variant's cache passes
    /// this one string rather than picking its own subset of the factors. That
    /// subset-picking is exactly what went wrong before — `go_golist` keyed on
    /// `build_tags` and `go_compile` deliberately did not, so one variant got two
    /// caches and neither warmed the other.
    ///
    /// Legible rather than hashed, for the same reason the rest of the addr is:
    /// it shows up in `heph tool scratch ls`, and "which cache is this?" is a
    /// question a hash cannot answer. Empty for a plain variant, so the common
    /// case carries no arg at all.
    ///
    /// Deliberately the *whole* variant, including `gcflags`, `ldflags` and
    /// `buildmode` — even though `go list` reads none of them and only
    /// `buildmode` reaches a compile. Splitting a cache by a factor its entries
    /// do not depend on costs a cold start on a rare variant; letting two drivers
    /// disagree about which factors count costs a permanently split cache on
    /// every variant. The predictable rule is worth the rare miss.
    pub fn variant_id(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.build_tags.is_empty() {
            parts.push(format!("tags={}", self.build_tags.join("+")));
        }
        if !self.goexperiment.is_empty() {
            parts.push(format!("exp={}", self.goexperiment.join("+")));
        }
        if !self.gcflags.is_empty() {
            parts.push(format!("gc={}", self.gcflags.join("+")));
        }
        if !self.ldflags.is_empty() {
            parts.push(format!("ld={}", self.ldflags.join("+")));
        }
        if self.buildmode != BuildMode::default() {
            parts.push(format!("mode={}", self.buildmode.as_str()));
        }
        if self.race {
            parts.push("race".to_string());
        }
        parts.join(",")
    }
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

impl Factors {
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
    /// The plain variant carries nothing, so the common workspace's cache addr
    /// stays short and an empty `var=` never appears.
    #[test]
    fn a_plain_variant_has_an_empty_id() {
        assert_eq!(Factors::default().variant_id(), "");
    }

    /// Every factor must move the id. A factor that does not silently shares one
    /// cache between two variants — which is what `go_golist` and `go_compile`
    /// used to do to each other by keying on different subsets.
    #[test]
    fn every_factor_moves_the_variant_id() {
        let base = Factors::default().variant_id();
        let mut cases: Vec<(&str, Factors)> = Vec::new();

        cases.push((
            "build_tags",
            Factors {
                build_tags: vec!["integration".to_string()],
                ..Default::default()
            },
        ));
        cases.push((
            "goexperiment",
            Factors {
                goexperiment: vec!["arenas".to_string()],
                ..Default::default()
            },
        ));
        cases.push((
            "gcflags",
            Factors {
                gcflags: vec!["-N".to_string()],
                ..Default::default()
            },
        ));
        cases.push((
            "ldflags",
            Factors {
                ldflags: vec!["-s".to_string()],
                ..Default::default()
            },
        ));
        cases.push((
            "buildmode",
            Factors {
                buildmode: BuildMode::Pie,
                ..Default::default()
            },
        ));
        cases.push((
            "race",
            Factors {
                race: true,
                ..Default::default()
            },
        ));

        for (what, f) in cases {
            assert_ne!(f.variant_id(), base, "{what} must move the variant id");
        }
    }

    /// `goos`/`goarch` are deliberately absent — callers carry them as their own
    /// addr args, and duplicating them here would put each in the addr twice.
    #[test]
    fn the_variant_id_excludes_goos_and_goarch() {
        let f = Factors {
            goos: "darwin".to_string(),
            goarch: "arm64".to_string(),
            ..Default::default()
        };
        assert_eq!(f.variant_id(), "");
    }

    /// Two variants that agree land on one id, so the sharing is not a coin flip.
    #[test]
    fn equal_variants_share_one_id() {
        let a = Factors {
            build_tags: vec!["x".to_string(), "y".to_string()],
            race: true,
            ..Default::default()
        };
        let b = a.clone();
        assert_eq!(a.variant_id(), b.variant_id());
    }

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

    // The compile-side `-shared` and the link-side `-buildmode=` are a single
    // decision expressed twice. A `-shared` archive cannot be linked into a
    // non-PIE executable — the link dies on `cannot handle R_ARM64_TLS_IE (sym
    // runtime.load_g)`. If these two ever disagree, every Go binary stops linking.
    #[test]
    fn shared_is_required_exactly_for_pie() {
        assert!(BuildMode::Pie.needs_shared());
        assert!(!BuildMode::Exe.needs_shared());
    }

    // `NAMES` feeds the "allowed: …" errors and the state schema; it must stay in
    // step with what `parse` accepts and what `as_str` emits.
    #[test]
    fn buildmode_names_roundtrip_through_parse() {
        for name in BuildMode::NAMES {
            let mode = BuildMode::parse(name).unwrap_or_else(|| panic!("`{name}` must parse"));
            assert_eq!(mode.as_str(), *name);
        }
        assert!(BuildMode::parse("c-archive").is_none());
    }

    #[test]
    fn buildmode_defaults_to_exe() {
        assert_eq!(Factors::default().buildmode, BuildMode::Exe);
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
    /// A linux race build must not link PIE — `go tool link` accepts the
    /// combination `go build` rejects and produces a binary that dies at startup
    /// with "ThreadSanitizer failed to allocate". The override wins over an
    /// explicit `buildmode = "pie"` in the variant, since the alternative is
    /// emitting something that cannot run.
    #[test]
    fn linux_race_is_forced_off_pie() {
        assert_eq!(BuildMode::Pie.for_race("linux", true), BuildMode::Exe);
        assert_eq!(BuildMode::Exe.for_race("linux", true), BuildMode::Exe);
    }

    /// …and nothing else is touched: darwin supports pie+race, and an ordinary
    /// build keeps exactly the mode its variant asked for.
    #[test]
    fn for_race_leaves_every_other_case_alone() {
        assert_eq!(BuildMode::Pie.for_race("darwin", true), BuildMode::Pie);
        assert_eq!(BuildMode::Pie.for_race("linux", false), BuildMode::Pie);
        assert_eq!(BuildMode::Pie.for_race("darwin", false), BuildMode::Pie);
        assert_eq!(BuildMode::Exe.for_race("darwin", true), BuildMode::Exe);
    }

    /// The override has to keep the compile and link sides agreeing: forcing
    /// `exe` also drops `-shared`, which is what makes the archives linkable.
    #[test]
    fn forcing_exe_for_race_also_drops_shared() {
        assert!(!BuildMode::Pie.for_race("linux", true).needs_shared());
        assert!(BuildMode::Pie.for_race("darwin", true).needs_shared());
    }

    #[test]
    fn factors_expose_the_cgo_rule() {
        let f = Factors {
            goos: "linux".into(),
            goarch: "arm64".into(),
            race: true,
            ..Default::default()
        };
        assert_eq!(f.cgo_enabled_value(), "1");
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
