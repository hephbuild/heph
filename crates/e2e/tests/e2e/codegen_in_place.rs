//! `codegen = "in_place"` write-back: a target that rewrites the very files it
//! took as inputs (a formatter, a lint fixer).
#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::codegen_index_write::{copy_target, must_row, run_on, status};
use crate::common;

use common::Workspace;
use heph::engine::error::{FailureFixes, FixesNotWritten, TargetFailure};
use heph::engine::event::BuildEventKind;
use heph::engine::{
    Engine, HomeDir, InteractiveInner, InteractiveWrapper, OutputMatcher, ResultOptions,
};
use heph::htaddr::parse_addr;
use heph::htwalk::codegen_index::{RowState, StatusKind};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A target that uppercases its own source in place.
///
/// `mutate` is bash spliced in after the transform: the test uses it to move the
/// tree *while the target runs*, standing in for the editor save / `git checkout`
/// / concurrent run that the write-back guard exists to catch.
fn build_file(mutate: &str) -> String {
    format!(
        r#"
target(
    name = "upper",
    driver = "bash",
    deps = [file("src.txt")],
    out = "src.txt",
    codegen = "in_place",
    run = "up=$(tr a-z A-Z < src.txt); printf '%s' \"$up\" > $OUT; {mutate}",
)
"#
    )
}

/// The happy path: a settled tree is transformed and written back, and running
/// again over the already-transformed tree is a clean no-op. The second run is
/// what proves the guard doesn't false-positive on the write-back's *own*
/// change to the tree.
#[tokio::test]
async fn in_place_writes_back_and_reruns_clean() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("fix", &build_file("true"));
    ws.write_file("fix/src.txt", "hello");

    ws.run("//fix:upper").await?;
    let src = ws.dir.path().join("fix/src.txt");
    assert_eq!(
        std::fs::read_to_string(&src)?,
        "HELLO",
        "the transformed bytes must land on the tracked source file"
    );

    ws.run("//fix:upper").await?;
    assert_eq!(
        std::fs::read_to_string(&src)?,
        "HELLO",
        "a re-run over the already-transformed tree must change nothing"
    );
    Ok(())
}

/// The guard: when the source moves between being hashed and being written back,
/// the transform is stale — it was computed from the older bytes. Writing it
/// would silently discard the newer ones, so the run fails and the tree is left
/// exactly as the concurrent writer left it.
#[tokio::test]
async fn in_place_refuses_to_write_back_over_a_changed_source() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let src = ws.dir.path().join("fix/src.txt");
    // Absolute path: this write deliberately escapes the sandbox to model an
    // *external* edit landing mid-run, which is exactly what the guard watches
    // for. No ordinary target may do this.
    ws.write_build_file(
        "fix",
        &build_file(&format!(
            "printf 'edited by someone else' > {}",
            src.display()
        )),
    );
    ws.write_file("fix/src.txt", "hello");

    let msg = match ws.run("//fix:upper").await {
        Err(e) => format!("{e:#}"),
        Ok(_) => panic!("a stale in-place write-back must fail, not clobber"),
    };
    assert!(
        msg.contains("changed while it ran"),
        "error must name the staleness, got: {msg}"
    );

    assert_eq!(
        std::fs::read_to_string(&src)?,
        "edited by someone else",
        "the newer bytes must survive: nothing is written back"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Write-back on failure: `codegen = "in_place"` writes a failed run's fixes
// back over the tree after an exit code from 1 to 127, and the target still
// fails. `in_place_on_success`, a signal, an exit of 128 and above, `--frozen`
// and `--shell` write nothing.
// ---------------------------------------------------------------------------

const ADDR: &str = "//fix:upper";

/// [`build_file`] with the codegen mode and the script's tail spelled out: the
/// target uppercases `src.txt` in place, then runs `tail` (`exit 1`, a kill).
fn failing_build_file(codegen: &str, tail: &str) -> String {
    format!(
        r#"
target(
    name = "upper",
    driver = "bash",
    deps = [file("src.txt")],
    out = "src.txt",
    codegen = "{codegen}",
    run = "up=$(tr a-z A-Z < src.txt); printf '%s' \"$up\" > $OUT; {tail}",
)
"#
    )
}

/// What resolving `addr` once did: the error it failed with, and the failures
/// the request recorded.
struct Failed {
    error: String,
    failures: Vec<Arc<TargetFailure>>,
}

impl Failed {
    /// The recorded failure of `addr`: exactly one.
    fn of(&self, addr: &str) -> &TargetFailure {
        let found: Vec<&TargetFailure> = self
            .failures
            .iter()
            .filter(|f| f.addr.format() == addr)
            .map(|f| &**f)
            .collect();
        assert_eq!(
            found.len(),
            1,
            "one failure recorded for {addr}: {}",
            self.error
        );
        found.first().copied().expect("one failure")
    }
}

/// Resolve `addr` on `engine`, which must fail, in a request of its own.
async fn resolve_failing(
    engine: &Arc<Engine>,
    addr: &str,
    opts: &ResultOptions,
) -> anyhow::Result<Failed> {
    let rs = engine.new_state();
    let res = engine
        .clone()
        .result_addr(rs.clone(), &parse_addr(addr)?, OutputMatcher::All, opts)
        .await;
    assert!(res.is_err(), "{addr} must fail");
    Ok(Failed {
        error: res.err().map(|e| format!("{e:#}")).unwrap_or_default(),
        failures: rs.take_failures(),
    })
}

fn slot_dir(engine: &Engine, addr: &str) -> PathBuf {
    engine
        .failure_slots()
        .slot_dir(&parse_addr(addr).expect("addr"))
}

fn read(ws: &Workspace, rel: &str) -> String {
    std::fs::read_to_string(ws.dir.path().join(rel)).expect("read a workspace file")
}

/// Every file under `dir` whose bytes contain `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files_containing(&p, needle));
        } else if std::fs::read(&p)
            .map(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
            .unwrap_or(false)
        {
            out.push(p);
        }
    }
    out
}

