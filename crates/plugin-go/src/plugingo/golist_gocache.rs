//! The `GOCACHE` that `go list` runs against, shared across golist sandboxes.
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
//! cut `go list` CPU by 60% (778s -> 309s) and moved wall time by nothing at all
//! (interleaved A/B: 217.3s vs 219.5s), because it merely swapped Go's compute
//! for heph's `mkdir`/`link`/`unlink`. Only *not materializing a cache per
//! sandbox* moves the number: 2.4x on the same corpus (205s -> 84s wall).
//!
//! # Why sharing it is sound
//!
//! Go's build cache is content-addressed and self-verifying: an entry is keyed by
//! an action ID derived from the full input set (tool build ID, source content,
//! flags), and re-checked on read. A hit is provably the same answer as a miss,
//! and an entry that does not apply is simply not found. Nothing a golist run
//! writes here can change what a *different* golist run computes — only how fast
//! it computes it.
//!
//! This is the same trust heph already extends to `GOMODCACHE`/`GOPATH`/
//! `GOPROXY`, which the golist driver passes through from the host on the
//! explicit grounds that modules are content-addressed and verified. A cache that
//! heph owns, under the engine home, keyed by toolchain and build factors, is
//! strictly more controlled than that existing passthrough — and strictly more
//! hermetic than the host `GOCACHE` the sandbox-local cache was introduced to
//! avoid, which is still never touched.
//!
//! What it costs: `go list` metadata for the repo's packages accumulates here
//! (tens of MB for a large repo, not the compiled-object cache — `go list`
//! without `-export` compiles nothing). Go trims its own cache of entries unused
//! for five days, so this is self-limiting without heph managing it.
//!
//! # Keying
//!
//! Go's action IDs incorporate the GOROOT path and the build factors, so a cache
//! populated under one toolchain is inert under another rather than wrong. The
//! directory is keyed on both anyway, so unrelated toolchains do not pile into
//! one directory and defeat Go's trimming.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Everything a `go list` cache entry can depend on. Two golist runs sharing a
/// key can share a cache directory.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct GocacheKey {
    /// Resolved GOROOT. Part of the key because Go's action IDs incorporate it —
    /// see the module docs.
    pub goroot: PathBuf,
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
    /// Directory name for this key. Hashed rather than spelled out because
    /// GOROOT is an absolute path and the tag lists are unbounded.
    fn slot(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = xxhash_rust::xxh3::Xxh3Default::new();
        self.hash(&mut h);
        format!("{:016x}", h.finish())
    }
}

/// Resolves the `GOCACHE` a golist run should use.
///
/// `root` is handed in by whoever constructs the driver (the engine home dir, via
/// the cdylib's `CreateConfig`) rather than discovered — a plugin has no
/// `$TMPDIR` to assume. `None` keeps the old sandbox-local behaviour, which is
/// what in-process constructions (tests, the e2e harness) use: they have no
/// engine home to share through, and their workloads are single-package anyway.
pub(crate) struct GolistGocache {
    root: Option<PathBuf>,
}

impl GolistGocache {
    pub(crate) fn new(root: Option<PathBuf>) -> Self {
        Self { root }
    }

    /// The `GOCACHE` directory for this key, created if absent.
    ///
    /// Falls back to `sandbox_local` when no shared root is configured, or when
    /// the shared directory cannot be created — a cold cache is slow, never
    /// wrong, so this must not fail a target.
    pub(crate) fn resolve(&self, key: &GocacheKey, sandbox_local: &Path) -> Result<PathBuf> {
        if let Some(root) = &self.root {
            let dir = root.join(key.slot());
            // Concurrent golist runs race here; `create_dir_all` is idempotent
            // and Go's own cache access is designed for concurrent use (it is
            // what `go build -p N` does).
            match std::fs::create_dir_all(&dir) {
                Ok(()) => return Ok(dir),
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        dir = %dir.display(),
                        "could not create the shared golist GOCACHE; falling back to a sandbox-local one"
                    );
                }
            }
        }
        std::fs::create_dir_all(sandbox_local)
            .with_context(|| format!("create gocache dir {sandbox_local:?}"))?;
        Ok(sandbox_local.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> GocacheKey {
        GocacheKey {
            goroot: PathBuf::from("/goroot"),
            goos: "linux".to_string(),
            goarch: "amd64".to_string(),
            build_tags: vec![],
            goexperiment: vec![],
            race: false,
        }
    }

    #[test]
    fn slot_differs_by_goroot() {
        let mut other = key();
        other.goroot = PathBuf::from("/other");
        assert_ne!(
            key().slot(),
            other.slot(),
            "Go's action IDs incorporate GOROOT, so two toolchains must not share a cache dir"
        );
    }

    #[test]
    fn slot_differs_by_every_factor() {
        for mutate in [
            (|k: &mut GocacheKey| k.goos = "darwin".to_string()) as fn(&mut GocacheKey),
            |k: &mut GocacheKey| k.goarch = "arm64".to_string(),
            |k: &mut GocacheKey| k.build_tags = vec!["netgo".to_string()],
            |k: &mut GocacheKey| k.goexperiment = vec!["arenas".to_string()],
        ] {
            let mut other = key();
            mutate(&mut other);
            assert_ne!(
                key().slot(),
                other.slot(),
                "a factor that changes what `go list` resolves must change the cache slot"
            );
        }
    }

    #[test]
    fn slot_is_stable_across_calls() {
        assert_eq!(
            key().slot(),
            key().slot(),
            "an unstable slot would give every run a fresh cache, defeating the point"
        );
    }

    #[test]
    fn shared_root_is_used_and_reused_across_sandboxes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = dir.path().join("shared");
        let g = GolistGocache::new(Some(shared.clone()));

        let a = g
            .resolve(&key(), &dir.path().join("sandboxA/.heph-gocache"))
            .expect("resolve A");
        let b = g
            .resolve(&key(), &dir.path().join("sandboxB/.heph-gocache"))
            .expect("resolve B");

        assert_eq!(a, b, "two sandboxes with the same key must share one cache");
        assert!(
            a.starts_with(&shared),
            "the cache must live under the shared root"
        );
        assert!(a.is_dir(), "the shared cache dir must exist after resolve");
        assert!(
            !dir.path().join("sandboxA/.heph-gocache").exists(),
            "nothing may be materialized in the sandbox — that churn is the cost being removed"
        );
    }

    #[test]
    fn different_keys_get_different_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let g = GolistGocache::new(Some(dir.path().join("shared")));
        let mut other = key();
        other.goarch = "arm64".to_string();
        let a = g
            .resolve(&key(), &dir.path().join("s/.heph-gocache"))
            .expect("a");
        let b = g
            .resolve(&other, &dir.path().join("s/.heph-gocache"))
            .expect("b");
        assert_ne!(a, b);
    }

    #[test]
    fn without_a_shared_root_it_falls_back_to_the_sandbox() {
        let dir = tempfile::tempdir().expect("tempdir");
        let local = dir.path().join("sandbox").join(".heph-gocache");
        let g = GolistGocache::new(None);
        let got = g.resolve(&key(), &local).expect("resolve");
        assert_eq!(got, local);
        assert!(
            got.is_dir(),
            "the fallback must still leave a usable GOCACHE"
        );
    }
}
