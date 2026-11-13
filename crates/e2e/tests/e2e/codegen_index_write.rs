//! The codegen file index's write side, end to end: what a `codegen = "copy"`
//! write-back records, deletes, keeps and refuses, through the real engine
//! (spec: codegen file index — Write protocol, Lifecycle rows 1–4, 8, 9, 12,
//! 22, 23, 25, the write-back hints, `seen_in_section`, locking).
//!
//! Assertions read the index (its db rows, `generation`, a snapshot's
//! `status`) and the filesystem; glob behaviour is another file's. A new
//! engine over the same root (`Workspace::reopen`) is the next `heph`
//! invocation: it re-reads the BUILD files and shares the on-disk cache, the
//! db and the lock files.
#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common;

use anyhow::Context as _;
use common::Workspace;
use heph::engine::{Config, Engine, HomeDir, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use heph::htwalk::codegen_index::{
    CodegenIndex, CodegenLockPaths, Row, RowState, StatusKind, StatusReason, Store, StoreOptions,
    codegen_dir, db_path,
};
use heph::htwalk::gitignore_section::{BEGIN_MARKER, END_MARKER};
use heph::{pluginbuildfile, pluginexec};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A `copy` target `name` writing `body` to `out` in its package.
pub(crate) fn copy_target(name: &str, out: &str, body: &str) -> String {
    format!(
        r#"
target(
    name = "{name}",
    driver = "bash",
    run = "printf '%s' '{body}' > $OUT",
    out = "{out}",
    codegen = "copy",
)
"#
    )
}

/// A `copy` target writing one file per word of its package's `n.txt` under
/// `gen/`, declared as the glob `gen/*.txt`: a glob output that can produce
/// fewer files than last time (Lifecycle row 4).
const GLOB_GEN: &str = r#"
target(
    name = "gen",
    driver = "bash",
    deps = [file("n.txt")],
    run = "mkdir -p gen; for w in $(cat n.txt); do printf 'file %s' \"$w\" > gen/$w.txt; done",
    out = "gen/*.txt",
    codegen = "copy",
)
"#;

/// Run `addr` on `engine` (all outputs), surfacing the root cause the way
/// `heph run` renders it.
pub(crate) async fn run_with(
    engine: &Arc<Engine>,
    addr: &str,
    opts: &ResultOptions,
) -> anyhow::Result<()> {
    let parsed = parse_addr(addr)?;
    let rs = engine.new_state();
    let res = Arc::clone(engine)
        .result_addr(Arc::clone(&rs), &parsed, OutputMatcher::All, opts)
        .await;
    match res {
        Ok(_) => {
            common::drain(rs).await;
            Ok(())
        }
        Err(err) => {
            let failures = rs.take_failures();
            if failures.is_empty() {
                return Err(err);
            }
            let msg: Vec<String> = failures
                .iter()
                .map(|f| format!("{}: {:#}", f.addr.format(), f.source))
                .collect();
            Err(anyhow::anyhow!("{}", msg.join("\n")))
        }
    }
}

pub(crate) async fn run_on(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<()> {
    run_with(engine, addr, &ResultOptions::default()).await
}

/// Every target of package `pkg`, in one request: the shape of
/// `heph r build ./pkg`.
async fn run_pkg(engine: &Arc<Engine>, pkg: &str) -> anyhow::Result<()> {
    let (res, _) = common::run_drain_events(engine, &Matcher::Package(PkgBuf::from(pkg))).await;
    res.map(drop)
}

fn home(engine: &Engine) -> PathBuf {
    engine.home.as_path().to_path_buf()
}

/// This checkout's db file.
fn db_file(engine: &Engine) -> PathBuf {
    engine.codegen_index().db_path().to_path_buf()
}

/// The db as another process sees it: opened fresh, so nothing an engine
/// caches stands in for what was committed. `None` while there is no db.
pub(crate) fn store(engine: &Engine) -> anyhow::Result<Option<Store>> {
    let path = db_file(engine);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(Store::open(&path, &StoreOptions::default())?))
}

fn generation(engine: &Engine) -> anyhow::Result<u64> {
    Ok(match store(engine)? {
        Some(s) => s.generation()?,
        None => 0,
    })
}

pub(crate) fn row(engine: &Engine, rel: &str) -> anyhow::Result<Option<Row>> {
    Ok(match store(engine)? {
        Some(s) => s.row(rel.as_bytes())?,
        None => None,
    })
}

pub(crate) fn must_row(engine: &Engine, rel: &str) -> anyhow::Result<Row> {
    row(engine, rel)?.with_context(|| format!("no index row for {rel}"))
}

/// What a fresh snapshot says `rel` is.
pub(crate) fn status(
    engine: &Engine,
    rel: &str,
) -> anyhow::Result<(StatusKind, Option<StatusReason>)> {
    let index: &CodegenIndex = engine.codegen_index();
    index.open_existing()?;
    let snap = index.snapshot()?;
    let st = snap.status_path(&index.root().join(rel), false)?;
    Ok((st.kind, st.reason))
}

/// The content hash a row records for `bytes` as a non-executable file.
fn hashout(bytes: &str) -> String {
    heph::htwalk::bytes_hashout(bytes.as_bytes(), false)
}

/// A recorded, settled row of `addr` holding `bytes`.
fn assert_recorded(row: &Row, addr: &str, bytes: &str) {
    assert_eq!(row.addr, addr, "{row:?}");
    assert_eq!(row.state, RowState::Written, "{row:?}");
    assert_eq!(row.pending_hash, None, "settled: {row:?}");
    assert_eq!(
        row.record.as_ref().map(|r| r.hash.as_str()),
        Some(hashout(bytes).as_str()),
        "the record is the bytes written: {row:?}"
    );
}

pub(crate) fn read(ws: &Workspace, rel: &str) -> Option<String> {
    std::fs::read_to_string(ws.dir.path().join(rel)).ok()
}

/// A root `.gitignore` whose managed section holds `lines`.
pub(crate) fn write_section(ws: &Workspace, lines: &[&str]) {
    let mut text = format!("{BEGIN_MARKER}\n");
    for l in lines {
        text.push_str(l);
        text.push('\n');
    }
    text.push_str(END_MARKER);
    text.push('\n');
    ws.write_file(".gitignore", &text);
}

/// Another process holding one of the codegen flocks, until dropped.
struct HeldFlock {
    _file: std::fs::File,
}

fn hold_flock(path: &Path) -> anyhow::Result<HeldFlock> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    // `flock(2)` on unix, the primitive `hlock`'s `FLock` takes.
    f.try_lock()
        .map_err(|e| anyhow::anyhow!("flock {}: {e}", path.display()))?;
    Ok(HeldFlock { _file: f })
}

