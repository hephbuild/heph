//! The `GOCACHE` Go tooling runs against, declared as a scratch cache.
//!
//! # Why this is not sandbox-local
//!
//! Each `_golist` target used to get its own empty `GOCACHE` inside its sandbox.
//! That is the hermetic default, and it is expensive twice over:
//!
//! 1. `go list -e -test` must rebuild the standard library's *test* dependency
//!    metadata from cold every time — ~0.35s of mostly-kernel work per package,
//!    byte-for-byte identical for every package in the repo.
//! 2. Populating and then tearing down that cache churns ~500 filesystem entries
//!    per sandbox, and it is that churn — not CPU — that governs cold wall time.
//!
//! Measured on a 500-package corpus: 1945 `go list` invocations costing 778s of
//! CPU (687s of it system time), against 15s for every `go tool compile`
//! combined. Listing, not compiling, was the entire cold path.
//!
//! Point (2) is why pre-seeding a warm cache into each sandbox was not enough: it
//! cut `go list` CPU by 60% and moved wall time by nothing at all (interleaved
//! A/B: 217.3s vs 219.5s), because it merely swapped Go's compute for heph's
//! `mkdir`/`link`/`unlink`. Only *not materializing a cache per sandbox* moves
//! the number: 2.4x on the same corpus (205s -> 84s wall).
//!
//! # Why sharing it is sound
//!
//! Go's build cache is content-addressed and self-verifying: an entry is keyed by
//! an action ID derived from the full input set (tool build ID, source content,
//! flags), and re-checked on read. A hit is provably the same answer as a miss,
//! and an entry that does not apply is simply not found. Nothing one run writes
//! here can change what a *different* run computes — only how fast it computes
//! it. That is what `access = "shared"` asserts, and why the concurrency Go's own
//! `go build -p N` already relies on is safe here.
//!
//! # What replaced the hand-rolled version
//!
//! This used to be `golist_gocache.rs`: a directory under the engine home, keyed
//! by a hand-written struct, with no lock, no eviction, no remote, no visibility,
//! and available to exactly one driver. It is now an ordinary `scratch` target,
//! which supplies all of those.
//!
//! One deliberate difference. The old key included the resolved **GOROOT path**,
//! because Go's action IDs incorporate it. A scratch declaration is resolved
//! before the driver runs, and GOROOT is not known until it has — so the path is
//! gone from the key, and that is an improvement rather than a loss:
//!
//! * Under `gotool: "host"`, GOROOT is a function of the Go version, which *is*
//!   in the key. Nothing changes.
//! * Under a hermetic toolchain, the SDK stages at a **different absolute path in
//!   every sandbox**, so the old key gave every sandbox its own slot and every
//!   one of them fell back to cold. Dropping the path gives them one shared slot.
//!
//! That does not make a hermetic build's action IDs match across sandboxes — they
//! still embed the staged GOROOT — so the win there is bounded until the SDK is
//! staged at a stable path. It does mean the cache stops being fragmented for no
//! reason.

use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::provider::TargetSpec;
use std::collections::{BTreeMap, HashMap};

/// Synthetic package holding the shared Go build caches.
pub const GOCACHE_PKG: &str = "@heph/go/gocache";

/// Target name within [`GOCACHE_PKG`] for the build cache.
pub const GOCACHE_NAME: &str = "cache";

/// Target name within [`GOCACHE_PKG`] for the **module** cache.
///
/// Separate from the build cache because they are different things with
/// different portability: `GOCACHE` holds host-toolchain artifacts, `GOMODCACHE`
/// holds downloaded module *source*.
pub const GOMODCACHE_NAME: &str = "modcache";

/// The module cache's address.
///
/// Keyed on nothing but the target name. Module cache entries are
/// `module@version` source trees, verified against `go.sum` — the same bytes on
/// every platform, under every toolchain and every set of build tags. Keying it
/// on anything would fragment a cache that has no reason to be fragmented.
pub fn modcache_addr() -> Addr {
    Addr::new(
        PkgBuf::from(GOCACHE_PKG),
        GOMODCACHE_NAME.to_string(),
        Default::default(),
    )
}

