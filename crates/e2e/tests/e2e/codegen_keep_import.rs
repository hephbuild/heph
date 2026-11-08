//! `heph tool codegen keep`, `discard` and `import`, end to end through the
//! command functions and a real engine (spec: codegen file index — Commands,
//! Upgrade from the xattr, Lifecycle rows 13, 16, 19, 20, 27 and 28, J5, J8,
//! J9).
//!
//! Each command runs in-process, the way `heph tool codegen <cmd>` runs it,
//! and the assertions read its report, the db (opened fresh, as another
//! process sees it), a snapshot's `status`, the files and `.gitignore`. A new
//! engine over the same root (`Workspace::reopen`) is the next `heph`
//! invocation: it re-reads the BUILD files, so a target removed from one is
//! gone for it. Stamping a file with `user.heph.codegen` stands in for what an
//! older heph wrote; a test that needs it skips, saying so, on a filesystem
//! without xattrs, and fails instead in CI or with `HEPH_REQUIRE_XATTRS` set.
#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common;

use anyhow::Context as _;
use common::Workspace;
use heph::commands::tool::codegen::import::{self as import_cmd, ImportReport};
use heph::commands::tool::codegen::keep::{
    self as keep_cmd, Options as DiscardOptions, PathResult, PathResultKind, PathsReport, Selection,
};
use heph::commands::tool::codegen::{CodegenExit, wrap_exit};
use heph::commands::tool::codegen::{Report as _, gitignore as gitignore_cmd, status, why};
use heph::engine::event::{BuildEvent, BuildEventKind};
use heph::engine::{Config, Engine, HomeDir, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htwalk::codegen_index::import::{CODEGEN_XATTR, RealXattr, XattrProbe};
use heph::htwalk::codegen_index::{Row, RowState, StatusKind, StatusReason, Store, StoreOptions};
use heph::htwalk::gitignore_section::{BEGIN_MARKER, END_MARKER};
use heph::{pluginbuildfile, pluginexec};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A `copy` target `name` writing `body` to `out` in its package.
fn copy_target(name: &str, out: &str, body: &str) -> String {
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
/// `gen/`, declared as the glob `gen/*.txt`.
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

/// Run `addr` on `engine` (all outputs), surfacing the root cause.
async fn run_on(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<()> {
    let parsed = parse_addr(addr)?;
    let rs = engine.new_state();
    let res = Arc::clone(engine)
        .result_addr(
            Arc::clone(&rs),
            &parsed,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
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

fn strings(paths: &[&str]) -> Vec<String> {
    paths.iter().map(ToString::to_string).collect()
}

/// `heph tool codegen keep <paths>` from the workspace root.
async fn keep(engine: &Arc<Engine>, paths: &[&str]) -> anyhow::Result<PathsReport> {
    let rs = engine.new_state();
    let res = keep_cmd::keep(engine, &rs, &strings(paths), engine.root()).await;
    common::drain(rs).await;
    res
}

/// `heph tool codegen discard` from the workspace root.
async fn discard(
    engine: &Arc<Engine>,
    selection: Selection,
    dry_run: bool,
    force: bool,
) -> anyhow::Result<PathsReport> {
    let rs = engine.new_state();
    let opts = DiscardOptions {
        selection,
        cwd: engine.root().to_path_buf(),
        dry_run,
        force,
    };
    let res = keep_cmd::discard(engine, &rs, &opts).await;
    common::drain(rs).await;
    res
}

fn named(paths: &[&str]) -> Selection {
    Selection::Paths(strings(paths))
}

/// `heph tool codegen import`, with the events it emitted.
async fn import_with_events(
    engine: &Arc<Engine>,
) -> (anyhow::Result<ImportReport>, Vec<BuildEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = engine.new_state_with_events(false, Some(tx));
    let res = import_cmd::import(engine, &rs).await;
    common::drain(rs).await;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    (res, events)
}

async fn import(engine: &Arc<Engine>) -> anyhow::Result<ImportReport> {
    import_with_events(engine).await.0
}

/// `heph tool codegen gitignore`: the section from the current defs.
async fn gitignore(engine: &Arc<Engine>) -> anyhow::Result<()> {
    let rs = engine.new_state();
    let res = gitignore_cmd::gitignore(engine, &rs, &gitignore_cmd::Options::default()).await;
    common::drain(rs).await;
    res.map(drop)
}

async fn status_report(engine: &Arc<Engine>) -> anyhow::Result<status::StatusReport> {
    let rs = engine.new_state();
    let opts = status::Options {
        hint: true,
        ..status::Options::default()
    };
    let res = status::status(engine, &rs, opts).await;
    common::drain(rs).await;
    res
}

async fn why_report(engine: &Arc<Engine>, paths: &[&str]) -> anyhow::Result<why::WhyReport> {
    let rs = engine.new_state();
    let res = why::why(engine, &rs, &strings(paths), engine.root()).await;
    common::drain(rs).await;
    res
}

/// The one result for `path`.
fn result_for<'r>(report: &'r PathsReport, path: &str) -> &'r PathResult {
    let found: Vec<&PathResult> = report.results.iter().filter(|r| r.path == path).collect();
    assert_eq!(found.len(), 1, "one result for {path}: {report:?}");
    found.first().copied().expect("asserted above")
}

fn reason(r: &PathResult) -> &str {
    r.reason.as_deref().unwrap_or_default()
}

/// The db as another process sees it: opened fresh. `None` while there is no
/// db.
fn store(engine: &Engine) -> anyhow::Result<Option<Store>> {
    let path = engine.codegen_index().db_path();
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(Store::open(path, &StoreOptions::default())?))
}

fn row(engine: &Engine, rel: &str) -> anyhow::Result<Option<Row>> {
    Ok(match store(engine)? {
        Some(s) => s.row(rel.as_bytes())?,
        None => None,
    })
}

fn must_row(engine: &Engine, rel: &str) -> anyhow::Result<Row> {
    row(engine, rel)?.with_context(|| format!("no index row for {rel}"))
}

fn generation(engine: &Engine) -> anyhow::Result<u64> {
    Ok(match store(engine)? {
        Some(s) => s.generation()?,
        None => 0,
    })
}

/// What a fresh snapshot says `rel` is.
fn status_of(engine: &Engine, rel: &str) -> anyhow::Result<(StatusKind, Option<StatusReason>)> {
    let index = engine.codegen_index();
    index.open_existing()?;
    let snap = index.snapshot()?;
    let st = snap.status_path(&index.root().join(rel), false)?;
    Ok((st.kind, st.reason))
}

/// The content hash a row records for `bytes` as a non-executable file.
fn hashout(bytes: &str) -> String {
    heph::htwalk::bytes_hashout(bytes.as_bytes(), false)
}

fn abs(ws: &Workspace, rel: &str) -> PathBuf {
    ws.dir.path().join(rel)
}

fn read(ws: &Workspace, rel: &str) -> Option<String> {
    std::fs::read_to_string(abs(ws, rel)).ok()
}

fn gitignore_text(ws: &Workspace) -> String {
    read(ws, ".gitignore").unwrap_or_default()
}

/// A root `.gitignore` whose managed section holds `lines`, after one line
/// heph does not own.
fn write_section(ws: &Workspace, lines: &[&str]) {
    let mut text = format!("/user-owned\n{BEGIN_MARKER}\n");
    for l in lines {
        text.push_str(l);
        text.push('\n');
    }
    text.push_str(END_MARKER);
    text.push('\n');
    ws.write_file(".gitignore", &text);
}

/// Whether `.gitignore`'s managed section has a line for `pattern`.
fn section_has(ws: &Workspace, pattern: &str) -> bool {
    gitignore_text(ws).lines().any(|l| l.trim() == pattern)
}

/// Whether `dir`'s filesystem stores user xattrs. Where they are required
/// ([`common::xattrs_required`]) a filesystem without them fails the test.
fn xattrs_supported(dir: &Path) -> bool {
    let probe = dir.join(".xattr-probe");
    let ok = std::fs::write(&probe, b"").is_ok() && xattr::set(&probe, CODEGEN_XATTR, b"x").is_ok();
    let ok = std::fs::remove_file(&probe).is_ok() && ok;
    assert!(
        ok || !common::xattrs_required(),
        "{} stores no user xattrs, and CI or HEPH_REQUIRE_XATTRS requires them",
        dir.display()
    );
    ok
}

/// Skip the test, saying so, where the workspace's filesystem has no xattrs.
macro_rules! require_xattrs {
    ($ws:expr) => {
        if !xattrs_supported($ws.dir.path()) {
            eprintln!("skipping: this filesystem stores no user xattrs");
            return Ok(());
        }
    };
}

/// Stamp `rel` the way an older heph did.
fn stamp(ws: &Workspace, rel: &str, value: &[u8]) -> anyhow::Result<()> {
    xattr::set(abs(ws, rel), CODEGEN_XATTR, value).with_context(|| format!("stamping {rel}"))
}

fn stamp_of(ws: &Workspace, rel: &str) -> anyhow::Result<Option<Vec<u8>>> {
    xattr::get(abs(ws, rel), CODEGEN_XATTR).with_context(|| format!("reading the stamp of {rel}"))
}

/// A file an older heph wrote for `addr`: bytes and stamp.
fn stamped_file(ws: &Workspace, rel: &str, body: &str, addr: &str) -> anyhow::Result<()> {
    ws.write_file(rel, body);
    stamp(ws, rel, addr.as_bytes())
}

/// Remove this checkout's db and its sidecars: a lost index.
fn lose_db(engine: &Engine) -> anyhow::Result<()> {
    let db = engine.codegen_index().db_path().to_path_buf();
    for suffix in ["", "-wal", "-shm"] {
        let mut p = db.clone().into_os_string();
        p.push(suffix);
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {p:?}")),
        }
    }
    Ok(())
}

/// Two removable targets: `//p:a` writing `p/a.txt` and `//q:b` writing
/// `q/b.txt`, both built, the section written from them, then both removed
/// from their BUILD files (`//p:keep` stays so `p` still evaluates). Returns
/// the next invocation's engine, which no longer sees them.
async fn released_pair(ws: &Workspace) -> anyhow::Result<Arc<Engine>> {
    ws.write_build_file(
        "p",
        &(copy_target("a", "a.txt", "aaa") + &copy_target("keep", "keep.txt", "kkk")),
    );
    ws.write_build_file("q", &copy_target("b", "b.txt", "bbb"));
    run_on(&ws.engine, "//p:a").await?;
    run_on(&ws.engine, "//p:keep").await?;
    run_on(&ws.engine, "//q:b").await?;
    gitignore(&ws.engine).await?;
    for p in ["/p/a.txt", "/p/keep.txt", "/q/b.txt"] {
        assert!(section_has(ws, p), "{p} in {}", gitignore_text(ws));
    }
    ws.write_build_file("p", &copy_target("keep", "keep.txt", "kkk"));
    ws.write_build_file("q", "");
    ws.reopen()
}

/// A `MakeWriter` into a shared buffer, for capturing warnings.
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
/// task the command spawns is polled on this thread.
async fn capture_warnings<F: std::future::Future>(fut: F) -> (F::Output, String) {
    use tracing_subscriber::util::SubscriberInitExt as _;
    let log = Captured::default();
    let writer = log.clone();
    let out = {
        // A second live dispatcher: see `resources::clamp_is_logged_once_per_addr_per_request`.
        let _second = tracing_subscriber::registry().set_default();
        let _guard = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
            .set_default();
        fut.await
    };
    (out, log.text())
}

/// An engine over `root` whose home is `home` (absolute, possibly shared by
/// several checkouts): buildfile provider, bash driver.
fn engine_with_home(root: &Path, home: &Path) -> anyhow::Result<Arc<Engine>> {
    Ok(Arc::new(unshared_engine(root, Some(home))?))
}

/// An engine over `root` (default home) whose xattr reads all go through
/// `probe`: the next invocation, with the probe a test stands in.
fn engine_with_probe(root: &Path, probe: Arc<dyn XattrProbe>) -> anyhow::Result<Arc<Engine>> {
    let mut e = unshared_engine(root, None)?;
    e.set_xattr_probe(probe);
    Ok(Arc::new(e))
}

fn unshared_engine(root: &Path, home: Option<&Path>) -> anyhow::Result<Engine> {
    let home = HomeDir::resolve(root, home)?;
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
    Ok(e)
}

/// The real probe, counting every read made through it.
#[derive(Debug, Default)]
struct CountingProbe(AtomicUsize);

impl CountingProbe {
    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl XattrProbe for CountingProbe {
    fn get(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        RealXattr.get(path)
    }
}

/// A probe whose first read blocks until an async task on the test's runtime
/// answers it, then reports `stamped` as stamped with `addr` (and nothing
/// else). Run on that runtime's only async worker, the read would block the
/// task that answers it: it gives up after [`HANDSHAKE_TIMEOUT`] and fails.
struct HandshakeProbe {
    entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    proceed: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    /// Root-relative.
    stamped: &'static str,
    addr: &'static str,
}

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

impl XattrProbe for HandshakeProbe {
    fn get(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
        fn poisoned<T>(_: T) -> std::io::Error {
            std::io::Error::other("probe lock poisoned")
        }
        let entered = self.entered.lock().map_err(poisoned)?.take();
        if let Some(entered) = entered {
            // The receiver is the async task; a send to it never blocks.
            if entered.send(()).is_err() {
                return Err(std::io::Error::other("the answering task is gone"));
            }
            let proceed = self.proceed.lock().map_err(poisoned)?;
            proceed.recv_timeout(HANDSHAKE_TIMEOUT).map_err(|e| {
                std::io::Error::other(format!(
                    "no async task ran while the xattr walk blocked ({e}): the walk is on the async worker"
                ))
            })?;
        }
        Ok(path
            .ends_with(self.stamped)
            .then(|| self.addr.as_bytes().to_vec()))
    }
}

// ---------------------------------------------------------------------------
// keep
// ---------------------------------------------------------------------------

/// A path a current target declares as a `copy` output is refused, naming the
/// target, the output and its BUILD file; nothing changes.
#[tokio::test]
async fn keep_refuses_declared_copy_path() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        &(copy_target("gen", "a.txt", "aaa")
            + r#"
target(
    name = "named",
    driver = "bash",
    run = "printf b > $OUT_PB",
    out = {"pb": "b.txt"},
    codegen = "copy",
)
"#),
    );
    run_on(&ws.engine, "//p:gen").await?;
    run_on(&ws.engine, "//p:named").await?;
    gitignore(&ws.engine).await?;
    let before = gitignore_text(&ws);

    let report = keep(&ws.engine, &["p/a.txt", "p/b.txt"]).await?;
    let r = result_for(&report, "p/a.txt");
    assert_eq!(r.result, PathResultKind::Refused, "{report:?}");
    let why = reason(r);
    for needle in ["//p:gen", "its default output", "p/BUILD"] {
        assert!(why.contains(needle), "the refusal names {needle}: {why}");
    }
    let r = result_for(&report, "p/b.txt");
    assert_eq!(r.result, PathResultKind::Refused, "{report:?}");
    let why = reason(r);
    for needle in ["//p:named", "output `pb`", "p/BUILD"] {
        assert!(why.contains(needle), "the refusal names {needle}: {why}");
    }
    assert_eq!(report.exit_code(), 1);
    assert!(report.lines_removed.is_empty(), "{report:?}");
    assert!(must_row(&ws.engine, "p/a.txt")?.state == RowState::Written);
    assert_eq!(gitignore_text(&ws), before, "the section is untouched");
    assert_eq!(read(&ws, "p/a.txt").as_deref(), Some("aaa"));
    Ok(())
}

/// `keep` drops the row and exactly the section lines that covered the kept
/// path; the other lines, the text outside the section and the file stay.
#[tokio::test]
async fn keep_drops_covering_lines_only() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file(".gitignore", "/user-owned\n");
    let engine = released_pair(&ws).await?;

    let report = keep(&engine, &["p/a.txt"]).await?;
    assert_eq!(
        result_for(&report, "p/a.txt").result,
        PathResultKind::Kept,
        "{report:?}"
    );
    assert_eq!(report.exit_code(), 0);
    let removed: Vec<&str> = report
        .lines_removed
        .iter()
        .map(|l| l.pattern.as_str())
        .collect();
    assert_eq!(removed, ["/p/a.txt"], "only the covering line: {report:?}");

    assert!(!section_has(&ws, "/p/a.txt"), "{}", gitignore_text(&ws));
    assert!(
        section_has(&ws, "/q/b.txt"),
        "another released file's line stays: {}",
        gitignore_text(&ws)
    );
    assert!(section_has(&ws, "/p/keep.txt"), "{}", gitignore_text(&ws));
    assert!(section_has(&ws, "/user-owned"), "{}", gitignore_text(&ws));
    assert!(row(&engine, "p/a.txt")?.is_none(), "the row is dropped");
    assert!(
        row(&engine, "q/b.txt")?.is_some(),
        "another target's row stays"
    );
    assert_eq!(
        read(&ws, "p/a.txt").as_deref(),
        Some("aaa"),
        "the file stays"
    );
    Ok(())
}

