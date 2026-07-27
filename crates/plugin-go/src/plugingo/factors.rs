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

    #[test]
    fn test_goexperiment_env() {
        let mut f = Factors::default();
        assert_eq!(f.goexperiment_env(), None);
        f.goexperiment = vec!["arenas".into(), "loopvar".into()];
        assert_eq!(f.goexperiment_env(), Some("arenas,loopvar".to_string()));
    }
}
