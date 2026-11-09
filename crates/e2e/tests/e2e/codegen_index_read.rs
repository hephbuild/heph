//! The codegen file index's read side, end to end: what fs globs, the fs
//! `file` and `copy_output` targets and `heph.fs.glob` answer for a path the
//! index records (spec: codegen file index — Readers, Decisions 18, 25, 26,
//! E14, E15, Lifecycle rows 5, 11–17, 19, 21).
//!
//! Rows are written by real `copy` write-backs where the test is about what a
//! write-back leaves behind, and seeded through the store (another process's
//! commit, as the engine sees it) where it is about one row state.

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::codegen_index_write::{
    capture_logs, capture_warnings, copy_target, must_row, read, row, run_on, status, store,
    write_section,
};
use crate::common;

use common::Workspace;
use heph::engine::event::BuildEventKind;
use heph::engine::fault_provider::{FaultProvider, Faults};
use heph::engine::{Config, Engine, OutputMatcher, ResultOptions};
use heph::htaddr::{Addr, parse_addr};
use heph::htwalk::codegen_index::{Row, RowState, StatusKind, StatusReason};
use heph::pluginfs::{copy_output_addr, file_addr, glob_addr};
use heph::{pluginbuildfile, pluginexec};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::filter::LevelFilter;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// What one request resolving `addr` returned: the artifact paths, or the
/// failure as `heph run` renders it, and every event the request emitted.
async fn resolve(
    engine: &Arc<Engine>,
    addr: &Addr,
) -> (anyhow::Result<Vec<String>>, Vec<BuildEventKind>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = engine.new_state_with_events(false, Some(tx));
    let res = Arc::clone(engine)
        .result_addr(
            Arc::clone(&rs),
            addr,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await;
    let res = match res {
        Ok(r) => Ok(common::artifact_paths(&r)
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()),
        Err(err) => {
            let failures = rs.take_failures();
            if failures.is_empty() {
                Err(err)
            } else {
                let msg: Vec<String> = failures
                    .iter()
                    .map(|f| format!("{}: {:#}", f.addr.format(), f.source))
                    .collect();
                Err(anyhow::anyhow!("{}", msg.join("\n")))
            }
        }
    };
    common::drain(rs).await;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev.kind);
    }
    (res, events)
}

/// The artifacts of the fs `file` target over `rel`.
async fn fs_file(engine: &Arc<Engine>, rel: &str) -> anyhow::Result<Vec<String>> {
    resolve(engine, &file_addr(rel)).await.0
}

/// The files a glob over `pattern` returns.
async fn glob(engine: &Arc<Engine>, pattern: &str) -> anyhow::Result<Vec<String>> {
    resolve(engine, &glob_addr(pattern, &[])).await.0
}

/// The error `fs_file` must fail with, rendered.
async fn fs_file_err(engine: &Arc<Engine>, rel: &str) -> String {
    let res = fs_file(engine, rel).await;
    let err = res.expect_err(&format!("the fs file target over {rel} resolved"));
    format!("{err:#}")
}

/// Ends in `name` (artifact paths are root- or package-relative depending
/// on the producer; the file name is what the tests compare).
fn names(paths: &[String]) -> Vec<String> {
    let mut v: Vec<String> = paths
        .iter()
        .map(|p| {
            Path::new(p)
                .file_name()
                .map_or_else(|| p.clone(), |n| n.to_string_lossy().into_owned())
        })
        .collect();
    v.sort();
    v
}

/// Commit `row` as another process would: through its own store handle.
fn put_row(engine: &Engine, row: &Row) -> anyhow::Result<()> {
    let s = store(engine)?.ok_or_else(|| anyhow::anyhow!("no db yet"))?;
    s.write(|txn| txn.put(row))?;
    Ok(())
}

/// Delete `rel`'s row as `codegen keep` does.
fn delete_row(engine: &Engine, rel: &str) -> anyhow::Result<()> {
    let r = must_row(engine, rel)?;
    let s = store(engine)?.ok_or_else(|| anyhow::anyhow!("no db yet"))?;
    s.write(|txn| txn.delete(rel.as_bytes(), &r.addr))?;
    Ok(())
}

