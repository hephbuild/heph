//! `heph tool codegen gitignore`, `status` and `why`, and `heph validate`'s
//! codegen checks, driven in-process against a real engine (spec: codegen
//! file index — Commands, JSON, Lifecycle rows 5–7 and 10, Decisions 17, 20
//! and 21).
//!
//! Each command's logic is the `pub async fn` the CLI runs inside its TUI app;
//! these tests call it directly and read its report (text, `--json`, exit
//! code) and the index. A new engine over the same root (`Workspace::reopen`)
//! is the next `heph` invocation: it re-reads the BUILD files.
//! `keep`/`discard`/`import` are another file's.
#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common;

use anyhow::Context as _;
use common::Workspace;
use heph::commands::tool::codegen::gitignore::{self as gi, GitignoreReport};
use heph::commands::tool::codegen::keep::PathResultKind;
use heph::commands::tool::codegen::status::{self as st, ItemKind, StatusReport};
use heph::commands::tool::codegen::why::{self, WhyReport};
use heph::commands::tool::codegen::{CodegenExit, Report as _, wrap_exit};
use heph::commands::validate::{CodegenFindings, codegen_findings};
use heph::engine::{Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use heph::htwalk::codegen_index::{Row, RowState, StatusKind, Store, StoreOptions};
use heph::htwalk::gitignore_section::{BEGIN_MARKER, END_MARKER};
use std::path::Path;
use std::sync::Arc;
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

/// A `copy` target `gen` writing `a.txt` and `b.txt`.
const TWO_OUTPUTS: &str = r#"
target(
    name = "gen",
    driver = "bash",
    run = "printf a > a.txt; printf b > b.txt",
    out = ["a.txt", "b.txt"],
    codegen = "copy",
)
"#;

/// A BUILD file that fails to evaluate.
const BROKEN_BUILD: &str = "target(name = \n";

/// The section text an older heph wrote its BEGIN line as.
const OLD_BEGIN: &str =
    "# BEGIN heph-generated (managed by `heph tool gen-gitignore` — do not edit)";

/// Build `addr` (all outputs) on `engine`, write-back included.
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

async fn gitignore_with(
    engine: &Arc<Engine>,
    check: bool,
    matcher: Option<Matcher>,
) -> anyhow::Result<GitignoreReport> {
    let rs = engine.new_state();
    gi::gitignore(engine, &rs, &gi::Options { check, matcher }).await
}

async fn gitignore(engine: &Arc<Engine>) -> anyhow::Result<GitignoreReport> {
    gitignore_with(engine, false, None).await
}

async fn gitignore_check(engine: &Arc<Engine>) -> anyhow::Result<GitignoreReport> {
    gitignore_with(engine, true, None).await
}

async fn status_with(engine: &Arc<Engine>, opts: st::Options) -> anyhow::Result<StatusReport> {
    let rs = engine.new_state();
    st::status(engine, &rs, opts).await
}

/// `heph tool codegen status` as the CLI runs it: settling.
async fn status(engine: &Arc<Engine>) -> anyhow::Result<StatusReport> {
    status_with(
        engine,
        st::Options {
            check: false,
            settle: true,
            hint: true,
        },
    )
    .await
}

async fn why_in(engine: &Arc<Engine>, cwd: &Path, paths: &[&str]) -> anyhow::Result<WhyReport> {
    let rs = engine.new_state();
    let paths: Vec<String> = paths.iter().map(|p| (*p).to_string()).collect();
    why::why(engine, &rs, &paths, cwd).await
}

async fn validate(engine: &Arc<Engine>) -> CodegenFindings {
    let rs = engine.new_state();
    codegen_findings(engine, &rs, engine.root()).await
}

/// The exit code the CLI ends with: the report's, or 2 for an error (through
/// the same `wrap_exit` every codegen subcommand ends in).
fn exit_of<R: heph::commands::tool::codegen::Report>(res: anyhow::Result<R>) -> u8 {
    match res {
        Ok(r) => r.exit_code(),
        Err(e) => {
            let wrapped = wrap_exit(Err(e)).expect_err("an error stays an error");
            wrapped
                .downcast_ref::<CodegenExit>()
                .map_or(255, |c| c.code)
        }
    }
}

/// The db as another process sees it.
fn store(engine: &Engine) -> anyhow::Result<Option<Store>> {
    let path = engine.codegen_index().db_path().to_path_buf();
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(Store::open(&path, &StoreOptions::default())?))
}

fn must_row(engine: &Engine, rel: &str) -> anyhow::Result<Row> {
    store(engine)?
        .context("no db")?
        .row(rel.as_bytes())?
        .with_context(|| format!("no index row for {rel}"))
}

/// What a fresh snapshot says `rel` is.
fn status_of(engine: &Engine, rel: &str) -> anyhow::Result<StatusKind> {
    let index = engine.codegen_index();
    index.open_existing()?;
    let snap = index.snapshot()?;
    Ok(snap.status_path(&index.root().join(rel), false)?.kind)
}

fn hashout(bytes: &str) -> String {
    heph::htwalk::bytes_hashout(bytes.as_bytes(), false)
}