/// The workspace-relative paths of every file under `dir`, sorted.
fn walk(dir: &Path) -> Vec<String> {
    fn go(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                go(base, &p, out);
            } else if let Ok(rel) = p.strip_prefix(base) {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    let mut out = Vec::new();
    go(dir, dir, &mut out);
    out.sort();
    out
}

/// The fixes an engine at `root` reports: written under that root.
fn written(root: &Path, paths: &[&str], list: Option<PathBuf>) -> Option<FailureFixes> {
    Some(FailureFixes::Written {
        root: root.to_path_buf(),
        paths: paths.iter().map(|p| (*p).to_string()).collect(),
        list,
    })
}

/// An exit code from 1 to 127: the fixes reach the tree, and the target still
/// fails (D3, D12). Nothing is cached at the failed run's `hashin` (I4): a run
/// over the same inputs again executes, and fails, rather than being served.
#[tokio::test]
async fn writes_fixes_on_failure_and_fails() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("fix", &failing_build_file("in_place", "exit 1"));
    ws.write_file("fix/src.txt", "fixme-marker");

    let failed = resolve_failing(&ws.engine, ADDR, &ResultOptions::default()).await?;
    assert_eq!(
        read(&ws, "fix/src.txt"),
        "FIXME-MARKER",
        "the fix reaches the tree"
    );
    let slot = slot_dir(&ws.engine, ADDR);
    assert_eq!(
        failed.of(ADDR).fixes,
        written(
            ws.engine.root(),
            &["fix/src.txt"],
            Some(slot.join("fixes.txt"))
        ),
        "{}",
        failed.error
    );

    let home = ws.engine.home.to_path_buf();
    assert_eq!(
        files_containing(&home.join("cache"), "FIXME-MARKER"),
        Vec::<PathBuf>::new(),
        "a failed run's fixes never reach the cache"
    );

    // Same inputs as the failed run: a manifest at its hashin would serve
    // this from the cache. It must run again, and fail again.
    ws.write_file("fix/src.txt", "fixme-marker");
    let again = resolve_failing(&ws.engine, ADDR, &ResultOptions::default()).await?;
    assert_eq!(
        again.of(ADDR).fixes,
        written(
            ws.engine.root(),
            &["fix/src.txt"],
            Some(slot.join("fixes.txt"))
        ),
        "re-executed, not served from a cache entry: {}",
        again.error
    );
    Ok(())
}

/// A fixer that died before writing one of its files must not lose the others
/// (D18): a declared output missing from the sandbox is skipped, and the tree's
/// copy of it is left alone.
#[tokio::test]
async fn failure_with_some_outputs_missing_still_writes_the_rest() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "fix",
        r#"