/// A `MakeWriter` into a shared buffer, for capturing the engine's warnings.
#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log buffer")).into_owned()
    }
}

/// Run `fut` with a thread-local `warn` subscriber capturing into the
/// returned buffer. The callers run on a current-thread runtime, so every
/// engine task — where the write-back's messages are logged — is polled on
/// this thread.
pub(crate) async fn capture_warnings<F: std::future::Future>(fut: F) -> (F::Output, String) {
    capture_logs(tracing_subscriber::filter::LevelFilter::WARN, fut).await
}

/// [`capture_warnings`] at `level`.
pub(crate) async fn capture_logs<F: std::future::Future>(
    level: tracing_subscriber::filter::LevelFilter,
    fut: F,
) -> (F::Output, String) {
    use tracing_subscriber::util::SubscriberInitExt as _;
    let log = Captured::default();
    let writer = log.clone();
    let out = {
        // A second live dispatcher: see `resources::clamp_is_logged_once_per_addr_per_request`.
        let _second = tracing_subscriber::registry().set_default();
        let _guard = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(level)
            .set_default();
        fut.await
    };
    (out, log.text())
}

/// An engine over `root` whose home is `home` (absolute, possibly shared by
/// several checkouts): buildfile provider, bash driver.
fn engine_with_home(root: &Path, home: &Path) -> anyhow::Result<Arc<Engine>> {
    let home = HomeDir::resolve(root, Some(home))?;
    let mut e = Engine::new(Config::new(root.to_path_buf(), home))?;
    let provider_root = root.to_path_buf();
    e.register_provider(move |init| {
        Box::new(pluginbuildfile::Provider::new(
            provider_root,
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))?;
    Ok(Arc::new(e))
}

// ---------------------------------------------------------------------------
// Lifecycle rows 1–3
// ---------------------------------------------------------------------------

/// Row 1: a first build records the file it wrote, settled, and readers
/// exclude it.
#[tokio::test]
async fn first_build_records_and_excludes() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;

    assert_eq!(read(&ws, "p/gen.txt").as_deref(), Some("v1"));
    let r = must_row(&ws.engine, "p/gen.txt")?;
    assert_recorded(&r, "//p:gen", "v1");
    assert!(
        r.output.is_some(),
        "the producing output is recorded: {r:?}"
    );
    assert!(!r.seen_in_section, "no section line covers it");
    let (kind, reason) = status(&ws.engine, "p/gen.txt")?;
    assert_eq!(kind, StatusKind::Generated);
    assert_eq!(reason, Some(StatusReason::Recorded));
    assert!(kind.is_excluded(), "readers leave a recorded file out");
    Ok(())
}

/// Row 3: an output added to the target gets its own row on the next build;
/// the existing one is left as it was.
#[tokio::test]
async fn added_output_recorded() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "a.txt", "a"));
    run_on(&ws.engine, "//p:gen").await?;
    let a_before = must_row(&ws.engine, "p/a.txt")?;

    ws.write_build_file(
        "p",
        r#"
target(
    name = "gen",
    driver = "bash",
    run = "printf a > a.txt; printf b > b.txt",
    out = ["a.txt", "b.txt"],
    codegen = "copy",
)
"#,
    );
    let e2 = ws.reopen()?;
    run_on(&e2, "//p:gen").await?;

    assert_recorded(&must_row(&e2, "p/b.txt")?, "//p:gen", "b");
    let a_after = must_row(&e2, "p/a.txt")?;
    assert_recorded(&a_after, "//p:gen", "a");
    assert_eq!(
        a_after.generation, a_before.generation,
        "an unchanged output's row is not rewritten"
    );
    assert_eq!(status(&e2, "p/b.txt")?.0, StatusKind::Generated);
    Ok(())
}

// ---------------------------------------------------------------------------
// Row 2: a rebuild that changes nothing writes nothing
// ---------------------------------------------------------------------------

/// A rebuilt target (new hashin) whose bytes are unchanged commits nothing
/// and does not touch the file.
#[tokio::test]
async fn rebuild_with_same_bytes_writes_nothing() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "same"));
    run_on(&ws.engine, "//p:gen").await?;
    let generation_before = generation(&ws.engine)?;
    let row_before = must_row(&ws.engine, "p/gen.txt")?;
    let mtime_before = std::fs::metadata(ws.dir.path().join("p/gen.txt"))?.modified()?;

    // Same bytes, different def: the target runs again.
    ws.write_build_file(
        "p",
        r#"
target(
    name = "gen",
    driver = "bash",
    env = {"BUMP": "1"},
    run = "printf '%s' 'same' > $OUT",
    out = "gen.txt",
    codegen = "copy",
)
"#,
    );
    let e2 = ws.reopen()?;
    run_on(&e2, "//p:gen").await?;

    assert_eq!(generation(&e2)?, generation_before, "no commit");
    assert_eq!(
        must_row(&e2, "p/gen.txt")?,
        row_before,
        "the row is untouched"
    );
    assert_eq!(
        std::fs::metadata(ws.dir.path().join("p/gen.txt"))?.modified()?,
        mtime_before,
        "the file is untouched"
    );
    Ok(())
}

/// A warm rerun takes no lock, makes no commit and creates no lock file.
#[tokio::test]
async fn warm_rerun_makes_zero_index_writes() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    let generation_before = generation(&ws.engine)?;
    let lock_dir = CodegenLockPaths::new(&home(&ws.engine)).dir().to_path_buf();
    assert!(lock_dir.exists(), "the cold run took the target lock");
    std::fs::remove_dir_all(&lock_dir)?;

    let e2 = ws.reopen()?;
    run_on(&e2, "//p:gen").await?;
    run_on(&ws.engine, "//p:gen").await?;

    assert_eq!(generation(&e2)?, generation_before, "no commit");
    assert!(
        !lock_dir.exists(),
        "a warm run takes no codegen lock, so it creates no lock file"
    );
    Ok(())
}