/// An engine over `root` whose first provider misbehaves as `faults` says,
/// in front of the buildfile provider.
fn faulty_engine(root: &Path, faults: Faults) -> anyhow::Result<Arc<Engine>> {
    let mut e = Engine::new(Config::for_tests(root.to_path_buf()))?;
    let provider = FaultProvider::new(vec![], faults)?;
    e.register_provider(move |_| Box::new(provider))?;
    let r = root.to_path_buf();
    e.register_provider(move |init| {
        Box::new(pluginbuildfile::Provider::new(
            r,
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))?;
    Ok(Arc::new(e))
}

/// `//p:gen` writes `p/gen.txt` and records it: the starting point of most
/// tests below.
async fn generated(ws: &Workspace) -> anyhow::Result<()> {
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await
}

const GEN_ERR: &str = "p/gen.txt is generated by //p:gen; depend on //p:gen, not the file (heph tool codegen why p/gen.txt)";

// ---------------------------------------------------------------------------
// The fs `file` target (Decision 18)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fs_file_of_generated_path_errors_with_why() -> anyhow::Result<()> {
    let ws = Workspace::new();
    generated(&ws).await?;
    let err = fs_file_err(&ws.engine, "p/gen.txt").await;
    assert!(err.contains(GEN_ERR), "{err}");
    Ok(())
}

#[tokio::test]
async fn fs_file_of_pending_path_errors() -> anyhow::Result<()> {
    let ws = Workspace::new();
    generated(&ws).await?;
    let mut r = must_row(&ws.engine, "p/gen.txt")?;
    r.pending_hash = Some("in-flight".to_string());
    put_row(&ws.engine, &r)?;
    assert_eq!(
        status(&ws.engine, "p/gen.txt")?,
        (StatusKind::Generated, Some(StatusReason::Pending))
    );
    let err = fs_file_err(&ws.engine, "p/gen.txt").await;
    assert!(err.contains(GEN_ERR), "{err}");
    Ok(())
}

#[tokio::test]
async fn fs_file_of_modified_path_errors() -> anyhow::Result<()> {
    let ws = Workspace::new();
    write_section(&ws, &["# //p:gen", "/p/gen.txt"]);
    generated(&ws).await?;
    ws.write_file("p/gen.txt", "hand edit");
    assert_eq!(status(&ws.engine, "p/gen.txt")?.0, StatusKind::Modified);
    let err = fs_file_err(&ws.engine, "p/gen.txt").await;
    assert!(err.contains(GEN_ERR), "{err}");
    Ok(())
}

/// E14: zero artifacts and a warning naming the line, never "generated by".
#[tokio::test(flavor = "current_thread")]
async fn fs_file_of_unrecorded_path_warns_with_section_line() -> anyhow::Result<()> {
    let ws = Workspace::new();
    write_section(&ws, &["# //p:gen", "/p/x.txt"]);
    ws.write_file("p/x.txt", "x");
    let (res, log) = capture_warnings(fs_file(&ws.engine, "p/x.txt")).await;
    assert_eq!(res?, Vec::<String>::new(), "excluded: zero artifacts");
    assert!(
        log.contains("p/x.txt matches section line `.gitignore:3 /p/x.txt` (# //p:gen) and is excluded; depend on //p:gen, or drop the line with heph tool codegen keep"),
        "{log}"
    );
    assert!(!log.contains("generated by"), "{log}");
    Ok(())
}

/// A row whose path a present section no longer lists is the file again.
#[tokio::test]
async fn fs_file_of_released_path_resolves() -> anyhow::Result<()> {
    let ws = Workspace::new();
    write_section(&ws, &["# //p:gen", "/p/gen.txt"]);
    generated(&ws).await?;
    assert!(must_row(&ws.engine, "p/gen.txt")?.seen_in_section);
    write_section(&ws, &["/elsewhere/"]);
    assert_eq!(status(&ws.engine, "p/gen.txt")?.0, StatusKind::Released);
    assert_eq!(names(&fs_file(&ws.engine, "p/gen.txt").await?), ["gen.txt"]);
    Ok(())
}

/// fs targets are uncached: after `codegen keep` (the row deleted), the next
/// run on the same engine resolves the file instead of replaying the error.
#[tokio::test]
async fn fs_file_error_not_served_from_cache() -> anyhow::Result<()> {
    let ws = Workspace::new();
    generated(&ws).await?;
    fs_file_err(&ws.engine, "p/gen.txt").await;
    delete_row(&ws.engine, "p/gen.txt")?;
    assert_eq!(names(&fs_file(&ws.engine, "p/gen.txt").await?), ["gen.txt"]);
    Ok(())
}

#[tokio::test]
async fn fs_file_of_dropped_row_names_keep_and_discard() -> anyhow::Result<()> {
    let ws = Workspace::new();
    generated(&ws).await?;
    let mut r = must_row(&ws.engine, "p/gen.txt")?;
    r.state = RowState::Dropped;
    put_row(&ws.engine, &r)?;
    let err = fs_file_err(&ws.engine, "p/gen.txt").await;
    assert!(
        err.contains(
            "p/gen.txt was generated by //p:gen, which no longer produces it; keep it with \
             heph tool codegen keep p/gen.txt, or delete it with heph tool codegen discard p/gen.txt"
        ),
        "{err}"
    );
    assert!(!err.contains("depend on"), "{err}");
    Ok(())
}

#[tokio::test]
async fn fs_file_of_removed_target_names_keep_and_discard() -> anyhow::Result<()> {
    let ws = Workspace::new();
    generated(&ws).await?;
    ws.write_build_file("p", "");
    let e2 = ws.reopen()?;
    let err = fs_file_err(&e2, "p/gen.txt").await;
    assert!(
        err.contains(
            "p/gen.txt was generated by //p:gen, which no longer exists; keep it with \
             heph tool codegen keep p/gen.txt, or delete it with heph tool codegen discard p/gen.txt"
        ),
        "{err}"
    );
    assert!(!err.contains("depend on"), "{err}");
    Ok(())
}

/// The removed-target lookup runs on the fs target's failure path. When the
/// generator's own `get` builds that fs target (the go provider's `_golist`
/// shape), the lookup must not wait on the run it is rewording.
#[tokio::test]
async fn generator_lookup_never_waits_on_the_failing_fs_target() -> anyhow::Result<()> {
    let ws = Workspace::new();
    generated(&ws).await?;
    let e2 = faulty_engine(
        ws.dir.path(),
        Faults {
            builds: vec![(parse_addr("//p:gen")?, file_addr("p/gen.txt"))],
            ..Default::default()
        },
    )?;
    let res = tokio::time::timeout(Duration::from_secs(60), run_on(&e2, "//p:gen"))
        .await
        .expect("the generator lookup waited on the fs target it was rewording");
    let err = format!(
        "{:#}",
        res.expect_err("the generator's get builds a refused file")
    );
    assert!(err.contains(GEN_ERR), "{err}");
    Ok(())
}

const USE: &str = r#"
target(
    name = "use",
    driver = "bash",
    deps = [file("gen.txt")],
    run = "cat gen.txt > $OUT",
    out = "use.txt",
)
"#;

/// Within one request, an fs target that runs before the generator's
/// write-back reads the bytes on disk; the next run fails. The order is forced
/// by making the generator depend on the reader.
#[tokio::test]
async fn fs_file_before_generator_write_back_resolves_then_fails_next_run() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("p/gen.txt", "old");
    let gen_after_use = r#"
target(
    name = "gen",
    driver = "bash",
    deps = [":use"],
    run = "printf v1 > $OUT",
    out = "gen.txt",
    codegen = "copy",
)
"#;
    ws.write_build_file("p", &format!("{gen_after_use}{USE}"));
    run_on(&ws.engine, "//p:gen").await?;
    assert_eq!(read(&ws, "p/use.txt"), None, "use is not a copy target");
    assert_eq!(must_row(&ws.engine, "p/gen.txt")?.addr, "//p:gen");

    let e2 = ws.reopen()?;
    let err = format!("{:#}", run_on(&e2, "//p:use").await.expect_err("next run"));
    assert!(err.contains(GEN_ERR), "{err}");
    Ok(())
}