fn read(ws: &Workspace, rel: &str) -> Option<String> {
    std::fs::read_to_string(ws.dir.path().join(rel)).ok()
}

/// A root `.gitignore` holding `begin`, `lines`, then the END marker.
fn write_section_with(ws: &Workspace, begin: &str, lines: &[&str]) {
    let mut text = format!("{begin}\n");
    for l in lines {
        text.push_str(l);
        text.push('\n');
    }
    text.push_str(END_MARKER);
    text.push('\n');
    ws.write_file(".gitignore", &text);
}

fn write_section(ws: &Workspace, lines: &[&str]) {
    write_section_with(ws, BEGIN_MARKER, lines);
}

/// Insert `line` just above the section's END marker.
fn add_section_line(ws: &Workspace, line: &str) -> anyhow::Result<()> {
    let text = read(ws, ".gitignore").context(".gitignore")?;
    let text = text.replacen(END_MARKER, &format!("{line}\n{END_MARKER}"), 1);
    ws.write_file(".gitignore", &text);
    Ok(())
}

fn kinds(report: &StatusReport) -> Vec<(Option<&str>, ItemKind)> {
    report
        .items
        .iter()
        .map(|i| (i.path.as_deref(), i.kind))
        .collect()
}

// ---------------------------------------------------------------------------
// `gitignore`
// ---------------------------------------------------------------------------

/// It prints one `+` / `-` line per changed pattern with its target, and
/// nothing about the lines that stay.
#[tokio::test]
async fn gitignore_prints_line_diff_only() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        &format!(
            "{}{}",
            copy_target("a", "a.txt", "a"),
            copy_target("b", "b.txt", "b")
        ),
    );
    let first = gitignore(&ws.engine).await?;
    let pm: Vec<String> = first
        .text()
        .into_iter()
        .filter(|l| l.starts_with(['+', '-']))
        .collect();
    assert_eq!(
        pm,
        ["+ /p/a.txt  (//p:a)", "+ /p/b.txt  (//p:b)"],
        "{first:?}"
    );

    // `b` goes, `c` arrives, `a` stays.
    ws.write_build_file(
        "p",
        &format!(
            "{}{}",
            copy_target("a", "a.txt", "a"),
            copy_target("c", "c.txt", "c")
        ),
    );
    let e2 = ws.reopen()?;
    let second = gitignore(&e2).await?;
    let text = second.text();
    let pm: Vec<&String> = text.iter().filter(|l| l.starts_with(['+', '-'])).collect();
    assert_eq!(
        pm,
        ["- /p/b.txt  (//p:b)", "+ /p/c.txt  (//p:c)"],
        "{text:?}"
    );
    assert!(
        !text.iter().any(|l| l.contains("a.txt")),
        "an unchanged line is not printed: {text:?}"
    );
    assert!(
        text.last().is_some_and(|l| l.starts_with("Updated")),
        "{text:?}"
    );
    let file = read(&ws, ".gitignore").context(".gitignore")?;
    assert!(file.contains("/p/a.txt") && file.contains("/p/c.txt"));
    assert!(!file.contains("/p/b.txt"), "{file}");
    Ok(())
}

/// `--check`: clean exits 0, stale exits 1 and writes nothing, an error
/// (a BUILD file that fails to evaluate) is a `CodegenExit` of 2.
#[tokio::test]
async fn gitignore_check_result_kinds() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));

    let stale = gitignore_check(&ws.engine).await?;
    assert!(stale.stale && !stale.written, "{stale:?}");
    assert_eq!(stale.exit_code(), 1);
    assert_eq!(read(&ws, ".gitignore"), None, "--check writes nothing");

    gitignore(&ws.engine).await?;
    let clean = gitignore_check(&ws.engine).await?;
    assert!(!clean.stale, "{clean:?}");
    assert_eq!(clean.exit_code(), 0);
    assert!(
        clean
            .text()
            .last()
            .is_some_and(|l| l.ends_with("already up to date")),
        "{:?}",
        clean.text()
    );

    ws.write_build_file("q", BROKEN_BUILD);
    let e2 = ws.reopen()?;
    let res = gitignore_check(&e2).await;
    assert!(res.is_err(), "a broken BUILD file fails the check: {res:?}");
    assert_eq!(exit_of(res), 2, "an error is not a finding");
    Ok(())
}

/// With a matcher, only the lines of targets under it change, and only the
/// files those lines covered are released.
#[tokio::test]
async fn gitignore_matcher_releases_only_its_lines() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "p"));
    ws.write_build_file("q", &copy_target("gen", "gen.txt", "q"));
    run_on(&ws.engine, "//p:gen").await?;
    run_on(&ws.engine, "//q:gen").await?;
    gitignore(&ws.engine).await?;

    // Both targets removed; only `p` is rewritten.
    ws.write_build_file("p", "");
    ws.write_build_file("q", "");
    let e2 = ws.reopen()?;
    let report =
        gitignore_with(&e2, false, Some(Matcher::PackagePrefix(PkgBuf::from("p")))).await?;
    let lines: Vec<(&str, bool)> = report
        .lines
        .iter()
        .map(|l| (l.pattern.as_str(), l.added))
        .collect();
    assert_eq!(lines, [("/p/gen.txt", false)], "{report:?}");
    let released: Vec<&str> = report.released.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(released, ["p/gen.txt"], "{report:?}");

    let file = read(&ws, ".gitignore").context(".gitignore")?;
    assert!(file.contains("/q/gen.txt"), "q's line stays: {file}");
    assert!(!file.contains("/p/gen.txt"), "{file}");
    assert_eq!(status_of(&e2, "q/gen.txt")?, StatusKind::Generated);
    assert_eq!(status_of(&e2, "p/gen.txt")?, StatusKind::Released);
    Ok(())
}

