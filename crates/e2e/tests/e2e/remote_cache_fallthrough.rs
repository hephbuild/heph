//! Two `file://` remotes, both written by a cold run, read first-then-second.
//! Each test damages the first remote's copy of a revision the way a partial
//! restore, a stray copy or an eviction would, deletes the local cache, and
//! runs again with a fresh engine.
//!
//! Every target's output is random, so a re-execution produces a *different*
//! output. An output identical to the cold run's therefore proves the second
//! remote served it: the first could not, and a rebuild would not match. The
//! run must also succeed — a damaged entry on one remote is never a failure.

use crate::common::{build_engine_with_remotes, drain};
use heph::engine::HomeDir;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Remotes {
    first: tempfile::TempDir,
    second: tempfile::TempDir,
}

impl Remotes {
    fn new() -> Self {
        Self {
            first: tempfile::tempdir().expect("first remote"),
            second: tempfile::tempdir().expect("second remote"),
        }
    }

    /// A fresh engine over `root` reading `first`, then `second`.
    fn engine(&self, root: &Path) -> Arc<heph::engine::Engine> {
        let first = format!("file://{}", self.first.path().display());
        let second = format!("file://{}", self.second.path().display());
        build_engine_with_remotes(root, &[("first", &first), ("second", &second)])
    }
}

/// Run `addr` with all its outputs, drain the background uploads, and return
/// the output's bytes. Panics if the target fails.
async fn run(engine: &Arc<heph::engine::Engine>, addr: &str) -> Vec<u8> {
    let addr = heph::htaddr::parse_addr(addr).expect("parse addr");
    let rs = engine.new_state();
    let result = Arc::clone(engine)
        .result_addr(
            rs.clone(),
            &addr,
            heph::engine::OutputMatcher::All,
            &heph::engine::ResultOptions::default(),
        )
        .await
        .expect("a damaged entry on one remote must not fail the target");
    drain(rs).await;
    crate::common::artifact_bytes(&result)
}

fn write_build(root: &Path, build: &str) {
    std::fs::create_dir_all(root.join("pkg")).expect("mkdir pkg");
    std::fs::write(root.join("pkg").join("BUILD"), build).expect("write BUILD");
}

fn rm_local_cache(root: &Path) {
    std::fs::remove_dir_all(HomeDir::for_tests(root).join("cache")).expect("rm local cache");
}

/// The revision directories `remote` holds for `//pkg:<target>`, one per hashin.
fn revisions(remote: &Path, target: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(remote.join("pkg").join(target))
        .expect("the remote holds the target")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.join("manifest-v2.borsh").is_file())
        .collect();
    dirs.sort();
    dirs
}

/// The one revision `remote` holds for `//pkg:<target>`.
fn revision(remote: &Path, target: &str) -> PathBuf {
    let mut dirs = revisions(remote, target);
    assert_eq!(dirs.len(), 1, "one revision of {target}: {dirs:?}");
    dirs.remove(0)
}

/// The output blob of the revision in `dir`.
fn out_blob(dir: &Path) -> PathBuf {
    let blobs: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read revision")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("out_"))
        })
        .collect();
    assert_eq!(
        blobs.len(),
        1,
        "one output blob in {}: {blobs:?}",
        dir.display()
    );
    blobs.into_iter().next().expect("one blob")
}

/// Replace every object in revision `dst` with `src`'s, keeping `dst`'s key:
/// another revision restored under this one's name.
fn copy_revision_over(src: &Path, dst: &Path) {
    std::fs::remove_dir_all(dst).expect("clear revision");
    std::fs::create_dir_all(dst).expect("recreate revision");
    for entry in std::fs::read_dir(src).expect("read source revision") {
        let entry = entry.expect("dir entry");
        std::fs::copy(entry.path(), dst.join(entry.file_name())).expect("copy object");
    }
}