target(
    name = "upper",
    driver = "bash",
    deps = [file("a.txt"), file("b.txt")],
    out = ["a.txt", "b.txt"],
    codegen = "in_place",
    run = "up=$(tr a-z A-Z < a.txt); printf '%s' \"$up\" > a.txt; rm b.txt; exit 1",
)
"#,
    );
    ws.write_file("fix/a.txt", "a-orig");
    ws.write_file("fix/b.txt", "b-orig");

    let failed = resolve_failing(&ws.engine, ADDR, &ResultOptions::default()).await?;
    assert_eq!(read(&ws, "fix/a.txt"), "A-ORIG", "{}", failed.error);
    assert_eq!(
        read(&ws, "fix/b.txt"),
        "b-orig",
        "a missing output is skipped, never deleted from the tree"
    );
    assert_eq!(
        failed.of(ADDR).fixes,
        written(
            ws.engine.root(),
            &["fix/a.txt"],
            Some(slot_dir(&ws.engine, ADDR).join("fixes.txt"))
        )
    );
    Ok(())
}

/// The opt-out keeps today's behaviour: a failed run writes nothing.
#[tokio::test]
async fn on_success_writes_nothing_on_failure() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("fix", &failing_build_file("in_place_on_success", "exit 1"));
    ws.write_file("fix/src.txt", "hello");

    let failed = resolve_failing(&ws.engine, ADDR, &ResultOptions::default()).await?;
    assert_eq!(read(&ws, "fix/src.txt"), "hello");
    assert_eq!(failed.of(ADDR).fixes, None, "{}", failed.error);
    assert!(
        !slot_dir(&ws.engine, ADDR).join("fixes.txt").exists(),
        "no fixes.txt for a run that wrote nothing"
    );
    Ok(())
}

/// A killed fixer may have half-written a file (D8): nothing is written, and
/// the failure says why.
#[tokio::test]
async fn skips_write_back_on_signal() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("fix", &failing_build_file("in_place", "kill -9 $$"));
    ws.write_file("fix/src.txt", "hello");

    let failed = resolve_failing(&ws.engine, ADDR, &ResultOptions::default()).await?;
    assert_eq!(read(&ws, "fix/src.txt"), "hello");
    assert_eq!(
        failed.of(ADDR).fixes,
        Some(FailureFixes::NotWritten(FixesNotWritten::Signal(9))),
        "{}",
        failed.error
    );
    Ok(())
}

/// Bash reports a killed child as 128+n (137: SIGKILL, an OOM kill): counted
/// as a signal (D8).
#[tokio::test]
async fn skips_write_back_on_exit_137() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("fix", &failing_build_file("in_place", "exit 137"));
    ws.write_file("fix/src.txt", "hello");

    let failed = resolve_failing(&ws.engine, ADDR, &ResultOptions::default()).await?;
    assert_eq!(read(&ws, "fix/src.txt"), "hello");
    assert_eq!(
        failed.of(ADDR).fixes,
        Some(FailureFixes::NotWritten(FixesNotWritten::HighExit(137))),
        "{}",
        failed.error
    );
    Ok(())
}

/// The moved-source guard holds on failure exactly as on success (I5): the
/// newer bytes survive, and the guard's error is reported beside the tool's.
#[tokio::test]
async fn write_back_on_failure_respects_moved_source_guard() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let src = ws.dir.path().join("fix/src.txt");
    // Absolute path: an external edit landing mid-run (see
    // `in_place_refuses_to_write_back_over_a_changed_source`).
    ws.write_build_file(
        "fix",
        &failing_build_file(
            "in_place",
            &format!(
                "printf 'edited by someone else' > {}; exit 1",
                src.display()
            ),
        ),
    );
    ws.write_file("fix/src.txt", "hello");

    let failed = resolve_failing(&ws.engine, ADDR, &ResultOptions::default()).await?;
    assert_eq!(std::fs::read_to_string(&src)?, "edited by someone else");
    assert!(
        failed.error.contains("changed while it ran"),
        "the guard's error joins the tool's: {}",
        failed.error
    );
    assert_eq!(
        failed.of(ADDR).fixes,
        Some(FailureFixes::NotWritten(FixesNotWritten::MovedSource))
    );
    Ok(())
}