/// An old BEGIN line is rewritten to today's text, lines unchanged.
#[tokio::test]
async fn gitignore_rewrites_old_marker_text() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    write_section_with(&ws, OLD_BEGIN, &["# //p:gen", "/p/gen.txt"]);

    let report = gitignore(&ws.engine).await?;
    assert!(report.stale && report.written, "{report:?}");
    assert!(report.lines.is_empty(), "no line changes: {report:?}");
    let file = read(&ws, ".gitignore").context(".gitignore")?;
    assert!(file.starts_with(BEGIN_MARKER), "{file}");
    assert!(!file.contains("gen-gitignore"), "{file}");
    assert!(file.contains("# //p:gen\n/p/gen.txt\n"), "{file}");

    let again = gitignore_check(&ws.engine).await?;
    assert!(!again.stale, "{again:?}");
    Ok(())
}

/// `--check` on an old BEGIN line: stale with its own message and its own
/// `--json` reason.
#[tokio::test]
async fn gitignore_check_stale_on_old_marker() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    write_section_with(&ws, OLD_BEGIN, &["# //p:gen", "/p/gen.txt"]);
    let before = read(&ws, ".gitignore");

    let report = gitignore_check(&ws.engine).await?;
    assert_eq!(report.exit_code(), 1);
    let text = report.text().join("\n");
    assert!(text.contains("section marker predates heph"), "{text}");
    assert!(
        text.contains("run heph tool codegen gitignore once and commit"),
        "{text}"
    );
    assert!(!text.contains("out of date"), "lines are current: {text}");
    assert_eq!(report.json()["reasons"], serde_json::json!(["old_marker"]));
    assert_eq!(read(&ws, ".gitignore"), before, "--check writes nothing");
    Ok(())
}

/// `{"schema": 1, "stale", "reasons", "written", "lines": [{op, pattern,
/// addr}], "released": [{path, addr}]}`, arrays sorted.
#[tokio::test]
async fn gitignore_json_schema() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        &format!(
            "{}{}",
            copy_target("z", "z.txt", "z"),
            copy_target("a", "a.txt", "a")
        ),
    );
    write_section(&ws, &["# //p:gone", "/p/gone.txt"]);

    let check = gitignore_check(&ws.engine).await?.json();
    assert_eq!(check["schema"], 1);
    assert_eq!(check["stale"], true);
    assert_eq!(check["written"], false, "--check never writes");
    assert_eq!(check["reasons"], serde_json::json!(["lines"]));

    let doc = gitignore(&ws.engine).await?.json();
    let keys: Vec<&String> = doc.as_object().context("object")?.keys().collect();
    assert_eq!(
        keys,
        ["lines", "reasons", "released", "schema", "stale", "written"],
        "{doc}"
    );
    assert_eq!(doc["written"], true);
    assert_eq!(
        doc["lines"],
        serde_json::json!([
            {"op": "add", "pattern": "/p/a.txt", "addr": "//p:a"},
            {"op": "remove", "pattern": "/p/gone.txt", "addr": "//p:gone"},
            {"op": "add", "pattern": "/p/z.txt", "addr": "//p:z"},
        ]),
        "sorted by pattern: {doc}"
    );
    assert_eq!(doc["released"], serde_json::json!([]));

    let clean = gitignore(&ws.engine).await?.json();
    assert_eq!(clean["stale"], false);
    assert_eq!(clean["reasons"], serde_json::json!([]));
    assert_eq!(clean["lines"], serde_json::json!([]));
    Ok(())
}

/// The rows the written section covers get `seen_in_section`; `--check`
/// marks nothing.
#[tokio::test]
async fn gitignore_sets_seen_flag() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    assert!(!must_row(&ws.engine, "p/gen.txt")?.seen_in_section);

    gitignore_check(&ws.engine).await?;
    assert!(
        !must_row(&ws.engine, "p/gen.txt")?.seen_in_section,
        "--check marks nothing"
    );

    gitignore(&ws.engine).await?;
    assert!(must_row(&ws.engine, "p/gen.txt")?.seen_in_section);
    Ok(())
}