/// The text output ends with the `git add` that commits the kept files and
/// the section change.
#[tokio::test]
async fn keep_prints_git_add() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let engine = released_pair(&ws).await?;

    let report = keep(&engine, &["q/b.txt", "p/a.txt"]).await?;
    let text = report.text();
    assert_eq!(
        text.last().map(String::as_str),
        Some("git add p/a.txt q/b.txt .gitignore"),
        "{text:?}"
    );
    let json = report.json();
    assert_eq!(json["schema"], 1);
    assert_eq!(json["results"][0]["path"], "p/a.txt");
    assert_eq!(json["results"][0]["result"], "kept");
    assert_eq!(json["results"][1]["path"], "q/b.txt");
    Ok(())
}

/// One refused path among kept ones: the others are kept, and the command
/// exits 1.
#[tokio::test]
async fn keep_partial_refusal_exits_1() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let engine = released_pair(&ws).await?;
    ws.write_file("p/plain.txt", "source");

    let report = keep(&engine, &["p/a.txt", "p/keep.txt", "p/plain.txt"]).await?;
    assert_eq!(result_for(&report, "p/a.txt").result, PathResultKind::Kept);
    assert_eq!(
        result_for(&report, "p/keep.txt").result,
        PathResultKind::Refused
    );
    assert_eq!(
        result_for(&report, "p/plain.txt").result,
        PathResultKind::Unknown,
        "a file nothing generated: {report:?}"
    );
    assert_eq!(report.exit_code(), 1);
    assert!(row(&engine, "p/a.txt")?.is_none());
    assert!(
        row(&engine, "p/keep.txt")?.is_some(),
        "a refused path keeps its row"
    );
    assert!(!section_has(&ws, "/p/a.txt"));
    assert!(section_has(&ws, "/p/keep.txt"));
    let text = report.text();
    assert_eq!(
        text.last().map(String::as_str),
        Some("git add p/a.txt .gitignore"),
        "git add names only the kept path: {text:?}"
    );
    Ok(())
}