/// Within one request, an fs target that runs after the generator's
/// write-back fails. The order is forced by a provider whose `get` of the
/// reader builds the generator first.
#[tokio::test]
async fn fs_file_after_generator_write_back_fails() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        &format!("{}{USE}", copy_target("gen", "gen.txt", "v1")),
    );
    let e2 = faulty_engine(
        ws.dir.path(),
        Faults {
            builds: vec![(parse_addr("//p:use")?, parse_addr("//p:gen")?)],
            ..Default::default()
        },
    )?;
    let err = format!(
        "{:#}",
        run_on(&e2, "//p:use").await.expect_err("same request")
    );
    assert!(err.contains(GEN_ERR), "{err}");
    assert_eq!(read(&ws, "p/gen.txt").as_deref(), Some("v1"));
    Ok(())
}

// ---------------------------------------------------------------------------
// Strict elsewhere, lenient `copy_output` (Decisions 25, 26; E15)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn user_build_file_addr_is_strict() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        &format!("{}{USE}", copy_target("gen", "gen.txt", "v1")),
    );
    run_on(&ws.engine, "//p:gen").await?;
    let e2 = ws.reopen()?;
    let err = format!("{:#}", run_on(&e2, "//p:use").await.expect_err("strict"));
    assert!(err.contains(GEN_ERR), "{err}");
    Ok(())
}

