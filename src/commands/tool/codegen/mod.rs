//! `heph tool codegen`: the commands that decide what happens to the files a
//! `codegen = copy` target writes into the tree, and to the managed section of
//! the root `.gitignore` (spec: codegen file index, Commands).
//!
//! Each subcommand's logic is a `pub async fn` returning a report (text, the
//! `--json` record and exit code), so a test can drive it in-process against
//! a real engine; the clap layer only parses, runs it inside the TUI app and
//! prints.
//!
//! Exit codes are 0 / 1 / 2 (Decision 20), unlike every other heph command:
//! `--check` must tell findings from failures. A command ends in a typed
//! [`CodegenExit`], which `main.rs` downcasts.

pub mod gitignore;
pub mod import;
pub mod keep;
mod paths;
pub mod status;
pub mod why;

use crate::tui::session::SessionSlot;
use std::future::Future;
use std::sync::Arc;

use clap::Subcommand;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::progress_app::run_engine_command_out;
use crate::engine::Engine;
use crate::engine::request_state::RequestState;
use crate::tui::{BufferedStdout, LogSink};

pub use paths::{ResolvedPath, resolve_path, root_hint, with_hint};

/// The `--help` epilogue every codegen subcommand carries.
pub const EXIT_CODES_HELP: &str = "Exit codes:\n  0  OK\n  1  findings (--check), or a refused or unknown path\n  2  error; a usage error (an unknown flag, a missing argument) also exits 2";

/// How a codegen command ended, when not with 0. `main.rs` exits with
/// `code`; `error`, when set, is rendered first (any failure that is not a
/// finding is wrapped as code 2).
#[derive(Debug)]
pub struct CodegenExit {
    pub code: u8,
    pub error: Option<anyhow::Error>,
}

impl CodegenExit {
    /// Findings, or a refused path: exit 1, nothing more to render.
    pub fn findings() -> Self {
        Self {
            code: 1,
            error: None,
        }
    }

    /// A failure: exit 2, rendering `error`.
    pub fn failed(error: anyhow::Error) -> Self {
        Self {
            code: 2,
            error: Some(error),
        }
    }
}

impl std::fmt::Display for CodegenExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.error {
            Some(e) => write!(f, "{e:#}"),
            None => write!(f, "exit {}", self.code),
        }
    }
}

impl std::error::Error for CodegenExit {}

/// What a codegen command prints and how it exits. The report itself is the
/// `--json` record: every codegen command is record-shaped.
pub trait Report: serde::Serialize + output::Record<Static = Self> + Send + Sync + 'static {
    /// The human output, one entry per line.
    fn text(&self) -> Vec<String>;
    /// 0 or 1; a failure is an `Err`, not a report.
    fn exit_code(&self) -> u8;
}

#[derive(clap::Args)]
pub struct CodegenArgs {
    #[command(subcommand)]
    pub command: CodegenCommands,
}

#[derive(Subcommand)]
pub enum CodegenCommands {
    /// Rewrite the managed section of the root .gitignore from the targets
    ///
    /// Prints one `+` / `-` line per pattern with its target, then every
    /// recorded file the rewrite releases. With a package matcher, only lines
    /// from targets under it change. `--check` writes nothing and exits 1 when
    /// the section is stale.
    ///
    /// Example: `heph tool codegen gitignore --check`
    #[command(after_help = EXIT_CODES_HELP)]
    Gitignore(gitignore::Args),
    /// List the generated files that need a decision
    ///
    /// One line per file, sorted by path, each ending with the command that
    /// settles it. `--check` exits 1 when anything is listed.
    ///
    /// Example: `heph tool codegen status`
    #[command(after_help = EXIT_CODES_HELP)]
    Status(status::Args),
    /// Make generated files source from now on
    ///
    /// Drops their rows and the section lines that covered them, then prints
    /// the `git add` to commit them. A path a current target still declares
    /// as a `copy` output is refused.
    ///
    /// Example: `heph tool codegen keep p/foo.pb.go`
    #[command(after_help = EXIT_CODES_HELP)]
    Keep(keep::KeepArgs),
    /// Delete generated files no target produces any more
    ///
    /// Deletes a file only when a fresh hash equals what heph recorded; an
    /// edited or imported file needs `--force`. `--all` covers every file
    /// `status` lists as no longer generated, and drops dangling rows.
    ///
    /// Example: `heph tool codegen discard --all --dry-run`
    #[command(after_help = EXIT_CODES_HELP)]
    Discard(keep::DiscardArgs),
    /// Explain why a path is, or is not, read as generated
    ///
    /// Prints its status and reason, the row recording it, how its bytes were
    /// verified, and the section line that matched.
    ///
    /// Example: `heph tool codegen why p/foo.pb.go`
    #[command(after_help = EXIT_CODES_HELP)]
    Why(why::Args),
    /// Record the files an older heph stamped with its xattr
    ///
    /// Run once per checkout after upgrading. A second run records nothing
    /// new.
    ///
    /// Example: `heph tool codegen import`
    #[command(after_help = EXIT_CODES_HELP)]
    Import(import::Args),
}