/// A rewrite that drops a line lists every recorded file it releases, with
/// the target that wrote it.
#[tokio::test]
async fn gitignore_lists_released_files() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    gitignore(&ws.engine).await?;

    ws.write_build_file("p", "");
    let e2 = ws.reopen()?;
    let check = gitignore_check(&e2).await?;
    assert_eq!(check.released.len(), 1, "--check lists them too: {check:?}");

    let report = gitignore(&e2).await?;
    let released: Vec<(&str, &str)> = report
        .released
        .iter()
        .map(|r| (r.path.as_str(), r.addr.as_str()))
        .collect();
    assert_eq!(released, [("p/gen.txt", "//p:gen")]);
    assert!(
        report.text().contains(
            &"released p/gen.txt (was //p:gen): now untracked; keep or discard it".to_string()
        ),
        "{:?}",
        report.text()
    );
    assert_eq!(read(&ws, "p/gen.txt").as_deref(), Some("v1"), "file kept");
    Ok(())
}

/// Lifecycle row 7: a removed target's file stays excluded, and `status`
/// names the missing target, until `gitignore` releases it.
#[tokio::test]
async fn removed_target_released_after_gitignore() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    gitignore(&ws.engine).await?;

    ws.write_build_file("p", "");
    let e2 = ws.reopen()?;
    assert_eq!(status_of(&e2, "p/gen.txt")?, StatusKind::Generated);
    let before = status(&e2).await?;
    assert!(
        kinds(&before).contains(&(Some("p/gen.txt"), ItemKind::TargetMissing)),
        "{before:?}"
    );

    gitignore(&e2).await?;
    assert_eq!(status_of(&e2, "p/gen.txt")?, StatusKind::Released);
    assert_eq!(read(&ws, "p/gen.txt").as_deref(), Some("v1"), "file kept");
    Ok(())
}

/// Lifecycle rows 5 and 10: a removed output's file, rebuilt without it,
/// stays excluded (its row dropped) until `gitignore` drops its line.
#[tokio::test]
async fn removed_output_excluded_until_gitignore() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", TWO_OUTPUTS);
    run_on(&ws.engine, "//p:gen").await?;
    gitignore(&ws.engine).await?;

    ws.write_build_file("p", &copy_target("gen", "a.txt", "a"));
    let e2 = ws.reopen()?;
    run_on(&e2, "//p:gen").await?;
    assert_eq!(must_row(&e2, "p/b.txt")?.state, RowState::Dropped);
    assert_eq!(status_of(&e2, "p/b.txt")?, StatusKind::Generated);

    let report = gitignore(&e2).await?;
    let released: Vec<&str> = report.released.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(released, ["p/b.txt"], "{report:?}");
    assert_eq!(status_of(&e2, "p/b.txt")?, StatusKind::Released);
    assert_eq!(status_of(&e2, "p/a.txt")?, StatusKind::Generated);
    assert_eq!(read(&ws, "p/b.txt").as_deref(), Some("b"), "file kept");
    Ok(())
}

// ---------------------------------------------------------------------------
// `status`
// ---------------------------------------------------------------------------

/// Rows 5, 6 and 10: `status` lists a released (and a dropped) file as no
/// longer generated, with the keep / discard commands.
#[tokio::test]
async fn codegen_status_lists_released_files() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", TWO_OUTPUTS);
    run_on(&ws.engine, "//p:gen").await?;
    gitignore(&ws.engine).await?;

    ws.write_build_file("p", &copy_target("gen", "a.txt", "a"));
    let e2 = ws.reopen()?;
    // Row 6: not rebuilt; the def no longer declares it.
    let unbuilt = status(&e2).await?;
    assert!(
        kinds(&unbuilt).contains(&(Some("p/b.txt"), ItemKind::NoLongerGenerated)),
        "{unbuilt:?}"
    );
    run_on(&e2, "//p:gen").await?;
    gitignore(&e2).await?;

    let report = status(&e2).await?;
    assert_eq!(
        kinds(&report),
        [(Some("p/b.txt"), ItemKind::NoLongerGenerated)],
        "{report:?}"
    );
    let item = &report.items[0];
    assert_eq!(item.addr.as_deref(), Some("//p:gen"));
    assert!(
        item.fix.contains("heph tool codegen keep p/b.txt")
            && item.fix.contains("heph tool codegen discard p/b.txt"),
        "{item:?}"
    );
    Ok(())
}

