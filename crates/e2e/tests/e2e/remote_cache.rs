//! End-to-end remote cache test over a `file://` backend, exercising the full
//! cold → warm → hot lifecycle with *separate engines over the same root*:
//!
//! - **cold**: empty everything. The target executes; its artifacts are written
//!   to the local cache and pushed to the remote (background task).
//! - **warm**: a fresh engine (empty in-memory cache) with the on-disk local
//!   cache deleted — a genuine local miss. The revision is pulled from the
//!   remote into the local cache instead of re-executing.
//! - **hot**: the same warm engine again — now a fully local cache hit.
//!
//! The target's output is a random value, so a re-execution would produce a
//! *different* output. An output that stays identical across runs therefore
//! proves the result came from a cache, not a fresh execution.

use crate::common::{build_engine, build_engine_with_remote, run_drain};
use heph::engine::HomeDir;
use std::path::Path;

/// Count the objects (files) under a `file://` cache directory.
fn remote_object_count(dir: &Path) -> usize {
    let mut count = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            count += remote_object_count(&path);
        } else {
            count += 1;
        }
    }
    count
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_cache_cold_warm_hot() {
    let root = tempfile::tempdir().expect("root tempdir");
    let remote = tempfile::tempdir().expect("remote tempdir");
    let remote_uri = format!("file://{}", remote.path().display());

    // The output is a random value, so re-executing the target yields a
    // *different* output. An unchanged output across runs ⇒ it came from a cache.
    std::fs::create_dir_all(root.path().join("pkg")).expect("mkdir pkg");
    std::fs::write(
        root.path().join("pkg").join("BUILD"),
        r#"target(name = "t", driver = "bash", run = "echo $RANDOM$RANDOM$RANDOM > $OUT", out = "out.txt")"#,
    )
    .expect("write BUILD");

    // ---- COLD: empty everything → execute, write local + remote. ----
    let cold = build_engine(root.path(), &remote_uri);
    let out_cold = run_drain(&cold, "//pkg:t").await;
    assert!(!out_cold.trim().is_empty(), "cold run produced no output");
    assert!(
        remote_object_count(remote.path()) > 0,
        "cold run must populate the remote cache"
    );
    // Drop the cold engine (and its in-memory cache tier) before the warm run.
    drop(cold);

    // Delete the on-disk local cache so the warm run cannot hit it locally.
    std::fs::remove_dir_all(HomeDir::for_tests(root.path()).join("cache")).expect("rm local cache");

    // ---- WARM: fresh engine (empty mem) + no local cache → pull from remote. ----
    // A re-execution would produce a new random value, so equality with the cold
    // output proves the artifacts were served from the remote cache.
    let warm = build_engine(root.path(), &remote_uri);
    let out_warm = run_drain(&warm, "//pkg:t").await;
    assert_eq!(
        out_warm, out_cold,
        "warm run must serve the remote-cached output, not re-execute"
    );

    // ---- HOT: same engine again → fully local cache hit. ----
    let out_hot = run_drain(&warm, "//pkg:t").await;
    assert_eq!(
        out_hot, out_cold,
        "hot run must serve the local-cached output"
    );
}

/// Total bytes of every file under `dir`.
fn dir_bytes(dir: &Path) -> u64 {
    let mut bytes = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            bytes += dir_bytes(&path);
        } else if let Ok(meta) = entry.metadata() {
            bytes += meta.len();
        }
    }
    bytes
}