/// An `in_place` target's own output is read through the strict `file`
/// target: a path another target's write-back recorded fails loudly.
#[tokio::test]
async fn in_place_file_output_addr_is_strict() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        &format!(
            "{}{}",
            copy_target("gen", "gen.txt", "v1"),
            r#"
target(
    name = "ip",
    driver = "bash",
    deps = ["//@heph/introspect:outputs"],
    run = "printf ip > $OUT",
    out = "gen.txt",
    codegen = "in_place",
)
"#
        ),
    );
    run_on(&ws.engine, "//p:gen").await?;
    let e2 = ws.reopen()?;
    let res = run_on(&e2, "//p:ip").await;
    let err = format!("{:#}", res.expect_err("in_place reads its output strictly"));
    assert!(err.contains(GEN_ERR), "{err}");
    Ok(())
}

/// A `copy` target that reads its own outputs: an excluded previous output is
/// zero artifacts, so the sandbox never sees the bytes it wrote last time.
const SELF_READ: &str = r#"
target(
    name = "gen",
    driver = "bash",
    deps = ["//@heph/introspect:outputs"],
    run = "saw=$(cat gen.txt 2>/dev/null || echo none); rm -f gen.txt; printf '%s saw %s' {tag} \"$saw\" > $OUT",
    out = "gen.txt",
    codegen = "copy",
)
"#;