/// Every kind but `unresolved` (which suppresses `section_stale`; see
/// `status_reports_unresolved_package_instead_of_failing`), one line per
/// file, sorted by path, each ending with its fix.
#[tokio::test]
async fn status_lists_each_kind() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("a", &copy_target("gen", "a.txt", "a"));
    ws.write_build_file("d", TWO_OUTPUTS);
    ws.write_build_file("m", &copy_target("gen", "m.txt", "m"));
    ws.write_build_file("i", &copy_target("gen", "i.txt", "i"));
    for addr in ["//a:gen", "//d:gen", "//m:gen", "//i:gen"] {
        run_on(&ws.engine, addr).await?;
    }
    gitignore(&ws.engine).await?;

    // target_missing: `//a:gen` is gone.
    ws.write_build_file("a", "");
    // no_longer_generated: `d/b.txt` is no longer an output.
    ws.write_build_file("d", &copy_target("gen", "a.txt", "a"));
    // not_in_section: a new target, built after the section was written.
    ws.write_build_file("n", &copy_target("gen", "n.txt", "n"));
    let e2 = ws.reopen()?;
    run_on(&e2, "//d:gen").await?;
    run_on(&e2, "//n:gen").await?;
    // modified: edited after heph wrote it.
    ws.write_file("m/m.txt", "edited by hand");
    // interrupted: a write-back crashed midway; the bytes are neither.
    let s = store(&e2)?.context("db")?;
    s.write(|txn| {
        let mut r = txn.row(b"i/i.txt")?.context("row")?;
        r.pending_hash = Some(hashout("partial"));
        txn.put(&r)
    })?;
    ws.write_file("i/i.txt", "half");
    // foreign_line: a line no target wrote.
    add_section_line(&ws, "*.log")?;

    let e3 = ws.reopen()?;
    let report = status(&e3).await?;
    assert_eq!(
        kinds(&report),
        [
            (Some("*.log"), ItemKind::ForeignLine),
            (Some(".gitignore"), ItemKind::SectionStale),
            (Some("a/a.txt"), ItemKind::TargetMissing),
            (Some("d/b.txt"), ItemKind::NoLongerGenerated),
            (Some("i/i.txt"), ItemKind::Interrupted),
            (Some("m/m.txt"), ItemKind::Modified),
            (Some("n/n.txt"), ItemKind::NotInSection),
        ],
        "{report:#?}"
    );
    let text = report.text();
    assert_eq!(
        text.len(),
        report.items.len(),
        "one line per item: {text:?}"
    );
    for (line, item) in text.iter().zip(&report.items) {
        assert!(!item.fix.is_empty(), "{item:?}");
        assert!(line.ends_with(&item.fix), "{line} ends with its fix");
    }
    assert_eq!(report.exit_code(), 0, "without --check");
    Ok(())
}

/// `--check` exits 1 when anything is listed, 0 when nothing is, and an
/// error is 2.
#[tokio::test]
async fn status_check_result_kinds() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    gitignore(&ws.engine).await?;
    let check = st::Options {
        check: true,
        settle: true,
        hint: true,
    };

    let clean = status_with(&ws.engine, check).await?;
    assert!(clean.items.is_empty(), "{clean:?}");
    assert_eq!(clean.exit_code(), 0);
    assert_eq!(clean.text(), ["nothing needs a decision"]);

    ws.write_file("p/gen.txt", "edited");
    let listed = status_with(&ws.engine, check).await?;
    assert_eq!(listed.exit_code(), 1, "{listed:?}");

    // A `.gitignore` that cannot be read.
    std::fs::remove_file(ws.dir.path().join(".gitignore"))?;
    std::fs::create_dir(ws.dir.path().join(".gitignore"))?;
    let e2 = ws.reopen()?;
    let res = status_with(&e2, check).await;
    assert!(res.is_err(), "{res:?}");
    assert_eq!(exit_of(res), 2);
    Ok(())
}

/// An `unresolved` item carries `package` and `error`, and no `path`.
#[tokio::test]
async fn status_unresolved_item_has_no_path() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("z", BROKEN_BUILD);
    let report = status(&ws.engine).await?;
    let unresolved: Vec<_> = report
        .items
        .iter()
        .filter(|i| i.kind == ItemKind::Unresolved)
        .collect();
    assert_eq!(unresolved.len(), 1, "{report:?}");
    assert_eq!(unresolved[0].path, None);

    let doc = report.json();
    let items = doc["items"].as_array().context("items")?;
    let item = items
        .iter()
        .find(|i| i["kind"] == "unresolved")
        .context("unresolved item")?;
    let obj = item.as_object().context("object")?;
    assert!(!obj.contains_key("path"), "{item}");
    assert_eq!(item["package"], "//z");
    assert!(
        item["error"].as_str().is_some_and(|e| !e.is_empty()),
        "{item}"
    );
    Ok(())
}

/// `{"schema": 1, "items": [{path, kind, addr, output, fix}]}`: sorted by
/// path, `kind` from the enum, and no `written_at`.
#[tokio::test]
async fn status_json_schema() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", TWO_OUTPUTS);
    run_on(&ws.engine, "//p:gen").await?;
    gitignore(&ws.engine).await?;
    ws.write_file("p/b.txt", "edited");
    ws.write_file("p/a.txt", "edited too");

    let doc = status(&ws.engine).await?.json();
    assert_eq!(doc["schema"], 1);
    let items = doc["items"].as_array().context("items")?;
    let paths: Vec<&str> = items.iter().filter_map(|i| i["path"].as_str()).collect();
    assert_eq!(paths, ["p/a.txt", "p/b.txt"], "sorted: {doc}");
    const KINDS: [&str; 8] = [
        "no_longer_generated",
        "target_missing",
        "modified",
        "interrupted",
        "not_in_section",
        "foreign_line",
        "unresolved",
        "section_stale",
    ];
    for item in items {
        let keys: Vec<&String> = item.as_object().context("object")?.keys().collect();
        assert_eq!(keys, ["addr", "fix", "kind", "output", "path"], "{item}");
        assert!(
            KINDS.contains(&item["kind"].as_str().unwrap_or("")),
            "{item}"
        );
        assert_eq!(item["kind"], "modified");
        assert_eq!(item["addr"], "//p:gen");
    }
    assert!(!doc.to_string().contains("written_at"), "{doc}");
    Ok(())
}