/// Cut `file` to half its length: a copy that died part-way.
fn truncate_half(file: &Path) {
    let len = std::fs::metadata(file).expect("stat blob").len();
    assert!(len > 1, "blob too small to truncate: {len}");
    std::fs::OpenOptions::new()
        .write(true)
        .open(file)
        .expect("open blob")
        .set_len(len / 2)
        .expect("truncate blob");
}

const RANDOM_TARGET: &str = r#"target(name = "t", driver = "bash", run = "echo $RANDOM$RANDOM$RANDOM > $OUT", out = "out.txt")"#;

/// Build `//pkg:t` cold into both remotes and drop the local cache. Returns
/// the cold output.
async fn cold(root: &Path, remotes: &Remotes) -> Vec<u8> {
    let out = run(&remotes.engine(root), "//pkg:t").await;
    assert!(!out.is_empty(), "cold run produced no output");
    rm_local_cache(root);
    out
}

/// A manifest that does not deserialize on the first remote.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_garbage_manifest_falls_through() {
    let root = tempfile::tempdir().expect("root");
    let remotes = Remotes::new();
    write_build(root.path(), RANDOM_TARGET);
    let out = cold(root.path(), &remotes).await;

    let rev = revision(remotes.first.path(), "t");
    std::fs::write(rev.join("manifest-v2.borsh"), b"not a manifest").expect("garble manifest");

    assert_eq!(
        run(&remotes.engine(root.path()), "//pkg:t").await,
        out,
        "the second remote must serve the revision, not a rebuild"
    );
}

/// Another target's revision restored under this one's key on the first
/// remote: its manifest names `//pkg:u`, and its blobs are `u`'s output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_targets_revision_under_the_key_falls_through() {
    let root = tempfile::tempdir().expect("root");
    let remotes = Remotes::new();
    write_build(
        root.path(),
        &format!(
            "{RANDOM_TARGET}\n{}",
            r#"target(name = "u", driver = "bash", run = "echo $RANDOM$RANDOM$RANDOM > $OUT", out = "out.txt")"#
        ),
    );
    let cold_engine = remotes.engine(root.path());
    let out = run(&cold_engine, "//pkg:t").await;
    let other = run(&cold_engine, "//pkg:u").await;
    drop(cold_engine);
    assert_ne!(out, other, "the two targets must be told apart by output");
    rm_local_cache(root.path());

    copy_revision_over(
        &revision(remotes.first.path(), "u"),
        &revision(remotes.first.path(), "t"),
    );

    assert_eq!(
        run(&remotes.engine(root.path()), "//pkg:t").await,
        out,
        "the second remote must serve t's revision, not u's bytes or a rebuild"
    );
}

/// An older revision of the same target restored under the current hashin's
/// key on the first remote: its manifest names another hashin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_hashins_revision_under_the_key_falls_through() {
    let root = tempfile::tempdir().expect("root");
    let remotes = Remotes::new();
    write_build(root.path(), RANDOM_TARGET);
    let old = run(&remotes.engine(root.path()), "//pkg:t").await;
    let old_rev = revision(remotes.first.path(), "t");

    // A changed definition: a new hashin, a second revision on each remote.
    write_build(
        root.path(),
        r#"target(name = "t", driver = "bash", run = "echo new $RANDOM$RANDOM$RANDOM > $OUT", out = "out.txt")"#,
    );
    let out = run(&remotes.engine(root.path()), "//pkg:t").await;
    assert_ne!(out, old);
    rm_local_cache(root.path());
    let new_rev = revisions(remotes.first.path(), "t")
        .into_iter()
        .find(|d| *d != old_rev)
        .expect("a revision for the new hashin");

    copy_revision_over(&old_rev, &new_rev);

    assert_eq!(
        run(&remotes.engine(root.path()), "//pkg:t").await,
        out,
        "the second remote must serve the new revision, not the old one's bytes or a rebuild"
    );
}