#[tokio::test]
async fn copy_output_never_sources_excluded_bytes() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("p/gen.txt", "old");
    ws.write_build_file("p", &SELF_READ.replace("{tag}", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    assert_eq!(
        read(&ws, "p/gen.txt").as_deref(),
        Some("v1 saw old"),
        "an unrecorded file is source"
    );
    ws.write_build_file("p", &SELF_READ.replace("{tag}", "v2"));
    let e2 = ws.reopen()?;
    run_on(&e2, "//p:gen").await?;
    assert_eq!(
        read(&ws, "p/gen.txt").as_deref(),
        Some("v2 saw none"),
        "the recorded previous output is absent from the inputs"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn introspect_outputs_over_copy_output_is_zero_artifacts_without_error_or_warning()
-> anyhow::Result<()> {
    let ws = Workspace::new();
    write_section(&ws, &["# //p:gen", "/p/gen.txt"]);
    ws.write_file("p/gen.txt", "old");
    ws.write_build_file("p", &SELF_READ.replace("{tag}", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    // Edited, under a line: `Modified`, which `file` would refuse and warn on.
    ws.write_file("p/gen.txt", "hand edit");
    let ((res, _), log) =
        capture_warnings(resolve(&ws.engine, &copy_output_addr("p/gen.txt"))).await;
    assert_eq!(res?, Vec::<String>::new());
    assert!(log.is_empty(), "no warning: {log}");
    Ok(())
}

/// `copy_output` has its own def-hash tag; `file`'s key is the one it had
/// before the codegen index (pinned at 5375abba).
#[tokio::test]
async fn copy_output_leaves_strict_file_keys_unchanged() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("p/a.txt", "a");
    let def_hash = |addr: Addr| {
        let engine = Arc::clone(&ws.engine);
        async move {
            let def = Arc::clone(&engine)
                .get_def(engine.new_state(), &addr)
                .await?;
            anyhow::Ok(String::from_utf8(def.target_def.hash.clone())?)
        }
    };
    let file = def_hash(file_addr("p/a.txt")).await?;
    let lenient = def_hash(copy_output_addr("p/a.txt")).await?;
    assert_eq!(file, FILE_DEF_HASH_AT_5375ABBA, "the strict file key moved");
    assert_ne!(file, lenient);
    Ok(())
}

/// `//@heph/fs:file@f=p/a.txt`'s def hash before the codegen index.
const FILE_DEF_HASH_AT_5375ABBA: &str = "c4241918a5808fa";

// ---------------------------------------------------------------------------
// Globs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn glob_after_write_back_in_same_process_excludes_without_section() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("p/a.txt", "a");
    generated(&ws).await?;
    assert_eq!(names(&glob(&ws.engine, "p/*.txt").await?), ["a.txt"]);
    Ok(())
}

/// BUILD `file()` builds the addr only: the def hash is the same with and
/// without rows, and evaluation succeeds with an index every lookup of which
/// fails. The failing index is a root `.gitignore` that is a directory: every
/// snapshot reads that file before it answers anything, so every lookup (the
/// fs `file` and glob targets', shown failing here) errors.
#[tokio::test]
async fn file_builtin_never_reads_the_index() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        &format!("{}{USE}", copy_target("gen", "gen.txt", "v1")),
    );
    ws.write_file("p/gen.txt", "old");
    let def_hash = |engine: Arc<Engine>| async move {
        let def = Arc::clone(&engine)
            .get_def(engine.new_state(), &parse_addr("//p:use")?)
            .await?;
        anyhow::Ok(def.target_def.hash.clone())
    };
    let before = def_hash(ws.reopen()?).await?;
    run_on(&ws.engine, "//p:gen").await?;
    assert_eq!(
        def_hash(ws.reopen()?).await?,
        before,
        "rows moved the def hash"
    );

    // Every snapshot reads the root `.gitignore` first; as a directory it
    // cannot be read, so every lookup fails.
    std::fs::create_dir(ws.dir.path().join(".gitignore"))?;
    let broken = ws.reopen()?;
    let lookup = fs_file(&broken, "p/gen.txt").await;
    assert!(
        format!("{:#}", lookup.expect_err("the index is unreadable")).contains("codegen index"),
    );
    let lookup = glob(&broken, "p/*.txt").await;
    assert!(
        format!("{:#}", lookup.expect_err("the index is unreadable")).contains("codegen index"),
    );
    assert_eq!(def_hash(broken).await?, before, "evaluation read the index");
    Ok(())
}

/// `heph.fs.glob` reads the same index snapshot as the fs glob target,
/// without executing anything.
#[tokio::test]
async fn eval_glob_reads_index_without_engine_call() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("p/a.txt", "a");
    ws.write_build_file(
        "p",
        &format!(
            "{}\ntarget(name = \"t\", driver = \"bash\", run = \"true\", srcs = heph.fs.glob(\"*.txt\"))\n",
            copy_target("gen", "gen.txt", "v1")
        ),
    );
    run_on(&ws.engine, "//p:gen").await?;
    let e2 = ws.reopen()?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = e2.new_state_with_events(false, Some(tx));
    let spec = Arc::clone(&e2)
        .get_spec(Arc::clone(&rs), &parse_addr("//p:t")?)
        .await?;
    common::drain(rs).await;
    let srcs = format!("{:?}", spec.spec.config.get("srcs"));
    assert!(
        srcs.contains("a.txt") && !srcs.contains("gen.txt"),
        "{srcs}"
    );
    while let Ok(ev) = rx.try_recv() {
        assert!(
            !matches!(ev.kind, BuildEventKind::ExecuteStart { .. }),
            "evaluation executed a target: {:?}",
            ev.kind
        );
    }
    Ok(())
}

/// A glob under one package loads that package's rows from the index, not
/// the whole index; a root glob loads everything.
#[tokio::test(flavor = "current_thread")]
async fn glob_loads_only_its_directory() -> anyhow::Result<()> {
    let ws = Workspace::new();
    write_section(&ws, &["# //p:gen", "/p/gen.txt", "# //q:gen", "/q/gen.txt"]);
    for pkg in ["p", "q"] {
        ws.write_build_file(pkg, &copy_target("gen", "gen.txt", "v1"));
        run_on(&ws.engine, &format!("//{pkg}:gen")).await?;
    }
    let engine = ws.reopen()?;
    assert!(glob(&engine, "p/*.txt").await?.is_empty());
    let st = engine.codegen_index().stats();
    assert_eq!((st.region_loads, st.reloads, st.rows_loaded), (1, 0, 1));
    assert!(glob(&engine, "**/gen.txt").await?.is_empty());
    assert_eq!(engine.codegen_index().stats().reloads, 1);
    Ok(())
}