/// Lifecycle row 11: a package whose BUILD file fails is listed once as
/// `unresolved`, and its rows still appear under their other kinds.
#[tokio::test]
async fn status_reports_unresolved_package_instead_of_failing() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", TWO_OUTPUTS);
    run_on(&ws.engine, "//p:gen").await?;
    gitignore(&ws.engine).await?;

    ws.write_build_file("p", BROKEN_BUILD);
    ws.write_file("p/a.txt", "edited");
    let e2 = ws.reopen()?;
    let report = status(&e2).await?;
    assert_eq!(
        kinds(&report),
        [
            (None, ItemKind::Unresolved),
            (Some("p/a.txt"), ItemKind::Modified),
        ],
        "{report:#?}"
    );
    assert_eq!(report.items[0].package.as_deref(), Some("//p"));
    // An incomplete set of lines cannot tell a stale section.
    assert!(
        !kinds(&report)
            .iter()
            .any(|(_, k)| *k == ItemKind::SectionStale),
        "{report:?}"
    );
    Ok(())
}

/// `status` lists a section line no target wrote.
#[tokio::test]
async fn foreign_line_listed_by_status_not_warned_by_build() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    gitignore(&ws.engine).await?;
    add_section_line(&ws, "*.pb.go")?;

    let e2 = ws.reopen()?;
    let report = status(&e2).await?;
    let foreign: Vec<_> = report
        .items
        .iter()
        .filter(|i| i.kind == ItemKind::ForeignLine)
        .collect();
    assert_eq!(foreign.len(), 1, "{report:?}");
    assert_eq!(foreign[0].path.as_deref(), Some("*.pb.go"));
    assert_eq!(foreign[0].fix, "heph tool codegen gitignore");
    assert!(
        foreign[0].detail.contains(".gitignore:4"),
        "names its line: {:?}",
        foreign[0]
    );
    Ok(())
}

/// A `status` that settles a pending row, while a build of the same target
/// writes back: both finish (a lock is taken only after every def resolved,
/// and one at a time).
#[tokio::test]
async fn status_during_write_back_does_not_deadlock() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    gitignore(&ws.engine).await?;
    let s = store(&ws.engine)?.context("db")?;
    s.write(|txn| {
        let mut r = txn.row(b"p/gen.txt")?.context("row")?;
        r.pending_hash = Some(hashout("v1"));
        txn.put(&r)
    })?;
    ws.write_build_file(
        "p",
        r#"
target(
    name = "gen",
    driver = "bash",
    run = "sleep 0.3; printf v2 > $OUT",
    out = "gen.txt",
    codegen = "copy",
)
"#,
    );
    let e2 = ws.reopen()?;
    let both = async {
        let (built, listed) = tokio::join!(run_on(&e2, "//p:gen"), status(&e2));
        built?;
        listed?;
        anyhow::Ok(())
    };
    tokio::time::timeout(Duration::from_secs(120), both)
        .await
        .context("status and a write-back deadlocked")??;
    assert_eq!(read(&ws, "p/gen.txt").as_deref(), Some("v2"));
    let row = must_row(&e2, "p/gen.txt")?;
    assert_eq!(row.pending_hash, None, "settled: {row:?}");
    Ok(())
}

// ---------------------------------------------------------------------------
// `why`
// ---------------------------------------------------------------------------

/// A plain source file: `source`, no row, no line.
#[tokio::test]
async fn why_on_source_path() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    ws.write_file("p/src.txt", "mine");
    run_on(&ws.engine, "//p:gen").await?;
    gitignore(&ws.engine).await?;

    let report = why_in(&ws.engine, ws.dir.path(), &["p/src.txt"]).await?;
    assert_eq!(report.exit_code(), 0);
    let item = &report.items[0];
    assert_eq!(item.result, PathResultKind::Explained);
    assert_eq!(item.status, Some("source"));
    assert_eq!(item.row, None);
    assert_eq!(item.section_line, None);
    let text = report.text();
    assert_eq!(text[0], "p/src.txt: source", "{text:?}");
    assert!(
        text.contains(&"  no row records it".to_string()),
        "{text:?}"
    );
    assert!(
        text.contains(&"  no section line matches".to_string()),
        "{text:?}"
    );
    Ok(())
}