/// From a subdirectory, a root-relative spelling without `//` names nothing
/// there: `unknown`, suggesting the root's path, and nothing is kept.
#[tokio::test]
async fn keep_from_subdirectory_suggests_the_root_path() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let engine = released_pair(&ws).await?;
    let cwd = engine.root().join("p");
    let rs = engine.new_state();
    let res = keep_cmd::keep(&engine, &rs, &strings(&["p/a.txt"]), &cwd).await;
    common::drain(rs).await;
    let report = res?;
    let r = result_for(&report, "p/p/a.txt");
    assert_eq!(r.result, PathResultKind::Unknown, "{report:?}");
    assert_eq!(
        r.reason.as_deref(),
        Some(
            "not generated: no row records it and no section line covers it; \
             did you mean //p/a.txt?"
        )
    );
    assert!(row(&engine, "p/a.txt")?.is_some(), "nothing was kept");
    Ok(())
}

/// A file no row records but a section line covers (an older heph's, before
/// the import) is accepted; only its covering line is dropped.
#[tokio::test]
async fn keep_accepts_unrecorded_path() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("p/x.txt", "x");
    ws.write_file("p/y.txt", "y");
    write_section(&ws, &["/p/x.txt", "/p/y.txt"]);
    assert_eq!(
        status_of(&ws.engine, "p/x.txt")?,
        (StatusKind::Generated, Some(StatusReason::Unrecorded))
    );

    let report = keep(&ws.engine, &["p/x.txt"]).await?;
    assert_eq!(
        result_for(&report, "p/x.txt").result,
        PathResultKind::Kept,
        "{report:?}"
    );
    assert_eq!(report.exit_code(), 0);
    assert!(!section_has(&ws, "/p/x.txt"), "{}", gitignore_text(&ws));
    assert!(section_has(&ws, "/p/y.txt"), "{}", gitignore_text(&ws));
    assert!(section_has(&ws, "/user-owned"), "{}", gitignore_text(&ws));
    assert_eq!(status_of(&ws.engine, "p/x.txt")?.0, StatusKind::Source);
    Ok(())
}

/// `keep` removes the provenance xattr, recorded path or unrecorded.
#[tokio::test]
async fn keep_strips_xattr() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    let engine = released_pair(&ws).await?;
    if stamp_of(&ws, "p/a.txt")?.is_none() {
        stamp(&ws, "p/a.txt", b"//p:a")?;
    }
    stamped_file(&ws, "q/old.txt", "old", "//q:old")?;
    write_section(&ws, &["/p/a.txt", "/q/b.txt", "/q/old.txt"]);

    let report = keep(&engine, &["p/a.txt", "q/old.txt"]).await?;
    assert_eq!(report.exit_code(), 0, "{report:?}");
    assert_eq!(stamp_of(&ws, "p/a.txt")?, None, "a recorded file's stamp");
    assert_eq!(
        stamp_of(&ws, "q/old.txt")?,
        None,
        "an unrecorded file's stamp"
    );
    Ok(())
}