/// The glob's count names its reasons.
#[tokio::test(flavor = "current_thread")]
async fn glob_exclusion_count_names_reasons() -> anyhow::Result<()> {
    let ws = Workspace::new();
    write_section(&ws, &["# //p:gen", "/p/gen.txt", "# //p:other", "/p/x.txt"]);
    ws.write_file("p/x.txt", "unrecorded");
    ws.write_file("p/a.txt", "source");
    ws.write_build_file(
        "p",
        &format!(
            "{}{}",
            copy_target("gen", "gen.txt", "v1"),
            copy_target("ed", "ed.txt", "v1")
        ),
    );
    run_on(&ws.engine, "//p:gen").await?;
    run_on(&ws.engine, "//p:ed").await?;
    write_section(
        &ws,
        &[
            "# //p:gen",
            "/p/gen.txt",
            "# //p:other",
            "/p/x.txt",
            "# //p:ed",
            "/p/ed.txt",
        ],
    );
    ws.write_file("p/ed.txt", "hand edit");
    let (res, log) = capture_logs(LevelFilter::DEBUG, glob(&ws.engine, "p/*.txt")).await;
    assert_eq!(names(&res?), ["a.txt"]);
    assert!(
        // A glob does not verify a row under a line: the edited one counts
        // as recorded.
        log.contains("excluded 3 generated files (2 recorded, 1 unrecorded)"),
        "{log}"
    );
    Ok(())
}

/// Excluding a generated file prints nothing at `info`: the per-path line
/// is `debug`, and a request logs no count when it ends.
#[tokio::test(flavor = "current_thread")]
async fn glob_exclusions_silent_at_info() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("p/a.txt", "a");
    generated(&ws).await?;
    let (res, log) = capture_logs(LevelFilter::INFO, async {
        let rs = ws.engine.new_state();
        let res = Arc::clone(&ws.engine)
            .result_addr(
                Arc::clone(&rs),
                &glob_addr("p/*.txt", &[]),
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await;
        common::drain(rs).await;
        res
    })
    .await;
    res?;
    assert!(!log.contains("excluded"), "{log}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Lifecycle rows
// ---------------------------------------------------------------------------

/// Row 12: a glob excludes an edited generated file without verifying it,
/// so without a warning and as `recorded`; a point lookup (`why`, `status`)
/// still tells it is `Modified`.
#[tokio::test(flavor = "current_thread")]
async fn edited_generated_file_excluded_by_glob_unverified() -> anyhow::Result<()> {
    let ws = Workspace::new();
    write_section(&ws, &["# //p:gen", "/p/gen.txt"]);
    generated(&ws).await?;
    ws.write_file("p/gen.txt", "hand edit");
    let ((res, events), log) =
        capture_warnings(resolve(&ws.engine, &glob_addr("p/*.txt", &[]))).await;
    assert_eq!(res?, Vec::<String>::new());
    assert!(log.is_empty(), "no warning: {log}");
    assert!(
        events.iter().any(|e| matches!(
            e,
            BuildEventKind::CodegenExcluded { path, status, reason, .. }
                if path == "p/gen.txt" && status == "generated" && reason.as_deref() == Some("recorded")
        )),
        "{events:?}"
    );
    assert_eq!(
        status(&ws.engine, "p/gen.txt")?,
        (StatusKind::Modified, Some(StatusReason::Edited))
    );
    Ok(())
}

/// Rows 5, 6, 10: an output removed from its target stays excluded (its row
/// `dropped`, the file kept) until it is kept or discarded.
#[tokio::test]
async fn removed_output_excluded_until_gitignore() -> anyhow::Result<()> {
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
    ws.write_build_file("p", &copy_target("gen", "a.txt", "a"));
    let e2 = ws.reopen()?;
    run_on(&e2, "//p:gen").await?;
    assert_eq!(read(&ws, "p/b.txt").as_deref(), Some("b"), "kept");
    assert_eq!(must_row(&e2, "p/b.txt")?.state, RowState::Dropped);
    assert_eq!(glob(&e2, "p/*.txt").await?, Vec::<String>::new());
    Ok(())
}

/// Row 11: a broken BUILD file in the generator's package changes nothing a
/// glob answers.
#[tokio::test]
async fn broken_build_file_does_not_fail_glob() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("p/a.txt", "a");
    generated(&ws).await?;
    ws.write_build_file("p", "this is not starlark(");
    let e2 = ws.reopen()?;
    assert_eq!(names(&glob(&e2, "p/*.txt").await?), ["a.txt"]);
    Ok(())
}

/// Rows 13, 16: a deleted generated file leaves a dangling row that changes
/// no answer.
#[tokio::test]
async fn dangling_rows_change_no_answer() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("p/a.txt", "a");
    generated(&ws).await?;
    std::fs::remove_file(ws.dir.path().join("p/gen.txt"))?;
    assert!(row(&ws.engine, "p/gen.txt")?.is_some(), "dangling");
    assert_eq!(names(&glob(&ws.engine, "p/*.txt").await?), ["a.txt"]);
    let err = fs_file_err(&ws.engine, "p/gen.txt").await;
    assert!(
        !err.contains("generated by"),
        "a missing file is missing: {err}"
    );
    Ok(())
}