/// Build the `scratch` spec for the shared module cache.
///
/// **No `path`.** `go mod download` finds the cache through `GOMODCACHE`, so the
/// directory is never placed in the sandbox tree — which is what makes this
/// possible at all: the thirdparty download target collects `out = "**/*"` from
/// its package, and an in-tree mount there would be swept into the artifact. Its
/// own source comment already recorded working around exactly that by hand.
pub fn build_modcache_spec(addr: Addr) -> TargetSpec {
    let config: HashMap<String, Value> = HashMap::from([
        ("env".to_string(), Value::String("GOMODCACHE".to_string())),
        // Go's module cache is content-addressed and `go.sum`-verified, and
        // concurrent `go mod download` is ordinary — the same trust heph already
        // extended to the host modcache passthrough this replaces.
        ("access".to_string(), Value::String("shared".to_string())),
        // Module source, not objects. The showcase for `any`: one cache serves a
        // Linux CI runner and a macOS laptop alike.
        ("platform".to_string(), Value::String("any".to_string())),
        ("remote".to_string(), Value::Bool(false)),
    ]);

    TargetSpec {
        addr,
        driver: "scratch".to_string(),
        config,
        labels: vec!["go-gomodcache".to_string()],
        ..Default::default()
    }
}

/// Where the cache is mounted in a consuming sandbox.
///
/// Kept identical to the path the sandbox-local cache used, so a stale directory
/// left by an older heph is simply overwritten by the mount rather than sitting
/// beside it.
const GOCACHE_MOUNT: &str = ".heph-gocache";

/// Everything a `GOCACHE` entry can depend on that is known before the driver
/// runs. Two runs sharing these can share a cache directory.
///
/// GOROOT is deliberately absent — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GocacheKey {
    /// The Go module this cache belongs to: the `go.mod` directory, relative to
    /// the workspace root, and empty for the root module or for stdlib (which
    /// belongs to no module).
    ///
    /// One cache per module rather than one per workspace. Sharing is *correct*
    /// either way — Go's cache is content-addressed and self-verifying — so this
    /// buys management rather than correctness: a module's cache is bounded,
    /// evicted, published and inspected on its own, and a monorepo where one
    /// module churns does not push another module's entries out.
    ///
    /// The cost is honest and worth stating: each module's first `go list` pays
    /// for the standard library's metadata again, because Go writes whatever it
    /// computes into whichever `GOCACHE` it was pointed at. That is once per
    /// module, not once per target, and it is the price of the isolation.
    pub module: String,
    /// Pinned Go release, or the host toolchain's version.
    pub go_version: String,
    pub goos: String,
    pub goarch: String,
    pub build_tags: Vec<String>,
    pub goexperiment: Vec<String>,
    /// Race builds see a different file set (`//go:build race`) and a different
    /// import graph, so they get their own slot rather than churning the ordinary
    /// one. Go's own cache would key the entries correctly either way; this keeps
    /// the two working sets from evicting each other.
    pub race: bool,
}

impl GocacheKey {
    /// Addr args identifying this key.
    ///
    /// Readable rather than hashed, because they show up in `heph tool scratch
    /// ls` and in any diagnostic naming the target — "which cache is this?" is a
    /// question a hash cannot answer. The unbounded lists are joined, not
    /// hashed, for the same reason; they are short in practice and a synthetic
    /// addr has no length limit worth respecting over legibility.
    fn args(&self) -> BTreeMap<String, String> {
        let mut args = BTreeMap::from([
            ("go".to_string(), self.go_version.clone()),
            ("goos".to_string(), self.goos.clone()),
            ("goarch".to_string(), self.goarch.clone()),
        ]);
        // Omitted when empty, so the root module and stdlib share the bare form
        // rather than carrying a `mod=` that says nothing.
        if !self.module.is_empty() {
            args.insert("mod".to_string(), self.module.clone());
        }
        if !self.build_tags.is_empty() {
            args.insert("tags".to_string(), self.build_tags.join("+"));
        }
        if !self.goexperiment.is_empty() {
            args.insert("exp".to_string(), self.goexperiment.join("+"));
        }
        if self.race {
            args.insert("race".to_string(), "1".to_string());
        }
        args
    }