/// A command resolves its defs before it takes any codegen lock (J9): with
/// the target's and the section's locks held and a BUILD file that fails to
/// evaluate, `keep` fails on the BUILD file instead of waiting on a lock.
#[tokio::test]
async fn keep_resolves_defs_before_locking() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let engine = released_pair(&ws).await?;
    ws.write_build_file("broken", "target(name = \n");
    let engine2 = ws.reopen()?;
    drop(engine);

    let rs = engine2.new_state();
    let _target = engine2.codegen_locks().target("//p:a", rs.ctoken()).await?;
    let _section = engine2.codegen_locks().section(rs.ctoken()).await?;

    let res = tokio::time::timeout(Duration::from_secs(60), keep(&engine2, &["p/a.txt"])).await;
    let err = match res {
        Err(_) => panic!("keep waited on a codegen lock before resolving its defs"),
        Ok(Ok(report)) => panic!("keep succeeded over a broken BUILD file: {report:?}"),
        Ok(Err(e)) => e,
    };
    assert!(
        format!("{err:#}").contains("broken"),
        "the error is the BUILD file's: {err:#}"
    );
    assert!(row(&engine2, "p/a.txt")?.is_some(), "nothing changed");
    assert!(section_has(&ws, "/p/a.txt"));
    Ok(())
}

/// Row 28: a kept file has no stamp, so a lost db and an import leave it
/// source.
#[tokio::test]
async fn keep_strips_xattr_so_reimport_leaves_file_source() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    let engine = released_pair(&ws).await?;
    if stamp_of(&ws, "p/a.txt")?.is_none() {
        stamp(&ws, "p/a.txt", b"//p:a")?;
    }
    let report = keep(&engine, &["p/a.txt"]).await?;
    assert_eq!(report.exit_code(), 0, "{report:?}");

    lose_db(&engine)?;
    drop(engine);
    let engine = ws.reopen()?;
    let imported = import(&engine).await?;
    assert!(
        row(&engine, "p/a.txt")?.is_none(),
        "the kept file is not imported"
    );
    assert!(
        row(&engine, "q/b.txt")?.is_some(),
        "the other released file, still stamped, is: {imported:?}"
    );
    assert_eq!(status_of(&engine, "p/a.txt")?.0, StatusKind::Source);
    Ok(())
}

// ---------------------------------------------------------------------------
// discard
// ---------------------------------------------------------------------------

/// Each refusal, with and without `--force`: a path a current target declares
/// (rebuild instead) and a file heph never wrote are refused either way; an
/// edited file only without `--force`.
#[tokio::test]
async fn discard_refusals_and_force() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let engine = released_pair(&ws).await?;
    ws.write_file("q/never.txt", "hand");
    write_section(&ws, &["/p/keep.txt", "/q/b.txt", "/q/never.txt"]);
    ws.write_file("q/b.txt", "edited");
    let paths = ["p/keep.txt", "q/never.txt", "q/b.txt"];

    let report = discard(&engine, named(&paths), false, false).await?;
    let declared = result_for(&report, "p/keep.txt");
    assert_eq!(declared.result, PathResultKind::Refused);
    assert!(
        reason(declared).contains("//p:keep") && reason(declared).contains("rebuild"),
        "{declared:?}"
    );
    let never = result_for(&report, "q/never.txt");
    assert_eq!(never.result, PathResultKind::Refused);
    assert!(reason(never).contains("never wrote"), "{never:?}");
    let edited = result_for(&report, "q/b.txt");
    assert_eq!(edited.result, PathResultKind::Refused);
    assert!(reason(edited).contains("--force"), "{edited:?}");
    assert_eq!(report.exit_code(), 1);
    for p in paths {
        assert!(abs(&ws, p).exists(), "{p} is kept on a refusal");
    }

    let report = discard(&engine, named(&paths), false, true).await?;
    assert_eq!(
        result_for(&report, "p/keep.txt").result,
        PathResultKind::Refused,
        "--force never deletes a declared output"
    );
    assert_eq!(
        result_for(&report, "q/never.txt").result,
        PathResultKind::Refused,
        "--force never deletes a file heph never wrote"
    );
    assert_eq!(
        result_for(&report, "q/b.txt").result,
        PathResultKind::Discarded,
        "{report:?}"
    );
    assert!(!abs(&ws, "q/b.txt").exists());
    assert!(abs(&ws, "q/never.txt").exists());
    assert!(abs(&ws, "p/keep.txt").exists());
    assert!(row(&engine, "q/b.txt")?.is_none());
    assert!(!section_has(&ws, "/q/b.txt"), "{}", gitignore_text(&ws));
    Ok(())
}

/// An edited file is refused without `--force`: bytes, row and line stay.
#[tokio::test]
async fn discard_modified_without_force_refused() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let engine = released_pair(&ws).await?;
    ws.write_file("q/b.txt", "my edit");

    let report = discard(&engine, named(&["q/b.txt"]), false, false).await?;
    let r = result_for(&report, "q/b.txt");
    assert_eq!(r.result, PathResultKind::Refused, "{report:?}");
    assert!(reason(r).contains("modified"), "{r:?}");
    assert_eq!(report.exit_code(), 1);
    assert_eq!(read(&ws, "q/b.txt").as_deref(), Some("my edit"));
    assert!(row(&engine, "q/b.txt")?.is_some());
    assert!(section_has(&ws, "/q/b.txt"));
    Ok(())
}

/// The delete is gated on a fresh hash, never the stat (J5): an edit that
/// keeps the size, the inode and the mtime the row records is refused.
#[tokio::test]
async fn discard_hashes_fresh_before_delete() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let engine = released_pair(&ws).await?;
    let path = abs(&ws, "q/b.txt");
    let before = std::fs::metadata(&path)?;
    std::fs::write(&path, "BBB")?;
    std::fs::File::options()
        .write(true)
        .open(&path)?
        .set_modified(before.modified()?)?;
    let after = std::fs::metadata(&path)?;
    let rec = must_row(&engine, "q/b.txt")?
        .record
        .context("a settled row")?;
    assert_eq!(
        (
            after.size(),
            after.ino(),
            after.mtime() * 1_000_000_000 + after.mtime_nsec()
        ),
        (rec.size, rec.ino, rec.mtime_ns),
        "the premise: the stat equals the record"
    );

    let report = discard(&engine, named(&["q/b.txt"]), false, false).await?;
    assert_eq!(
        result_for(&report, "q/b.txt").result,
        PathResultKind::Refused,
        "{report:?}"
    );
    assert_eq!(read(&ws, "q/b.txt").as_deref(), Some("BBB"));
    Ok(())
}

/// `--all` deletes every file no longer generated, in every package, and
/// leaves what a current target produces.
#[tokio::test]
async fn discard_all_removes_every_released_file() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let engine = released_pair(&ws).await?;

    let report = discard(&engine, Selection::All, false, false).await?;
    assert_eq!(report.exit_code(), 0, "{report:?}");
    let got: Vec<(&str, PathResultKind)> = report
        .results
        .iter()
        .map(|r| (r.path.as_str(), r.result))
        .collect();
    assert_eq!(
        got,
        [
            ("p/a.txt", PathResultKind::Discarded),
            ("q/b.txt", PathResultKind::Discarded)
        ]
    );
    assert!(!abs(&ws, "p/a.txt").exists());
    assert!(!abs(&ws, "q/b.txt").exists());
    assert_eq!(read(&ws, "p/keep.txt").as_deref(), Some("kkk"));
    assert!(row(&engine, "p/a.txt")?.is_none());
    assert!(row(&engine, "q/b.txt")?.is_none());
    assert!(row(&engine, "p/keep.txt")?.is_some());
    assert!(!section_has(&ws, "/p/a.txt"));
    assert!(!section_has(&ws, "/q/b.txt"));
    assert!(section_has(&ws, "/p/keep.txt"));
    Ok(())
}