/// Row 14: on a branch where the path is tracked source (other bytes, no
/// section line), it is source; the row stays until the next write-back.
#[tokio::test]
async fn branch_switch_to_tracked_file_is_source() -> anyhow::Result<()> {
    let ws = Workspace::new();
    generated(&ws).await?;
    ws.write_file("p/gen.txt", "git's file");
    assert_eq!(names(&glob(&ws.engine, "p/*.txt").await?), ["gen.txt"]);
    assert_eq!(names(&fs_file(&ws.engine, "p/gen.txt").await?), ["gen.txt"]);
    assert!(row(&ws.engine, "p/gen.txt")?.is_some(), "row kept");
    Ok(())
}

/// Row 15: on a branch without the target, whose section omits the path, the
/// kept file is released.
#[tokio::test]
async fn branch_without_target_releases() -> anyhow::Result<()> {
    let ws = Workspace::new();
    write_section(&ws, &["# //p:gen", "/p/gen.txt"]);
    generated(&ws).await?;
    ws.write_build_file("p", "");
    write_section(&ws, &["/elsewhere/"]);
    let e2 = ws.reopen()?;
    assert_eq!(status(&e2, "p/gen.txt")?.0, StatusKind::Released);
    assert_eq!(names(&glob(&e2, "p/*.txt").await?), ["gen.txt"]);
    Ok(())
}

/// Row 17: another process's write-back is seen by this engine's next glob.
#[tokio::test]
async fn second_process_write_back_seen_by_next_glob() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    ws.write_file("p/gen.txt", "before");
    // A first write-back creates the db, so both engines have it open.
    ws.write_build_file("q", &copy_target("other", "other.txt", "o"));
    run_on(&ws.engine, "//q:other").await?;
    assert_eq!(names(&glob(&ws.engine, "p/*.txt").await?), ["gen.txt"]);
    let other = ws.reopen()?;
    run_on(&other, "//p:gen").await?;
    assert_eq!(glob(&ws.engine, "p/*.txt").await?, Vec::<String>::new());
    Ok(())
}

/// Row 17: a glob running while a write-back is in flight sees its pending
/// row as `Generated` and leaves it pending.
#[tokio::test]
async fn concurrent_glob_during_write_back_keeps_pending_row() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("p/a.txt", "a");
    generated(&ws).await?;
    let mut r = must_row(&ws.engine, "p/gen.txt")?;
    r.pending_hash = Some("in-flight".to_string());
    put_row(&ws.engine, &r)?;
    assert_eq!(names(&glob(&ws.engine, "p/*.txt").await?), ["a.txt"]);
    assert_eq!(
        must_row(&ws.engine, "p/gen.txt")?.pending_hash.as_deref(),
        Some("in-flight"),
        "a reader settles nothing"
    );
    Ok(())
}

/// An engine started before any db existed opens the one another process
/// created, from its next glob.
#[tokio::test]
async fn long_lived_engine_opens_db_created_after_start() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    ws.write_file("p/gen.txt", "before");
    assert!(store(&ws.engine)?.is_none(), "no db yet");
    assert_eq!(names(&glob(&ws.engine, "p/*.txt").await?), ["gen.txt"]);
    let other = ws.reopen()?;
    run_on(&other, "//p:gen").await?;
    assert_eq!(glob(&ws.engine, "p/*.txt").await?, Vec::<String>::new());
    assert_eq!(ws.engine.codegen_index().stats().late_opens, 1);
    Ok(())
}