/// A blob stored verbatim (its payload is already compressed, and over the
/// 1 MiB sniff floor) cut short on the first remote.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_truncated_raw_blob_falls_through() {
    let root = tempfile::tempdir().expect("root");
    let remotes = Remotes::new();
    write_build(
        root.path(),
        r#"target(name = "t", driver = "bash", run = "head -c 2000000 /dev/urandom | gzip > $OUT", out = "out.gz")"#,
    );
    let out = cold(root.path(), &remotes).await;
    assert!(
        out.len() > 1_500_000,
        "the output must clear the sniff floor"
    );

    let blob = out_blob(&revision(remotes.first.path(), "t"));
    assert!(
        !blob.to_string_lossy().ends_with(".zst"),
        "a gzip payload must be stored raw: {}",
        blob.display()
    );
    truncate_half(&blob);

    assert_eq!(
        run(&remotes.engine(root.path()), "//pkg:t").await,
        out,
        "the second remote must serve the whole blob, not a rebuild"
    );
}

/// A zstd blob cut short on the first remote: its seek table is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_truncated_zstd_blob_falls_through() {
    let root = tempfile::tempdir().expect("root");
    let remotes = Remotes::new();
    write_build(root.path(), RANDOM_TARGET);
    let out = cold(root.path(), &remotes).await;

    let blob = out_blob(&revision(remotes.first.path(), "t"));
    assert!(
        blob.to_string_lossy().ends_with(".zst"),
        "a small text output must be stored zstd: {}",
        blob.display()
    );
    truncate_half(&blob);

    assert_eq!(
        run(&remotes.engine(root.path()), "//pkg:t").await,
        out,
        "the second remote must serve the whole blob, not a rebuild"
    );
}

/// The hit decision: the first remote's manifest is whole, but its output
/// blob is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_blob_falls_through_on_the_hit() {
    let root = tempfile::tempdir().expect("root");
    let remotes = Remotes::new();
    write_build(root.path(), RANDOM_TARGET);
    let out = cold(root.path(), &remotes).await;

    std::fs::remove_file(out_blob(&revision(remotes.first.path(), "t"))).expect("evict blob");

    assert_eq!(
        run(&remotes.engine(root.path()), "//pkg:t").await,
        out,
        "the second remote must serve the revision, not a rebuild"
    );
}

/// Total bytes of every file under `dir`.
fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            let path = e.path();
            if path.is_dir() {
                dir_bytes(&path)
            } else {
                e.metadata().map_or(0, |m| m.len())
            }
        })
        .sum()
}

/// The lazy pull: `top` is a remote hit, so `dep`'s manifest is mirrored
/// locally and its (multi-MiB) blob left on the remotes. The first remote then loses
/// `dep`'s blob, and a later run asking for `dep`'s output must pull it from
/// the second rather than rebuild.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_blob_falls_through_on_a_lazy_pull() {
    let root = tempfile::tempdir().expect("root");
    let remotes = Remotes::new();
    write_build(
        root.path(),
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
    );
    let cold_engine = remotes.engine(root.path());
    let top = run(&cold_engine, "//pkg:top").await;
    let dep = run(&cold_engine, "//pkg:dep").await;
    drop(cold_engine);
    rm_local_cache(root.path());

    // `top` from the remote; `dep` only for its hashout, so its blob stays put.
    assert_eq!(run(&remotes.engine(root.path()), "//pkg:top").await, top);
    let local = dir_bytes(&HomeDir::for_tests(root.path()).join("cache"));
    assert!(
        local < dep.len() as u64 / 2,
        "resolving top pulled {local} bytes: dep's {}-byte blob came down with it, \
         so the run below would not pull it lazily",
        dep.len()
    );

    std::fs::remove_file(out_blob(&revision(remotes.first.path(), "dep"))).expect("evict blob");

    assert_eq!(
        run(&remotes.engine(root.path()), "//pkg:dep").await,
        dep,
        "the lazy pull must fall through to the second remote, not rebuild"
    );
}