/// One refused path among discarded ones: the others are deleted, and the
/// command exits 1.
#[tokio::test]
async fn discard_partial_refusal_exits_1() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let engine = released_pair(&ws).await?;

    let report = discard(&engine, named(&["p/keep.txt", "q/b.txt"]), false, false).await?;
    assert_eq!(
        result_for(&report, "p/keep.txt").result,
        PathResultKind::Refused
    );
    assert_eq!(
        result_for(&report, "q/b.txt").result,
        PathResultKind::Discarded
    );
    assert_eq!(report.exit_code(), 1);
    assert!(!abs(&ws, "q/b.txt").exists());
    assert!(abs(&ws, "p/keep.txt").exists());
    Ok(())
}

/// `--dry-run` reports what it would delete and drop, and changes nothing:
/// files, rows, `.gitignore` and the db's generation.
#[tokio::test]
async fn discard_dry_run_changes_nothing() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let engine = released_pair(&ws).await?;
    let section = gitignore_text(&ws);
    let gen_before = generation(&engine)?;

    for selection in [named(&["q/b.txt"]), Selection::All] {
        let report = discard(&engine, selection, true, false).await?;
        assert_eq!(
            result_for(&report, "q/b.txt").result,
            PathResultKind::WouldDiscard,
            "{report:?}"
        );
        assert!(
            report.lines_removed.iter().any(|l| l.pattern == "/q/b.txt"),
            "the line it would drop: {report:?}"
        );
        assert!(
            report.text().iter().any(|l| l.starts_with("would drop")),
            "{:?}",
            report.text()
        );
        assert_eq!(report.exit_code(), 0);
    }
    assert_eq!(read(&ws, "q/b.txt").as_deref(), Some("bbb"));
    assert_eq!(read(&ws, "p/a.txt").as_deref(), Some("aaa"));
    assert!(row(&engine, "q/b.txt")?.is_some());
    assert!(row(&engine, "p/a.txt")?.is_some());
    assert_eq!(gitignore_text(&ws), section);
    assert_eq!(generation(&engine)?, gen_before, "no commit");
    Ok(())
}

/// Rows 13 and 16: `--all` drops the row of a file the user deleted, a
/// current target's included, and keeps that target's line.
#[tokio::test]
async fn discard_all_drops_dangling_rows() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let engine = released_pair(&ws).await?;
    std::fs::remove_file(abs(&ws, "p/keep.txt"))?;
    std::fs::remove_file(abs(&ws, "q/b.txt"))?;

    let report = discard(&engine, Selection::All, false, false).await?;
    assert_eq!(report.exit_code(), 0, "{report:?}");
    for p in ["p/keep.txt", "q/b.txt"] {
        let r = result_for(&report, p);
        assert_eq!(r.result, PathResultKind::Discarded, "{r:?}");
        assert!(reason(r).contains("already gone"), "{r:?}");
        assert!(row(&engine, p)?.is_none(), "{p}'s dangling row is dropped");
    }
    assert!(
        section_has(&ws, "/p/keep.txt"),
        "a current target's line stays: {}",
        gitignore_text(&ws)
    );
    assert!(!section_has(&ws, "/q/b.txt"));
    Ok(())
}

/// An `imported` row's file is deleted only with `--force` (J5).
#[tokio::test]
async fn discard_imported_needs_force() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    stamped_file(&ws, "q/x.txt", "old", "//q:old")?;
    import(&ws.engine).await?;
    assert_eq!(must_row(&ws.engine, "q/x.txt")?.state, RowState::Imported);

    let report = discard(&ws.engine, named(&["q/x.txt"]), false, false).await?;
    let r = result_for(&report, "q/x.txt");
    assert_eq!(r.result, PathResultKind::Refused, "{report:?}");
    assert!(reason(r).contains("--force"), "{r:?}");
    assert!(abs(&ws, "q/x.txt").exists());

    let report = discard(&ws.engine, named(&["q/x.txt"]), false, true).await?;
    assert_eq!(
        result_for(&report, "q/x.txt").result,
        PathResultKind::Discarded,
        "{report:?}"
    );
    assert!(!abs(&ws, "q/x.txt").exists());
    assert!(row(&ws.engine, "q/x.txt")?.is_none());
    Ok(())
}

/// A write-back that no longer produces an imported file leaves it on disk:
/// only a `written` row's file is ever deleted on its record (J5).
#[tokio::test]
async fn imported_row_never_deleted_by_write_back() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    ws.write_build_file("p", GLOB_GEN);
    ws.write_file("p/n.txt", "new");
    stamped_file(&ws, "p/gen/old.txt", "file old", "//p:gen")?;
    let report = import(&ws.engine).await?;
    assert_eq!(report.recorded, 1, "{report:?}");

    run_on(&ws.engine, "//p:gen").await?;
    assert_eq!(read(&ws, "p/gen/new.txt").as_deref(), Some("file new"));
    assert_eq!(
        read(&ws, "p/gen/old.txt").as_deref(),
        Some("file old"),
        "the imported file is kept"
    );
    let old = must_row(&ws.engine, "p/gen/old.txt")?;
    assert_ne!(old.state, RowState::Written, "{old:?}");
    Ok(())
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

/// Row 19: a lost db is rebuilt from the stamps the write-back set.
#[tokio::test]
async fn codegen_import_rebuilds_lost_index_from_xattrs() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    ws.write_build_file("p", &copy_target("gen", "a.txt", "aaa"));
    run_on(&ws.engine, "//p:gen").await?;
    assert_eq!(
        stamp_of(&ws, "p/a.txt")?.as_deref(),
        Some(b"//p:gen".as_slice()),
        "the write-back stamps what it writes"
    );
    lose_db(&ws.engine)?;
    let engine = ws.reopen()?;
    assert_eq!(status_of(&engine, "p/a.txt")?.0, StatusKind::Source);

    let report = import(&engine).await?;
    assert_eq!((report.recorded, report.unchanged), (1, 0), "{report:?}");
    let r = must_row(&engine, "p/a.txt")?;
    assert_eq!(r.addr, "//p:gen");
    assert_eq!(r.state, RowState::Imported);
    assert_eq!(r.record.map(|r| r.hash), Some(hashout("aaa")));
    assert!(status_of(&engine, "p/a.txt")?.0.is_excluded());
    Ok(())
}

/// Each stamped file becomes an `imported` row for the addr in its value,
/// with its current hash and no output; an unstamped file records nothing.
#[tokio::test]
async fn codegen_import_records_stamped_files() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    stamped_file(&ws, "p/a.txt", "aaa", "//p:a")?;
    stamped_file(&ws, "q/sub/b.txt", "bbb", "//q:b")?;
    ws.write_file("p/plain.txt", "source");

    let report = import(&ws.engine).await?;
    assert_eq!(
        (report.recorded, report.skipped, report.unchanged),
        (2, 0, 0),
        "{report:?}"
    );
    for (p, addr, body) in [("p/a.txt", "//p:a", "aaa"), ("q/sub/b.txt", "//q:b", "bbb")] {
        let r = must_row(&ws.engine, p)?;
        assert_eq!(r.addr, addr);
        assert_eq!(r.state, RowState::Imported);
        assert_eq!(r.output, None);
        assert_eq!(r.pending_hash, None);
        assert_eq!(r.record.map(|r| r.hash), Some(hashout(body)));
    }
    assert!(row(&ws.engine, "p/plain.txt")?.is_none());
    Ok(())
}