impl CodegenArgs {
    pub fn execute(
        &self,
        sink: LogSink,
        session: &Arc<SessionSlot>,
        global: &GlobalOptions,
    ) -> anyhow::Result<()> {
        let res = match &self.command {
            CodegenCommands::Gitignore(a) => gitignore::execute(a, sink, session, global),
            CodegenCommands::Status(a) => status::execute(a, sink, session, global),
            CodegenCommands::Keep(a) => keep::execute_keep(a, sink, session, global),
            CodegenCommands::Discard(a) => keep::execute_discard(a, sink, session, global),
            CodegenCommands::Why(a) => why::execute(a, sink, session, global),
            CodegenCommands::Import(a) => import::execute(a, sink, session, global),
        };
        wrap_exit(res)
    }
}

/// Any error that is not already a [`CodegenExit`], or an output failure
/// that decides its own code (a `--jq` runtime error), becomes one with
/// code 2.
pub fn wrap_exit(res: anyhow::Result<()>) -> anyhow::Result<()> {
    res.map_err(|e| {
        if e.downcast_ref::<CodegenExit>().is_some() || output::exit_code(&e).is_some() {
            e
        } else {
            CodegenExit::failed(e).into()
        }
    })
}

/// The removed `heph tool gen-gitignore`: kept only to fail (Decision 7).
/// It has no output flags; `rest` swallows an old `--json` too, so every
/// invocation reaches the message.
#[derive(clap::Args, Clone, Debug)]
pub struct RemovedGenGitignoreArgs {
    /// Accepted and ignored, so an old invocation reaches the message.
    #[arg(hide = true, num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

/// The message the removed command fails with.
pub const GEN_GITIGNORE_REMOVED: &str = "heph tool gen-gitignore was removed; use heph tool codegen gitignore (and --check for the up-to-date test)";

/// `heph tool gen-gitignore`: the removal on stderr, exit 2, nothing on
/// stdout.
pub fn removed_gen_gitignore(_args: &RemovedGenGitignoreArgs) -> anyhow::Result<()> {
    Err(CodegenExit::failed(anyhow::anyhow!(GEN_GITIGNORE_REMOVED)).into())
}

/// Run `job` as a codegen command: bootstrap the engine, run it inside the
/// TUI app, print its report (the text, or the record under the output
/// flags), and end with its exit code. Findings (exit 1) print their record
/// too; a failure (exit 2) prints nothing on stdout, and `main` renders it on
/// stderr.
fn run_cli<R, F, Fut>(
    title: &'static str,
    out: &output::JsonArgs<R>,
    dashboard: bool,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
    job: F,
) -> anyhow::Result<()>
where
    R: Report,
    F: FnOnce(Arc<Engine>, Arc<RequestState>) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<R>> + Send + 'static,
{
    let json = out.clone();
    bootstrap::block_on(run_engine_command_out(
        title,
        sink,
        session,
        global,
        out,
        dashboard,
        move |engine, rs, ctx| async move {
            let res = job(Arc::clone(&engine), rs.clone()).await;
            let code = match &res {
                Ok(report) if json.enabled() => {
                    let cancelled = || rs.ctoken().is_cancelled();
                    json.record(report, &cancelled).map(|()| report.exit_code())
                }
                Ok(report) => {
                    let text = BufferedStdout::new(&ctx);
                    for line in report.text() {
                        text.println(line);
                    }
                    text.close().await;
                    Ok(report.exit_code())
                }
                Err(_) => Ok(0),
            };
            let res = res.and(code);
            crate::commands::errors::finalize(&ctx, &rs, res, None, |code| {
                if code == 0 {
                    Ok(())
                } else {
                    Err(CodegenExit { code, error: None }.into())
                }
            })
            .await
        },
    ))
    .and_then(|r| r)
}

/// Run a blocking job on the blocking pool.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    hcore::blocking::run(f).await
}