/// A remote hit must not drag down output blobs nobody reads.
///
/// `top` depends on `dep`, and `dep`'s output is several MiB. On a run where
/// `top` is itself a cache hit, `dep` is resolved only to feed `top`'s `hashin`
/// — its `hashout` comes from the manifest — so none of its output bytes are
/// needed. The local cache must therefore stay tiny: only manifests (and the
/// blob `top` itself hands back) come down.
///
/// Then asking for `dep`'s output directly pulls that blob, on demand, and it
/// comes back byte-identical — proving the deferral is a deferral, not a loss.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_pull_is_lazy_per_output() {
    let root = tempfile::tempdir().expect("root tempdir");
    let remote = tempfile::tempdir().expect("remote tempdir");
    let remote_uri = format!("file://{}", remote.path().display());

    // `dep`'s output is large AND random: large so downloading it is impossible to
    // miss in the local-cache byte count, random so serving it later proves it came
    // from the cache rather than a re-execution.
    std::fs::create_dir_all(root.path().join("pkg")).expect("mkdir pkg");
    std::fs::write(
        root.path().join("pkg").join("BUILD"),
        r#"
dep = target(
    name = "dep",
    driver = "bash",
    run = "head -c 3000000 /dev/urandom | base64 > $OUT",
    out = "dep.txt",
)

target(
    name = "top",
    driver = "bash",
    run = "echo $RANDOM$RANDOM$RANDOM$(wc -c < $SRC_DEP) > $OUT",
    deps = {"dep": dep},
    out = "top.txt",
)
"#,
    )
    .expect("write BUILD");

    // ---- COLD: build both, push both to the remote. ----
    let cold = build_engine(root.path(), &remote_uri);
    let top_cold = run_drain(&cold, "//pkg:top").await;
    let dep_cold = run_drain(&cold, "//pkg:dep").await;
    let dep_bytes = dep_cold.len() as u64;
    assert!(
        dep_bytes > 3_000_000,
        "dep output should be multi-MiB, got {dep_bytes} bytes"
    );
    drop(cold);

    let local_cache = HomeDir::for_tests(root.path()).join("cache");
    std::fs::remove_dir_all(&local_cache).expect("rm local cache");

    // ---- WARM: `top` is a remote hit; `dep` is only needed for its hashout. ----
    let warm = build_engine(root.path(), &remote_uri);
    let top_warm = run_drain(&warm, "//pkg:top").await;
    assert_eq!(
        top_warm, top_cold,
        "warm run must serve the remote-cached output, not re-execute"
    );

    let after_top = dir_bytes(&local_cache);
    assert!(
        after_top < dep_bytes / 2,
        "resolving //pkg:top pulled {after_top} bytes into the local cache — \
         //pkg:dep's {dep_bytes}-byte output was downloaded despite only its \
         hashout being needed"
    );

    // ---- Asking for dep's output pulls exactly that blob, on demand. ----
    let dep_warm = run_drain(&warm, "//pkg:dep").await;
    assert_eq!(
        dep_warm, dep_cold,
        "the deferred blob must still be pullable, byte-identical"
    );
    assert!(
        dir_bytes(&local_cache) > dep_bytes / 2,
        "asking for dep's output must actually download it"
    );
}

/// Companion check: with **no** remote configured, deleting the local cache and
/// re-running *does* re-execute (different output). This guards against the
/// cold→warh→hot test passing for the wrong reason (e.g. a stray reproducible
/// output that would match even without a cache).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_is_nondeterministic_without_cache() {
    let root = tempfile::tempdir().expect("root tempdir");
    std::fs::create_dir_all(root.path().join("pkg")).expect("mkdir pkg");
    std::fs::write(
        root.path().join("pkg").join("BUILD"),
        r#"target(name = "t", driver = "bash", run = "echo $RANDOM$RANDOM$RANDOM > $OUT", out = "out.txt")"#,
    )
    .expect("write BUILD");

    // No `caches:` → no remote.
    let build_plain = |root: &Path| build_engine_with_remote(root, None);

    let e1 = build_plain(root.path());
    let first = run_drain(&e1, "//pkg:t").await;
    drop(e1);
    std::fs::remove_dir_all(HomeDir::for_tests(root.path()).join("cache")).expect("rm local cache");

    let e2 = build_plain(root.path());
    let second = run_drain(&e2, "//pkg:t").await;

    assert_ne!(
        first, second,
        "without a cache, a fresh run must re-execute and produce a different output"
    );
}