/// With no section, an imported file is excluded on its row alone.
#[tokio::test]
async fn codegen_import_without_section_stays_excluded() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    stamped_file(&ws, "p/a.txt", "aaa", "//p:a")?;
    import(&ws.engine).await?;

    assert!(!must_row(&ws.engine, "p/a.txt")?.seen_in_section);
    assert_eq!(
        status_of(&ws.engine, "p/a.txt")?,
        (StatusKind::Generated, Some(StatusReason::Recorded))
    );
    Ok(())
}

/// A section that does not cover an imported file does not release it.
#[tokio::test]
async fn codegen_import_with_stale_section_stays_excluded() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    stamped_file(&ws, "p/a.txt", "aaa", "//p:a")?;
    write_section(&ws, &["/p/other.txt"]);
    import(&ws.engine).await?;

    assert!(!must_row(&ws.engine, "p/a.txt")?.seen_in_section);
    assert_eq!(
        status_of(&ws.engine, "p/a.txt")?,
        (StatusKind::Generated, Some(StatusReason::Recorded))
    );
    Ok(())
}

/// The import sets `seen_in_section` on each row a line covers, whether it
/// creates the db or merges into it, and never on one no line covers.
#[tokio::test]
async fn codegen_import_sets_seen_in_section() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    stamped_file(&ws, "p/a.txt", "aaa", "//p:a")?;
    stamped_file(&ws, "p/b.txt", "bbb", "//p:b")?;
    write_section(&ws, &["/p/a.txt"]);
    import(&ws.engine).await?;
    assert!(must_row(&ws.engine, "p/a.txt")?.seen_in_section);
    assert!(!must_row(&ws.engine, "p/b.txt")?.seen_in_section);

    // Merge: the db exists, and a line now covers b.
    write_section(&ws, &["/p/a.txt", "/p/b.txt"]);
    let engine = ws.reopen()?;
    let report = import(&engine).await?;
    assert_eq!((report.recorded, report.unchanged), (0, 2), "{report:?}");
    assert!(must_row(&engine, "p/b.txt")?.seen_in_section);
    assert!(must_row(&engine, "p/a.txt")?.seen_in_section);
    Ok(())
}

/// The command reports its counts, warns once about the skipped values and
/// emits `CodegenImported`.
#[tokio::test]
async fn codegen_import_reports_count_and_event() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    stamped_file(&ws, "p/a.txt", "aaa", "//p:a")?;
    stamped_file(&ws, "p/b.txt", "bbb", "//p:b")?;
    ws.write_file("p/bad.txt", "x");
    stamp(&ws, "p/bad.txt", b"not an addr")?;
    ws.write_file("p/binary.txt", "x");
    stamp(&ws, "p/binary.txt", &[0xff, 0xfe])?;

    let ((res, events), log) = capture_warnings(import_with_events(&ws.engine)).await;
    let report = res?;
    assert_eq!(
        (report.recorded, report.skipped, report.unchanged),
        (2, 2, 0),
        "{report:?}"
    );
    assert_eq!(
        report.text(),
        ["recorded 2 generated files from the previous heph's xattrs; 2 skipped, 0 unchanged"]
    );
    assert_eq!(report.exit_code(), 0);
    let imported: Vec<(usize, usize)> = events
        .iter()
        .filter_map(|e| match e.kind {
            BuildEventKind::CodegenImported { files, skipped } => Some((files, skipped)),
            _ => None,
        })
        .collect();
    assert_eq!(imported, [(2, 2)], "one event: {events:?}");
    assert_eq!(
        log.matches("unreadable codegen xattr").count(),
        1,
        "one warning: {log}"
    );
    assert!(row(&ws.engine, "p/bad.txt")?.is_none());
    assert!(row(&ws.engine, "p/binary.txt")?.is_none());
    Ok(())
}

/// Rows 19 and 20 before the import: a stamped file reads as `unrecorded`
/// under a line and as `Source` without one; nothing imports on its own.
#[tokio::test]
async fn upgrade_before_import_reads_lines_as_unrecorded() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    stamped_file(&ws, "p/a.txt", "aaa", "//p:a")?;
    stamped_file(&ws, "p/b.txt", "bbb", "//p:b")?;
    write_section(&ws, &["/p/a.txt"]);

    assert_eq!(
        status_of(&ws.engine, "p/a.txt")?,
        (StatusKind::Generated, Some(StatusReason::Unrecorded))
    );
    assert_eq!(status_of(&ws.engine, "p/b.txt")?.0, StatusKind::Source);
    status_report(&ws.engine).await?;
    why_report(&ws.engine, &["p/a.txt"]).await?;
    assert!(
        store(&ws.engine)?.is_none(),
        "no command but import creates the db from the stamps"
    );
    Ok(())
}

/// `status` hints at the import when the index is empty and a section line
/// matches a file on disk, and only then.
#[tokio::test]
async fn status_hints_import_when_index_empty_and_lines_match() -> anyhow::Result<()> {
    let ws = Workspace::new();
    assert_eq!(status_report(&ws.engine).await?.hint, None, "no section");

    write_section(&ws, &["/p/a.txt", "/p/missing.txt"]);
    assert_eq!(
        status_report(&ws.engine).await?.hint,
        None,
        "no file matches"
    );

    ws.write_file("p/a.txt", "aaa");
    let report = status_report(&ws.engine).await?;
    let hint = report.hint.clone().context("the hint")?;
    assert!(hint.contains("1 file matches"), "{hint}");
    assert!(hint.contains("heph tool codegen import"), "{hint}");
    assert!(
        report.text().iter().any(|l| l == &hint),
        "{:?}",
        report.text()
    );
    assert_eq!(report.json()["hint"], hint.as_str());

    ws.write_build_file("q", &copy_target("gen", "gen.txt", "g"));
    let engine = ws.reopen()?;
    run_on(&engine, "//q:gen").await?;
    assert_eq!(
        status_report(&engine).await?.hint,
        None,
        "the index has rows"
    );
    Ok(())
}

/// `why` adds the hint for a path a section line matches while the index is
/// empty; not for a path no line matches.
#[tokio::test]
async fn why_hints_import_on_matched_path_with_empty_index() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("p/a.txt", "aaa");
    ws.write_file("p/z.txt", "zzz");
    write_section(&ws, &["/p/a.txt"]);

    let report = why_report(&ws.engine, &["p/a.txt"]).await?;
    let hint = report.hint.clone().context("the hint")?;
    assert!(hint.contains("heph tool codegen import"), "{hint}");
    assert!(
        report.text().iter().any(|l| l == &hint),
        "{:?}",
        report.text()
    );

    assert_eq!(why_report(&ws.engine, &["p/z.txt"]).await?.hint, None);
    Ok(())
}