/// A `dropped` row never needs work again: the rerun after the drop
/// commits nothing.
#[tokio::test]
async fn warm_rerun_with_dropped_rows_writes_nothing() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"
target(
    name = "gen",
    driver = "bash",
    run = "printf a > a.txt; printf b > b.txt",
    out = ["a.txt", "b.txt"],
    codegen = "copy",
)
"#,
    );
    run_on(&ws.engine, "//p:gen").await?;

    // Output `b.txt` removed: its row is dropped and the file kept (row 5).
    ws.write_build_file("p", &copy_target("gen", "a.txt", "a"));
    let e2 = ws.reopen()?;
    run_on(&e2, "//p:gen").await?;
    assert_eq!(must_row(&e2, "p/b.txt")?.state, RowState::Dropped);
    assert_eq!(read(&ws, "p/b.txt").as_deref(), Some("b"), "file kept");
    let generation_after_drop = generation(&e2)?;

    let e3 = ws.reopen()?;
    run_on(&e3, "//p:gen").await?;
    assert_eq!(
        generation(&e3)?,
        generation_after_drop,
        "a dropped row needs no work"
    );
    Ok(())
}

/// A pending row (a crashed write-back) is settled by the next run, even a
/// warm one.
#[tokio::test]
async fn warm_rerun_with_pending_row_settles_it() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;

    // Another process crashed between steps 2 and 4.
    let s = store(&ws.engine)?.context("db")?;
    s.write(|txn| {
        let mut r = txn.row(b"p/gen.txt")?.context("row")?;
        r.pending_hash = Some(hashout("partial"));
        txn.put(&r)
    })?;
    let crashed_at = s.generation()?;
    assert_eq!(
        status(&ws.engine, "p/gen.txt")?.1,
        Some(StatusReason::Pending)
    );

    let e2 = ws.reopen()?;
    run_on(&e2, "//p:gen").await?;
    assert_recorded(&must_row(&e2, "p/gen.txt")?, "//p:gen", "v1");
    assert!(generation(&e2)? > crashed_at, "the settle committed");
    assert_eq!(read(&ws, "p/gen.txt").as_deref(), Some("v1"));
    Ok(())
}

/// Step 3 fails for real when the tree cannot take the bytes: `break_tree`
/// puts something in the way of `p/<out>`, `fix_tree` removes it. The
/// failure reaches the caller, the target's codegen lock is released (the
/// next run takes it and fails the same way, rather than hanging), the
/// row step 2 marked pending keeps the path excluded, and the first run
/// after the tree is fixed writes and settles it.
async fn byte_move_failure(
    out: &str,
    break_tree: impl Fn(&Path),
    fix_tree: impl Fn(&Path),
) -> anyhow::Result<()> {
    let ws = Workspace::new();
    let rel = format!("p/{out}");
    ws.write_build_file("p", &copy_target("gen", out, "v1"));
    let pkg = ws.dir.path().join("p");
    break_tree(&pkg);

    let err = run_on(&ws.engine, "//p:gen")
        .await
        .expect_err("the byte move cannot succeed");
    let msg = format!("{err:#}");
    assert!(msg.contains(out), "the error names the path: {msg}");

    let r = must_row(&ws.engine, &rel)?;
    assert_eq!(
        r.pending_hash,
        Some(hashout("v1")),
        "step 2 committed: {r:?}"
    );
    assert!(r.record.is_none(), "nothing was recorded: {r:?}");
    let (kind, reason) = status(&ws.engine, &rel)?;
    assert_eq!(reason, Some(StatusReason::Pending));
    assert!(kind.is_excluded(), "a pending path stays out of readers");

    // The lock went with the failed job: a second run takes it.
    let again = tokio::time::timeout(Duration::from_secs(60), run_on(&ws.engine, "//p:gen"))
        .await
        .context("the second run waited on the failed run's codegen lock")?;
    assert!(again.is_err(), "the tree is still broken");
    let e2 = ws.reopen()?;
    let again = tokio::time::timeout(Duration::from_secs(60), run_on(&e2, "//p:gen"))
        .await
        .context("a new engine waited on the failed run's codegen lock")?;
    assert!(again.is_err(), "the tree is still broken");

    fix_tree(&pkg);
    let e3 = ws.reopen()?;
    run_on(&e3, "//p:gen").await?;
    assert_recorded(&must_row(&e3, &rel)?, "//p:gen", "v1");
    assert_eq!(read(&ws, &rel).as_deref(), Some("v1"));
    assert_eq!(
        status(&e3, &rel)?,
        (StatusKind::Generated, Some(StatusReason::Recorded))
    );
    Ok(())
}

/// The destination is a directory: the write itself fails.
#[tokio::test]
async fn byte_move_failure_destination_is_a_directory() -> anyhow::Result<()> {
    byte_move_failure(
        "gen.txt",
        |pkg| std::fs::create_dir_all(pkg.join("gen.txt/inner")).unwrap(),
        |pkg| std::fs::remove_dir_all(pkg.join("gen.txt")).unwrap(),
    )
    .await
}

/// A parent of the destination is a file: creating the parent fails.
#[tokio::test]
async fn byte_move_failure_parent_is_a_file() -> anyhow::Result<()> {
    byte_move_failure(
        "sub/gen.txt",
        |pkg| std::fs::write(pkg.join("sub"), "a file").unwrap(),
        |pkg| std::fs::remove_file(pkg.join("sub")).unwrap(),
    )
    .await
}

// ---------------------------------------------------------------------------
// Row 4: a glob output that produces fewer files
// ---------------------------------------------------------------------------

/// Builds `GLOB_GEN` with `words`, then again with `next` on a new engine.
async fn glob_gen_twice(
    ws: &Workspace,
    words: &str,
    between: impl FnOnce(&Workspace) -> anyhow::Result<()>,
    next: &str,
) -> anyhow::Result<Arc<Engine>> {
    gen_twice(ws, GLOB_GEN, words, between, next).await
}

/// Builds `build` (a `//p:gen` reading `p/n.txt`) with `words`, then again
/// with `next` on a new engine.
async fn gen_twice(
    ws: &Workspace,
    build: &str,
    words: &str,
    between: impl FnOnce(&Workspace) -> anyhow::Result<()>,
    next: &str,
) -> anyhow::Result<Arc<Engine>> {
    ws.write_build_file("p", build);
    ws.write_file("p/n.txt", words);
    run_on(&ws.engine, "//p:gen").await?;
    between(ws)?;
    ws.write_file("p/n.txt", next);
    let e2 = ws.reopen()?;
    run_on(&e2, "//p:gen").await?;
    Ok(e2)
}

/// A file heph wrote, unchanged, inside a current output and no longer
/// produced, is deleted with its row.
#[tokio::test]
async fn leftover_in_live_output_is_deleted() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let e2 = glob_gen_twice(&ws, "a b", |_| Ok(()), "a").await?;

    assert_eq!(read(&ws, "p/gen/b.txt"), None, "the leftover is deleted");
    assert_eq!(row(&e2, "p/gen/b.txt")?, None, "and its row");
    assert_recorded(&must_row(&e2, "p/gen/a.txt")?, "//p:gen", "file a");
    Ok(())
}