/// Read the root `.gitignore` as bytes, as the index's reader does (a
/// non-UTF-8 line is skipped with a warning, not an error); a missing file is
/// empty.
pub(crate) fn read_gitignore(root: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context as _;

    let path = root.join(".gitignore");
    match std::fs::read(&path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Replace `<root>/.gitignore` with `content`, atomically.
///
/// `std::fs::write` truncates and then fills, so a concurrent `heph.fs.glob`
/// reading the section in between saw an empty file, and a crash in between
/// left the committed file truncated. This writes a sibling temp file and
/// renames it over the old one: a reader sees the old bytes or the new ones.
/// The file keeps its permissions, and a symlinked `.gitignore` keeps its
/// link: the rename replaces the file it points to.
pub(crate) fn write_gitignore(root: &std::path::Path, content: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::Write as _;

    let link = root.join(".gitignore");
    let path = match std::fs::canonicalize(&link) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => link,
        Err(e) => return Err(e).with_context(|| format!("resolving {}", link.display())),
    };
    let dir = path.parent().unwrap_or(root);
    let mut tmp = tempfile::Builder::new()
        .prefix(".gitignore.")
        .suffix(".heph-tmp")
        .tempfile_in(dir)
        .with_context(|| format!("creating a temp file next to {}", path.display()))?;
    match std::fs::metadata(&path) {
        Ok(meta) => tmp
            .as_file()
            .set_permissions(meta.permissions())
            .with_context(|| format!("copying the permissions of {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    }
    tmp.write_all(content)
        .and_then(|()| tmp.as_file().sync_all())
        .with_context(|| format!("writing a temp file for {}", path.display()))?;
    tmp.persist(&path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// The empty-index hint (spec: Upgrade, Discovery), from a count the caller
/// already has.
pub fn import_hint(matching_files: usize) -> String {
    format!(
        "the codegen index is empty but {matching_files} file{} match{} the heph section of .gitignore; if an older heph wrote them, run heph tool codegen import",
        if matching_files == 1 { "" } else { "s" },
        if matching_files == 1 { "es" } else { "" },
    )
}

/// The `status` / `why` hint, when the index has no rows and section lines
/// match files on disk. Walks the tree only in that case.
async fn empty_index_hint(engine: &Arc<Engine>) -> anyhow::Result<Option<String>> {
    let index = Arc::clone(engine.codegen_index());
    let (dirs, globs) = (engine.skip_dirs(), engine.skip_globs());
    blocking(move || {
        let snap = index.snapshot()?;
        if !snap.is_empty() {
            return Ok(None);
        }
        let ignore = hwalk::Ignore::new(&dirs, &globs)?;
        let n = hwalk::codegen_index::import::count_section_matches(
            index.root(),
            &ignore,
            index.fs().as_ref(),
            snap.section(),
        )?;
        Ok((n > 0).then(|| import_hint(n)))
    })
    .await
}

/// A root-relative path for display.
fn display(rel: &[u8]) -> String {
    String::from_utf8_lossy(rel).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader racing the rewrite sees the old bytes or the new ones, never
    /// the empty or half-written file `std::fs::write` exposed between its
    /// truncate and its write.
    #[test]
    fn write_gitignore_never_exposes_a_partial_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        let a = "# heph\n".repeat(20_000).into_bytes();
        let b = "/gen/\n".repeat(30_000).into_bytes();
        write_gitignore(&root, &a)?;

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = std::thread::spawn({
            let (root, stop, a, b) = (root.clone(), Arc::clone(&stop), a.clone(), b.clone());
            move || -> usize {
                let mut reads = 0;
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    let got = read_gitignore(&root).expect("read");
                    assert!(got == a || got == b, "a reader saw {} bytes", got.len());
                    reads += 1;
                }
                reads
            }
        });
        for i in 0..200 {
            write_gitignore(&root, if i % 2 == 0 { &b } else { &a })?;
        }
        stop.store(true, std::sync::atomic::Ordering::Release);
        let reads = reader
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        assert!(reads > 0);

        let left: Vec<_> = std::fs::read_dir(&root)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<_, _>>()?;
        assert_eq!(
            left,
            [std::ffi::OsString::from(".gitignore")],
            "temp files left"
        );
        Ok(())
    }

    #[test]
    fn write_gitignore_keeps_permissions_and_a_symlink() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir()?;
        let root = dir.path();
        std::fs::write(root.join("real"), "old\n")?;
        std::fs::set_permissions(root.join("real"), std::fs::Permissions::from_mode(0o640))?;
        std::os::unix::fs::symlink("real", root.join(".gitignore"))?;

        write_gitignore(root, b"new\n")?;

        assert!(std::fs::symlink_metadata(root.join(".gitignore"))?.is_symlink());
        assert_eq!(std::fs::read(root.join("real"))?, b"new\n");
        let mode = std::fs::metadata(root.join("real"))?.permissions().mode();
        assert_eq!(mode & 0o777, 0o640);

        // No `.gitignore` yet: created, as `std::fs::write` did.
        let fresh = tempfile::tempdir()?;
        write_gitignore(fresh.path(), b"x\n")?;
        assert_eq!(read_gitignore(fresh.path())?, b"x\n");
        Ok(())
    }

    #[test]
    fn wrap_exit_keeps_a_codegen_exit_and_wraps_anything_else_as_two() {
        let e = wrap_exit(Err(CodegenExit::findings().into())).unwrap_err();
        assert_eq!(e.downcast_ref::<CodegenExit>().map(|c| c.code), Some(1));

        let e = wrap_exit(Err(anyhow::anyhow!("boom"))).unwrap_err();
        let c = e.downcast_ref::<CodegenExit>().expect("wrapped");
        assert_eq!(c.code, 2);
        assert!(format!("{e}").contains("boom"));
        assert!(wrap_exit(Ok(())).is_ok());

        // A `--jq` runtime error keeps its own code: 2 on every command, so
        // it is never read as `--check`'s findings.
        let jq: anyhow::Result<()> = Err(anyhow::Error::from(output::OutputError::JqRuntime(
            "boom".into(),
        ))
        .context("printing"));
        let e = wrap_exit(jq).unwrap_err();
        assert!(e.downcast_ref::<CodegenExit>().is_none());
        assert_eq!(output::exit_code(&e), Some(2));
    }

    fn status_report() -> status::StatusReport {
        let item = |kind, path: Option<&str>, package: Option<&str>| status::StatusItem {
            kind,
            path: path.map(str::to_string),
            addr: path.map(|_| "//p:gen".to_string()),
            output: path.map(|_| "out".to_string()),
            fix: "fix it".to_string(),
            detail: "detail".to_string(),
            package: package.map(str::to_string),
            error: package.map(|_| "boom".to_string()),
        };
        status::StatusReport {
            check: true,
            items: vec![
                item(status::ItemKind::Modified, Some("p/a.go"), None),
                item(status::ItemKind::Unresolved, None, Some("//q")),
            ],
            hint: Some("run import".to_string()),
        }
    }

    fn gitignore_report() -> gitignore::GitignoreReport {
        gitignore::GitignoreReport {
            check: false,
            stale: true,
            reasons: vec![
                crate::engine::gitignore::StaleReason::Lines,
                crate::engine::gitignore::StaleReason::OldMarker,
            ],
            written: true,
            lines: vec![gitignore::LineChange {
                added: true,
                pattern: "/p/a.go".to_string(),
                addr: Some("//p:gen".to_string()),
            }],
            released: vec![gitignore::Released {
                path: "p/old.go".to_string(),
                addr: "//p:gen".to_string(),
            }],
            entries: 1,
            gitignore: std::path::PathBuf::from(".gitignore"),
        }
    }

    fn paths_report() -> keep::PathsReport {
        keep::PathsReport {
            results: vec![keep::PathResult {
                path: "p/a.go".to_string(),
                result: keep::PathResultKind::WouldDiscard,
                reason: None,
            }],
            lines_removed: vec![crate::engine::gitignore::GitignoreEntry {
                pattern: "/p/a.go".to_string(),
                addr: Some(crate::htaddr::parse_addr("//p:gen").expect("addr")),
            }],
            also_released: vec!["p/b.go".to_string()],
            dry_run: true,
            git_add: false,
        }
    }

    fn why_report() -> why::WhyReport {
        why::WhyReport {
            items: vec![why::WhyItem {
                path: "p/a.go".to_string(),
                result: keep::PathResultKind::Explained,
                reason: None,
                status: Some("generated"),
                status_reason: Some("recorded"),
                row: Some(why::WhyRow {
                    path: "p/a.go".to_string(),
                    addr: "//p:gen".to_string(),
                    output: None,
                    state: "written",
                    written_at: Some(7),
                    pending: false,
                    verified: "stat",
                }),
                section_line: Some(why::WhyLine {
                    line: 3,
                    pattern: "/p/a.go".to_string(),
                    heph_wrote: true,
                    negated: false,
                }),
            }],
            hint: None,
        }
    }

    /// The record `--json` prints for `report`, compact, parsed back.
    fn printed<R: Report>(report: &R) -> anyhow::Result<serde_json::Value> {
        let mut buf = Vec::new();
        output::JsonArgs::<R>::try_parse_from(["--json"])?
            .printer_to(&mut buf, output::Style::Compact)
            .record(report, &output::never_cancelled)?;
        Ok(serde_json::from_slice(&buf)?)
    }

    #[test]
    fn record_keys_subset_of_fields() -> anyhow::Result<()> {
        output::check_keys(&status_report())?;
        output::check_keys(&gitignore_report())?;
        output::check_keys(&paths_report())?;
        output::check_keys(&why_report())?;
        output::check_keys(&import::ImportReport {
            recorded: 1,
            skipped: 2,
            unchanged: 3,
            supported: true,
        })
    }

    /// The records keep the documents' keys, minus the `schema` envelope
    /// (docs/CODEGEN.md).
    #[test]
    fn records_keep_the_document_keys() -> anyhow::Result<()> {
        assert_eq!(
            printed(&status_report())?,
            serde_json::json!({
                "items": [
                    {"path": "p/a.go", "kind": "modified", "addr": "//p:gen", "output": "out", "fix": "fix it"},
                    {"kind": "unresolved", "package": "//q", "error": "boom", "fix": "fix it"},
                ],
                "hint": "run import",
            })
        );
        assert_eq!(
            printed(&status::StatusReport::default())?,
            serde_json::json!({ "items": [] }),
            "no hint key without a hint"
        );
        assert_eq!(
            printed(&gitignore_report())?,
            serde_json::json!({
                "stale": true,
                "reasons": ["lines", "old_marker"],
                "written": true,
                "lines": [{"op": "add", "pattern": "/p/a.go", "addr": "//p:gen"}],
                "released": [{"path": "p/old.go", "addr": "//p:gen"}],
            })
        );
        assert_eq!(
            printed(&paths_report())?,
            serde_json::json!({
                "results": [{"path": "p/a.go", "result": "would_discard", "reason": null}],
                "lines": [{"op": "remove", "pattern": "/p/a.go", "addr": "//p:gen"}],
                "released": ["p/b.go"],
            })
        );
        assert_eq!(
            printed(&why_report())?,
            serde_json::json!({
                "results": [{
                    "path": "p/a.go", "result": "explained", "reason": null,
                    "status": "generated", "status_reason": "recorded",
                    "row": {"path": "p/a.go", "addr": "//p:gen", "output": null, "state": "written",
                            "written_at": 7, "pending": false, "verified": "stat"},
                    "section_line": {"line": 3, "pattern": "/p/a.go", "heph_wrote": true, "negated": false},
                }],
            })
        );
        assert_eq!(
            printed(&import::ImportReport {
                recorded: 412,
                skipped: 3,
                unchanged: 0,
                supported: true,
            })?,
            serde_json::json!({ "recorded": 412, "skipped": 3, "unchanged": 0 })
        );
        Ok(())
    }

    /// `--check` findings still print their record and exit 1, and a `--jq`
    /// filter that fails at runtime is exit 2, never read as findings.
    #[test]
    fn jq_runtime_error_is_not_findings() -> anyhow::Result<()> {
        let report = status_report();
        assert_eq!(report.exit_code(), 1);
        let mut buf = Vec::new();
        let err =
            output::JsonArgs::<status::StatusReport>::try_parse_from(["--jq", ".items | error"])?
                .printer_to(&mut buf, output::Style::Compact)
                .record(&report, &output::never_cancelled)
                .unwrap_err();
        assert_eq!(
            output::exit_code(&wrap_exit(Err(err)).unwrap_err()),
            Some(2)
        );
        Ok(())
    }

    #[test]
    fn import_hint_agrees_in_number() {
        assert!(import_hint(1).contains("1 file matches"));
        assert!(import_hint(412).contains("412 files match the heph section"));
    }
}