/// The hint counts section matches, never stamps: the files it counts are the
/// ones a line covers, stamped or not, and the stamped files outside every
/// line are neither counted nor recorded. And it reads no xattr: the engine's
/// probe ([`Engine::xattr_probe`], the one every command reads a stamp
/// through) counts 0 reads across `status` and `why` with the hint printed,
/// while `import` on the same engine, the control, reads through it.
#[tokio::test]
async fn import_hint_probes_no_xattr() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    ws.write_file("p/a.txt", "aaa");
    stamped_file(&ws, "p/b.txt", "bbb", "//p:b")?;
    stamped_file(&ws, "q/c.txt", "ccc", "//q:c")?;
    write_section(&ws, &["/p/a.txt"]);
    let probe = Arc::new(CountingProbe::default());
    let engine = engine_with_probe(ws.dir.path(), Arc::clone(&probe) as Arc<dyn XattrProbe>)?;

    let hint = status_report(&engine).await?.hint.context("the hint")?;
    assert!(hint.contains("1 file matches"), "{hint}");
    let hint = why_report(&engine, &["p/a.txt", "p/b.txt"])
        .await?
        .hint
        .context("the why hint")?;
    assert!(hint.contains("1 file matches"), "{hint}");
    assert_eq!(probe.count(), 0, "status and why probed an xattr");
    assert!(store(&engine)?.is_none(), "nothing was recorded");
    assert_eq!(status_of(&engine, "p/b.txt")?.0, StatusKind::Source);

    // The control: the import reads every file's xattr through the same probe.
    let report = import(&engine).await?;
    assert_eq!(report.recorded, 2, "{report:?}");
    assert!(
        probe.count() >= 3,
        "the import read each file's xattr through the engine's probe: {}",
        probe.count()
    );
    Ok(())
}

/// Row 27: after N → N-1 → N, the import records the files N-1 wrote, and
/// leaves N's verified rows as they are.
#[tokio::test]
async fn codegen_import_after_n_minus_1_records_its_files() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    ws.write_build_file("p", GLOB_GEN);
    ws.write_file("p/n.txt", "one");
    run_on(&ws.engine, "//p:gen").await?;
    let one = must_row(&ws.engine, "p/gen/one.txt")?;
    assert_eq!(one.state, RowState::Written);
    if stamp_of(&ws, "p/gen/one.txt")?.is_none() {
        stamp(&ws, "p/gen/one.txt", b"//p:gen")?;
    }
    // N-1 ran a newer revision of the target and wrote another file.
    stamped_file(&ws, "p/gen/two.txt", "file two", "//p:gen")?;

    let report = import(&ws.engine).await?;
    assert_eq!((report.recorded, report.unchanged), (1, 1), "{report:?}");
    let two = must_row(&ws.engine, "p/gen/two.txt")?;
    assert_eq!(two.state, RowState::Imported);
    assert_eq!(two.addr, "//p:gen");
    let again = must_row(&ws.engine, "p/gen/one.txt")?;
    assert_eq!(again.state, RowState::Written, "N's own row is untouched");
    assert_eq!(again.output, one.output);
    assert!(status_of(&ws.engine, "p/gen/two.txt")?.0.is_excluded());
    Ok(())
}

/// A second import records nothing and commits nothing.
#[tokio::test]
async fn codegen_import_is_idempotent() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    stamped_file(&ws, "p/a.txt", "aaa", "//p:a")?;
    stamped_file(&ws, "q/b.txt", "bbb", "//q:b")?;
    write_section(&ws, &["/p/a.txt"]);
    let first = import(&ws.engine).await?;
    assert_eq!(first.recorded, 2, "{first:?}");
    let gen_after_first = generation(&ws.engine)?;
    let rows_after_first = (
        must_row(&ws.engine, "p/a.txt")?,
        must_row(&ws.engine, "q/b.txt")?,
    );

    for engine in [Arc::clone(&ws.engine), ws.reopen()?] {
        let again = import(&engine).await?;
        assert_eq!(
            (again.recorded, again.skipped, again.unchanged),
            (0, 0, 2),
            "{again:?}"
        );
        assert_eq!(generation(&engine)?, gen_after_first, "no commit");
        assert_eq!(
            (must_row(&engine, "p/a.txt")?, must_row(&engine, "q/b.txt")?),
            rows_after_first
        );
    }
    Ok(())
}

/// `--json`: `{"schema": 1, "recorded", "skipped", "unchanged"}`, nothing
/// else.
#[tokio::test]
async fn codegen_import_json_shape() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    stamped_file(&ws, "p/a.txt", "aaa", "//p:a")?;
    ws.write_file("p/bad.txt", "x");
    stamp(&ws, "p/bad.txt", b"not an addr")?;

    let report = import(&ws.engine).await?;
    assert_eq!(
        report.json(),
        serde_json::json!({"schema": 1, "recorded": 1, "skipped": 1, "unchanged": 0})
    );
    let report = import(&ws.engine).await?;
    assert_eq!(
        report.json(),
        serde_json::json!({"schema": 1, "recorded": 0, "skipped": 1, "unchanged": 1})
    );
    Ok(())
}

/// Recording nothing new is not a failure: exit 0, with and without stamps.
#[tokio::test]
async fn codegen_import_exits_0_when_nothing_new() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let report = import(&ws.engine).await?;
    assert_eq!(report.recorded, 0, "{report:?}");
    assert_eq!(report.exit_code(), 0);

    if xattrs_supported(ws.dir.path()) {
        stamped_file(&ws, "p/a.txt", "aaa", "//p:a")?;
        let engine = ws.reopen()?;
        assert_eq!(import(&engine).await?.recorded, 1);
        let again = import(&engine).await?;
        assert_eq!((again.recorded, again.unchanged), (0, 1), "{again:?}");
        assert_eq!(again.exit_code(), 0);
    }
    Ok(())
}

/// Two imports at once (two `heph` invocations): one creates the db, the
/// other waits on the import lock, then records nothing new.
#[tokio::test]
async fn concurrent_codegen_imports_record_once() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    for (p, addr) in [
        ("p/a.txt", "//p:a"),
        ("p/b.txt", "//p:b"),
        ("q/c.txt", "//q:c"),
    ] {
        stamped_file(&ws, p, "x", addr)?;
    }
    let other = ws.reopen()?;

    let (a, b) = tokio::join!(import(&ws.engine), import(&other));
    let (a, b) = (a?, b?);
    let mut counts = [(a.recorded, a.unchanged), (b.recorded, b.unchanged)];
    counts.sort_unstable();
    assert_eq!(counts, [(0, 3), (3, 0)], "{a:?} {b:?}");
    for p in ["p/a.txt", "p/b.txt", "q/c.txt"] {
        assert_eq!(must_row(&ws.engine, p)?.state, RowState::Imported);
    }
    Ok(())
}

/// A write-back and an import both finding no db: one creates it under the
/// import lock, the other commits or merges into it, and both kinds of row
/// end up in the one db.
#[tokio::test]
async fn write_back_and_import_create_db_once() -> anyhow::Result<()> {
    let ws = Workspace::new();
    require_xattrs!(ws);
    ws.write_build_file("p", &copy_target("gen", "a.txt", "aaa"));
    stamped_file(&ws, "q/x.txt", "old", "//q:old")?;
    let other = ws.reopen()?;
    assert!(store(&ws.engine)?.is_none());

    let (built, imported) = tokio::join!(run_on(&ws.engine, "//p:gen"), import(&other));
    built?;
    let imported = imported?;
    assert!(imported.recorded >= 1, "{imported:?}");

    let a = must_row(&ws.engine, "p/a.txt")?;
    assert_eq!((a.addr.as_str(), a.state), ("//p:gen", RowState::Written));
    let x = must_row(&ws.engine, "q/x.txt")?;
    assert_eq!((x.addr.as_str(), x.state), ("//q:old", RowState::Imported));

    let dir = ws
        .engine
        .codegen_index()
        .db_path()
        .parent()
        .context("db dir")?;
    let leftovers: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "no temp db is left: {leftovers:?}");
    Ok(())
}