/// A leftover the user edited is kept; its row is dropped and, under a
/// section line, it reads as `Modified`.
#[tokio::test]
async fn edited_leftover_is_kept_and_modified() -> anyhow::Result<()> {
    let ws = Workspace::new();
    write_section(&ws, &["/p/gen/"]);
    let e2 = glob_gen_twice(
        &ws,
        "a b",
        |ws| {
            ws.write_file("p/gen/b.txt", "hand edited");
            Ok(())
        },
        "a",
    )
    .await?;

    assert_eq!(read(&ws, "p/gen/b.txt").as_deref(), Some("hand edited"));
    let r = must_row(&e2, "p/gen/b.txt")?;
    assert_eq!(r.state, RowState::Dropped, "{r:?}");
    assert_eq!(
        status(&e2, "p/gen/b.txt")?,
        (StatusKind::Modified, Some(StatusReason::Edited))
    );
    Ok(())
}

/// The delete decision hashes the file fresh: bytes that changed behind an
/// unchanged stat (same size, inode and mtime) are kept.
#[tokio::test]
async fn leftover_deleted_only_after_fresh_hash() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let e2 = glob_gen_twice(
        &ws,
        "a b",
        |ws| {
            let path = ws.dir.path().join("p/gen/b.txt");
            let before = std::fs::metadata(&path)?;
            // Same length as "file b", same inode, mtime put back.
            std::fs::write(&path, "FILE B")?;
            std::fs::File::options()
                .write(true)
                .open(&path)?
                .set_modified(before.modified()?)?;
            let after = std::fs::metadata(&path)?;
            anyhow::ensure!(
                (after.len(), after.ino(), after.mtime(), after.mtime_nsec())
                    == (
                        before.len(),
                        before.ino(),
                        before.mtime(),
                        before.mtime_nsec()
                    ),
                "the stat must be unchanged for this test to mean anything"
            );
            Ok(())
        },
        "a",
    )
    .await?;

    assert_eq!(
        read(&ws, "p/gen/b.txt").as_deref(),
        Some("FILE B"),
        "bytes the record does not hold are never deleted"
    );
    assert_eq!(must_row(&e2, "p/gen/b.txt")?.state, RowState::Dropped);
    Ok(())
}

/// An `imported` leftover is never deleted on its record (J5): the row is
/// dropped and the file kept.
#[tokio::test]
async fn imported_leftover_is_dropped_not_deleted() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let e2 = glob_gen_twice(
        &ws,
        "a b",
        |ws| {
            // The row came from the import, not a write-back.
            let s = store(&ws.engine)?.context("db")?;
            s.write(|txn| {
                let mut r = txn.row(b"p/gen/b.txt")?.context("row")?;
                r.state = RowState::Imported;
                r.output = None;
                txn.put(&r)
            })?;
            Ok(())
        },
        "a",
    )
    .await?;

    assert_eq!(read(&ws, "p/gen/b.txt").as_deref(), Some("file b"), "kept");
    assert_eq!(must_row(&e2, "p/gen/b.txt")?.state, RowState::Dropped);
    Ok(())
}

/// A `copy` target shaped like a `node_modules` install: one package
/// directory per word of `n.txt`, each with a nested file, under the
/// directory output `node_modules/`.
const DIR_GEN: &str = r#"
target(
    name = "gen",
    driver = "bash",
    deps = [file("n.txt")],
    run = "mkdir -p node_modules; for w in $(cat n.txt); do mkdir -p node_modules/$w/lib; printf '{}' > node_modules/$w/package.json; printf 'm %s' \"$w\" > node_modules/$w/lib/index.js; done",
    out = "node_modules/",
    codegen = "copy",
)
"#;

/// `GLOB_GEN` one directory deeper: `gen/<word>/x.txt` under `gen/**/*.txt`.
const DEEP_GLOB_GEN: &str = r#"
target(
    name = "gen",
    driver = "bash",
    deps = [file("n.txt")],
    run = "for w in $(cat n.txt); do mkdir -p gen/$w; printf 'file %s' \"$w\" > gen/$w/x.txt; done",
    out = "gen/**/*.txt",
    codegen = "copy",
)
"#;

fn exists(ws: &Workspace, rel: &str) -> bool {
    ws.dir.path().join(rel).symlink_metadata().is_ok()
}

/// A package dropped from the install goes whole: its files, then the
/// directories their deletion emptied, as `rsync --delete` did. The
/// packages still produced are untouched.
#[tokio::test]
async fn dropped_package_dir_is_removed() -> anyhow::Result<()> {
    let ws = Workspace::new();
    gen_twice(&ws, DIR_GEN, "a b", |_| Ok(()), "a").await?;

    assert!(
        !exists(&ws, "p/node_modules/b"),
        "the emptied package dir goes"
    );
    assert_eq!(
        read(&ws, "p/node_modules/a/lib/index.js").as_deref(),
        Some("m a")
    );
    Ok(())
}

/// Only a directory the deletion emptied goes: one still holding a file heph
/// never wrote stays (with it), and an empty directory nothing was deleted
/// from (a tool's cache) is never touched.
#[tokio::test]
async fn emptied_dir_holding_a_foreign_file_is_kept() -> anyhow::Result<()> {
    let ws = Workspace::new();
    gen_twice(
        &ws,
        DIR_GEN,
        "a b",
        |ws| {
            ws.write_file("p/node_modules/b/.marker", "mine");
            std::fs::create_dir_all(ws.dir.path().join("p/node_modules/.vite/deps"))?;
            Ok(())
        },
        "a",
    )
    .await?;

    assert_eq!(
        read(&ws, "p/node_modules/b/.marker").as_deref(),
        Some("mine")
    );
    assert!(
        !exists(&ws, "p/node_modules/b/lib"),
        "its emptied subdir goes"
    );
    assert!(!exists(&ws, "p/node_modules/b/package.json"));
    assert!(exists(&ws, "p/node_modules/.vite/deps"), "untouched");
    Ok(())
}

/// The output's own directory is never removed, even when nothing is left
/// in it.
#[tokio::test]
async fn emptied_output_root_is_kept() -> anyhow::Result<()> {
    let ws = Workspace::new();
    gen_twice(&ws, DIR_GEN, "a", |_| Ok(()), "").await?;

    assert!(!exists(&ws, "p/node_modules/a"));
    assert!(
        ws.dir.path().join("p/node_modules").is_dir(),
        "the output root stays"
    );
    Ok(())
}