/// The matching section line, with its number and whether heph wrote it.
#[tokio::test]
async fn why_prints_matching_section_line() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    ws.write_file(".gitignore", "node_modules\n");
    run_on(&ws.engine, "//p:gen").await?;
    gitignore(&ws.engine).await?;
    add_section_line(&ws, "/p/*.log")?;
    ws.write_file("p/x.log", "log");
    let file = read(&ws, ".gitignore").context(".gitignore")?;
    let line_of = |pat: &str| {
        file.lines()
            .position(|l| l == pat)
            .map(|i| i + 1)
            .expect("line in .gitignore")
    };

    let e2 = ws.reopen()?;
    let report = why_in(&e2, ws.dir.path(), &["p/gen.txt", "p/x.log"]).await?;
    let gen_line = report.items[0].section_line.clone().context("gen line")?;
    assert_eq!(gen_line.line, line_of("/p/gen.txt"));
    assert_eq!(gen_line.pattern, "/p/gen.txt");
    assert!(gen_line.heph_wrote && !gen_line.negated);
    let log_line = report.items[1].section_line.clone().context("log line")?;
    assert_eq!(log_line.line, line_of("/p/*.log"));
    assert!(!log_line.heph_wrote);

    let text = report.text();
    let want_gen = format!(
        "  .gitignore:{} /p/gen.txt (written by heph)",
        gen_line.line
    );
    let want_log = format!(
        "  .gitignore:{} /p/*.log (not written by heph)",
        log_line.line
    );
    assert!(text.contains(&want_gen), "{text:?}");
    assert!(text.contains(&want_log), "{text:?}");
    assert_eq!(report.items[0].status, Some("generated"));
    assert_eq!(report.items[0].status_reason, Some("recorded"));
    Ok(())
}

/// With no db at all: a file under a line is `generated` (unrecorded) with
/// the `codegen import` hint; a path with no file, row or line is `unknown`
/// and exits 1.
#[tokio::test]
async fn why_on_unrecorded_and_empty_index() -> anyhow::Result<()> {
    let ws = Workspace::new();
    write_section(&ws, &["# //p:old", "/p/old.txt"]);
    ws.write_file("p/old.txt", "written by an older heph");

    let report = why_in(&ws.engine, ws.dir.path(), &["p/old.txt"]).await?;
    assert!(store(&ws.engine)?.is_none(), "why creates no db");
    let item = &report.items[0];
    assert_eq!(item.status, Some("generated"));
    assert_eq!(item.status_reason, Some("unrecorded"));
    assert_eq!(item.row, None);
    assert!(item.section_line.is_some());
    let hint = report.hint.as_deref().context("import hint")?;
    assert!(hint.contains("heph tool codegen import"), "{hint}");
    assert_eq!(report.exit_code(), 0);

    let unknown = why_in(&ws.engine, ws.dir.path(), &["p/nothing.txt"]).await?;
    assert_eq!(unknown.items[0].result, PathResultKind::Unknown);
    assert_eq!(unknown.items[0].status, None);
    assert_eq!(unknown.exit_code(), 1);
    assert_eq!(unknown.hint, None, "no line matched: no hint");
    Ok(())
}

/// `why --json`: `results` with `status`, `reason`, `row` (with
/// `written_at`) and `section_line`.
#[tokio::test]
async fn why_json_shape() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    gitignore(&ws.engine).await?;

    let doc = why_in(&ws.engine, ws.dir.path(), &["p/gen.txt", "../out"])
        .await?
        .json();
    assert_eq!(doc["schema"], 1);
    let results = doc["results"].as_array().context("results")?;
    assert_eq!(results.len(), 2);
    let r = &results[0];
    let keys: Vec<&String> = r.as_object().context("object")?.keys().collect();
    assert_eq!(
        keys,
        [
            "path",
            "reason",
            "result",
            "row",
            "section_line",
            "status",
            "status_reason"
        ],
        "{r}"
    );
    assert_eq!(r["path"], "p/gen.txt");
    assert_eq!(r["result"], "explained");
    assert_eq!(r["status"], "generated");
    assert_eq!(r["status_reason"], "recorded");
    assert_eq!(r["row"]["addr"], "//p:gen");
    assert_eq!(r["row"]["state"], "written");
    assert_eq!(r["row"]["pending"], false);
    assert!(r["row"]["written_at"].as_i64().is_some(), "{r}");
    assert_eq!(r["section_line"]["pattern"], "/p/gen.txt");
    assert_eq!(r["section_line"]["heph_wrote"], true);

    let refused = &results[1];
    assert_eq!(refused["result"], "refused");
    assert_eq!(refused["status"], serde_json::Value::Null);
    assert!(refused["reason"].as_str().is_some(), "{refused}");
    Ok(())
}

