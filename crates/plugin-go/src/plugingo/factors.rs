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
    /// [`BuildMode::needs_shared`].
    pub buildmode: BuildMode,
}

impl Factors {
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
}

impl VariantRef {
    pub fn new(name: impl Into<String>, pkg: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            pkg: pkg.into(),
        }
    }

    /// Encode as addr args `{v, vp}`. `vp` is always emitted (even empty, for the
    /// root package) so a present `vp` key unambiguously marks a fully-resolved
    /// internal address vs. a bare user `@v=` address.
    pub fn to_args(&self) -> BTreeMap<String, String> {
        let mut args = BTreeMap::new();
        args.insert("v".to_string(), self.name.clone());
        args.insert("vp".to_string(), self.pkg.clone());
        args
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

    #[test]
    fn test_goexperiment_env() {
        let mut f = Factors::default();
        assert_eq!(f.goexperiment_env(), None);
        f.goexperiment = vec!["arenas".into(), "loopvar".into()];
        assert_eq!(f.goexperiment_env(), Some("arenas,loopvar".to_string()));
    }
}