/// A glob output prunes below its literal prefix (`p/gen`), never the
/// prefix itself.
#[tokio::test]
async fn glob_output_prunes_below_its_prefix_only() -> anyhow::Result<()> {
    let ws = Workspace::new();
    gen_twice(&ws, DEEP_GLOB_GEN, "a b", |_| Ok(()), "a").await?;
    assert!(
        !exists(&ws, "p/gen/b"),
        "the emptied dir below the prefix goes"
    );
    assert_eq!(read(&ws, "p/gen/a/x.txt").as_deref(), Some("file a"));

    let ws = Workspace::new();
    gen_twice(&ws, DEEP_GLOB_GEN, "a", |_| Ok(()), "").await?;
    assert!(!exists(&ws, "p/gen/a"));
    assert!(ws.dir.path().join("p/gen").is_dir(), "the prefix stays");
    Ok(())
}

// ---------------------------------------------------------------------------
// Row 8, 9: renamed targets and the in-place guard
// ---------------------------------------------------------------------------

/// Row 8: a renamed target writing the same path takes the row over.
#[tokio::test]
async fn renamed_target_takes_over_rows() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("old", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:old").await?;
    assert_eq!(must_row(&ws.engine, "p/gen.txt")?.addr, "//p:old");

    ws.write_build_file("p", &copy_target("new", "gen.txt", "v1"));
    let e2 = ws.reopen()?;
    run_on(&e2, "//p:new").await?;
    assert_recorded(&must_row(&e2, "p/gen.txt")?, "//p:new", "v1");
    assert_eq!(read(&ws, "p/gen.txt").as_deref(), Some("v1"));
    assert_eq!(status(&e2, "p/gen.txt")?.0, StatusKind::Generated);
    Ok(())
}

/// An `in_place` target `name` that rewrites `out` with `body`, reading
/// nothing.
fn in_place_target(name: &str, out: &str, body: &str) -> String {
    format!(
        r#"
target(
    name = "{name}",
    driver = "bash",
    run = "printf '%s' '{body}' > $OUT",
    out = "{out}",
    codegen = "in_place",
)
"#
    )
}

/// Row 9: a target switched from `copy` to `in_place` refuses to rewrite a
/// path recorded as its own `copy` output, and names `codegen keep`.
#[tokio::test]
async fn in_place_guard_refuses_own_recorded_path() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("t", "src.txt", "generated"));
    run_on(&ws.engine, "//p:t").await?;

    ws.write_build_file("p", &in_place_target("t", "src.txt", "FORMATTED"));
    let e2 = ws.reopen()?;
    let msg = match run_on(&e2, "//p:t").await {
        Err(e) => format!("{e:#}"),
        Ok(()) => panic!("an in_place rewrite of its own recorded copy output must be refused"),
    };
    assert!(
        msg.contains("codegen keep") && msg.contains("p/src.txt"),
        "the refusal names the path and the way out: {msg}"
    );
    assert_eq!(read(&ws, "p/src.txt").as_deref(), Some("generated"));
    Ok(())
}

/// An `in_place` target leaves a path recorded for another target to its
/// owner: skipped, not refused, not written.
#[tokio::test]
async fn in_place_guard_skips_other_targets_path() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        &format!(
            "{}{}",
            copy_target("gen", "gen.txt", "generated"),
            in_place_target("fmt", "gen.txt", "FORMATTED")
        ),
    );
    run_on(&ws.engine, "//p:gen").await?;
    run_on(&ws.engine, "//p:fmt").await?;
    assert_eq!(
        read(&ws, "p/gen.txt").as_deref(),
        Some("generated"),
        "another target's recorded file is its owner's"
    );
    assert_recorded(&must_row(&ws.engine, "p/gen.txt")?, "//p:gen", "generated");
    Ok(())
}

/// The frozen check, symmetric with the guard, does not flag a copy-owned
/// file an `in_place` target would have skipped.
#[tokio::test]
async fn frozen_check_skips_copy_owned_in_place() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        &format!(
            "{}{}",
            copy_target("gen", "gen.txt", "generated"),
            in_place_target("fmt", "gen.txt", "FORMATTED")
        ),
    );
    run_on(&ws.engine, "//p:gen").await?;
    run_with(
        &ws.engine,
        "//p:fmt",
        &ResultOptions {
            frozen: true,
            ..ResultOptions::default()
        },
    )
    .await
    .context("a copy-owned file is not the in_place target's drift")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Rows 12, 23, 25
// ---------------------------------------------------------------------------

/// Row 12: the next write-back overwrites a hand edit and says so.
#[tokio::test]
async fn write_back_reports_overwritten_hand_edits() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    ws.write_file("p/gen.txt", "hand edit");

    let e2 = ws.reopen()?;
    let (res, log) = capture_warnings(run_on(&e2, "//p:gen")).await;
    res?;
    assert_eq!(read(&ws, "p/gen.txt").as_deref(), Some("v1"));
    assert_recorded(&must_row(&e2, "p/gen.txt")?, "//p:gen", "v1");
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("overwrote hand edits"))
        .collect();
    assert_eq!(lines.len(), 1, "one warning, naming the file: {log}");
    assert!(
        lines.iter().all(|l| l.contains("p/gen.txt")),
        "names the file: {log}"
    );
    Ok(())
}

/// Row 23: a `copy` output over a file that was already there (tracked
/// source) overwrites it and records it.
#[tokio::test]
async fn copy_over_tracked_file_recorded() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("p/gen.txt", "tracked source");
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    assert_eq!(read(&ws, "p/gen.txt").as_deref(), Some("v1"));
    assert_recorded(&must_row(&ws.engine, "p/gen.txt")?, "//p:gen", "v1");
    assert_eq!(status(&ws.engine, "p/gen.txt")?.0, StatusKind::Generated);
    Ok(())
}

/// Row 25: the write-back still sets the provenance xattr, for N-1.
#[tokio::test]
async fn write_back_still_sets_xattr() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let probe = ws.dir.path().join("xattr-probe");
    std::fs::write(&probe, "")?;
    if let Err(e) = xattr::set(&probe, "user.heph.probe", b"1") {
        assert!(
            !common::xattrs_required(),
            "no xattr support here ({e}), and CI or HEPH_REQUIRE_XATTRS requires it"
        );
        eprintln!("skipping write_back_still_sets_xattr: no xattr support here ({e})");
        return Ok(());
    }

    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    let got = xattr::get(
        ws.dir.path().join("p/gen.txt"),
        heph::pluginfs::CODEGEN_XATTR,
    )?;
    assert_eq!(got.as_deref(), Some("//p:gen".as_bytes()));
    Ok(())
}