    /// The scratch target this key resolves to.
    ///
    /// One addr per distinct key, and the engine keys a scratch slot on the addr —
    /// so two runs agreeing on every factor share a directory, and two that
    /// disagree on any of them do not.
    pub fn addr(&self) -> Addr {
        Addr::new(
            PkgBuf::from(GOCACHE_PKG),
            GOCACHE_NAME.to_string(),
            self.args(),
        )
    }
}

/// True when `pkg` is the synthetic gocache package.
///
/// Matched before package decoding in `handle_get`, exactly as the toolchain is:
/// there is no such directory on disk, and asking the filesystem about it would
/// only produce a confusing "not found".
pub fn is_gocache_pkg(pkg: &str) -> bool {
    pkg == GOCACHE_PKG
}

/// Build the `scratch` target spec for a gocache addr.
///
/// The factors are already in the addr, so `version` stays empty: the slot key
/// folds the addr in, and duplicating the factors into `version` would only give
/// two ways to say the same thing.
///
/// Note what is deliberately *not* in it: the host OS and arch. A Go build
/// cache's entries depend on the **target** (`GOOS`/`GOARCH`) and the toolchain,
/// both already in the addr — not on the machine that ran the compiler. Keying on
/// the host would give a laptop cross-compiling to `linux/amd64` and a CI runner
/// building it natively different slots for identical content.
pub fn build_spec(addr: Addr) -> TargetSpec {
    let config: HashMap<String, Value> = HashMap::from([
        ("path".to_string(), Value::String(GOCACHE_MOUNT.to_string())),
        ("env".to_string(), Value::String("GOCACHE".to_string())),
        // Go's build cache is safe under concurrent access by construction — it
        // is what `go build -p N` does — and serializing it would turn the win
        // above into a large loss.
        ("access".to_string(), Value::String("shared".to_string())),
        // Local-only for now. Publishing a Go build cache is worth doing and is a
        // separate change with its own before/after: the contents are large, and
        // whether the transfer beats a cold `go list` is a measurement, not a
        // guess.
        ("remote".to_string(), Value::Bool(false)),
    ]);

    TargetSpec {
        addr,
        driver: "scratch".to_string(),
        config,
        labels: vec!["go-gocache".to_string()],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> GocacheKey {
        GocacheKey {
            module: String::new(),
            go_version: "1.27.0".to_string(),
            goos: "linux".to_string(),
            goarch: "amd64".to_string(),
            build_tags: vec![],
            goexperiment: vec![],
            race: false,
        }
    }

    /// One cache per module, so two modules in one workspace never share a
    /// `GOCACHE` — the whole point of keying on it.
    #[test]
    fn two_modules_get_two_caches() {
        let mut a = key();
        a.module = "svc/api".to_string();
        let mut b = key();
        b.module = "tools".to_string();
        assert_ne!(a.addr(), b.addr());
        // And the arg is legible rather than hashed, because `heph tool scratch
        // ls` has to answer "which module is this?".
        assert!(
            a.addr().format().contains("mod=svc/api"),
            "{}",
            a.addr().format()
        );
    }

    /// The root module and stdlib carry no `mod=` at all. A `mod=` that is empty
    /// would be noise in every addr for the common single-module workspace, and
    /// it must not read as "a module named empty-string".
    #[test]
    fn the_module_less_cache_carries_no_mod_arg() {
        let formatted = key().addr().format();
        assert!(!formatted.contains("mod="), "{formatted}");
    }

    #[test]
    fn the_package_is_recognized_and_nothing_else_is() {
        assert!(is_gocache_pkg(GOCACHE_PKG));
        assert!(!is_gocache_pkg("@heph/go/toolchain/1.27.0"));
        assert!(!is_gocache_pkg("@heph/go/gocache/nested"));
        assert!(!is_gocache_pkg("mylib"));
    }

    /// Every factor must move the addr, because the engine keys the slot on the
    /// addr — a factor that does not move it silently shares a cache between two
    /// builds that disagree.
    #[test]
    fn every_factor_moves_the_addr() {
        let base = key().addr();
        let mut cases: Vec<(&str, GocacheKey)> = Vec::new();

        let mut k = key();
        k.module = "svc".to_string();
        cases.push(("module", k));
        let mut k = key();
        k.go_version = "1.26.0".to_string();
        cases.push(("go_version", k));
        let mut k = key();
        k.goos = "darwin".to_string();
        cases.push(("goos", k));
        let mut k = key();
        k.goarch = "arm64".to_string();
        cases.push(("goarch", k));
        let mut k = key();
        k.build_tags = vec!["integration".to_string()];
        cases.push(("build_tags", k));
        let mut k = key();
        k.goexperiment = vec!["arenas".to_string()];
        cases.push(("goexperiment", k));
        let mut k = key();
        k.race = true;
        cases.push(("race", k));

        for (what, k) in cases {
            assert_ne!(k.addr(), base, "{what} must key a distinct cache");
        }
    }

    /// Two keys that agree on everything must land on one addr, whatever order
    /// the lists were built in — otherwise the sharing this exists for is a
    /// coin flip.
    #[test]
    fn equal_factors_share_one_addr() {
        assert_eq!(key().addr(), key().addr());

        let mut a = key();
        a.build_tags = vec!["x".to_string(), "y".to_string()];
        let mut b = key();
        b.build_tags = vec!["x".to_string(), "y".to_string()];
        assert_eq!(a.addr(), b.addr());
    }

    /// Absent lists leave their args off entirely, so the common addr stays short
    /// and readable in `heph tool scratch ls`.
    #[test]
    fn empty_lists_do_not_appear_in_the_addr() {
        let args = key().args();
        assert!(!args.contains_key("tags"));
        assert!(!args.contains_key("exp"));
        assert!(!args.contains_key("race"));
        assert_eq!(args["go"], "1.27.0");
    }

    #[test]
    fn the_spec_declares_a_shared_gocache_scratch() {
        let spec = build_spec(key().addr());
        assert_eq!(spec.driver, "scratch");
        assert_eq!(spec.config["env"], Value::String("GOCACHE".to_string()));
        assert_eq!(spec.config["access"], Value::String("shared".to_string()));
        assert_eq!(
            spec.config["path"],
            Value::String(GOCACHE_MOUNT.to_string())
        );
        // The factors live in the addr; duplicating them into `version` would be
        // two ways to say one thing.
        assert!(!spec.config.contains_key("version"));
    }

    /// The spec must survive the driver that will actually parse it.
    ///
    /// Asserting individual config keys, as the test above does, cannot catch a
    /// key the driver does not accept — and a `scratch` declaration rejects
    /// unknown fields, so emitting one is not a warning but a hard failure of
    /// every Go target that references the cache. This plugin shipped exactly
    /// that: it kept setting `platform` after the field was removed from the
    /// declaration, and no unit test noticed because none of them parsed.
    #[test]
    fn the_spec_parses_as_a_declaration() {
        let spec = build_spec(key().addr());
        hbuiltins::pluginscratch::parse_declaration(&spec)
            .unwrap_or_else(|e| panic!("the gocache spec must parse as a declaration: {e:#}"));
    }
}
