#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! What `heph run` prints of a build target, and what it keeps of it, per flag:
//! `--raw` prints the redacted output, a plain build captures it, and `--shell`
//! hands over the terminal with no log and no redaction.
//!
//! These drive the real `Engine` and driver through the wrapper `run.rs` hands
//! them, with files where the real one has fds 1 and 2. What the wrapper does on
//! a real terminal, and that `--raw` gives an action no stdin, needs a process:
//! `crates/bin-e2e`.

mod common;

use common::Workspace;
use heph::commands::GlobalOptions;
use heph::commands::run::{RunArgs, execute, plain_output_wrapper};
use heph::engine::{InteractiveInner, InteractiveWrapper, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::tui::LogSink;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt as _;

const SECRET: &str = "extremely-secret-value";

/// Set a host variable for one test, which an `env` credential source reads.
/// Each test has its own, since they share the process's environment.
struct EnvVar(&'static str);

impl EnvVar {
    fn set(name: &'static str) -> Self {
        // SAFETY: each test sets a variable of its own, restored on drop.
        unsafe { std::env::set_var(name, SECRET) };
        Self(name)
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        // SAFETY: see `set`.
        unsafe { std::env::remove_var(self.0) };
    }
}

fn credential_build(ws: &Workspace, var: &str) {
    ws.write_build_file(
        "auth",
        &format!(
            r#"target(name = "t", driver = "auth.credential",
       sources = [heph.auth.env(["{var}"])],
       present = {{"env": {{"TOKEN": "${{{}}}"}}}})"#,
            var.to_lowercase()
        ),
    );
}

fn appending(path: &Path) -> tokio::fs::File {
    tokio::fs::File::from_std(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open capture file"),
    )
}

/// fds 1 and 2 of a run, as files.
struct Fds {
    _dir: tempfile::TempDir,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl Fds {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let (stdout, stderr) = (dir.path().join("stdout"), dir.path().join("stderr"));
        std::fs::write(&stdout, "").expect("create stdout");
        std::fs::write(&stderr, "").expect("create stderr");
        Self {
            _dir: dir,
            stdout,
            stderr,
        }
    }

    fn raw_wrapper(&self) -> InteractiveWrapper {
        let (stdout, stderr) = (self.stdout.clone(), self.stderr.clone());
        plain_output_wrapper(move || (appending(&stdout), appending(&stderr)))
    }

    fn stdout(&self) -> String {
        std::fs::read_to_string(&self.stdout).expect("read stdout")
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr).expect("read stderr")
    }
}

async fn run_with(ws: &Workspace, addr: &str, opts: ResultOptions) -> anyhow::Result<()> {
    let engine = Arc::clone(&ws.engine);
    let rs = engine.new_state();
    engine
        .result_addr(rs, &parse_addr(addr)?, OutputMatcher::All, &opts)
        .await
        .map(drop)
}

fn log_files(ws: &Workspace) -> Vec<PathBuf> {
    walk(&ws.engine.home.join("sandbox"))
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n == "log.txt"))
        .collect()
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found
}

/// Refused through the command's own entry point, before it has so much as
/// looked for a workspace: nothing can have built.
#[test]
fn raw_refused_with_shell() {
    let args = RunArgs {
        arg1: Some("//does/not:exist".to_string()),
        arg2: None,
        expr: None,
        force: false,
        shell: Some(String::new()),
        cat_out: false,
        list_out: false,
        copy_out: None,
        output: Vec::new(),
        frozen: false,
        log_lines: 10,
        raw: true,
        args: Vec::new(),
    };
    let err = execute(&args, LogSink::new_direct(), &GlobalOptions::default())
        .expect_err("`--raw --shell` has no meaning");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("`--raw` and `--shell` cannot be combined"),
        "{msg}"
    );
}