// ---------------------------------------------------------------------------
// Row 22: the db's creation
// ---------------------------------------------------------------------------

/// A checkout that builds nothing `copy` has no db and no codegen dir; the
/// first write-back creates the db.
#[tokio::test]
async fn fresh_checkout_has_no_db_until_first_write_back() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        &format!(
            r#"
target(name = "plain", driver = "bash", run = "printf x > $OUT", out = "x.txt")
{}"#,
            copy_target("gen", "gen.txt", "v1")
        ),
    );
    run_on(&ws.engine, "//p:plain").await?;
    let dir = codegen_dir(&home(&ws.engine));
    assert!(!dir.exists(), "nothing creates {}", dir.display());
    assert!(!db_file(&ws.engine).exists());

    run_on(&ws.engine, "//p:gen").await?;
    assert!(
        db_file(&ws.engine).exists(),
        "the first write-back creates it"
    );
    assert!(row(&ws.engine, "p/gen.txt")?.is_some());
    Ok(())
}

/// Polls `fut` for `dur`, returning it unfinished, or its output.
async fn pending_for<F: std::future::Future + Unpin>(
    fut: &mut F,
    dur: Duration,
) -> Option<F::Output> {
    tokio::time::timeout(dur, fut).await.ok()
}

/// The first write-back creates the db under the import lock: while another
/// process holds it, no db appears and the run waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_write_back_creates_db_under_import_lock() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    let import = CodegenLockPaths::new(&home(&ws.engine)).import();
    let held = hold_flock(&import)?;

    let engine = Arc::clone(&ws.engine);
    let mut run = Box::pin(tokio::spawn(
        async move { run_on(&engine, "//p:gen").await },
    ));
    assert!(
        pending_for(&mut run, Duration::from_millis(1500))
            .await
            .is_none(),
        "the write-back waits on the import lock"
    );
    assert!(
        !db_file(&ws.engine).exists(),
        "no db while the lock is held"
    );
    assert_eq!(read(&ws, "p/gen.txt"), None, "no byte moved");

    drop(held);
    tokio::time::timeout(Duration::from_secs(60), run).await???;
    assert!(db_file(&ws.engine).exists());
    assert_recorded(&must_row(&ws.engine, "p/gen.txt")?, "//p:gen", "v1");
    Ok(())
}

/// Two first commits racing on a checkout with no db: both find none, one
/// creates it under the import lock, the other commits into it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_back_and_import_create_db_once() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        &format!(
            "{}{}",
            copy_target("a", "a.txt", "a"),
            copy_target("b", "b.txt", "b")
        ),
    );
    let (e1, e2) = (ws.reopen()?, ws.reopen()?);
    let held = hold_flock(&CodegenLockPaths::new(&home(&e1)).import())?;
    let r1 = tokio::spawn(async move { run_on(&e1, "//p:a").await });
    let r2 = tokio::spawn(async move { run_on(&e2, "//p:b").await });
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(!db_file(&ws.engine).exists());
    drop(held);
    tokio::time::timeout(Duration::from_secs(60), r1).await???;
    tokio::time::timeout(Duration::from_secs(60), r2).await???;

    assert_recorded(&must_row(&ws.engine, "p/a.txt")?, "//p:a", "a");
    assert_recorded(&must_row(&ws.engine, "p/b.txt")?, "//p:b", "b");
    let dbs: Vec<_> = std::fs::read_dir(codegen_dir(&home(&ws.engine)))?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".db"))
        .collect();
    assert_eq!(dbs.len(), 1, "one db file: {dbs:?}");
    Ok(())
}

/// `clean` and `gc` prune caches; the index lives beside `cache/` and
/// survives both.
#[tokio::test]
async fn index_survives_clean_and_gc() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    let before = must_row(&ws.engine, "p/gen.txt")?;

    let e2 = ws.reopen()?;
    Arc::clone(&e2)
        .clean(
            e2.new_state(),
            &Matcher::Package(PkgBuf::from("p")),
            heph::engine::Discovery::Complete,
        )
        .await?;
    Arc::clone(&e2).gc_all(e2.new_state()).await?;

    assert!(db_file(&e2).exists());
    assert_eq!(must_row(&e2, "p/gen.txt")?, before);
    Ok(())
}

/// A home outside the root, shared by two checkouts: each checkout keys its
/// own db file by its root.
#[tokio::test]
async fn shared_absolute_home_keys_db_by_root() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let (a, b) = (tempfile::tempdir()?, tempfile::tempdir()?);
    for root in [a.path(), b.path()] {
        std::fs::create_dir_all(root.join("p"))?;
        std::fs::write(root.join("p/BUILD"), copy_target("gen", "gen.txt", "v1"))?;
    }
    let ea = engine_with_home(a.path(), home.path())?;
    let eb = engine_with_home(b.path(), home.path())?;
    run_on(&ea, "//p:gen").await?;
    run_on(&eb, "//p:gen").await?;

    let (da, dbb) = (db_file(&ea), db_file(&eb));
    assert_ne!(da, dbb, "one file per root");
    assert_eq!(da, db_path(a.path(), home.path()));
    assert!(da.starts_with(codegen_dir(home.path())) && dbb.starts_with(codegen_dir(home.path())));
    assert!(da.exists() && dbb.exists());
    assert_eq!(
        Store::open(&da, &StoreOptions::default())?
            .load_all()?
            .1
            .len(),
        1,
        "A's db holds A's row only"
    );
    assert_eq!(
        Store::open(&dbb, &StoreOptions::default())?
            .load_all()?
            .1
            .len(),
        1,
        "B's db holds B's row only"
    );
    Ok(())
}