/// The write-back on failure reads the codegen index as a passing one does:
/// a path another target recorded as its `copy` output is left to its owner
/// (the file and its row untouched), while the target's source is fixed. An
/// `in_place` file is a source, so the fixed one is read as source after the
/// failure exactly as after a pass.
#[tokio::test]
async fn write_back_on_failure_leaves_another_targets_recorded_path() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        &format!(
            "{}{}",
            copy_target("gen", "gen.txt", "generated"),
            r#"
target(
    name = "fmt",
    driver = "bash",
    out = ["gen.txt", "src.txt"],
    codegen = "in_place",
    run = "printf FORMATTED > gen.txt; printf FORMATTED > src.txt; exit 1",
)
"#
        ),
    );
    ws.write_file("p/src.txt", "unformatted");
    run_on(&ws.engine, "//p:gen").await?;

    let failed = resolve_failing(&ws.engine, "//p:fmt", &ResultOptions::default()).await?;
    assert_eq!(read(&ws, "p/src.txt"), "FORMATTED", "{}", failed.error);
    assert_eq!(
        read(&ws, "p/gen.txt"),
        "generated",
        "another target's recorded file is its owner's, on failure too"
    );
    assert_eq!(
        failed.of("//p:fmt").fixes,
        written(
            ws.engine.root(),
            &["p/src.txt"],
            Some(slot_dir(&ws.engine, "//p:fmt").join("fixes.txt"))
        )
    );
    let row = must_row(&ws.engine, "p/gen.txt")?;
    assert_eq!(row.addr, "//p:gen", "{row:?}");
    assert_eq!(row.state, RowState::Written, "{row:?}");
    assert_eq!(status(&ws.engine, "p/gen.txt")?.0, StatusKind::Generated);
    assert_eq!(status(&ws.engine, "p/src.txt")?.0, StatusKind::Source);
    Ok(())
}

/// A target switched from `copy` to `in_place` refuses, on failure as on a
/// pass, to rewrite a path recorded as its own `copy` output: the file is
/// untouched and the refusal, naming `codegen keep`, joins the tool's error.
#[tokio::test]
async fn write_back_on_failure_refuses_own_recorded_path() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("t", "src.txt", "generated"));
    run_on(&ws.engine, "//p:t").await?;

    ws.write_build_file(
        "p",
        r#"
target(
    name = "t",
    driver = "bash",
    out = "src.txt",
    codegen = "in_place",
    run = "printf FORMATTED > $OUT; exit 1",
)
"#,
    );
    let e2 = ws.reopen()?;
    let failed = resolve_failing(&e2, "//p:t", &ResultOptions::default()).await?;
    assert_eq!(read(&ws, "p/src.txt"), "generated");
    assert!(
        failed.error.contains("codegen keep") && failed.error.contains("p/src.txt"),
        "the refusal names the path and the way out: {}",
        failed.error
    );
    assert_eq!(failed.of("//p:t").fixes, None);
    assert_eq!(must_row(&e2, "p/src.txt")?.addr, "//p:t");
    Ok(())
}

/// `--frozen` verifies the tree, it never writes it (D11).
#[tokio::test]
async fn frozen_writes_nothing_on_failure() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("fix", &failing_build_file("in_place", "exit 1"));
    ws.write_file("fix/src.txt", "hello");

    let opts = ResultOptions {
        frozen: true,
        ..ResultOptions::default()
    };
    let failed = resolve_failing(&ws.engine, ADDR, &opts).await?;
    assert_eq!(read(&ws, "fix/src.txt"), "hello");
    assert_eq!(
        failed.of(ADDR).fixes,
        Some(FailureFixes::NotWritten(FixesNotWritten::Frozen)),
        "{}",
        failed.error
    );
    Ok(())
}