/// Path arguments: cwd-relative, root-relative, absolute and `..` all name
/// the same file; outside the root is refused; a missing path is unknown.
#[tokio::test]
async fn command_path_argument_forms() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    std::fs::create_dir_all(ws.dir.path().join("q"))?;
    run_on(&ws.engine, "//p:gen").await?;
    let root = ws.dir.path();
    let cwd = root.join("p");
    let abs = root.join("p/gen.txt");

    for form in [
        "gen.txt",
        "//p/gen.txt",
        abs.to_str().context("utf-8")?,
        "../q/../p/./gen.txt",
    ] {
        let report = why_in(&ws.engine, &cwd, &[form]).await?;
        let item = &report.items[0];
        assert_eq!(item.path, "p/gen.txt", "{form}: {item:?}");
        assert_eq!(item.result, PathResultKind::Explained, "{form}");
        assert_eq!(
            item.row.as_ref().map(|r| r.addr.as_str()),
            Some("//p:gen"),
            "{form}"
        );
    }

    let all = why_in(&ws.engine, &cwd, &["gen.txt", "//p/gen.txt"]).await?;
    assert_eq!(all.items.len(), 1, "one result per file: {all:?}");

    for outside in ["../../elsewhere", "/etc/hosts", "//../x", ".."] {
        let report = why_in(&ws.engine, &cwd, &[outside]).await?;
        assert_eq!(
            report.items[0].result,
            PathResultKind::Refused,
            "{outside}: {report:?}"
        );
        assert_eq!(report.exit_code(), 1);
    }

    let missing = why_in(&ws.engine, &cwd, &["missing.txt"]).await?;
    assert_eq!(missing.items[0].path, "p/missing.txt");
    assert_eq!(missing.items[0].result, PathResultKind::Unknown);
    assert_eq!(missing.exit_code(), 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// `validate`
// ---------------------------------------------------------------------------

/// Only a stale section fails; findings local to this checkout do not.
#[tokio::test]
async fn validate_fails_only_on_commit_findings() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", TWO_OUTPUTS);
    run_on(&ws.engine, "//p:gen").await?;
    gitignore(&ws.engine).await?;
    ws.write_file("p/a.txt", "edited");

    let local = validate(&ws.engine).await;
    let failures = local.failures?;
    assert!(
        failures.is_empty(),
        "a modified file is local: {failures:?}"
    );
    assert!(!local.warnings.is_empty());

    ws.write_build_file("p", &copy_target("gen", "a.txt", "a"));
    let e2 = ws.reopen()?;
    let stale = validate(&e2).await.failures?;
    assert_eq!(stale.len(), 1, "{stale:?}");
    Ok(())
}

/// Local findings are warnings, one per item, then one pointing at
/// `heph tool codegen status`.
#[tokio::test]
async fn validate_warns_local_findings() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    gitignore(&ws.engine).await?;
    ws.write_file("p/gen.txt", "edited");
    add_section_line(&ws, "*.pb.go")?;
    // The foreign line makes the section stale too; that fails, not warns.

    let e2 = ws.reopen()?;
    let findings = validate(&e2).await;
    let w = &findings.warnings;
    assert_eq!(w.len(), 3, "{w:?}");
    assert!(w.iter().all(|l| l.starts_with("warning: ")), "{w:?}");
    assert!(
        w.iter()
            .any(|l| l.contains("p/gen.txt") && l.contains("modified"))
    );
    assert!(
        w.iter()
            .any(|l| l.contains("*.pb.go") && l.contains("foreign_line"))
    );
    assert!(
        w.last()
            .is_some_and(|l| l.ends_with("run heph tool codegen status")),
        "{w:?}"
    );
    assert!(!w.iter().any(|l| l.contains("section_stale")), "{w:?}");
    Ok(())
}

/// Lifecycle row 22: on a fresh checkout (no index, a section whose line
/// matches a file) `codegen status` walks the tree for the `codegen import`
/// hint; `validate`'s listing does not, so CI walks nothing.
#[tokio::test]
async fn validate_skips_the_import_hint_walk() -> anyhow::Result<()> {
    let ws = Workspace::new();
    write_section(&ws, &["/p/a.txt"]);
    ws.write_file("p/a.txt", "aaa");

    let cli = status(&ws.engine).await?;
    assert!(cli.hint.is_some(), "the CLI computes the hint: {cli:?}");

    let listed = status_with(&ws.engine, st::Options::VALIDATE).await?;
    assert_eq!(listed.hint, None, "{listed:?}");

    let findings = validate(&ws.engine).await;
    assert!(
        !findings
            .warnings
            .iter()
            .any(|w| w.contains("codegen import")),
        "{:?}",
        findings.warnings
    );
    Ok(())
}

/// The stale-section failure names `heph tool codegen gitignore`, never the
/// removed command.
#[tokio::test]
async fn validate_message_names_codegen_gitignore() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    let failures = validate(&ws.engine).await.failures?;
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].contains("run heph tool codegen gitignore"),
        "{failures:?}"
    );
    assert!(!failures[0].contains("gen-gitignore"), "{failures:?}");

    gitignore(&ws.engine).await?;
    assert!(validate(&ws.engine).await.failures?.is_empty());
    Ok(())
}

/// An old BEGIN line fails with the `old_marker` message.
#[tokio::test]
async fn validate_old_marker_message() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    write_section_with(&ws, OLD_BEGIN, &["# //p:gen", "/p/gen.txt"]);
    let failures = validate(&ws.engine).await.failures?;
    assert_eq!(failures, [gi::old_marker_message()]);
    Ok(())
}

/// Lifecycle row 7: a line whose target is gone fails `validate`; the file
/// it covers is a local warning naming the missing target.
#[tokio::test]
async fn validate_fails_on_line_of_missing_target() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &copy_target("gen", "gen.txt", "v1"));
    run_on(&ws.engine, "//p:gen").await?;
    gitignore(&ws.engine).await?;

    ws.write_build_file("p", "");
    let e2 = ws.reopen()?;
    let findings = validate(&e2).await;
    let failures = findings.failures?;
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("out of date"), "{failures:?}");
    assert!(
        findings
            .warnings
            .iter()
            .any(|l| l.contains("p/gen.txt") && l.contains("target_missing")),
        "{:?}",
        findings.warnings
    );
    Ok(())
}