/// The walk runs on the blocking pool (spec: How the import runs), shown
/// without timing: on a current-thread runtime, the walk's first xattr read
/// blocks until an async task spawned on that runtime answers it. On the
/// blocking pool, the one async worker runs that task and the import
/// completes; on the async worker, nothing could run it, the read gives up
/// after [`HANDSHAKE_TIMEOUT`] and the import fails. Needs no xattr support:
/// the probe stands in for the filesystem.
#[tokio::test(flavor = "current_thread")]
async fn codegen_import_runs_on_blocking_pool() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("p/a.txt", "aaa");
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
    let probe = HandshakeProbe {
        entered: std::sync::Mutex::new(Some(entered_tx)),
        proceed: std::sync::Mutex::new(proceed_rx),
        stamped: "p/a.txt",
        addr: "//p:a",
    };
    let engine = engine_with_probe(ws.dir.path(), Arc::new(probe))?;

    // Answers the probe only once it is blocked waiting: a send before the
    // walk would let a walk on the async worker through.
    let answer =
        tokio::spawn(async move { entered_rx.await.is_ok() && proceed_tx.send(()).is_ok() });
    let report = tokio::time::timeout(HANDSHAKE_TIMEOUT * 4, import(&engine))
        .await
        .context("the import never finished")??;
    assert!(answer.await?, "the walk never read an xattr");

    assert_eq!(report.recorded, 1, "{report:?}");
    assert_eq!(
        row(&engine, "p/a.txt")?.map(|r| r.addr),
        Some("//p:a".into())
    );
    Ok(())
}

/// A shared home: checkout B's import creates B's own db file although
/// `codegen/` holds A's, and A's db is untouched.
#[tokio::test]
async fn shared_home_codegen_import_writes_its_own_db_file() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let (root_a, root_b) = (tempfile::tempdir()?, tempfile::tempdir()?);
    if !xattrs_supported(root_b.path()) {
        eprintln!("skipping: this filesystem stores no user xattrs");
        return Ok(());
    }
    std::fs::create_dir_all(root_a.path().join("p"))?;
    std::fs::write(
        root_a.path().join("p/BUILD"),
        copy_target("gen", "a.txt", "aaa"),
    )?;
    let a = engine_with_home(root_a.path(), home.path())?;
    run_on(&a, "//p:gen").await?;
    let db_a = a.codegen_index().db_path().to_path_buf();
    let a_rows = store(&a)?.context("A's db")?.load_all()?.1;
    let a_meta = std::fs::metadata(&db_a)?;

    std::fs::create_dir_all(root_b.path().join("q"))?;
    std::fs::write(root_b.path().join("q/x.txt"), "old")?;
    xattr::set(root_b.path().join("q/x.txt"), CODEGEN_XATTR, b"//q:old")?;
    let b = engine_with_home(root_b.path(), home.path())?;
    let report = import(&b).await?;
    assert_eq!(report.recorded, 1, "{report:?}");

    let db_b = b.codegen_index().db_path().to_path_buf();
    assert_ne!(db_a, db_b, "each checkout has its own db file");
    assert_eq!(db_a.parent(), db_b.parent(), "in the one shared codegen/");
    let b_rows = store(&b)?.context("B's db")?.load_all()?.1;
    assert_eq!(b_rows.len(), 1);
    assert_eq!(b_rows[0].path, b"q/x.txt");

    let after = std::fs::metadata(&db_a)?;
    assert_eq!(
        (after.ino(), after.size(), after.mtime_nsec(), after.mtime()),
        (
            a_meta.ino(),
            a_meta.size(),
            a_meta.mtime_nsec(),
            a_meta.mtime()
        ),
        "A's db file is untouched"
    );
    assert_eq!(store(&a)?.context("A's db")?.load_all()?.1, a_rows);
    Ok(())
}

/// A home that cannot be written fails the import with exit 2, naming the
/// path.
#[tokio::test]
async fn codegen_import_on_read_only_home_exits_2() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let (root, home) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let engine = engine_with_home(root.path(), home.path())?;
    let codegen_dir = engine
        .codegen_index()
        .db_path()
        .parent()
        .context("db dir")?
        .to_path_buf();
    // The lock directory exists; the db's directory cannot be written.
    std::fs::create_dir_all(home.path().join("lock/codegen"))?;
    std::fs::create_dir_all(&codegen_dir)?;
    std::fs::set_permissions(&codegen_dir, std::fs::Permissions::from_mode(0o555))?;
    let writable = std::fs::write(codegen_dir.join("probe"), b"").is_ok();
    let res = if writable {
        None
    } else {
        Some(import(&engine).await)
    };
    std::fs::set_permissions(&codegen_dir, std::fs::Permissions::from_mode(0o755))?;
    let Some(res) = res else {
        eprintln!("skipping: a read-only directory is writable here (running as root?)");
        return Ok(());
    };

    let err = match res {
        Ok(report) => panic!("the import succeeded on a read-only home: {report:?}"),
        Err(e) => e,
    };
    let shown = format!("{err:#}");
    assert!(
        shown.contains(&codegen_dir.display().to_string()),
        "the error names the path: {shown}"
    );
    let wrapped = wrap_exit(Err(err)).expect_err("still an error");
    assert_eq!(
        wrapped.downcast_ref::<CodegenExit>().map(|c| c.code),
        Some(2),
        "{wrapped:#}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// locks
// ---------------------------------------------------------------------------

/// Every section writer takes the section lock (J8, J9): with it held, `keep`
/// and `discard` wait and `.gitignore` stays as it was; once it is released,
/// both finish and neither loses the other's change.
#[tokio::test]
async fn section_writers_are_serialised() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let engine = released_pair(&ws).await?;
    let before = gitignore_text(&ws);

    let rs = engine.new_state();
    let held = engine.codegen_locks().section(rs.ctoken()).await?;
    let kept = tokio::spawn({
        let engine = Arc::clone(&engine);
        async move { keep(&engine, &["p/a.txt"]).await }
    });
    let discarded = tokio::spawn({
        let engine = Arc::clone(&engine);
        async move { discard(&engine, named(&["q/b.txt"]), false, false).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!kept.is_finished(), "keep waits on the section lock");
    assert!(
        !discarded.is_finished(),
        "discard waits on the section lock"
    );
    assert_eq!(
        gitignore_text(&ws),
        before,
        "no section write under the lock"
    );
    drop(held);

    let (kept, discarded) = (kept.await??, discarded.await??);
    assert_eq!(kept.exit_code(), 0, "{kept:?}");
    assert_eq!(discarded.exit_code(), 0, "{discarded:?}");
    assert!(!section_has(&ws, "/p/a.txt"), "{}", gitignore_text(&ws));
    assert!(!section_has(&ws, "/q/b.txt"), "{}", gitignore_text(&ws));
    assert!(section_has(&ws, "/p/keep.txt"), "{}", gitignore_text(&ws));
    Ok(())
}