/// A failed run records no revision, so its write-back files no fixpoint: the
/// next run, over the fixed tree, executes instead of being served from the
/// cache — the lint-fix loop (fail and fix, then pass).
#[tokio::test]
async fn failure_registers_no_fixpoint() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let runs = tempfile::tempdir()?;
    let count = runs.path().join("count");
    // Fails while it changed something, passes once the tree is fixed.
    ws.write_build_file(
        "fix",
        &format!(
            r#"
target(
    name = "upper",
    driver = "bash",
    deps = [file("src.txt")],
    out = "src.txt",
    codegen = "in_place",
    run = "echo run >> {count}; orig=$(cat src.txt); up=$(printf '%s' \"$orig\" | tr a-z A-Z); printf '%s' \"$up\" > $OUT; [ \"$up\" = \"$orig\" ] || exit 1",
)
"#,
            count = count.display()
        ),
    );
    ws.write_file("fix/src.txt", "hello");

    let failed = resolve_failing(&ws.engine, ADDR, &ResultOptions::default()).await?;
    assert_eq!(read(&ws, "fix/src.txt"), "HELLO", "{}", failed.error);

    ws.run(ADDR).await?;
    assert_eq!(
        std::fs::read_to_string(&count)?.lines().count(),
        2,
        "the second run executed: no fixpoint was filed for the failed one"
    );
    Ok(())
}

/// The "unavailable, rebuild" branch of `execute_and_cache` (D15): a hit whose
/// bytes are gone is rebuilt, and when that rebuild fails its fixes go over
/// the tree through the same single-flight cell as a passing write-back —
/// one run, one write, however many output variants of the addr asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unavailable_rebuild_writes_once() -> anyhow::Result<()> {
    let remote = tempfile::tempdir()?;
    let remote_uri = format!("file://{}", remote.path().display());
    // Outside both workspaces, and the same path in both: the run string is
    // hashed, so this is what lets one revision pass in one and fail in the
    // other.
    let side = tempfile::tempdir()?;
    let (flag, count) = (side.path().join("fail"), side.path().join("count"));
    let build = format!(
        r#"
target(
    name = "upper",
    driver = "bash",
    deps = [file("src.txt")],
    out = {{"src": "src.txt"}},
    codegen = "in_place",
    run = "echo run >> {count}; if [ -e {flag} ]; then printf \"FIXED-%s\" $(wc -l < {count}) > $OUT_SRC; exit 1; fi",
)
"#,
        count = count.display(),
        flag = flag.display(),
    );
    let root = tempfile::tempdir()?;
    std::fs::create_dir_all(root.path().join("fix"))?;
    std::fs::write(root.path().join("fix/BUILD"), &build)?;
    std::fs::write(root.path().join("fix/src.txt"), "HELLO")?;

    // A passing run files the revision on the remote...
    let seeder = common::build_engine(root.path(), &remote_uri);
    let rs = seeder.new_state();
    seeder
        .clone()
        .result_addr(
            rs.clone(),
            &parse_addr(ADDR)?,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    common::drain(rs).await;
    drop(seeder);

    // ...whose bytes then cannot be read: the local cache is gone, and the
    // remote still lists its blobs but cannot serve them (a deleted blob
    // reads as a plain miss on a `file://` remote). So the remote hit is
    // taken back and rebuilt, and the rebuild fails.
    std::fs::remove_dir_all(HomeDir::for_tests(root.path()).join("cache"))?;
    let mut unreadable = 0;
    for f in walk(remote.path()) {
        if !f.ends_with("manifest-v2.borsh") {
            let blob = remote.path().join(f);
            std::fs::set_permissions(&blob, std::fs::Permissions::from_mode(0o000))?;
            // Root reads a mode-000 file anyway: the remote would serve the
            // hit, nothing would be rebuilt, and the test would prove nothing.
            assert!(
                std::fs::File::open(&blob).is_err(),
                "{} is still readable at mode 000: running as root?",
                blob.display()
            );
            unreadable += 1;
        }
    }
    assert!(unreadable > 0, "the seeder uploaded no blob");
    std::fs::write(&flag, "")?;

    let engine = common::build_engine(root.path(), &remote_uri);
    let addr = parse_addr(ADDR)?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = engine.new_state_with_events(false, Some(tx));
    let resolve = |outputs: OutputMatcher| {
        let (engine, rs, addr) = (Arc::clone(&engine), Arc::clone(&rs), addr.clone());
        tokio::spawn(async move {
            engine
                .result_addr(rs, &addr, outputs, &ResultOptions::default())
                .await
                .map(drop)
        })
    };
    let all = resolve(OutputMatcher::All);
    let src = resolve(OutputMatcher::Exact(vec!["src".to_string()]));
    assert!(all.await?.is_err(), "the rebuild fails");
    assert!(src.await?.is_err(), "the rebuild fails");
    let failures = rs.take_failures();
    drop(rs);
    let mut rebuilt = 0;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev.kind, BuildEventKind::CacheHitRebuilt { .. }) {
            rebuilt += 1;
        }
    }
    assert_eq!(rebuilt, 1, "the remote hit was taken back and rebuilt");

    assert_eq!(
        std::fs::read_to_string(&count)?.lines().count(),
        2,
        "one seeding run, one rebuild"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("fix/src.txt"))?,
        "FIXED-2",
        "the rebuild's fixes, written once"
    );
    let failed = Failed {
        error: String::new(),
        failures,
    };
    assert_eq!(
        failed.of(ADDR).fixes,
        written(
            engine.root(),
            &["fix/src.txt"],
            Some(slot_dir(&engine, ADDR).join("fixes.txt"))
        )
    );
    Ok(())
}