/// A long-lived engine sees an edit to the root `.gitignore` from its next
/// glob.
#[tokio::test]
async fn gitignore_edit_seen_by_next_glob() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("p/x.txt", "x");
    assert_eq!(names(&glob(&ws.engine, "p/*.txt").await?), ["x.txt"]);
    write_section(&ws, &["# //p:gen", "/p/x.txt"]);
    assert_eq!(glob(&ws.engine, "p/*.txt").await?, Vec::<String>::new());
    Ok(())
}

/// Row 19: with the db lost, a section line still excludes the file.
#[tokio::test]
async fn lost_index_covered_by_section() -> anyhow::Result<()> {
    let ws = Workspace::new();
    write_section(&ws, &["# //p:gen", "/p/gen.txt"]);
    generated(&ws).await?;
    remove_db(&ws)?;
    let e2 = ws.reopen()?;
    assert_eq!(
        status(&e2, "p/gen.txt")?,
        (StatusKind::Generated, Some(StatusReason::Unrecorded))
    );
    assert_eq!(glob(&e2, "p/*.txt").await?, Vec::<String>::new());
    Ok(())
}

/// Row 19, E3: with the db lost and no section, the file is source again
/// (double-sourced until the generator is next resolved).
#[tokio::test]
async fn lost_index_without_section_double_sources() -> anyhow::Result<()> {
    let ws = Workspace::new();
    generated(&ws).await?;
    remove_db(&ws)?;
    let e2 = ws.reopen()?;
    assert_eq!(names(&glob(&e2, "p/*.txt").await?), ["gen.txt"]);
    Ok(())
}

/// Delete the db and its WAL and shared-memory files.
fn remove_db(ws: &Workspace) -> anyhow::Result<()> {
    let db = ws.engine.codegen_index().db_path().to_path_buf();
    for suffix in ["", "-wal", "-shm"] {
        let mut p = db.clone().into_os_string();
        p.push(suffix);
        match std::fs::remove_file(&p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(())
}

/// Row 21: a workspace copied with its home (new inodes) still excludes its
/// generated files, after one hash per file.
#[tokio::test]
async fn copied_workspace_verifies_by_hash_once() -> anyhow::Result<()> {
    let ws = Workspace::new();
    generated(&ws).await?;
    let copy = tempfile::tempdir()?;
    copy_tree(ws.dir.path(), copy.path())?;
    let e2 = engine_over(copy.path())?;
    assert_eq!(
        status(&e2, "p/gen.txt")?,
        (StatusKind::Generated, Some(StatusReason::Recorded)),
        "verified by hash"
    );
    assert_eq!(glob(&e2, "p/*.txt").await?, Vec::<String>::new());
    assert_eq!(glob(&e2, "p/*.txt").await?, Vec::<String>::new());
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        let ty = entry.file_type()?;
        if ty.is_dir() {
            std::fs::create_dir_all(&dest)?;
            copy_tree(&entry.path(), &dest)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

fn engine_over(root: &Path) -> anyhow::Result<Arc<Engine>> {
    let mut e = Engine::new(Config::for_tests(root.to_path_buf()))?;
    let r = root.to_path_buf();
    e.register_provider(move |init| {
        Box::new(pluginbuildfile::Provider::new(
            r,
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))?;
    Ok(Arc::new(e))
}

/// A section line no heph target owns: the build excludes the file silently;
/// it is `codegen status`'s to list.
#[tokio::test(flavor = "current_thread")]
async fn foreign_line_listed_by_status_not_warned_by_build() -> anyhow::Result<()> {
    let ws = Workspace::new();
    write_section(&ws, &["/p/x.txt"]);
    ws.write_file("p/x.txt", "x");
    let (res, log) = capture_warnings(async {
        let file = fs_file(&ws.engine, "p/x.txt").await;
        let glob = glob(&ws.engine, "p/*.txt").await;
        (file, glob)
    })
    .await;
    assert_eq!(res.0?, Vec::<String>::new());
    assert_eq!(res.1?, Vec::<String>::new());
    assert!(log.is_empty(), "no warning for a foreign line: {log}");
    Ok(())
}
