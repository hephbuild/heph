//! `heph tool codegen`: the commands that decide what happens to the files a
//! `codegen = copy` target writes into the tree, and to the managed section of
//! the root `.gitignore` (spec: codegen file index, Commands).
//!
//! Each subcommand's logic is a `pub async fn` returning a report (text, JSON
//! and exit code), so a test can drive it in-process against a real engine;
//! the clap layer only parses, runs it inside the TUI app and prints.
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

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use clap::Subcommand;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::Engine;
use crate::engine::request_state::RequestState;
use crate::tui::{self, App, AppContext, BufferedStdout, LogSink};

pub use paths::{ResolvedPath, resolve_path, root_hint, with_hint};

/// The `--help` epilogue every codegen subcommand carries.
pub const EXIT_CODES_HELP: &str = "Exit codes:\n  0  OK\n  1  findings (--check), or a refused or unknown path\n  2  error; a usage error (an unknown flag, a missing argument) also exits 2";

/// The `--json` schema version of every codegen command.
pub const JSON_SCHEMA: u32 = 1;

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

/// What a codegen command prints and how it exits.
pub trait Report: Send + 'static {
    /// The human output, one entry per line.
    fn text(&self) -> Vec<String>;
    /// The `--json` document (`{"schema": 1, ...}`).
    fn json(&self) -> serde_json::Value;
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
    pub fn execute(&self, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
        let res = match &self.command {
            CodegenCommands::Gitignore(a) => gitignore::execute(a, sink, global),
            CodegenCommands::Status(a) => status::execute(a, sink, global),
            CodegenCommands::Keep(a) => keep::execute_keep(a, sink, global),
            CodegenCommands::Discard(a) => keep::execute_discard(a, sink, global),
            CodegenCommands::Why(a) => why::execute(a, sink, global),
            CodegenCommands::Import(a) => import::execute(a, sink, global),
        };
        wrap_exit(res)
    }
}

/// Any error that is not already a [`CodegenExit`] becomes one with code 2.
pub fn wrap_exit(res: anyhow::Result<()>) -> anyhow::Result<()> {
    res.map_err(|e| {
        if e.downcast_ref::<CodegenExit>().is_some() {
            e
        } else {
            CodegenExit::failed(e).into()
        }
    })
}

/// The removed `heph tool gen-gitignore`: kept only to fail (Decision 7).
#[derive(clap::Args, Clone, Debug)]
pub struct RemovedGenGitignoreArgs {
    #[arg(long)]
    pub json: bool,
    /// Accepted and ignored, so an old invocation reaches the message.
    #[arg(hide = true, num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

/// The message the removed command fails with.
pub const GEN_GITIGNORE_REMOVED: &str = "heph tool gen-gitignore was removed; use heph tool codegen gitignore (and --check for the up-to-date test)";

/// `heph tool gen-gitignore`: prints the removal and exits 2.
pub fn removed_gen_gitignore(args: &RemovedGenGitignoreArgs) -> anyhow::Result<()> {
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "schema": JSON_SCHEMA,
                "error": "command_removed",
                "use": "heph tool codegen gitignore",
            })
        );
        return Err(CodegenExit {
            code: 2,
            error: None,
        }
        .into());
    }
    Err(CodegenExit::failed(anyhow::anyhow!(GEN_GITIGNORE_REMOVED)).into())
}

type Job<R> = Box<
    dyn FnOnce(
            Arc<Engine>,
            Arc<RequestState>,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<R>> + Send>>
        + Send,
>;

struct CodegenApp<R> {
    engine: Arc<Engine>,
    title: &'static str,
    json: bool,
    fail_fast: bool,
    job: Job<R>,
}

#[async_trait]
impl<R: Report> App for CodegenApp<R> {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new(self.title.to_string())
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new(self.title.to_string())
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());
        let out = BufferedStdout::new(&ctx);
        let res = (self.job)(Arc::clone(&self.engine), rs.clone()).await;
        let code = match &res {
            Ok(report) => {
                if self.json {
                    out.println(report.json().to_string());
                } else {
                    for line in report.text() {
                        out.println(line);
                    }
                }
                Ok(report.exit_code())
            }
            Err(_) => Ok(0),
        };
        out.close().await;
        let res = res.and(code);
        crate::commands::errors::finalize!(ctx, rs, res, code => {
            if code == 0 {
                Ok(())
            } else {
                Err(CodegenExit { code, error: None }.into())
            }
        })
    }
}

/// Run `job` as a codegen command: bootstrap the engine, run it inside the
/// TUI app, print its report, and end with its exit code.
fn run_cli<R, F, Fut>(
    title: &'static str,
    json: bool,
    sink: LogSink,
    global: &GlobalOptions,
    job: F,
) -> anyhow::Result<()>
where
    R: Report,
    F: FnOnce(Arc<Engine>, Arc<RequestState>) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<R>> + Send + 'static,
{
    let global = global.clone();
    let res = bootstrap::block_on(async move {
        let (engine, shutdown) = bootstrap::new_engine(&global)?;
        let app = CodegenApp {
            engine,
            title,
            json,
            fail_fast: global.fail_fast,
            job: Box::new(move |e, rs| Box::pin(job(e, rs))),
        };
        let interactive = tui::should_use_tui(global.no_tui) && !json;
        tui::run_app(app, sink, interactive, shutdown).await
    })
    .and_then(|r| r);
    // The app has ended (and `--json` never takes the TUI), so stdout is
    // ours. The error is still rendered on stderr by `main`.
    if json && let Some(doc) = json_error(&res) {
        println!("{doc}");
    }
    res
}

/// The `--json` document for a run that failed (exit 2): `{"schema": 1,
/// "error": "<message>"}`, so a consumer of stdout never reads an empty
/// document. `None` for success and for findings (exit 1), which printed
/// their report, and for an exit that already printed its own document.
fn json_error(res: &anyhow::Result<()>) -> Option<serde_json::Value> {
    let err = res.as_ref().err()?;
    let message = match err.downcast_ref::<CodegenExit>() {
        Some(exit) => match &exit.error {
            Some(e) if exit.code >= 2 => format!("{e:#}"),
            _ => return None,
        },
        None => format!("{err:#}"),
    };
    Some(serde_json::json!({ "schema": JSON_SCHEMA, "error": message }))
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
    }

    #[test]
    fn json_error_only_for_a_failure() {
        let failed: anyhow::Result<()> =
            Err(anyhow::anyhow!("opening the index").context("codegen status"));
        assert_eq!(
            json_error(&failed),
            Some(serde_json::json!({
                "schema": 1,
                "error": "codegen status: opening the index",
            }))
        );
        let wrapped: anyhow::Result<()> = Err(CodegenExit::failed(anyhow::anyhow!("boom")).into());
        assert_eq!(
            json_error(&wrapped),
            Some(serde_json::json!({ "schema": 1, "error": "boom" }))
        );
        // Findings printed their report; an exit with no error its own
        // document.
        assert_eq!(json_error(&Err(CodegenExit::findings().into())), None);
        let silent: anyhow::Result<()> = Err(CodegenExit {
            code: 2,
            error: None,
        }
        .into());
        assert_eq!(json_error(&silent), None);
        assert_eq!(json_error(&Ok(())), None);
    }

    #[test]
    fn import_hint_agrees_in_number() {
        assert!(import_hint(1).contains("1 file matches"));
        assert!(import_hint(412).contains("412 files match the heph section"));
    }
}