/// `--shell` with `exit 1` would otherwise write the shell sandbox's edits over
/// the tree (D16).
#[tokio::test]
async fn shell_exit_1_writes_nothing() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("fix", &failing_build_file("in_place", "exit 1"));
    ws.write_file("fix/src.txt", "hello");

    let wrapper: InteractiveWrapper = Arc::new(move |inner: InteractiveInner| {
        Box::pin(async move {
            let mut typed: &[u8] = b"printf SHELL-EDIT > \"$OUT\"\nexit 1\n";
            let (mut out, mut err) = (tokio::io::sink(), tokio::io::sink());
            inner(Some(&mut typed), Some(&mut out), Some(&mut err), None).await
        })
    });
    let opts = ResultOptions {
        shell: true,
        interactive: Some(wrapper),
        ..ResultOptions::default()
    };
    let failed = resolve_failing(&ws.engine, ADDR, &opts).await?;
    assert_eq!(read(&ws, "fix/src.txt"), "hello", "{}", failed.error);
    assert!(
        !files_containing(&ws.engine.home.join("sandbox"), "SHELL-EDIT").is_empty(),
        "the shell did edit its sandbox's copy: this run had fixes to write"
    );
    assert!(
        failed.failures.iter().all(|f| f.fixes.is_none()),
        "{:?}",
        failed.failures.iter().map(|f| &f.fixes).collect::<Vec<_>>()
    );
    Ok(())
}

/// `fixes.txt` in the slot lists every path written, one per line (D21), and
/// the failure carries the same list and where it is. A file the fix left as
/// it was is not written, so not listed.
#[tokio::test]
async fn failure_box_lists_written_files() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "fix",
        r#"
target(
    name = "upper",
    driver = "bash",
    deps = [file("c.txt"), file("a.txt"), file("b.txt"), file("d.txt")],
    out = ["c.txt", "a.txt", "b.txt", "d.txt"],
    codegen = "in_place",
    run = "for f in a b c d; do up=$(tr a-z A-Z < $f.txt); printf '%s' \"$up\" > $f.txt; done; exit 1",
)
"#,
    );
    for (f, s) in [("a", "a"), ("b", "b"), ("c", "c"), ("d", "D")] {
        ws.write_file(&format!("fix/{f}.txt"), s);
    }

    let failed = resolve_failing(&ws.engine, ADDR, &ResultOptions::default()).await?;
    let want = ["fix/a.txt", "fix/b.txt", "fix/c.txt"];
    let list = slot_dir(&ws.engine, ADDR).join("fixes.txt");
    assert_eq!(
        failed.of(ADDR).fixes,
        written(ws.engine.root(), &want, Some(list.clone())),
        "{}",
        failed.error
    );
    assert_eq!(
        std::fs::read_to_string(&list)?.lines().collect::<Vec<_>>(),
        want,
        "fixes.txt: every written path, one per line"
    );
    for f in ["a", "b", "c", "d"] {
        assert_eq!(read(&ws, &format!("fix/{f}.txt")), f.to_uppercase());
    }
    Ok(())
}

/// Bare `out` and bare `failure_out` on an `in_place` target share the default
/// group: the fixes reach the tree, the report reaches the slot, and neither
/// lands in the other's place.
#[tokio::test]
async fn bare_out_and_failure_out_do_not_collide() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "fix",
        r#"