/// Checkout B creating its db in a shared `codegen/` that already holds A's
/// renames onto B's own file: A's file and the directory are untouched.
#[tokio::test]
async fn shared_home_rename_never_replaces_codegen_dir() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let (a, b) = (tempfile::tempdir()?, tempfile::tempdir()?);
    for root in [a.path(), b.path()] {
        std::fs::create_dir_all(root.join("p"))?;
        std::fs::write(root.join("p/BUILD"), copy_target("gen", "gen.txt", "v1"))?;
    }
    let ea = engine_with_home(a.path(), home.path())?;
    run_on(&ea, "//p:gen").await?;
    let dir = codegen_dir(home.path());
    let dir_ino = std::fs::metadata(&dir)?.ino();
    let a_ino = std::fs::metadata(db_file(&ea))?.ino();
    let a_rows = Store::open(&db_file(&ea), &StoreOptions::default())?.load_all()?;

    let eb = engine_with_home(b.path(), home.path())?;
    run_on(&eb, "//p:gen").await?;

    assert_eq!(
        std::fs::metadata(&dir)?.ino(),
        dir_ino,
        "the directory stays"
    );
    assert_eq!(
        std::fs::metadata(db_file(&ea))?.ino(),
        a_ino,
        "A's file stays"
    );
    assert_eq!(
        Store::open(&db_file(&ea), &StoreOptions::default())?.load_all()?,
        a_rows,
        "A's rows are untouched"
    );
    assert!(db_file(&eb).exists());
    Ok(())
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

/// One path, two targets: B taking the row over means A's next write-back
/// (which no longer produces the path) neither deletes B's file nor its row.
#[tokio::test]
async fn two_targets_same_output_path() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        &format!(
            r#"{GLOB_GEN}
target(
    name = "b",
    driver = "bash",
    run = "mkdir -p gen; printf B > gen/b.txt",
    out = "gen/b.txt",
    codegen = "copy",
)
"#
        ),
    );
    ws.write_file("p/n.txt", "a b");
    run_on(&ws.engine, "//p:gen").await?;
    run_on(&ws.engine, "//p:b").await?;
    assert_recorded(&must_row(&ws.engine, "p/gen/b.txt")?, "//p:b", "B");

    ws.write_file("p/n.txt", "a");
    let e2 = ws.reopen()?;
    run_on(&e2, "//p:gen").await?;
    assert_eq!(read(&ws, "p/gen/b.txt").as_deref(), Some("B"));
    assert_recorded(&must_row(&e2, "p/gen/b.txt")?, "//p:b", "B");
    Ok(())
}

/// A write-back of one target waits for another process's write-back of the
/// same target: nothing is marked pending or written while the lock is held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_write_backs_of_one_target_are_serialised() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        &format!(
            "{}{}",
            copy_target("other", "other.txt", "o"),
            copy_target("gen", "gen.txt", "v1")
        ),
    );
    // The db exists, so the write-backs below take only the target lock.
    run_on(&ws.engine, "//p:other").await?;
    let generation_before = generation(&ws.engine)?;

    let held = hold_flock(&CodegenLockPaths::new(&home(&ws.engine)).target("//p:gen"))?;
    let (e1, e2) = (ws.reopen()?, ws.reopen()?);
    let r1 = tokio::spawn(async move { run_on(&e1, "//p:gen").await });
    let r2 = tokio::spawn(async move { run_on(&e2, "//p:gen").await });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !r1.is_finished() && !r2.is_finished(),
        "both wait on the lock"
    );
    assert_eq!(generation(&ws.engine)?, generation_before, "nothing marked");
    assert_eq!(read(&ws, "p/gen.txt"), None, "nothing written");

    drop(held);
    tokio::time::timeout(Duration::from_secs(60), r1).await???;
    tokio::time::timeout(Duration::from_secs(60), r2).await???;
    assert_recorded(&must_row(&ws.engine, "p/gen.txt")?, "//p:gen", "v1");
    assert_eq!(read(&ws, "p/gen.txt").as_deref(), Some("v1"));
    Ok(())
}

/// Another process's write-back of `addr`, landed: `files` written and
/// recorded, settled, as its step 4 leaves them. The caller holds the
/// target's lock.
fn other_process_write_back(
    ws: &Workspace,
    engine: &Engine,
    addr: &str,
    files: &[(&str, &str)],
) -> anyhow::Result<()> {
    let s = store(engine)?.context("db")?;
    let now_ns = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos(),
    )?;
    for (rel, bytes) in files {
        ws.write_file(rel, bytes);
    }
    s.write(|txn| {
        for (rel, bytes) in files {
            let md = std::fs::symlink_metadata(ws.dir.path().join(rel))?;
            let mut r = txn.row(rel.as_bytes())?.unwrap_or_else(|| {
                Row::new(
                    rel.as_bytes(),
                    addr,
                    heph::htwalk::codegen_index::RowKind::File,
                )
            });
            r.record = Some(heph::htwalk::codegen_index::Record {
                hash: hashout(bytes),
                size: md.size(),
                mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
                ino: md.ino(),
                dev: md.dev(),
                mode: md.mode(),
                written_at_ns: now_ns,
            });
            r.pending_hash = None;
            r.state = RowState::Written;
            txn.put(&r)?;
        }
        Ok(())
    })?;
    Ok(())
}

/// Write protocol step 2 reselects under the lock. P2 compares against a
/// tree holding its own `a.txt` (skip) and no `b.txt` (write), then waits
/// on the lock while P1 lands a whole other revision. Acting on its stale
/// selection would write only `b.txt` and leave P1's `a.txt`: a mix of two
/// revisions. Reselecting, it writes both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interleaved_write_backs_write_one_complete_revision() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"
target(
    name = "gen",
    driver = "bash",
    run = "printf 'r2 a' > a.txt; printf 'r2 b' > b.txt",
    out = ["a.txt", "b.txt"],
    codegen = "copy",
)
"#,
    );
    run_on(&ws.engine, "//p:gen").await?;
    std::fs::remove_file(ws.dir.path().join("p/b.txt"))?;

    let held = hold_flock(&CodegenLockPaths::new(&home(&ws.engine)).target("//p:gen"))?;
    let e2 = ws.reopen()?;
    let p2 = tokio::spawn(async move { run_on(&e2, "//p:gen").await });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!p2.is_finished(), "P2 waits on the lock");
    other_process_write_back(
        &ws,
        &ws.engine,
        "//p:gen",
        &[("p/a.txt", "r1 a"), ("p/b.txt", "r1 b")],
    )?;
    drop(held);
    tokio::time::timeout(Duration::from_secs(60), p2).await???;

    assert_eq!(
        read(&ws, "p/a.txt").as_deref(),
        Some("r2 a"),
        "one revision"
    );
    assert_eq!(
        read(&ws, "p/b.txt").as_deref(),
        Some("r2 b"),
        "one revision"
    );
    assert_recorded(&must_row(&ws.engine, "p/a.txt")?, "//p:gen", "r2 a");
    assert_recorded(&must_row(&ws.engine, "p/b.txt")?, "//p:gen", "r2 b");
    Ok(())
}