/// Only the action's output, each stream on its own fd, redacted as in `log.txt`;
/// and a cache hit runs no action, so it prints nothing.
#[tokio::test]
async fn raw_on_a_build_target_prints_plainly() -> anyhow::Result<()> {
    const VAR: &str = "HEPH_E2E_RAW_PRINTS_TOKEN";
    let _guard = EnvVar::set(VAR);
    let ws = Workspace::new();
    credential_build(&ws, VAR);
    ws.write_build_file(
        "app",
        r#"
target(name = "secret", driver = "bash", out = [], cache = False,
       credentials = ["//auth:t"],
       run = ["echo \"using $TOKEN\"", "echo on-stderr >&2"])
target(name = "cached", driver = "bash", out = "o.txt",
       run = ["echo building", "echo v > o.txt"])
"#,
    );

    let fds = Fds::new();
    let opts = ResultOptions {
        interactive: Some(fds.raw_wrapper()),
        ..Default::default()
    };
    run_with(&ws, "//app:secret", opts).await?;
    assert_eq!(fds.stdout(), "using [redacted]\n");
    assert_eq!(fds.stderr(), "on-stderr\n");

    let first = Fds::new();
    let opts = ResultOptions {
        interactive: Some(first.raw_wrapper()),
        ..Default::default()
    };
    run_with(&ws, "//app:cached", opts).await?;
    assert_eq!(first.stdout(), "building\n", "the first run executes");

    let hit = Fds::new();
    let opts = ResultOptions {
        interactive: Some(hit.raw_wrapper()),
        ..Default::default()
    };
    run_with(&ws, "//app:cached", opts).await?;
    assert_eq!(hit.stdout(), "", "a cache hit runs nothing");
    assert_eq!(hit.stderr(), "");
    Ok(())
}

/// The other side of [`build_target_shell_no_log_no_redaction`]: nothing about
/// the terminal changed what a plain build keeps.
#[tokio::test]
async fn plain_build_still_redacts_and_logs() -> anyhow::Result<()> {
    const VAR: &str = "HEPH_E2E_PLAIN_BUILD_TOKEN";
    let _guard = EnvVar::set(VAR);
    let ws = Workspace::new();
    credential_build(&ws, VAR);
    // Failing, so that the sandbox survives and `log.txt` can be read back.
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = [], cache = False,
       credentials = ["//auth:t"],
       run = ["echo \"using $TOKEN\"", "exit 3"])"#,
    );
    let res = run_with(&ws, "//app:a", ResultOptions::default()).await;
    assert!(res.is_err(), "the target exits 3");

    let logs = log_files(&ws);
    let bodies: Vec<String> = logs
        .iter()
        .map(|p| std::fs::read_to_string(p).unwrap_or_default())
        .collect();
    assert!(
        bodies.iter().any(|b| b.contains("using [redacted]")),
        "a log with the output scrubbed: {bodies:?}"
    );
    assert!(
        !bodies.iter().any(|b| b.contains(SECRET)),
        "no secret in a log: {bodies:?}"
    );
    Ok(())
}

/// A shell is the user's own session: it shows what they ask it to show, and
/// nothing of it is written down. Failing, so that the sandbox survives and the
/// absence of a `log.txt` means something.
#[tokio::test]
async fn build_target_shell_no_log_no_redaction() -> anyhow::Result<()> {
    const VAR: &str = "HEPH_E2E_SHELL_TOKEN";
    let _guard = EnvVar::set(VAR);
    let ws = Workspace::new();
    credential_build(&ws, VAR);
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = [], cache = False,
       credentials = ["//auth:t"], run = ["exit 0"])"#,
    );

    let fds = Fds::new();
    let (stdout, stderr) = (fds.stdout.clone(), fds.stderr.clone());
    let wrapper: InteractiveWrapper = Arc::new(move |inner: InteractiveInner| {
        let (mut out, mut err) = (appending(&stdout), appending(&stderr));
        Box::pin(async move {
            let mut typed: &[u8] = b"echo \"using ${TOKEN}\"\nexit 3\n";
            let result = inner(Some(&mut typed), Some(&mut out), Some(&mut err), None).await;
            out.flush().await?;
            err.flush().await?;
            result
        })
    });
    let opts = ResultOptions {
        shell: true,
        interactive: Some(wrapper),
        ..Default::default()
    };
    let res = run_with(&ws, "//app:a", opts).await;
    assert!(res.is_err(), "the shell exits 3");

    let seen = fds.stdout();
    assert!(
        seen.contains(&format!("using {SECRET}")),
        "shown as is: {seen}"
    );
    assert!(
        !walk(&ws.engine.home.join("sandbox")).is_empty(),
        "the failed shell's sandbox survives"
    );
    assert_eq!(log_files(&ws), Vec::<PathBuf>::new(), "no log.txt");
    Ok(())
}