target(
    name = "upper",
    driver = "bash",
    deps = [file("src.txt")],
    out = "src.txt",
    failure_out = "report.txt",
    codegen = "in_place",
    run = "up=$(tr a-z A-Z < src.txt); printf '%s' \"$up\" > $OUT; echo report-marker > report.txt; exit 1",
)
"#,
    );
    ws.write_file("fix/src.txt", "hello");

    let failed = resolve_failing(&ws.engine, ADDR, &ResultOptions::default()).await?;
    assert_eq!(read(&ws, "fix/src.txt"), "HELLO", "{}", failed.error);
    assert!(
        !ws.dir.path().join("fix/report.txt").exists(),
        "the report is not written over the tree"
    );
    let out = slot_dir(&ws.engine, ADDR).join("out");
    assert_eq!(
        walk(&out),
        ["fix/report.txt"],
        "only the report is kept in the slot"
    );
    assert_eq!(
        std::fs::read_to_string(out.join("fix/report.txt"))?,
        "report-marker\n"
    );
    let f = failed.of(ADDR);
    assert_eq!(f.outputs.as_deref(), Some(out.as_path()));
    assert_eq!(
        f.fixes,
        written(
            ws.engine.root(),
            &["fix/src.txt"],
            Some(slot_dir(&ws.engine, ADDR).join("fixes.txt"))
        )
    );
    Ok(())
}

/// A `failure_out` path inside an `in_place` output directory (I1): the report
/// is kept in the slot and left out of the write-back, the fixes beside it in
/// the same directory are written over the tree, and the slot holds none of
/// the fixes.
#[tokio::test]
async fn failure_out_inside_in_place_output() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "fix",
        r#"
target(
    name = "upper",
    driver = "bash",
    deps = [file("gen/a.txt")],
    out = "gen/",
    failure_out = "gen/report.log",
    codegen = "in_place",
    run = "up=$(tr a-z A-Z < gen/a.txt); printf '%s' \"$up\" > gen/a.txt; echo report-marker > gen/report.log; exit 1",
)
"#,
    );
    ws.write_file("fix/gen/a.txt", "hello");

    let failed = resolve_failing(&ws.engine, ADDR, &ResultOptions::default()).await?;
    assert_eq!(read(&ws, "fix/gen/a.txt"), "HELLO", "{}", failed.error);
    let out = slot_dir(&ws.engine, ADDR).join("out");
    assert_eq!(
        walk(&out),
        ["fix/gen/report.log"],
        "the slot keeps the report, not the fixes"
    );
    assert_eq!(
        std::fs::read_to_string(out.join("fix/gen/report.log"))?,
        "report-marker\n"
    );
    assert!(
        !ws.dir.path().join("fix/gen/report.log").exists(),
        "a failure_out match is never written back over the tree"
    );
    assert_eq!(
        failed.of(ADDR).fixes,
        written(
            ws.engine.root(),
            &["fix/gen/a.txt"],
            Some(slot_dir(&ws.engine, ADDR).join("fixes.txt"))
        ),
        "the report is not listed as a fix"
    );
    Ok(())
}

/// A run stopped by its own `timeout` was killed mid-write like a signalled
/// one: the fix it already made never reaches the tree, and the failure does
/// not report one written.
#[tokio::test]
async fn timeout_writes_nothing_back() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "fix",
        r#"
target(
    name = "upper",
    driver = "bash",
    deps = [file("src.txt")],
    out = "src.txt",
    codegen = "in_place",
    timeout = 1,
    run = "up=$(tr a-z A-Z < src.txt); printf '%s' \"$up\" > $OUT; sleep 60",
)
"#,
    );
    ws.write_file("fix/src.txt", "hello");

    let failed = resolve_failing(&ws.engine, ADDR, &ResultOptions::default()).await?;
    assert!(
        failed.error.contains("timed out after 1s"),
        "{}",
        failed.error
    );
    assert!(
        !files_containing(&ws.engine.home.join("sandbox"), "HELLO").is_empty(),
        "the run did make its fix in the sandbox before the timeout"
    );
    assert_eq!(read(&ws, "fix/src.txt"), "hello", "{}", failed.error);
    let fixes = &failed.of(ADDR).fixes;
    assert!(
        !matches!(fixes, Some(FailureFixes::Written { .. })),
        "{fixes:?}"
    );
    assert!(
        !slot_dir(&ws.engine, ADDR).join("fixes.txt").exists(),
        "no fixes.txt for a run that wrote nothing"
    );
    Ok(())
}