/// Two real processes, each with its own revision of the target, both
/// planned against an empty tree: whichever goes second rewrites what the
/// first landed, so the tree ends as one complete revision.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interleaved_write_backs_of_two_revisions_end_complete() -> anyhow::Result<()> {
    let rev = |tag: &str| {
        format!(
            r#"
target(name = "other", driver = "bash", run = "printf o > $OUT", out = "other.txt", codegen = "copy")
target(
    name = "gen",
    driver = "bash",
    run = "printf '{tag} a' > a.txt; printf '{tag} b' > b.txt",
    out = ["a.txt", "b.txt"],
    codegen = "copy",
)
"#
        )
    };
    let ws = Workspace::new();
    ws.write_build_file("p", &rev("r1"));
    run_on(&ws.engine, "//p:other").await?;
    let addr = parse_addr("//p:gen")?;

    // Each engine pins its revision of the BUILD file before the other is
    // written.
    let e1 = ws.reopen()?;
    Arc::clone(&e1).get_spec(e1.new_state(), &addr).await?;
    ws.write_build_file("p", &rev("r2"));
    let e2 = ws.reopen()?;
    Arc::clone(&e2).get_spec(e2.new_state(), &addr).await?;

    let held = hold_flock(&CodegenLockPaths::new(&home(&ws.engine)).target("//p:gen"))?;
    let r1 = tokio::spawn(async move { run_on(&e1, "//p:gen").await });
    let r2 = tokio::spawn(async move { run_on(&e2, "//p:gen").await });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    drop(held);
    tokio::time::timeout(Duration::from_secs(60), r1).await???;
    tokio::time::timeout(Duration::from_secs(60), r2).await???;

    let (a, b) = (
        read(&ws, "p/a.txt").context("a.txt")?,
        read(&ws, "p/b.txt").context("b.txt")?,
    );
    let tag = a.split(' ').next().unwrap_or_default().to_string();
    assert_eq!(b, format!("{tag} b"), "one revision: a={a:?} b={b:?}");
    assert_recorded(&must_row(&ws.engine, "p/a.txt")?, "//p:gen", &a);
    assert_recorded(&must_row(&ws.engine, "p/b.txt")?, "//p:gen", &b);
    Ok(())
}

// ---------------------------------------------------------------------------
// Write-back hints and `seen_in_section`
// ---------------------------------------------------------------------------

/// With a section that does not cover a written path, the write-back warns,
/// naming the target and the count.
#[tokio::test]
async fn write_back_warns_path_not_in_section() -> anyhow::Result<()> {
    let ws = Workspace::new();
    write_section(&ws, &["/elsewhere/"]);
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    let (res, log) = capture_warnings(run_on(&ws.engine, "//p:gen")).await;
    res?;
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("not in the heph section"))
        .collect();
    assert_eq!(lines.len(), 1, "{log}");
    assert!(
        lines
            .iter()
            .all(|l| l.contains("1 file written by //p:gen is")),
        "{log}"
    );
    assert!(
        !log.contains("has no heph section"),
        "a present section is not missing: {log}"
    );
    Ok(())
}

/// A workspace with no section prints the hint once, even with two targets
/// writing back in one run, and records it so a later run does not repeat
/// it.
#[tokio::test]
async fn no_section_hint_shown_once() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        &format!(
            "{}{}",
            copy_target("a", "a.txt", "a"),
            copy_target("b", "b.txt", "b")
        ),
    );
    let (res, log) = capture_warnings(run_pkg(&ws.engine, "p")).await;
    res?;
    assert_eq!(
        log.matches("has no heph section").count(),
        1,
        "one hint per run: {log}"
    );
    assert!(store(&ws.engine)?.context("db")?.no_section_hint_shown()?);

    ws.write_build_file(
        "p",
        &format!(
            "{}{}",
            copy_target("a", "a.txt", "a2"),
            copy_target("c", "c.txt", "c")
        ),
    );
    let e2 = ws.reopen()?;
    let (res, log) = capture_warnings(run_pkg(&e2, "p")).await;
    res?;
    assert!(
        !log.contains("has no heph section"),
        "recorded: never shown again: {log}"
    );
    Ok(())
}

/// A warm rerun sets `seen_in_section` once a line covers the path, then
/// writes nothing again.
#[tokio::test]
async fn warm_rerun_sets_seen_when_section_gains_line() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    assert!(!must_row(&ws.engine, "p/gen.txt")?.seen_in_section);

    write_section(&ws, &["/p/gen.txt"]);
    let e2 = ws.reopen()?;
    let before = generation(&e2)?;
    run_on(&e2, "//p:gen").await?;
    let r = must_row(&e2, "p/gen.txt")?;
    assert!(r.seen_in_section, "{r:?}");
    assert_recorded(&r, "//p:gen", "v1");
    let after = generation(&e2)?;
    assert!(after > before, "the flag is a commit");

    let e3 = ws.reopen()?;
    run_on(&e3, "//p:gen").await?;
    assert_eq!(generation(&e3)?, after, "set once, then nothing");
    Ok(())
}

/// A later write-back with no line over the path keeps the flag.
#[tokio::test]
async fn settle_never_clears_seen_flag() -> anyhow::Result<()> {
    let ws = Workspace::new();
    write_section(&ws, &["/p/gen.txt"]);
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    assert!(must_row(&ws.engine, "p/gen.txt")?.seen_in_section);

    write_section(&ws, &["/elsewhere/"]);
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v2"));
    let e2 = ws.reopen()?;
    run_on(&e2, "//p:gen").await?;
    let r = must_row(&e2, "p/gen.txt")?;
    assert_recorded(&r, "//p:gen", "v2");
    assert!(r.seen_in_section, "never cleared: {r:?}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A corrupt db is a loud error naming the file, never an empty index.
#[tokio::test]
async fn corrupt_index_errors_naming_file() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    let db = db_file(&ws.engine);
    std::fs::create_dir_all(db.parent().context("db dir")?)?;
    std::fs::write(
        &db,
        b"this is not a sqlite database, not even close.........",
    )?;

    let msg = match run_on(&ws.engine, "//p:gen").await {
        Err(e) => format!("{e:#}"),
        Ok(()) => panic!("a corrupt index must fail the run"),
    };
    assert!(
        msg.contains(&db.display().to_string()),
        "the error names {}: {msg}",
        db.display()
    );
    assert_eq!(read(&ws, "p/gen.txt"), None, "nothing written");
    Ok(())
}
