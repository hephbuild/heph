mod filterenv;
mod pty;
mod spec;

use anyhow::Context;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedRunRequest, ManagedRunResponse};
use hexecrunner::RunnerRef;
use hplugin::driver::sandbox::EnvValue;
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path};
use hplugin::driver::targetdef::{Input, InputMode, Output, TargetDef as EngineTargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr, outputartifact,
};
use hproc::proc_exec;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io;
use xxhash_rust::xxh3::Xxh3Default;

const SHELL_INIT_SH: &str = include_str!("./init.sh");

const EXEC_DEF_FORMAT_VERSION: u32 = 1;

/// Supplies the builtin-utility shim directory, on demand.
///
/// A closure rather than a path so the directory is materialized only when a
/// target actually runs — and a closure rather than a dependency on the
/// `coreutils` crate so that *this* crate, which most of the workspace links,
/// does not drag forty utility crates into every build and test binary that
/// merely wants the exec driver.
pub type CoreutilsShims = Arc<dyn Fn() -> anyhow::Result<PathBuf> + Send + Sync>;

pub struct Driver {
    name: String,
    /// PATH the driver injects into target processes. Empty falls back to a hardcoded default.
    search_path: Vec<String>,
    /// Workspace-wide default exec runner, from the driver's `runner:` option.
    ///
    /// A per-target field alone would mean editing every BUILD file to move a
    /// workspace into an environment, which is the wrong shape for the main use
    /// case. Resolved in `parse`, where it becomes the same hashed `Input` an
    /// explicit `runner =` would — a default that reached the child without
    /// reaching the cache key would serve every previously-cached artifact
    /// unchanged when it was switched on.
    default_runner: Option<String>,
    /// Whether this driver puts heph's builtin utilities on its targets' PATH,
    /// from the `coreutils:` option. On by default — the whole point is that a
    /// recipe behaves the same on both hosts without opting in. A driver built
    /// directly rather than from options starts off, because only a host that
    /// knows the heph home can supply the shims.
    coreutils_enabled: bool,
    /// The toolbox version to hash, and how to get its shim directory —
    /// supplied by the host, which is the thing that knows the heph home.
    /// `None` while the toolbox is off.
    coreutils: Option<(u32, CoreutilsShims)>,
    /// Resolved on first use and reused: the steady-state cost is one `stat`.
    coreutils_dir: std::sync::OnceLock<PathBuf>,
    wrap_run: fn(&std::path::Path, &[String]) -> anyhow::Result<Vec<String>>,
    wrap_run_shell: fn(&std::path::Path, &[String]) -> anyhow::Result<Vec<String>>,
}

#[derive(Clone, serde::Serialize)]
struct TargetDef {
    pub run: Vec<String>,
    /// Deps wired into SRC_*/LIST_* at runtime AND folded into the def hash
    /// (their structure invalidates cache when group membership changes).
    /// Built from `deps` and from transitive Deps with `hash=true, runtime=true`.
    pub dep_group_inputs: BTreeMap<String, Vec<Input>>,
    /// Deps wired into SRC_*/LIST_* at runtime but intentionally excluded
    /// from the def hash. Built from `runtime_deps` and from transitive
    /// Deps with `hash=false, runtime=true`. Changing their addresses must
    /// not invalidate the cache — that's the whole point of `runtime_deps`.
    pub runtime_dep_group_inputs: BTreeMap<String, Vec<Input>>,
    pub tool_group_inputs: BTreeMap<String, Vec<Input>>,
    /// Declared outputs (`out`), normalized and keyed by group. Folded into
    /// the def hash: the paths are wired into `$OUT`/`$OUT_<group>` and decide
    /// what the sandbox captures, so changing them is a semantic change to the
    /// target. Kept as a BTreeMap so the hash does not depend on `spec.outputs`
    /// HashMap iteration order.
    pub outputs: BTreeMap<String, Vec<Path>>,
    /// Declared `support_files`, normalized. Hashed for the same reason as
    /// `outputs` — they are packed into the target's artifact set.
    pub support_files: Vec<Path>,
    pub env: BTreeMap<String, String>,
    pub pass_env: BTreeMap<String, String>,
    pub runtime_pass_env: Vec<String>,
    pub runtime_env: HashMap<String, String>,
    /// Exec runner for this target, already resolved (per-target field, then
    /// the driver default, then none).
    ///
    /// Deliberately **absent from `Hash`**, like `hash_deps`. It reaches the
    /// cache key the same way they do — through the hashout of the `Input`
    /// `parse` emits for it, folded into `hashin` by the engine. Hashing the
    /// address here as well would be redundant, and worse: two runner targets
    /// at different addresses that emit byte-identical `runner.json` describe
    /// the same environment and should share cache entries, which hashing the
    /// address would prevent. Keeping it out also means every target that names
    /// no runner hashes byte-identically to before this field existed.
    pub runner: Option<TargetAddr>,
}

impl Hash for TargetDef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        EXEC_DEF_FORMAT_VERSION.hash(state);
        self.run.hash(state);
        self.dep_group_inputs.hash(state);
        // runtime_dep_group_inputs intentionally excluded — runtime_deps
        // (and runtime-only transitives) must not affect the cache key.
        self.tool_group_inputs.hash(state);
        self.outputs.hash(state);
        self.support_files.hash(state);
        self.env.hash(state);
        self.pass_env.hash(state);
        // runtime_pass_env and runtime_env intentionally excluded
    }
}

fn bash_args(so: Vec<String>, lo: Vec<String>) -> Vec<String> {
    // Bash also interprets a number of multi-character options. These options must appear on the command line
    // before the single-character options to be recognized.
    let mut args = vec!["bash".to_string(), "--noprofile".to_string()];

    args.extend(lo);
    args.push("-o".to_string());
    args.push("pipefail".to_string());
    args.extend(so);

    args
}

#[derive(serde::Serialize)]
struct InitLine {
    /// 1-based line number.
    n: usize,
    /// Line number padded to the width of the largest number, for `show`.
    label: String,
    /// The command text.
    text: String,
    /// The command text wrapped as a single bash single-quoted token, safe to
    /// drop into the `__heph_cmds` array even when `text` spans multiple lines
    /// or contains single quotes.
    quoted: String,
}

/// Wrap `s` in single quotes for bash, escaping embedded single quotes via the
/// `'\''` idiom. Newlines inside `s` are preserved literally.
fn bash_squote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Convert a dep-map group name into a valid environment-variable name
/// segment. POSIX env var names allow only `[A-Za-z0-9_]`; group names may
/// contain other characters (`.`, `-`, `/`, etc.), so we uppercase and
/// replace every char outside `[A-Z0-9_]` with `_`. The result is always
/// used with a leading `SRC_`/`OUT_`/`TOOL_` prefix, so the first-char
/// (no leading digit) rule is already satisfied by the caller.
fn env_key_segment(group: &str) -> String {
    group
        .chars()
        .map(|c| {
            let u = c.to_ascii_uppercase();
            if u.is_ascii_alphanumeric() || u == '_' {
                u
            } else {
                '_'
            }
        })
        .collect()
}

/// A `{group: [addr]}` spec attribute as a list ordered by group name.
///
/// Every such attribute arrives as a `HashMap`, whose iteration order is
/// randomized per instance. Consuming one in map order makes the resulting
/// `def.inputs` order — and anything derived from an input's position in it —
/// differ between processes, which is a moved def hash and a cache that can
/// never hit. Order by the group name instead, which is content, not layout.
fn sorted_by_group(m: HashMap<String, Vec<String>>) -> Vec<(String, Vec<String>)> {
    let mut v: Vec<(String, Vec<String>)> = m.into_iter().collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn render_shell_init(run: &[String]) -> anyhow::Result<String> {
    let cmds = (!run.is_empty()).then(|| run.join("\n"));

    // Width of the largest line number so the `|` column in `show` stays aligned.
    let width = run.len().to_string().len();
    let lines: Vec<InitLine> = run
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let n = i + 1;
            InitLine {
                n,
                label: format!("{n:>width$}"),
                quoted: bash_squote(text),
                text: text.clone(),
            }
        })
        .collect();

    let mut env = minijinja::Environment::new();
    env.add_template("init.sh", SHELL_INIT_SH)
        .context("compile init.sh template")?;
    env.get_template("init.sh")
        .context("load init.sh template")?
        .render(minijinja::context! { cmds, lines })
        .context("render init.sh template")
}

fn bash_args_shell(sandbox_dir: &std::path::Path, run: &[String]) -> anyhow::Result<Vec<String>> {
    let rendered = render_shell_init(run)?;
    let init_path = sandbox_dir.join("init.sh");
    std::fs::write(&init_path, rendered).context("write init.sh")?;

    Ok(bash_args(
        vec!["-i".to_string()],
        vec![
            "--rcfile".to_string(),
            init_path.to_string_lossy().into_owned(),
        ],
    ))
}

/// Threshold above which the bash command body is spilled to a script file
/// in the sandbox instead of being passed inline via `-c`. Keeps argv small
/// for tall generated scripts.
const BASH_C_INLINE_MAX: usize = 500;

pub fn bash_args_public(
    sandbox_dir: &std::path::Path,
    cmd: &str,
    termargs: Vec<String>,
) -> anyhow::Result<Vec<String>> {
    let mut args = if cmd.len() > BASH_C_INLINE_MAX {
        let script_path = sandbox_dir.join("cmd.sh");
        std::fs::write(&script_path, cmd).context("write cmd.sh")?;
        bash_args(
            vec![
                "-u".to_string(),
                "-e".to_string(),
                script_path.to_string_lossy().into_owned(),
            ],
            vec!["--norc".to_string()],
        )
    } else {
        bash_args(
            vec![
                "-u".to_string(),
                "-e".to_string(),
                "-c".to_string(),
                cmd.to_string(),
            ],
            vec!["--norc".to_string()],
        )
    };

    if !termargs.is_empty() {
        // https://unix.stackexchange.com/a/144519
        // We push "bash" as a placeholder for $0 before appending termargs
        args.push("bash".to_string());
        args.extend(termargs);
    }
    Ok(args)
}

impl Driver {
    pub fn new_exec() -> Self {
        Self {
            default_runner: None,
            name: "exec".to_string(),
            coreutils_enabled: false,
            coreutils: None,
            coreutils_dir: std::sync::OnceLock::new(),
            search_path: vec![],
            wrap_run: |_, run| Ok(run.to_vec()),
            wrap_run_shell: |sandbox_dir, run| {
                let joined: Vec<String> = if run.is_empty() {
                    Vec::new()
                } else {
                    vec![run.join(" ")]
                };
                bash_args_shell(sandbox_dir, &joined)
            },
        }
    }

    /// The default shell fallback managed drivers swap in when the inner driver
    /// doesn't support shell mode: an `exec` driver with `run = []` (an
    /// interactive bash session via `init.sh`). Built here because the fallback
    /// is this plugin's own exec driver.
    pub fn default_exec_shell_fallback()
    -> std::sync::Arc<hdriver_support::driver_managed::ShellFallback> {
        let mut config: std::collections::HashMap<String, hcore::htvalue::Value> =
            std::collections::HashMap::new();
        config.insert("run".to_string(), hcore::htvalue::Value::List(vec![]));
        std::sync::Arc::new(hdriver_support::driver_managed::ShellFallback {
            driver: std::sync::Arc::new(Driver::new_exec()),
            spec_template: std::sync::Arc::new(hplugin::provider::TargetSpec {
                addr: Default::default(),
                driver: "exec".to_string(),
                config,
                ..Default::default()
            }),
        })
    }

    pub fn new_bash() -> Self {
        Self {
            default_runner: None,
            name: "bash".to_string(),
            coreutils_enabled: false,
            coreutils: None,
            coreutils_dir: std::sync::OnceLock::new(),
            search_path: vec![],
            wrap_run: |sandbox_dir, run| {
                bash_args_public(sandbox_dir, run.join("\n").as_str(), vec![])
            },
            wrap_run_shell: bash_args_shell,
        }
    }

    pub fn from_options_exec(opts: &hplugin::config::Options) -> anyhow::Result<Self> {
        Ok(Self {
            search_path: decode_path(opts)?,
            default_runner: decode_runner(opts)?,
            coreutils_enabled: decode_coreutils(opts)?,
            ..Self::new_exec()
        })
    }

    pub fn from_options_bash(opts: &hplugin::config::Options) -> anyhow::Result<Self> {
        Ok(Self {
            search_path: decode_path(opts)?,
            default_runner: decode_runner(opts)?,
            coreutils_enabled: decode_coreutils(opts)?,
            ..Self::new_bash()
        })
    }

    /// Turn the toolbox on for a driver built directly rather than from
    /// options — the constructors used by tests and by the shell fallback.
    #[cfg(test)]
    #[must_use]
    fn with_coreutils_enabled_for_test(mut self) -> Self {
        self.coreutils_enabled = true;
        self
    }

    /// Hand the driver the toolbox: the version that reaches the cache key, and
    /// a way to get the shim directory when a target runs.
    ///
    /// A no-op unless `coreutils:` turned the toolbox on, so a host can call
    /// this unconditionally without deciding policy.
    #[must_use]
    pub fn with_coreutils(mut self, version: u32, shims: CoreutilsShims) -> Self {
        if self.coreutils_enabled {
            self.coreutils = Some((version, shims));
        }
        self
    }

    /// The shim directory to put on a target's PATH, materialized on first use,
    /// or `None` when the toolbox is off for this driver.
    ///
    /// Fallible on purpose. Degrading to `None` would run the target against the
    /// host's utilities while its cache key claims heph's — a silently wrong
    /// build, and the exact failure the version in that key exists to prevent.
    fn coreutils_dir(&self) -> anyhow::Result<Option<&std::path::Path>> {
        let Some((_, shims)) = self.coreutils.as_ref() else {
            // Configured on but never supplied is a host wiring bug, and the
            // wrong thing to shrug off: the target would run against the host's
            // utilities while nothing put heph's on its PATH.
            anyhow::ensure!(
                !self.coreutils_enabled,
                "`coreutils: true` is set but no shim directory was supplied to the \
                 {} driver — the host must call `with_coreutils`",
                self.name,
            );
            return Ok(None);
        };
        if let Some(dir) = self.coreutils_dir.get() {
            return Ok(Some(dir.as_path()));
        }
        let dir = shims().context("materialize the builtin-utility shim directory")?;
        Ok(Some(self.coreutils_dir.get_or_init(|| dir).as_path()))
    }

    /// The toolbox identity that reaches a target's cache key, or `None` when it
    /// is off — in which case nothing is hashed at all, so a workspace that
    /// never turns this on keeps the keys it has today.
    fn coreutils_version(&self) -> Option<u32> {
        self.coreutils.as_ref().map(|(version, _)| *version)
    }
}

fn spec_path_to_target_path(
    raw: &str,
    pkg: &hmodel::htpkg::PkgBuf,
    codegen: &CodegenMode,
) -> anyhow::Result<Path> {
    let path = hmodel::htpkg::join_rel_checked(pkg.as_str(), raw)
        .with_context(|| format!("resolving output path {raw:?} in package {pkg}"))?;
    let content = if ["*", "?", "["].iter().any(|&p| path.contains(p)) {
        Content::Glob(path)
    } else if path.ends_with('/') {
        Content::DirPath(path)
    } else {
        Content::FilePath(path)
    };
    Ok(Path {
        content,
        codegen_tree: codegen.clone(),
        collect: true,
    })
}

fn decode_path(opts: &hplugin::config::Options) -> anyhow::Result<Vec<String>> {
    hplugin::config::deny_unknown(
        "exec/bash/sh driver",
        opts,
        &["path", "runner", "coreutils"],
    )?;
    Ok(hplugin::config::decode_opt(opts, "exec/bash/sh driver", "path")?.unwrap_or_default())
}

/// `coreutils:` puts heph's builtin utilities on every target's PATH.
///
/// **On by default.** The point of shipping them is that a recipe behaves the
/// same on Linux and macOS without anyone opting in; a toolbox nobody enables
/// fixes nothing. `coreutils: false` is the escape hatch for a workspace that
/// genuinely wants the host's tools, and opting out is itself folded into the
/// def hash, so it is a cache-visible decision rather than a silent one.
fn decode_coreutils(opts: &hplugin::config::Options) -> anyhow::Result<bool> {
    Ok(hplugin::config::decode_opt(opts, "exec/bash/sh driver", "coreutils")?.unwrap_or(true))
}

/// The workspace-wide default runner, from `options.runner`.
///
/// Unlike its sibling `path` — which reaches the child's `PATH` and, today,
/// no def hash at all — this must be indistinguishable from an explicit
/// per-target `runner =` by the time `parse` is done. See `Driver::default_runner`.
fn decode_runner(opts: &hplugin::config::Options) -> anyhow::Result<Option<String>> {
    let runner: Option<String> =
        hplugin::config::decode_opt(opts, "exec/bash/sh driver", "runner")?;
    Ok(runner.filter(|r| !r.is_empty()))
}

/// RAII guard that restores the parent terminal's cooked mode when dropped.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        _ = crossterm::terminal::disable_raw_mode();
    }
}

async fn tee_stream(
    source: Option<impl tokio::io::AsyncRead + Unpin>,
    log: Arc<std::sync::Mutex<std::fs::File>>,
    mut sink: Option<&mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
    addr: &str,
    stream: &str,
    bytes_read: &std::sync::atomic::AtomicUsize,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Some(mut source) = source else { return };
    let mut buf = vec![0u8; 8192];
    let mut lost = SinkLoss::default();
    loop {
        match source.read(&mut buf).await {
            Ok(0) => break,
            Err(error) => {
                tracing::warn!(
                    addr,
                    stream,
                    %error,
                    bytes_read = bytes_read.load(std::sync::atomic::Ordering::Relaxed),
                    "pluginexec: error draining child output; log tail may be truncated"
                );
                break;
            }
            Ok(n) => {
                bytes_read.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                #[expect(
                    clippy::indexing_slicing,
                    reason = "n guaranteed <= buf.len() by AsyncRead contract"
                )]
                let slice = &buf[..n];
                if let Ok(mut g) = log.lock() {
                    drop(g.write_all(slice));
                }
                if let Some(ref mut out) = sink {
                    let wrote = out.write_all(slice).await;
                    // Flush immediately so interactive shells see each byte
                    // appear as it's typed (tokio::io::stdout is line-buffered
                    // when wired to a tty).
                    let flushed = out.flush().await;
                    // `tokio::io::stdout` buffers, so a rejected write usually
                    // surfaces at the flush rather than at `write_all` — take
                    // whichever failed.
                    if let Err(e) = wrote.and(flushed) {
                        lost.record(n, e);
                    }
                }
            }
        }
    }
    lost.finish(addr, stream);
}

/// Output the sink refused, and why.
///
/// The sink write used to be `drop(out.write_all(..).await)`. When it failed,
/// the target's output vanished from the user's terminal while `log.txt` kept
/// every byte — the two disagreed and nothing anywhere said why. That is how a
/// non-blocking stdout (see `tui::tty`, where the bug was) went unnoticed long
/// enough to read as a flaky test: `EAGAIN` on a full terminal queue is a
/// *silent* truncation, and it only shows up under output large enough to fill
/// the queue.
///
/// Counting is per-chunk, not per-byte: `write_all` does not report how far it
/// got before failing, so the whole chunk in flight is charged. The number is a
/// floor on what the terminal missed, and is reported as such.
#[derive(Default)]
struct SinkLoss {
    dropped_bytes: usize,
    first: Option<std::io::Error>,
}

impl SinkLoss {
    fn record(&mut self, chunk_len: usize, error: std::io::Error) {
        self.dropped_bytes = self.dropped_bytes.saturating_add(chunk_len);
        if self.first.is_none() {
            self.first = Some(error);
        }
    }

    /// Report once, at the end, rather than per failing chunk: the failure is
    /// almost always the same error repeating for every chunk that follows, and
    /// a warning per 8 KiB would bury the run's real output.
    fn finish(self, addr: &str, stream: &str) {
        if let Some(error) = self.first {
            tracing::warn!(
                addr,
                stream,
                dropped_bytes = self.dropped_bytes,
                %error,
                "heph could not write some of this target's output to the terminal, so at \
                 least this many bytes of it were not shown. The full output is in the \
                 target's log.txt"
            );
        }
    }
}

/// Tee chunks from a [`proc_exec::OutputReader`] into the log file and the
/// per-stream TUI sinks. Used in non-PTY mode, where the child's stdout and
/// stderr pipes are drained on dedicated `std::thread`s (inside `proc_exec`
/// on macOS) and surfaced to async-land over one `std::sync::mpsc`.
///
/// **One reader, both streams, one loop.** The obvious shape — a tee per
/// stream under `tokio::join!` — is wrong on macOS: `OutputReader::recv`
/// parks the worker in `block_in_place`, so whichever tee is polled first
/// owns the task until the child exits and the other stream is never read.
/// Its chunks pile up in the drain channel and nothing reaches `log.txt` or
/// the TUI until the target finishes. A build that is quiet on stdout and
/// chatty on stderr — a compile — is the common case, so that was the common
/// case. Merging the two streams into a single receiver makes fairness
/// automatic rather than something the scheduler has to arrange.
///
/// It also fixes `log.txt`: chunks are now appended in true arrival order
/// instead of one stream's entire output followed by the other's.
///
/// **Absorption is timed.** The child is throttled whenever this loop is
/// slower than the child writes — on macOS by the bounded drain channel, on
/// Linux by the 64 KiB kernel pipe. Backpressure is the design, but a
/// throttled target is otherwise indistinguishable from a slow one, so the
/// time is accumulated per stream and reported against the target's address.
/// Measuring here rather than in the drain is what makes the diagnostic name
/// a target and cover both platforms with no `cfg`.
///
/// `stdout_bytes`/`stderr_bytes` mirror `tee_stream`'s `bytes_read`: a
/// post-wait drain timeout reports how much of each stream it actually got
/// before giving up, rather than just "it timed out".
async fn tee_output<'io>(
    reader: Option<proc_exec::OutputReader>,
    log: Arc<std::sync::Mutex<std::fs::File>>,
    // One lifetime for both: the loop reborrows whichever sink the current
    // chunk belongs to, so the two must be interchangeable.
    mut stdout: Option<&'io mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
    mut stderr: Option<&'io mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
    addr: &str,
    stdout_bytes: &std::sync::atomic::AtomicUsize,
    stderr_bytes: &std::sync::atomic::AtomicUsize,
) {
    use tokio::io::AsyncWriteExt;
    let Some(mut reader) = reader else { return };
    let mut absorbed = SinkCost::default();
    let (mut lost_stdout, mut lost_stderr) = (SinkLoss::default(), SinkLoss::default());
    loop {
        let (stream, chunk) = match reader.recv().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            // One reader now carries both streams, so treating a read error as
            // EOF would stop teeing the *other* stream too — `log.txt` would
            // truncate with no note, and dropping the reader would SIGPIPE the
            // child, failing the target for a reason unrelated to the cause.
            // The failed stream has already retired itself (its drain dropped
            // its sender / `close(id)` ran), so the survivor runs on.
            Err(e) => {
                tracing::warn!(
                    addr,
                    error = %e,
                    "stopped reading one of this target's output streams"
                );
                continue;
            }
        };
        let started = std::time::Instant::now();
        let bytes_counter = match stream {
            proc_exec::StreamId::Stdout => stdout_bytes,
            proc_exec::StreamId::Stderr => stderr_bytes,
        };
        bytes_counter.fetch_add(chunk.len(), std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut g) = log.lock() {
            drop(g.write_all(&chunk));
        }
        let sink = match stream {
            proc_exec::StreamId::Stdout => stdout.as_mut(),
            proc_exec::StreamId::Stderr => stderr.as_mut(),
        };
        if let Some(out) = sink {
            let wrote = out.write_all(&chunk).await;
            // Flush immediately so an interactive consumer sees each chunk
            // as it appears rather than at process exit.
            let flushed = out.flush().await;
            // Same silent-truncation trap as the PTY path — see [`SinkLoss`].
            if let Err(e) = wrote.and(flushed) {
                match stream {
                    proc_exec::StreamId::Stdout => &mut lost_stdout,
                    proc_exec::StreamId::Stderr => &mut lost_stderr,
                }
                .record(chunk.len(), e);
            }
        }
        if absorbed.record(stream, started.elapsed()) {
            tracing::warn!(
                addr,
                %stream,
                "heph is writing this target's output more slowly than the target \
                 produces it, so the target is being paused. Reduce how much it \
                 prints, or run without a live terminal so the output is not rendered"
            );
        }
    }
    absorbed.finish(addr);
    lost_stdout.finish(addr, "stdout");
    lost_stderr.finish(addr, "stderr");
}

/// Cumulative time a target spent blocked while heph wrote the target's own
/// output, per stream.
///
/// Split out of [`tee_output`] so the hot loop stays a loop: `record` is two
/// adds and a compare per chunk.
#[derive(Default)]
struct SinkCost {
    stdout: std::time::Duration,
    stderr: std::time::Duration,
    warned: bool,
}

/// How much accumulated blocked time makes a target *demonstrably* throttled
/// by heph rather than slow on its own work. High enough that a chatty
/// compile writing to a file never trips it; low enough to catch a genuinely
/// stuck sink while the run is still going.
const SINK_STALL_WARN: std::time::Duration = std::time::Duration::from_secs(2);

impl SinkCost {
    /// Add this chunk's cost. Returns `true` exactly once — on the chunk that
    /// takes the *combined* cost past [`SINK_STALL_WARN`]. Separated from the
    /// emission so the threshold is testable without a subscriber.
    #[must_use]
    fn record(&mut self, stream: proc_exec::StreamId, elapsed: std::time::Duration) -> bool {
        let slot = match stream {
            proc_exec::StreamId::Stdout => &mut self.stdout,
            proc_exec::StreamId::Stderr => &mut self.stderr,
        };
        *slot = slot.saturating_add(elapsed);
        if self.warned || self.stdout.saturating_add(self.stderr) < SINK_STALL_WARN {
            return false;
        }
        self.warned = true;
        true
    }

    /// Always reported, threshold crossed or not — the number is the answer to
    /// "why did that target take so long", and it only exists here.
    fn finish(&self, addr: &str) {
        let total = self.stdout.saturating_add(self.stderr);
        if self.warned {
            tracing::warn!(
                addr,
                stalled_ms = total.as_millis(),
                stdout_ms = self.stdout.as_millis(),
                stderr_ms = self.stderr.as_millis(),
                "target finished; this much of its wall time was spent waiting for \
                 heph to write its output"
            );
        } else {
            tracing::debug!(
                addr,
                stalled_ms = total.as_millis(),
                stdout_ms = self.stdout.as_millis(),
                stderr_ms = self.stderr.as_millis(),
                "target output absorbed"
            );
        }
    }
}

/// PTY-mode stdin pump: copies bytes from the parent's stdin (TtyReader or
/// similar) into the PTY master via `AsyncPty`. Uses tokio `io::copy` since
/// `AsyncPty` is a normal tokio AsyncWrite over an `AsyncFd` — `EVFILT_READ`
/// / `EVFILT_WRITE` wake reliability is fine on macOS.
async fn pump_stdin_pty(
    mut src: &mut (dyn tokio::io::AsyncRead + Send + Sync + Unpin),
    mut sink: pty::AsyncPty,
    cancel: tokio::sync::oneshot::Receiver<()>,
) {
    use tokio::io::AsyncWriteExt;
    tokio::select! {
        _ = cancel => {}
        _ = io::copy(&mut src, &mut sink) => {}
    }
    drop(sink.shutdown().await);
}

/// Await `io` for up to [`proc_exec::DRAIN_DEADLINE`], reporting how long it
/// actually took and whether the deadline elapsed.
///
/// By the time this runs the child has already exited, so anything it wrote is
/// already read or sitting in the pipe — `io` arriving late means something
/// else (typically a surviving descendant sharing the same stdout/stderr fd)
/// still holds the write end open. The caller warns in that case rather than
/// silently discarding the timeout, so a truncated log.txt tail is diagnosable
/// instead of vanishing unremarked — mirroring `proc_exec`'s own abandonment
/// log, whose window this now shares.
///
/// **The deadline is `proc_exec`'s, not a local number.** It was an
/// independent 50 ms, which is not a bound on "a descendant is holding the
/// pipe" — it is a bound on *how promptly this task gets polled after the EOF
/// wake*, and under a burst of short-lived targets that loses the race
/// routinely. A `go` std build (a few hundred `sh -c 'mv …'` targets, each a
/// couple of milliseconds and silent) produced tens of these warnings per
/// couple of minutes with `stdout_bytes=0 stderr_bytes=0` — a `mv` forks
/// nothing, so there was never a descendant to abandon. Each false positive
/// cost that target 50 ms and, worse, abandoned the tee of a target whose
/// stderr is the only diagnostic it will ever produce. `DRAIN_DEADLINE` is
/// the window `proc_exec` already documents as the one knob for this, and
/// spending it only lengthens the genuinely-stuck case, which is a bug you
/// want to see.
///
/// Returns `(waited, timed_out)`. `timeout` polls the inner future before the
/// delay, so a drain that lands exactly on the deadline still counts as
/// finished.
async fn drain_bounded(io: impl std::future::Future<Output = ()>) -> (std::time::Duration, bool) {
    let started = std::time::Instant::now();
    let timed_out = tokio::time::timeout(proc_exec::DRAIN_DEADLINE, io)
        .await
        .is_err();
    (started.elapsed(), timed_out)
}

/// Pump bytes from an async source into a [`proc_exec::StdinPump`].
///
/// **The write runs on its own task, and that is load-bearing.** On macOS
/// `StdinPump::write_all` is a synchronous `write` under `block_in_place`,
/// which blocks — not `Pending` — once the child's 64 KiB stdin pipe fills
/// and the child is not reading. Sharing a task with [`tee_output`] would
/// then close a cycle with no participant able to break it: the tee stops
/// draining, the bounded drain channel fills, the child blocks in `write(2)`
/// on its *output*, so it never gets as far as reading the input we are
/// blocked writing. (Before the drain was bounded this healed by luck — the
/// child could still run to completion into an unbounded buffer and exit,
/// giving our write `EPIPE`.)
///
/// `src` is borrowed from the request and cannot be spawned, but the pump is
/// `'static`, so the split goes the other way: the borrowed reader stays on
/// this task and hands bytes to the spawned writer over a channel. Linux is
/// unaffected either way — its pump is a genuine `AsyncWrite` — but it runs
/// the same code so the two backends keep one shape.
///
/// Reproduced and covered by
/// `tests::test_run_slow_sink_does_not_deadlock_with_concurrent_stdin`: a
/// child that writes past the drain bound before reading stdin, against a
/// sink paced slower than it can drain, hangs indefinitely with the write
/// inlined on this task and completes once it is spawned. An earlier attempt
/// to build this reproducer (512 KiB of stdin into a child writing 4 MiB of
/// output, no artificial sink delay) did not wedge — the missing ingredient
/// was a consumer slow enough to leave the drain channel genuinely full,
/// which a real terminal or a loaded TUI can be and an in-memory `Vec` sink
/// never is.
async fn pump_stdin(
    src: &mut (dyn tokio::io::AsyncRead + Send + Sync + Unpin),
    mut sink: proc_exec::StdinPump,
    cancel: tokio::sync::oneshot::Receiver<()>,
) {
    use tokio::io::AsyncReadExt;
    // Small: this is a keystroke relay, and a deep queue would only delay the
    // EOF that `shutdown` below turns into the child's end-of-input.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    // Deliberately not awaited. If the child never reads its stdin this task
    // stays parked in `write` until the child exits and the pipe gives it
    // `EPIPE` — joining it here would drag that park back onto the tee's task,
    // which is the whole thing we are avoiding.
    tokio::spawn(async move {
        while let Some(chunk) = rx.recv().await {
            if sink.write_all(&chunk).await.is_err() {
                break;
            }
        }
        // Dropping the pump closes the write end, so the child sees EOF.
        drop(sink.shutdown().await);
    });

    let copy = async {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = match src.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            #[expect(
                clippy::indexing_slicing,
                reason = "n <= buf.len() by AsyncRead::read contract"
            )]
            let chunk = buf[..n].to_vec();
            if tx.send(chunk).await.is_err() {
                return;
            }
        }
    };
    tokio::select! {
        _ = cancel => {}
        _ = copy => {}
    }
    // Closes the channel, which ends the writer's loop and triggers the
    // shutdown that gives the child its EOF.
    drop(tx);
}

#[async_trait]
impl hdriver_support::driver_managed::ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: self.name.clone(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        spec::TargetSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let spec = spec::TargetSpec::from(&req.target_spec.config)?;

        let pkg = req.target_spec.addr.package.clone();

        // Dep groups opted into read-only staging (stage once, hardlink in)
        // rather than copied into every sandbox.
        let read_only_groups: std::collections::HashSet<String> =
            spec.read_only_deps.iter().cloned().collect();

        let build_dep_inputs = |deps: HashMap<String, Vec<String>>,
                                origin_prefix: &'static str,
                                hashed: bool,
                                runtime: bool|
         -> anyhow::Result<Vec<(String, Input)>> {
            let read_only_groups = &read_only_groups;
            // Sorted, not `HashMap` order: this is the order of `def.inputs`,
            // and the transitive collector numbers each merged sandbox by an
            // input's *position* in that list — a number that ends up in the
            // merged dep/tool ids, and so in this target's def hash. Left in
            // map order, a target with two or more dep groups hashes
            // differently in every process and never hits its cache.
            sorted_by_group(deps)
                .into_iter()
                .flat_map(|(k, v)| {
                    let pkg = pkg.clone();
                    let read_only = read_only_groups.contains(&k);
                    v.into_iter().enumerate().map(
                        move |(i, v)| -> anyhow::Result<(String, Input)> {
                            let annotations = if read_only {
                                BTreeMap::from([(
                                    hdriver_support::stage::READ_ONLY_ANNOTATION.to_string(),
                                    "true".to_string(),
                                )])
                            } else {
                                BTreeMap::new()
                            };
                            Ok((
                                k.parse()?,
                                Input {
                                    r#ref: TargetAddr::parse(&v, &pkg)?,
                                    mode: InputMode::Standard,
                                    origin_id: format!("{}|{}|{}", origin_prefix, k, i),
                                    annotations,
                                    hashed,
                                    runtime,
                                },
                            ))
                        },
                    )
                })
                .collect::<anyhow::Result<Vec<_>>>()
        };

        let dep_inputs = build_dep_inputs(spec.deps, "dep", true, true)?;
        let hash_dep_inputs = build_dep_inputs(spec.hash_deps, "hash_dep", true, false)?;
        let runtime_dep_inputs = build_dep_inputs(spec.runtime_deps, "runtime_dep", false, true)?;

        let mut dep_group_inputs: BTreeMap<String, Vec<Input>> = BTreeMap::new();
        for (group, input) in &dep_inputs {
            dep_group_inputs
                .entry(group.clone())
                .or_default()
                .push(input.clone());
        }

        let mut runtime_dep_group_inputs: BTreeMap<String, Vec<Input>> = BTreeMap::new();
        for (group, input) in &runtime_dep_inputs {
            runtime_dep_group_inputs
                .entry(group.clone())
                .or_default()
                .push(input.clone());
        }

        // Scratch references. `hashed: false, runtime: false` is the one
        // combination nothing else uses, and it is exactly right: a scratch
        // materializes no artifacts (its declaration has none) and must not touch
        // this target's cache key, because a target's outputs are required to be
        // identical whether its scratch is warm, cold, or absent. The edge exists
        // so the graph knows about it — which is what makes `heph query revdeps`
        // answer "who shares this cache?" and what turns a bad addr into an
        // ordinary `TargetNotFoundError`.
        //
        // Order is the declared order, deduped: a repeated reference would mount
        // one directory twice and set one env var twice, which is a BUILD-file
        // mistake worth naming rather than quietly collapsing.
        let mut seen_scratch: BTreeMap<String, usize> = BTreeMap::new();
        let mut scratch_inputs: Vec<Input> = Vec::with_capacity(spec.scratch.len());
        for (i, raw) in spec.scratch.iter().enumerate() {
            let r#ref = TargetAddr::parse(raw, &pkg)?;
            let key = r#ref.to_string();
            if let Some(first) = seen_scratch.insert(key.clone(), i) {
                anyhow::bail!(
                    "scratch {key} is referenced twice (positions {first} and {i}) — a scratch \
                     mounts at one path and sets one environment variable, so referencing it \
                     again does nothing; drop the duplicate"
                );
            }
            scratch_inputs.push(Input {
                r#ref,
                mode: InputMode::Standard,
                origin_id: format!("{}|{}", hdriver_support::scratch::SCRATCH_ORIGIN_PREFIX, i),
                annotations: BTreeMap::from([(
                    hdriver_support::scratch::SCRATCH_ANNOTATION.to_string(),
                    "true".to_string(),
                )]),
                hashed: false,
                runtime: false,
            });
        }

        let tool_inputs = sorted_by_group(spec.tools)
            .into_iter()
            .flat_map(|(k, v)| {
                let pkg = pkg.clone();
                v.into_iter()
                    .enumerate()
                    .map(move |(i, v)| -> anyhow::Result<(String, Input)> {
                        Ok((
                            k.parse()?,
                            Input {
                                r#ref: TargetAddr::parse(&v, &pkg)?,
                                mode: InputMode::Tool,
                                origin_id: format!("tool|{}|{}", k, i),
                                annotations: BTreeMap::from([(
                                    "unpack_root".to_string(),
                                    "tools".to_string(),
                                )]),
                                hashed: true,
                                runtime: true,
                            },
                        ))
                    })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let mut tool_group_inputs: BTreeMap<String, Vec<Input>> = BTreeMap::new();
        for (group, input) in &tool_inputs {
            tool_group_inputs
                .entry(group.clone())
                .or_default()
                .push(input.clone());
        }

        // `"*"` is a wildcard: pass through every host env var. Snapshotted at
        // parse time and hashed like any other pass_env (so the input hash
        // captures the whole environment — only use it on uncached targets).
        let pass_env: BTreeMap<String, String> = if spec.pass_env.iter().any(|n| n == "*") {
            std::env::vars().collect()
        } else {
            spec.pass_env
                .into_iter()
                .filter_map(|name| std::env::var(&name).ok().map(|val| (name, val)))
                .collect()
        };

        // Built before the def hash — `outputs` and `support_files` are part of
        // the hashed def, so they have to exist by the time we hash it.
        let output_groups = spec
            .outputs
            .iter()
            .map(|(k, v)| {
                Ok((
                    k.clone(),
                    v.iter()
                        .map(|p| spec_path_to_target_path(p, &pkg, &spec.codegen))
                        .collect::<anyhow::Result<Vec<_>>>()?,
                ))
            })
            .collect::<anyhow::Result<BTreeMap<String, Vec<Path>>>>()?;

        let support_files = spec
            .support_files
            .iter()
            .map(|p| spec_path_to_target_path(p, &pkg, &CodegenMode::None))
            .collect::<anyhow::Result<Vec<_>>>()?;

        // Per-target field wins, then the driver's workspace-wide default.
        // `"local"` is the explicit opt-out — the one value that is not an
        // address, so a per-package override of a workspace default can be
        // spelled without inventing a second knob.
        let (runner_spec, from_default) = if spec.runner.is_empty() {
            (self.default_runner.as_deref(), true)
        } else {
            (Some(spec.runner.as_str()), false)
        };
        let runner_spec = runner_spec.filter(|r| *r != "local");

        let runner = match runner_spec {
            None => None,
            Some(raw) => {
                let target = TargetAddr::parse(raw, &pkg).with_context(|| {
                    format!(
                        "`runner` must be a target address producing a runner.json, or the \
                         literal \"local\"; got {raw:?}"
                    )
                })?;
                // A runner target written with the `exec`/`bash` driver would
                // otherwise inherit the workspace-wide default and become its
                // own runner — the headline configuration cycling on the very
                // first build. Only the *implicit* default is excluded: an
                // explicit `runner = <self>` is a mistake the author made and
                // must surface as the dependency cycle it is, not be silently
                // turned into a local spawn.
                //
                // This is what `parse` can see. A runner target whose own
                // *dependencies* are exec targets still needs an explicit
                // `runner = "local"`; without one it gets a CycleError rather
                // than a hang, because the runner is a hashed Input and the
                // engine's synchronous cycle check covers it.
                if from_default && target.r#ref == req.target_spec.addr {
                    None
                } else {
                    Some(target)
                }
            }
        };

        // hashed + NOT runtime: the config keys the cache but never enters the
        // sandbox. `runtime: false` also keeps `collect_transitive_deps` from
        // merging the runner target's own tools/deps/env into every consumer.
        let runner_input = runner.as_ref().map(|target| Input {
            r#ref: target.clone(),
            mode: InputMode::Standard,
            origin_id: "runner".to_string(),
            annotations: BTreeMap::new(),
            hashed: true,
            runtime: false,
        });

        let def = TargetDef {
            run: spec.run,
            dep_group_inputs,
            runtime_dep_group_inputs,
            tool_group_inputs,
            outputs: output_groups,
            support_files: support_files.clone(),
            env: spec.env.into_iter().collect(),
            pass_env,
            runtime_pass_env: spec.runtime_pass_env,
            runtime_env: spec.runtime_env,
            runner: runner.clone(),
        };

        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("exec_def_{}", req.target_spec.addr.format())
            });
            def.hash(&mut h);
            // The builtin utilities sit on this target's PATH without being
            // declared, and nothing can tell which of them a shell command will
            // invoke without parsing it — so the whole toolbox's identity goes
            // in, or a heph upgrade that changes `cp` would keep serving
            // artifacts the old one built. Hashed only when the toolbox is on,
            // so a workspace that leaves it off keeps every key it has today.
            if let Some(version) = self.coreutils_version() {
                "coreutils".hash(&mut h);
                version.hash(&mut h);
            }

            format!("{:x}", h.finish()).into_bytes()
        };

        let outputs = def
            .outputs
            .iter()
            .map(|(group, paths)| Output {
                group: group.clone(),
                paths: paths.clone(),
            })
            .collect::<Vec<_>>();

        Ok(ParseResponse {
            target_def: EngineTargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs: dep_inputs
                    .into_iter()
                    .map(|(_, v)| v)
                    .chain(hash_dep_inputs.into_iter().map(|(_, v)| v))
                    .chain(runtime_dep_inputs.into_iter().map(|(_, v)| v))
                    .chain(tool_inputs.into_iter().map(|(_, v)| v))
                    .chain(runner_input)
                    .chain(scratch_inputs)
                    .collect(),
                outputs,
                support_files,
                cache: spec.cache.into(),
                pty: true,
                hash,
                transparent: false,
            },
        })
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        let mut def = req.target_def.clone();
        let mut xdef = def.def::<TargetDef>().clone();
        for tool in req.sandbox.tools {
            let input = Input {
                r#ref: tool.r#ref.clone(),
                mode: InputMode::Tool,
                origin_id: tool.id.clone(),
                // Tools are consumed read-only (executables on PATH, never
                // mutated by the target), so let the OS sandbox runner stage
                // them once into the shared stage dir and link them in rather
                // than copying every tool into every sandbox. `stage_per_file`
                // forces the per-file link path: the tool consumer below reads
                // each input's file list and flattens every tool file into a
                // `bin/` dir by basename, so it needs individual files listed —
                // not the single subtree-root symlink the default staging emits.
                annotations: BTreeMap::from([
                    ("unpack_root".to_string(), "tools".to_string()),
                    (
                        hdriver_support::stage::READ_ONLY_ANNOTATION.to_string(),
                        "true".to_string(),
                    ),
                    (
                        hdriver_support::stage::STAGE_PER_FILE_ANNOTATION.to_string(),
                        "true".to_string(),
                    ),
                ]),
                hashed: tool.hash,
                runtime: true,
            };
            xdef.tool_group_inputs
                .entry(tool.group)
                .or_default()
                .push(input.clone());
            def.inputs.push(input);
        }
        for dep in req.sandbox.deps {
            let input = Input {
                r#ref: dep.r#ref.clone(),
                mode: InputMode::Standard,
                origin_id: dep.id.clone(),
                annotations: BTreeMap::new(),
                hashed: dep.hash,
                runtime: dep.runtime,
            };
            // Route into runtime wiring only when the dep is meant to be
            // available at runtime. Hash-only transitive deps still need to
            // appear in `def.inputs` so the engine resolves them and folds
            // their hashout into hashin, but they must not be wired into
            // SRC_*/LIST_*.
            if dep.runtime {
                let target_map = if dep.hash {
                    &mut xdef.dep_group_inputs
                } else {
                    &mut xdef.runtime_dep_group_inputs
                };
                target_map.entry(dep.group).or_default().push(input.clone());
            }
            def.inputs.push(input);
        }
        for (name, env) in req.sandbox.env {
            match env.value {
                EnvValue::Pass => {
                    if env.hash {
                        if let Ok(value) = std::env::var(&name) {
                            xdef.pass_env.insert(name, value);
                        }
                    } else {
                        xdef.runtime_pass_env.push(name);
                    }
                }
                EnvValue::Literal(v) => {
                    if env.hash {
                        xdef.pass_env.insert(name, v);
                    } else {
                        xdef.runtime_env.insert(name, v);
                    }
                }
            }
        }

        def.hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("exec_def_tr_{}", def.addr.format())
            });
            xdef.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        def.set_def(xdef);

        Ok(ApplyTransitiveResponse { target_def: def })
    }

    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        self.run_inner(req, ctoken, false).await
    }

    fn supports_shell(&self) -> bool {
        true
    }

    async fn run_shell<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        self.run_inner(req, ctoken, true).await
    }
}

impl Driver {
    /// The PATH injected into target processes, formatted for both the child's
    /// env and the spawn-failure diagnostic. Empty `search_path` falls back to
    /// a hardcoded default.
    fn sandbox_path_display(&self) -> String {
        if self.search_path.is_empty() {
            ["/usr/local/bin", "/usr/bin", "/bin"].join(":")
        } else {
            self.search_path.join(":")
        }
    }

    async fn run_inner<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
        shell: bool,
    ) -> anyhow::Result<ManagedRunResponse> {
        hcore::hmemoizer::set_phase("pluginexec:sandbox_setup");
        let rreq = req.request;
        let def = rreq.target.def::<TargetDef>();

        let run = {
            if shell {
                (self.wrap_run_shell)(&rreq.sandbox_dir, &def.run)?
            } else {
                (self.wrap_run)(&rreq.sandbox_dir, &def.run)?
            }
        };

        if run.is_empty() {
            anyhow::bail!("`run` is empty")
        }

        let mut env = HashMap::<String, String>::new();
        if shell && let Ok(term) = std::env::var("TERM") {
            env.insert("TERM".to_string(), term);
        }
        // Only when nothing else supplies an environment. Under a runner the
        // environment is the runner's, and injecting `/usr/local/bin:/usr/bin:/bin`
        // here would put the host's copy of a tool ahead of the one the target
        // asked to run beside — silently, and inside a cache key that claims the
        // runner's environment. It travels as `PathPolicy::fallback` instead, so
        // a local spawn still gets it and an agent decides for itself.
        if def.runner.is_none() {
            env.insert("PATH".to_string(), self.sandbox_path_display());
        }
        env.insert(
            "WORKSPACE_ROOT".to_string(),
            req.sandbox_ws_dir.to_string_lossy().to_string(),
        );

        for (k, v) in &def.env {
            env.insert(k.clone(), v.clone());
        }

        let pkg_prefix = {
            let pkg = rreq.target.addr.package.as_str();
            if pkg.is_empty() {
                String::new()
            } else {
                format!("{}/", pkg)
            }
        };
        for output in &rreq.target.outputs {
            let key = if output.group.is_empty() {
                "OUT".to_string()
            } else {
                format!("OUT_{}", env_key_segment(&output.group))
            };
            let entry = env.entry(key).or_default();
            for path in &output.paths {
                match &path.content {
                    Content::Glob(_) => {}
                    Content::FilePath(p) | Content::DirPath(p) => {
                        if !entry.is_empty() {
                            entry.push(' ');
                        }
                        let rel = p.strip_prefix(&pkg_prefix).unwrap_or(p);
                        entry.push_str(rel);

                        let abs = req.sandbox_ws_dir.join(p);
                        let dir_to_create = match &path.content {
                            Content::FilePath(_) => abs.parent(),
                            Content::DirPath(_) => Some(abs.as_path()),
                            Content::Glob(_) => None,
                        };
                        if let Some(d) = dir_to_create {
                            std::fs::create_dir_all(d)
                                .with_context(|| format!("create output dir {}", d.display()))?;
                        }
                    }
                }
            }
        }

        // Merge hashed and runtime-only dep groups so each group ends up
        // with a single SRC_*/LIST_* env entry covering both sources.
        // `deps` come first to preserve their established input ordering;
        // `runtime_deps` (also `runtime=true` transitives with `hash=false`)
        // append after.
        let mut merged_dep_groups: BTreeMap<&str, Vec<&Input>> = BTreeMap::new();
        for (group, inputs) in &def.dep_group_inputs {
            merged_dep_groups
                .entry(group.as_str())
                .or_default()
                .extend(inputs.iter());
        }
        for (group, inputs) in &def.runtime_dep_group_inputs {
            merged_dep_groups
                .entry(group.as_str())
                .or_default()
                .extend(inputs.iter());
        }

        for (group, inputs) in &merged_dep_groups {
            let src_key = if group.is_empty() {
                "SRC".to_string()
            } else {
                format!("SRC_{}", env_key_segment(group))
            };

            let mut list_f = {
                let path = req
                    .sandbox_dir
                    .join("list")
                    .join(format!("dep_{}.list", group));
                env.entry(format!("LIST_{src_key}")).or_default().push_str(
                    path.as_path()
                        .to_str()
                        .expect("sandbox dir path must be valid UTF-8"),
                );
                std::fs::File::create(&path)
                    .with_context(|| format!("create dep list file {:?}", path))?
            };
            let entry = env.entry(src_key).or_default();

            for input in inputs {
                // Filter to Dep-type ManagedRunInputs only. A target may have
                // both Dep outputs and Support files sharing the same
                // origin_id; the support input has no list file and must not
                // leak into SRC_/LIST_ env routing.
                if let Some(m) = req.inputs.iter().find(|m| {
                    m.input.origin_id == input.origin_id
                        && matches!(
                            m.input.artifact.r#type,
                            hplugin::driver::inputartifact::Type::Dep
                        )
                }) {
                    let list_path = m.require_list_path()?;
                    let managed_list_f = std::fs::File::open(list_path).with_context(|| {
                        format!(
                            "open dep list file {:?} (origin_id={})",
                            list_path, input.origin_id
                        )
                    })?;
                    for line in std::io::BufReader::new(managed_list_f).lines() {
                        let line = line
                            .with_context(|| format!("read line from dep list {:?}", list_path))?;
                        if line.is_empty() {
                            continue;
                        }

                        if !entry.is_empty() {
                            entry.push(' ');
                        }
                        entry.push_str(&line);

                        list_f
                            .write_all(line.as_bytes())
                            .with_context(|| format!("write to dep list (group={group})"))?;
                        list_f.write_all("\n".as_bytes()).with_context(|| {
                            format!("write newline to dep list (group={group})")
                        })?;
                    }
                }
            }
        }

        let tool_bin_dir = if !def.tool_group_inputs.is_empty() {
            let bin_dir = req.sandbox_dir.join("bin");
            std::fs::create_dir_all(&bin_dir)
                .with_context(|| format!("create tool bin dir {:?}", bin_dir))?;

            // Symlink each unique `bin/<filename>` exactly once. Two tool
            // inputs producing the same filename — whether from the same
            // source target referenced through multiple groups, or from
            // distinct fs:* wrappers around the same on-disk binary — are
            // treated as the same logical entry. The address-level
            // invariant (no two engine inputs share `(r#ref, group)`) is
            // enforced upstream by `Sandbox::merge_sandbox`; here we just
            // skip the redundant symlink to avoid EEXIST.
            //
            // Multi-output tool refs produce N RunInputs sharing one
            // origin_id; the bridge appends every output's file path to the
            // same list file. engine/link.rs validates 1 FilePath per output.
            let mut linked: std::collections::HashSet<std::ffi::OsString> =
                std::collections::HashSet::new();
            for (group, inputs) in &def.tool_group_inputs {
                let tool_key = if group.is_empty() {
                    "TOOL".to_string()
                } else {
                    format!("TOOL_{}", env_key_segment(group))
                };
                // Per-group dedup for $TOOL_<G> values. `linked` is process-wide
                // (avoids EEXIST on symlink); `group_seen` is per-group so each
                // TOOL_<G> reflects that group's references without duplicates
                // when one group lists the same filename via multiple inputs.
                let mut group_seen: std::collections::HashSet<std::ffi::OsString> =
                    std::collections::HashSet::new();
                for input in inputs {
                    // Filter to Dep-type — Support inputs that travel with the
                    // tool target's deps share its origin_id but must not be
                    // symlinked into bin/.
                    let Some(m) = req.inputs.iter().find(|m| {
                        m.input.origin_id == input.origin_id
                            && matches!(
                                m.input.artifact.r#type,
                                hplugin::driver::inputartifact::Type::Dep
                            )
                    }) else {
                        continue;
                    };
                    let list_path = m.require_list_path()?;
                    let list_f = std::fs::File::open(list_path).with_context(|| {
                        format!(
                            "open tool list {:?} (group={group}, origin_id={})",
                            list_path, input.origin_id
                        )
                    })?;
                    let mut any = false;
                    for line in std::io::BufReader::new(list_f).lines() {
                        let file_path = line
                            .with_context(|| format!("read line from tool list {:?}", list_path))?;
                        if file_path.is_empty() {
                            continue;
                        }
                        any = true;
                        let filename = std::path::Path::new(&file_path)
                            .file_name()
                            .ok_or_else(|| {
                                anyhow::anyhow!("tool file path has no filename: {}", file_path)
                            })?
                            .to_os_string();
                        let bin_path = bin_dir.join(&filename);

                        if group_seen.insert(filename.clone()) {
                            let bin_path_str = bin_path.to_str().ok_or_else(|| {
                                anyhow::anyhow!("bin path is not valid UTF-8: {:?}", bin_path)
                            })?;
                            let entry = env.entry(tool_key.clone()).or_default();
                            if !entry.is_empty() {
                                entry.push(' ');
                            }
                            entry.push_str(bin_path_str);
                        }

                        if !linked.insert(filename.clone()) {
                            continue;
                        }
                        #[cfg(unix)]
                        std::os::unix::fs::symlink(&file_path, &bin_path).with_context(|| {
                            format!("symlink tool {file_path:?} -> {bin_path:?}")
                        })?;
                        #[cfg(not(unix))]
                        std::fs::copy(&file_path, &bin_path)
                            .with_context(|| format!("copy tool {file_path:?} -> {bin_path:?}"))?;
                    }
                    if !any {
                        anyhow::bail!("tool '{}' produced no files", input.origin_id);
                    }
                }
            }

            Some(bin_dir)
        } else {
            None
        };

        env.extend(def.pass_env.iter().map(|(k, v)| (k.clone(), v.clone())));

        // `"*"` passes through every host env var at run time (not hashed).
        if def.runtime_pass_env.iter().any(|n| n == "*") {
            env.extend(std::env::vars());
        } else {
            for name in &def.runtime_pass_env {
                if let Ok(value) = std::env::var(name) {
                    env.insert(name.clone(), value);
                }
            }
        }

        env.extend(def.runtime_env.iter().map(|(k, v)| (k.clone(), v.clone())));

        // Scratch caches: each declaration names the variable its tool reads the
        // directory from, so a consumer needs no wiring of its own — declaring
        // `env = "GOCACHE"` is what makes `scratch = [...]` sufficient.
        //
        // The value is the *canonical* slot path the host resolved, not the
        // in-sandbox symlink: tools bake absolute paths into their cache entries,
        // so every consumer must see one stable string or the cache restores and
        // is inert. Set after `runtime_env` and before PATH, and never hashed —
        // the path contains the engine home, so hashing it would make every cache
        // key machine-specific.
        //
        // A collision with the target's own env is rejected rather than resolved:
        // silently winning either way leaves one of the two settings inoperative
        // with nothing to see.
        for m in &rreq.scratch {
            let dir = m.dir.to_str().ok_or_else(|| {
                anyhow::anyhow!("scratch dir for {} is not valid UTF-8: {:?}", m.addr, m.dir)
            })?;
            if let Some(existing) = env.get(&m.env)
                && existing != dir
            {
                anyhow::bail!(
                    "scratch {} sets `{}`, but this target already sets it to {:?}. One would \
                     shadow the other — rename the variable on the scratch declaration, or drop \
                     it from this target's env",
                    m.addr,
                    m.env,
                    existing
                );
            }
            env.insert(m.env.clone(), dir.to_string());
        }

        // The target's own tools lead, wherever it ends up running — composed by
        // `hexecrunner` rather than spliced into the string here, because under
        // a runner the rest of `PATH` is not known until the runner (or its
        // agent) has had its say.
        //
        // The builtins go in the *suffix*, behind everything the environment
        // provides. They are what heph supplies rather than what the target
        // declared, so they fill a gap rather than win an argument: a workspace
        // that names a devenv or nix runner pinned that environment's `sed` on
        // purpose, and heph's arriving in front of it would be a silent
        // downgrade inside a cache key that claims the environment. A target
        // that declares its own tool still beats both — that is `prefix`.
        //
        // It also means the builtins do not reach a runner carrying the
        // environment out of band, which is what we want: the shim directory is
        // a host path holding symlinks into a host-platform binary, and a
        // container's filesystem is not this one.
        let coreutils_dir = self.coreutils_dir()?;
        let path_policy = hexecrunner::PathPolicy {
            prefix: tool_bin_dir
                .as_ref()
                .map(|d| d.as_os_str().to_os_string())
                .into_iter()
                .collect(),
            fallback: Some(std::ffi::OsString::from(self.sandbox_path_display())),
            suffix: coreutils_dir
                .map(|d| d.as_os_str().to_os_string())
                .into_iter()
                .collect(),
        };

        let output_log_path = req.sandbox_dir.join("log.txt");
        let output_log =
            std::fs::File::create(&output_log_path).with_context(|| "create log file")?;
        let output_log_file = Arc::new(std::sync::Mutex::new(output_log));

        // Shell mode runs the child attached to a freshly-allocated PTY so bash
        // sees a real terminal and runs interactively. The parent forwards
        // stdin/stdout via the PTY master through the same tee paths used by
        // the non-shell case (`tee_stream` on AsyncPty for PTY; `tee_output`
        // on the merged OutputReader for piped stdio).
        let pty_pair = if shell {
            Some(pty::open_pty().context("openpty")?)
        } else {
            None
        };

        // run is guaranteed non-empty by the bail! above, so [0] and [1..] are safe
        let (program, args) = {
            #[expect(
                clippy::indexing_slicing,
                reason = "run non-empty guaranteed by bail! check above"
            )]
            (run[0].clone(), run[1..].to_vec())
        };

        let args_os: Vec<OsString> = args.iter().map(OsString::from).collect();

        // execve caps total argv+envp at ARG_MAX and each entry at MAX_ARG_STRLEN.
        // Drop overlong entries and evict longest until under the limit.
        let env_vec: Vec<(String, String)> = env.into_iter().collect();
        let mut argv_for_filter: Vec<OsString> = Vec::with_capacity(1 + args_os.len());
        argv_for_filter.push(OsString::from(&program));
        argv_for_filter.extend(args_os.iter().cloned());
        let env_vec = filterenv::filter_long_env(env_vec, &argv_for_filter);
        let env_pairs: Vec<(OsString, OsString)> = env_vec
            .into_iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect();

        let spec = if let Some((master, slave)) = &pty_pair {
            // Inherit the parent's terminal size so bash can wrap and place the
            // prompt correctly. Falls back to 80x24 if the parent has no tty.
            let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            pty::set_winsize(master, rows, cols).context("set pty winsize")?;

            // Copy the parent's line discipline (ICRNL/ONLCR/etc.) onto the
            // slave so bash sees a standard cooked terminal. Must run BEFORE
            // we put the parent into raw mode below.
            pty::inherit_termios(slave).context("copy parent termios to pty slave")?;

            let stdin_fd = slave.try_clone().context("dup pty slave for stdin")?;
            let stdout_fd = slave.try_clone().context("dup pty slave for stdout")?;
            let stderr_fd = slave.try_clone().context("dup pty slave for stderr")?;
            // setsid+ctty: child becomes session leader and the inherited fd 0
            // becomes its controlling terminal. The supervisor's `killpg`
            // reaps the whole tree on hard-shutdown.
            proc_exec::Spec {
                program: PathBuf::from(program),
                args: args_os,
                env: env_pairs,
                cwd: req.sandbox_pkg_dir.clone(),
                stdin: proc_exec::StdioSpec::Fd(stdin_fd),
                stdout: proc_exec::StdioSpec::Fd(stdout_fd),
                stderr: proc_exec::StdioSpec::Fd(stderr_fd),
                setsid: true,
                ctty: true,
            }
        } else {
            // setsid: true so the child becomes its own process-group
            // leader (pid == pgid). Without this, any descendant that
            // double-forks (e.g. Go test runners spawning helper daemons)
            // gets reparented to launchd on the immediate child's exit
            // and keeps holding the stdout/stderr pipe write ends — the
            // drain threads' `read()` then never returns 0 and
            // `Handle::wait` blocks indefinitely waiting for EOF. With a
            // pgid, the supervisor sidecar's `killpg(pid, SIGKILL)` on
            // cancel/parent-death reaps the whole tree, and the bounded
            // drain-join in `Handle::wait` can kill stragglers with the
            // same call. No controlling terminal (ctty: false) since
            // these are non-interactive children.
            let stdin = if rreq.stdin.is_some() {
                proc_exec::StdioSpec::Piped
            } else {
                proc_exec::StdioSpec::Null
            };
            proc_exec::Spec {
                program: PathBuf::from(program),
                args: args_os,
                env: env_pairs,
                cwd: req.sandbox_pkg_dir.clone(),
                stdin,
                stdout: proc_exec::StdioSpec::Piped,
                stderr: proc_exec::StdioSpec::Piped,
                setsid: true,
                ctty: false,
            }
        };

        hcore::hmemoizer::set_phase("pluginexec:spawn");
        // Program/PATH/cwd are only formatted here, on the error path — `run`
        // and `req` are still fully owned locals at this point (only cloned
        // versions of their fields were moved into `spec` above), so no work
        // happens on the far more common spawn-succeeds path.
        // The runner was resolved at parse and is already a hashed input, so
        // the host's lookup here is a memoizer hit. The spawn error stays a
        // typed `io::Error` so the NotFound arm below can still render the
        // sandbox-PATH diagnostic — and `spawned_as` carries the program that
        // was *actually* executed, which under a runner is the wrapper rather
        // than the target's own command.
        let runner_ref = match &def.runner {
            Some(target) => RunnerRef::target(rreq.request_id, &target.r#ref),
            None => RunnerRef::local(),
        };
        let (spawned, spawned_as) =
            hexecrunner::spawn_io_with_path(runner_ref, spec, &path_policy, ctoken).await?;
        let mut handle = spawned.map_err(|e| {
            let program = run.first().map_or("", String::as_str);
            // Under a runner the program that failed to exec is the wrapper,
            // not the target's command — say which, or the message sends the
            // reader looking for the wrong binary.
            let via = match &def.runner {
                Some(target) => format!(
                    " (via exec runner {}, which spawned {:?})",
                    target.r#ref.format(),
                    spawned_as.program,
                ),
                None => String::new(),
            };
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!(
                    "spawn child process {program:?}{via}: {e} — not found in the driver's sandbox PATH ({path}). This PATH is set by the driver's `path` option in .hephconfig and is independent of the invoking shell's PATH — a program on your interactive PATH can still be missing here. Also check that the working directory {cwd:?} exists.",
                    path = self.sandbox_path_display(),
                    cwd = req.sandbox_pkg_dir,
                )
            } else {
                anyhow::Error::new(e).context(format!("spawn child process {program:?}{via}"))
            }
        })?;

        // Drop the parent's copy of the slave so the master sees EOF when the
        // child exits.
        let pty_master = pty_pair.map(|(master, _slave)| master);

        // Put the parent terminal into raw mode so keystrokes are forwarded
        // byte-by-byte to the child PTY without local echo or line buffering.
        // The child's PTY slave owns line discipline and echo.
        let _raw_guard = if shell {
            crossterm::terminal::enable_raw_mode()
                .ok()
                .map(|()| RawModeGuard)
        } else {
            None
        };

        // Signal that cancels the stdin pump once the child has exited. Without
        // it, shell mode would deadlock waiting on a parent-stdin read that
        // nothing intends to satisfy.
        let (stdin_cancel_tx, stdin_cancel_rx) = tokio::sync::oneshot::channel::<()>();

        // Build futures for the stdin pump and the output tee. The two modes
        // differ in plumbing — PTY uses AsyncPty over the master fd (tokio
        // IO driver, EVFILT_READ — reliable on macOS), pipe mode uses the
        // off-tokio OutputReader / StdinPump from proc_exec.
        //
        // Both live on one task under `join!` below, and in pipe mode the
        // stdin pump's `write_all` can park that task (it is a synchronous
        // write under `block_in_place` on macOS). Since `proc_exec::spawn`
        // bounds the drain, a child that is simultaneously ignoring >64 KiB
        // of stdin and producing more than the drain bound would deadlock:
        // we would be blocked writing its stdin while it blocks writing its
        // stdout. Unreachable as wired — pipe-mode stdin exists only for
        // interactive runs, where the source is a tty relay measured in
        // keystrokes — but it is the reason a future non-tty stdin source
        // needs its own task, not another `join!` arm.
        enum IoFutures<'r> {
            Pty {
                stdin: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'r>>,
                stdout: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'r>>,
            },
            Pipes {
                stdin: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'r>>,
                /// Single tee over both streams — see [`tee_output`].
                output: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'r>>,
            },
        }

        // Target addr for the drain diagnostics below, and per-stream byte
        // counters so a post-wait drain timeout can report how much of the
        // tail it actually got before giving up.
        let addr_str = rreq.target.addr.format();
        let stdout_bytes = std::sync::atomic::AtomicUsize::new(0);
        let stderr_bytes = std::sync::atomic::AtomicUsize::new(0);

        let io_futures: IoFutures<'_> = if let Some(master) = pty_master {
            let read_fd = master.try_clone().context("dup pty master for read")?;
            let reader = pty::AsyncPty::new(read_fd).context("async pty reader")?;
            let writer = pty::AsyncPty::new(master).context("async pty writer")?;

            let log_for_out = Arc::clone(&output_log_file);
            let stdout_fut = Box::pin(tee_stream(
                Some(reader),
                log_for_out,
                rreq.stdout,
                &addr_str,
                "pty",
                &stdout_bytes,
            ));

            let stdin_fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> =
                if let Some(src) = rreq.stdin {
                    Box::pin(pump_stdin_pty(src, writer, stdin_cancel_rx))
                } else {
                    drop(writer);
                    Box::pin(async {})
                };
            IoFutures::Pty {
                stdin: stdin_fut,
                stdout: stdout_fut,
            }
        } else {
            let stdin_pump = handle.take_stdin();
            let output_reader = handle.take_output();
            let log_for_out = Arc::clone(&output_log_file);
            let output_fut = Box::pin(tee_output(
                output_reader,
                log_for_out,
                rreq.stdout,
                rreq.stderr,
                &addr_str,
                &stdout_bytes,
                &stderr_bytes,
            ));
            let stdin_fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> =
                match (rreq.stdin, stdin_pump) {
                    (Some(src), Some(pump)) => Box::pin(pump_stdin(src, pump, stdin_cancel_rx)),
                    _ => Box::pin(async {}),
                };
            IoFutures::Pipes {
                stdin: stdin_fut,
                output: output_fut,
            }
        };

        hcore::hmemoizer::set_phase("pluginexec:wait_subprocess");
        // `spawn_wait` puts the wait on its own task, which is mandatory
        // rather than a preference: it parks its worker in `block_in_place`
        // until the child exits, so sharing a task with the IO pumps would
        // stop them draining and the child would wedge on a full pipe.
        // `proc_exec` owns that rule now — the wait is only reachable through
        // the spawn.
        //
        // Cancellation lives INSIDE the task via `wait_or_cancel`: it runs the
        // SIGINT → grace → SIGKILL escalation with no `tokio::time` timer, so a
        // Ctrl-C that races runtime teardown can't poll a timer on a
        // shutting-down runtime (the "context found, but it is being shutdown"
        // panic this design avoids). `ctoken` is a borrowed trait object, so we
        // hand the task an owned `clone_arc`; the original borrow stays usable
        // below for the post-wait cancellation check.
        let wait_handle = handle.spawn_wait(ctoken.clone_arc());
        tokio::pin!(wait_handle);

        // Drive the IO pumps alongside the wait. On a clean exit we briefly
        // drain residual IO before checking status; on cancellation
        // `wait_or_cancel` has already reaped the child, so we skip the drain
        // (the runtime may be tearing down) and surface `cancelled` below.
        let wait_res = match io_futures {
            IoFutures::Pty { stdin, stdout } => {
                let io = async {
                    tokio::join!(stdin, stdout);
                };
                tokio::pin!(io);
                tokio::select! {
                    wait_res = &mut wait_handle => {
                        // The child is gone, so nothing will read its stdin
                        // again; release the pump's borrow of the request.
                        _ = stdin_cancel_tx.send(());
                        if !ctoken.is_cancelled() {
                            hcore::hmemoizer::set_phase("pluginexec:post_wait_io_drain");
                            let (waited, timed_out) = drain_bounded(&mut io).await;
                            let pty_bytes = stdout_bytes.load(std::sync::atomic::Ordering::Relaxed);
                            if timed_out {
                                tracing::warn!(
                                    addr = addr_str.as_str(),
                                    pty_bytes,
                                    waited_ms = waited.as_millis(),
                                    "pluginexec: post-wait output drain timed out; log tail may be truncated"
                                );
                            } else {
                                tracing::debug!(
                                    addr = addr_str.as_str(),
                                    pty_bytes,
                                    waited_ms = waited.as_millis(),
                                    "pluginexec: post-wait output drain finished"
                                );
                            }
                        }
                        wait_res
                    }
                    _ = &mut io => {
                        hcore::hmemoizer::set_phase("pluginexec:post_io_wait");
                        (&mut wait_handle).await
                    }
                }
            }
            IoFutures::Pipes { stdin, output } => {
                let io = async {
                    tokio::join!(stdin, output);
                };
                tokio::pin!(io);
                tokio::select! {
                    wait_res = &mut wait_handle => {
                        // The child is gone, so nothing will read its stdin
                        // again; release the pump's borrow of the request.
                        _ = stdin_cancel_tx.send(());
                        if !ctoken.is_cancelled() {
                            hcore::hmemoizer::set_phase("pluginexec:post_wait_io_drain");
                            let (waited, timed_out) = drain_bounded(&mut io).await;
                            let out_bytes = stdout_bytes.load(std::sync::atomic::Ordering::Relaxed);
                            let err_bytes = stderr_bytes.load(std::sync::atomic::Ordering::Relaxed);
                            if timed_out {
                                tracing::warn!(
                                    addr = addr_str.as_str(),
                                    stdout_bytes = out_bytes,
                                    stderr_bytes = err_bytes,
                                    waited_ms = waited.as_millis(),
                                    "pluginexec: post-wait output drain timed out; log tail may be truncated"
                                );
                            } else {
                                tracing::debug!(
                                    addr = addr_str.as_str(),
                                    stdout_bytes = out_bytes,
                                    stderr_bytes = err_bytes,
                                    waited_ms = waited.as_millis(),
                                    "pluginexec: post-wait output drain finished"
                                );
                            }
                        }
                        wait_res
                    }
                    _ = &mut io => {
                        hcore::hmemoizer::set_phase("pluginexec:post_io_wait");
                        (&mut wait_handle).await
                    }
                }
            }
        };

        hcore::hmemoizer::set_phase("pluginexec:post_wait_status_check");
        // Cancellation takes precedence over whatever the wait task returned
        // (`wait_or_cancel` surfaces an io error on cancel) so we preserve the
        // `Err("cancelled")` contract callers and tests rely on.
        if ctoken.is_cancelled() {
            anyhow::bail!("cancelled");
        }
        let status = wait_res
            .context("wait task panicked")?
            .context("wait for child process")?;
        if !status.success() {
            // Carry a lazy handle to the log file rather than its bytes; the
            // diagnostic reads only the last N lines (the request's `--log-lines`)
            // at classification time. The file outlives the read — a failed
            // target's sandbox is reclaimed only at its next run.
            let log: std::sync::Arc<dyn hcore::hartifactcontent::Content> = std::sync::Arc::new(
                hcore::hartifactcontent::FileContent::new(output_log_path.clone()),
            );
            return Err(hplugin::error::ProcessFailed {
                status: status.to_string(),
                log,
            }
            .into());
        }
        hcore::hmemoizer::set_phase("pluginexec:post_wait_done");

        Ok(ManagedRunResponse {
            artifacts: vec![outputartifact::OutputArtifact {
                group: "".to_string(),
                name: "log.txt".to_string(),
                r#type: outputartifact::Type::Log,
                content: outputartifact::Content::File(outputartifact::ContentFile {
                    source_path: output_log_path
                        .to_str()
                        .ok_or_else(|| anyhow::anyhow!("log path is not valid UTF-8"))?
                        .parse()?,
                    out_path: "log.txt".to_string(),
                    x: false,
                    // Log lives in the sandbox (cleaned after caching): never
                    // passthrough — it must be packed into the cache.
                    passthrough: false,
                }),
                hashout: "".to_string(),
            }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use enclose::enclose;
    use hcore::hasync::StdCancellationToken;
    use hdriver_support::driver_managed::ManagedDriver;
    use hmodel::htaddr::{Addr, parse_addr};
    use hplugin::driver::RunRequest;
    use hplugin::driver::targetdef::CacheConfig;

    /// Captures `tracing` events emitted on the calling OS thread while the
    /// returned guard is alive. `#[tokio::test]`'s default `current_thread`
    /// flavor keeps every task (including ones `tokio::spawn`ed by the code
    /// under test) on this one thread, so the thread-local default this
    /// installs covers the whole drive, not just the top-level future.
    #[derive(Clone, Default)]
    struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut guard = self
                .0
                .lock()
                .map_err(|_e| std::io::Error::other("captured log mutex poisoned"))?;
            guard.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl CapturedLog {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("captured log mutex").clone())
                .expect("captured log is utf8")
        }
    }

    fn capture_tracing() -> (tracing::subscriber::DefaultGuard, CapturedLog) {
        let captured = CapturedLog::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        (tracing::subscriber::set_default(subscriber), captured)
    }

    /// `AsyncRead` double that always fails — `tee_stream`'s real IO error
    /// path (as opposed to clean `Ok(0)` EOF) has no other way to reach it,
    /// since a real pipe/PTY essentially never produces a read error in a
    /// controlled test.
    struct FailingReader;

    impl tokio::io::AsyncRead for FailingReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("synthetic read failure")))
        }
    }

    fn log_file(dir: &tempfile::TempDir) -> Arc<std::sync::Mutex<std::fs::File>> {
        let file = std::fs::File::create(dir.path().join("log.txt")).expect("create log file");
        Arc::new(std::sync::Mutex::new(file))
    }

    /// `AsyncWrite` double that swallows writes and then refuses to flush with
    /// `WouldBlock` — the shape a *non-blocking* stdout takes once the
    /// terminal's output queue fills. Refusing at the flush rather than the
    /// write is not incidental: `tokio::io::stdout` buffers, so that is where
    /// the real `EAGAIN` surfaced.
    struct WouldBlockSink;

    impl tokio::io::AsyncWrite for WouldBlockSink {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "synthetic EAGAIN",
            )))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Output the terminal refuses must be *reported*, not discarded.
    ///
    /// This is the regression for a silent truncation: the sink write was
    /// `drop(out.write_all(..).await)`, so when stdout was accidentally
    /// non-blocking (see `tui::tty`) heph read the target's output, wrote every
    /// byte to `log.txt`, and dropped an arbitrary tail of it on the way to the
    /// terminal — with nothing logged, and the two records silently disagreeing.
    /// A user's only symptom was output that stopped mid-stream.
    #[tokio::test]
    async fn tee_stream_reports_output_the_sink_refused() {
        let (guard, log) = capture_tracing();
        let tmp = tempfile::tempdir().expect("tempdir");
        let bytes_read = std::sync::atomic::AtomicUsize::new(0);
        let payload = vec![b'x'; 4096];
        let mut sink = WouldBlockSink;

        tee_stream(
            Some(&payload[..]),
            log_file(&tmp),
            Some(&mut sink),
            "//pkg:target",
            "pty",
            &bytes_read,
        )
        .await;
        drop(guard);

        let text = log.text();
        assert!(
            text.contains("could not write some of this target's output"),
            "the refused output was not reported: {text}"
        );
        assert!(text.contains("//pkg:target"), "missing addr field: {text}");
        // One 8 KiB read covers the whole payload, so the count is exact rather
        // than merely a floor here.
        assert!(
            text.contains("dropped_bytes=4096"),
            "missing or wrong dropped byte count: {text}"
        );
        assert!(
            text.contains("synthetic EAGAIN"),
            "missing underlying error: {text}"
        );
    }

    #[tokio::test]
    async fn tee_stream_logs_real_io_errors() {
        let (guard, log) = capture_tracing();
        let tmp = tempfile::tempdir().expect("tempdir");
        let bytes_read = std::sync::atomic::AtomicUsize::new(0);

        tee_stream(
            Some(FailingReader),
            log_file(&tmp),
            None,
            "//pkg:target",
            "stdout",
            &bytes_read,
        )
        .await;
        drop(guard);

        let text = log.text();
        assert!(
            text.contains("error draining child output"),
            "missing diagnostic: {text}"
        );
        assert!(text.contains("//pkg:target"), "missing addr field: {text}");
        assert!(text.contains("stdout"), "missing stream field: {text}");
        assert!(
            text.contains("synthetic read failure"),
            "missing underlying error: {text}"
        );
    }

    // `tee_output`'s own read-error path (the `Err(e) => { warn!(...); continue }`
    // arm — see its doc comment on why a merged reader must not treat one
    // stream's error as EOF for both) has no equivalent synthetic-fault test
    // here: `proc_exec::OutputReader` wraps a real drain channel with no public
    // constructor for injecting an IO error, the same constraint that made
    // `tee_chunks`'s `ChunkSource` seam necessary before this merge — and that
    // seam doesn't carry over to a merged single-reader design. The behavior
    // it exists to prove (a read error does not stop the surviving stream) is
    // exercised in proc_exec's own suite instead.

    /// Regression for the post-wait drain: before the fix, whether
    /// `tokio::time::timeout` elapsed was discarded (`_ = timeout(...).await`)
    /// so a truncated tail vanished silently. `drain_bounded` is the extracted
    /// decision — this proves it reports the timeout when time runs out on
    /// stalled IO (a stand-in for a stray descendant still holding the pipe
    /// open), which is what `run_inner` turns into `tracing::warn!` on both
    /// the PTY and pipes branches.
    ///
    /// Time is asserted against `proc_exec::DRAIN_DEADLINE` rather than a
    /// literal: the point of the change this covers is that the two are one
    /// knob, so a future divergence should fail here.
    #[tokio::test]
    async fn drain_bounded_reports_the_deadline_elapsing() {
        let (waited, timed_out) = drain_bounded(std::future::pending::<()>()).await;
        assert!(timed_out, "a drain that never finishes must be reported");
        assert!(
            waited >= proc_exec::DRAIN_DEADLINE,
            "waited {waited:?}, shorter than the shared deadline \
             {:?} — the window is not proc_exec's",
            proc_exec::DRAIN_DEADLINE
        );
    }

    #[tokio::test]
    async fn drain_bounded_stays_quiet_when_io_finishes_in_time() {
        let (waited, timed_out) = drain_bounded(async {}).await;
        assert!(!timed_out);
        assert!(waited < proc_exec::DRAIN_DEADLINE, "waited {waited:?}");
    }

    /// The failing-target case the drain bound must not break: a descendant
    /// holding the pipe write end open makes EOF unreachable, so the drain is
    /// abandoned — but everything the child itself wrote was already in the
    /// pipe when it exited, and `log.txt` is the *only* diagnostic a failed
    /// target produces. Abandoning must therefore never cost the tail.
    ///
    /// `sleep` inherits stdout/stderr and outlives the deadline, so the
    /// abandon path is taken deterministically rather than by racing a wake.
    /// The outer timeout is the test's failure mechanism, not the mechanism
    /// under test: without a bound, a regression here hangs the suite instead
    /// of failing it.
    #[tokio::test]
    async fn stray_descendant_does_not_truncate_a_failed_targets_log() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "sleep 5 & echo boom >&2; exit 3".to_string(),
                ],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            // Pipes, not a PTY: the PTY path shares one fd for both streams
            // and is not where the std build's targets run.
            pty: false,
            hash: vec![],
            transparent: false,
        };

        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };

        let res = tokio::time::timeout(
            proc_exec::DRAIN_DEADLINE * 20,
            driver.run(make_req(req), &ctoken),
        )
        .await
        .expect("the run must end on the drain bound, not wait out the descendant");
        assert!(res.is_err(), "exit 3 must fail the target");

        let log = std::fs::read_to_string(tmp.path().join("log.txt"))?;
        assert!(
            log.contains("boom"),
            "the child's own stderr must survive the abandoned drain, got {log:?}"
        );

        Ok(())
    }

    #[test]
    fn env_key_segment_sanitizes_invalid_chars() {
        // Plain names just uppercase.
        assert_eq!(env_key_segment("group"), "GROUP");
        assert_eq!(env_key_segment("G1_a"), "G1_A");
        // `.`, `-`, `/` (and other punctuation) become `_`, so the full
        // `SRC_`/`OUT_`/`TOOL_` env var name stays POSIX-valid.
        assert_eq!(env_key_segment("my-group"), "MY_GROUP");
        assert_eq!(env_key_segment("my.group"), "MY_GROUP");
        assert_eq!(env_key_segment("a/b:c"), "A_B_C");
        // Non-ASCII collapses to `_` rather than leaking bytes.
        assert_eq!(env_key_segment("café"), "CAF_");
    }

    #[test]
    fn spec_path_to_target_path_normalizes_and_classifies() {
        use hmodel::htpkg::PkgBuf;

        let pkg = PkgBuf::from("a/b/rest");

        // `./`-prefixed output path no longer leaks a `pkg/./sub` smell.
        let file = spec_path_to_target_path("./openapi/X.yaml", &pkg, &CodegenMode::Copy).unwrap();
        assert!(
            matches!(&file.content, Content::FilePath(p) if p == "a/b/rest/openapi/X.yaml"),
            "got {}",
            file.content
        );
        assert_eq!(file.codegen_tree, CodegenMode::Copy);

        // Trailing slash classifies as a directory output.
        let dir = spec_path_to_target_path("./gen/", &pkg, &CodegenMode::None).unwrap();
        assert!(
            matches!(&dir.content, Content::DirPath(p) if p == "a/b/rest/gen/"),
            "got {}",
            dir.content
        );

        // Glob metacharacters survive normalization.
        let glob = spec_path_to_target_path("./gen/**/*.go", &pkg, &CodegenMode::None).unwrap();
        assert!(
            matches!(&glob.content, Content::Glob(p) if p == "a/b/rest/gen/**/*.go"),
            "got {}",
            glob.content
        );

        // A `..` that escapes the workspace root (more `..` than path depth) is a
        // hard error.
        let err = spec_path_to_target_path("../../../../etc/passwd", &pkg, &CodegenMode::None)
            .unwrap_err();
        assert!(
            err.to_string().contains("resolving output path"),
            "got {err}"
        );
    }

    fn make_req<'a, 'io>(request: RunRequest<'a, 'io>) -> ManagedRunRequest<'a, 'io> {
        let path = request.sandbox_dir.clone();
        ManagedRunRequest {
            request,
            sandbox_dir: path.clone(),
            sandbox_ws_dir: path.clone(),
            sandbox_pkg_dir: path,
            inputs: vec![],
        }
    }

    #[test]
    fn bash_args_public_spills_long_cmd_to_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let short = "echo hi";
        let short_args = bash_args_public(dir.path(), short, vec![]).expect("short");
        assert!(short_args.iter().any(|a| a == "-c"));
        assert!(short_args.iter().any(|a| a == short));
        assert!(!dir.path().join("cmd.sh").exists());

        let long = "x".repeat(BASH_C_INLINE_MAX + 1);
        let long_args = bash_args_public(dir.path(), &long, vec![]).expect("long");
        assert!(!long_args.iter().any(|a| a == "-c"));
        let script = dir.path().join("cmd.sh");
        assert!(script.exists());
        assert_eq!(std::fs::read_to_string(&script).expect("read"), long);
        assert!(
            long_args
                .iter()
                .any(|a| a == script.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn from_options_exec_no_path() {
        let opts = hplugin::config::Options::new();
        let d = Driver::from_options_exec(&opts).expect("from_options");
        assert_eq!(d.name, "exec");
        assert!(d.search_path.is_empty());
    }

    #[test]
    fn from_options_exec_reads_path() {
        let mut opts = hplugin::config::Options::new();
        opts.insert(
            "path".to_string(),
            serde_yaml::from_str("[/usr/bin, /bin]").expect("yaml"),
        );
        let d = Driver::from_options_exec(&opts).expect("from_options");
        assert_eq!(d.search_path, vec!["/usr/bin", "/bin"]);
    }

    #[test]
    fn from_options_bash_rejects_unknown_key() {
        let mut opts = hplugin::config::Options::new();
        opts.insert("bogus".to_string(), serde_yaml::Value::Bool(true));
        let err = Driver::from_options_bash(&opts).err().expect("must error");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn test_render_shell_init_with_cmds() {
        let run = vec!["echo hi".to_string(), "ls -la".to_string()];
        let out = render_shell_init(&run).expect("render");
        assert!(out.contains("run()"), "missing run() definition: {out}");
        assert!(out.contains("xrun()"), "missing xrun() definition: {out}");
        assert!(out.contains("echo hi\nls -la"), "cmds not joined: {out}");
        assert!(!out.contains("{{"), "template tokens left: {out}");
    }

    #[test]
    fn test_render_shell_init_numbered_runs_and_show() {
        let run = vec![
            "echo one".to_string(),
            "echo two".to_string(),
            "echo three".to_string(),
        ];
        let out = render_shell_init(&run).expect("render");

        // Per-line run/xrun functions exist for every line, each a tiny O(1)
        // delegator to the shared helper (no per-line command duplication).
        for n in 1..=3 {
            assert!(
                out.contains(&format!("run{n}() {{ __heph_run_upto {n}; }}")),
                "missing run{n} delegator: {out}"
            );
            assert!(
                out.contains(&format!("xrun{n}() {{ __heph_run_upto {n} x; }}")),
                "missing xrun{n} delegator: {out}"
            );
        }

        // Commands live once in the array; runN replays a prefix via the helper.
        assert!(out.contains("__heph_cmds=("), "missing cmds array: {out}");
        assert!(out.contains("'echo one'"), "array missing line 1: {out}");
        assert!(out.contains("'echo three'"), "array missing line 3: {out}");
        // Each command is materialized exactly once as a quoted array element —
        // no per-line duplication (the old runN bodies were O(n^2)).
        assert_eq!(
            out.matches("'echo two'").count(),
            1,
            "command should appear once as a quoted array element: {out}"
        );

        // show prints raw commands, showl prints numbered "N | line" form.
        assert!(out.contains("show()"), "missing show: {out}");
        assert!(out.contains("showl()"), "missing showl: {out}");
        assert!(out.contains("1 │ echo one"), "showl line 1: {out}");
        assert!(out.contains("3 │ echo three"), "showl line 3: {out}");
        assert!(
            out.contains("echo one\necho two\necho three"),
            "show raw cmds: {out}"
        );
    }

    #[test]
    fn test_render_shell_init_pads_line_numbers() {
        // 10 lines -> numbers 1..10, width 2, so "1" is right-padded to " 1".
        let run: Vec<String> = (1..=10).map(|i| format!("echo {i}")).collect();
        let out = render_shell_init(&run).expect("render");
        assert!(out.contains(" 1 │ echo 1"), "line 1 not padded: {out}");
        assert!(out.contains("10 │ echo 10"), "line 10 misaligned: {out}");
    }

    #[test]
    fn test_render_shell_init_does_not_html_escape_cmds() {
        // The template renders a shell script, not HTML — special chars must pass
        // through verbatim, otherwise the generated bash is corrupted.
        let run = vec!["a && b < c > d \"e\" 'f'".to_string()];
        let out = render_shell_init(&run).expect("render");
        assert!(
            out.contains("a && b < c > d \"e\" 'f'"),
            "shell chars were escaped: {out}"
        );
    }

    #[test]
    fn test_bash_squote_escapes_single_quotes() {
        // Commands like `awk -F'"' '/Dir/'` carry single quotes — the array
        // element must use the `'\''` idiom so eval re-parses the original text.
        assert_eq!(bash_squote("echo hi"), "'echo hi'");
        assert_eq!(bash_squote("a'b"), "'a'\\''b'");
        // Multi-line command stays one token (newline preserved inside quotes).
        assert_eq!(bash_squote("if x; then\n  y\nfi"), "'if x; then\n  y\nfi'");
    }

    #[test]
    fn test_render_shell_init_quotes_array_elements() {
        // A command containing single quotes must be safely single-quoted in the
        // __heph_cmds array (otherwise the generated bash fails to parse).
        let run = vec!["awk -F'\"' '/Dir/'".to_string()];
        let out = render_shell_init(&run).expect("render");
        assert!(
            out.contains("'awk -F'\\''\"'\\'' '\\''/Dir/'\\'''"),
            "array element not safely single-quoted: {out}"
        );
    }

    // Functionally validate that the generated bash parses and that `runN`
    // replays exactly the first N commands — including a command with single
    // quotes and a multi-line compound statement (the cases the array+eval
    // design has to get right).
    #[test]
    fn test_render_shell_init_runn_executes_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let init_path = dir.path().join("init.sh");
        let run = vec![
            "printf 'a\\n' >> out.txt".to_string(),
            "if true; then\n  printf 'b\\n' >> out.txt\nfi".to_string(),
            "printf 'c\\n' >> out.txt".to_string(),
        ];
        std::fs::write(&init_path, render_shell_init(&run).expect("render")).expect("write");

        // Source the rcfile (defines the functions), then run a 2-line prefix.
        let output = std::process::Command::new("bash")
            .arg("--norc")
            .arg("-c")
            .arg(format!(
                "source '{}' >/dev/null 2>&1; run2",
                init_path.display()
            ))
            .current_dir(dir.path())
            .output()
            .expect("spawn bash");
        assert!(
            output.status.success(),
            "bash failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let produced = std::fs::read_to_string(dir.path().join("out.txt")).expect("read out.txt");
        // run2 → lines 1 and 2 only; line 3 ("c") must not run.
        assert_eq!(produced, "a\nb\n", "run2 should replay exactly lines 1-2");
    }

    #[test]
    fn test_render_shell_init_without_cmds() {
        let out = render_shell_init(&[]).expect("render");
        assert!(!out.contains("run()"), "should not have run() block: {out}");
        assert!(
            !out.contains("HEPH_EOF"),
            "should not have show() block: {out}"
        );
        assert!(!out.contains("{{"), "template tokens left: {out}");
    }

    #[tokio::test]
    async fn test_run_echo_hello() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec!["echo".to_string(), "hello".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let mut stdout = Vec::new();
        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: Some(&mut stdout),
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };

        let _res = driver.run(make_req(req), &ctoken).await?;

        let output = String::from_utf8(stdout)?;
        assert_eq!(output.trim(), "hello");

        Ok(())
    }

    #[tokio::test]
    async fn test_run_missing_program_reports_sandbox_path_not_shell_path() -> anyhow::Result<()> {
        let mut opts = hplugin::config::Options::new();
        opts.insert(
            "path".to_string(),
            serde_yaml::from_str("[/nonexistent-test-search-dir]").expect("yaml"),
        );
        // The toolbox is on by default, and this test is about the spawn
        // diagnostic rather than about `PATH` composition — so it opts out
        // rather than standing up a shim directory it would never look at.
        opts.insert(
            "coreutils".to_string(),
            serde_yaml::from_str("false").expect("yaml"),
        );
        let driver = Driver::from_options_exec(&opts)?;
        let ctoken = StdCancellationToken::new();

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec!["definitely-not-a-real-binary-xyz".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };

        let res = driver.run(make_req(req), &ctoken).await;
        let Err(err) = res else {
            panic!("missing binary must fail");
        };
        let msg = err.to_string();
        // Names the program and the sandbox PATH it was searched in (not the
        // ambient shell PATH), and points at the config knob that controls it —
        // otherwise this reads as a bare ENOENT with no actionable next step.
        assert!(
            msg.contains("definitely-not-a-real-binary-xyz"),
            "missing program name: {msg}"
        );
        assert!(
            msg.contains("/nonexistent-test-search-dir"),
            "missing sandbox PATH: {msg}"
        );
        assert!(
            msg.contains("`path` option in .hephconfig"),
            "missing config hint: {msg}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_run_cat_hang() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec!["cat".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let mut stdin = std::io::Cursor::new(b"test data");
        let mut stdout = Vec::new();
        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: Some(&mut stdin),
            stdout: Some(&mut stdout),
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };

        // Use a timeout to detect the hang
        let _res = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            driver.run(make_req(req), &ctoken),
        )
        .await?;

        let output = String::from_utf8(stdout)?;
        assert_eq!(output, "test data");

        Ok(())
    }

    #[tokio::test]
    async fn test_run_stdin_to_stdout_timeout() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec!["cat".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;

        // Use a pipe that will never resolve to simulate a hang
        let (mut reader, _writer) = io::duplex(64);

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: Some(&mut reader),
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };

        let run_fut = driver.run(make_req(req), &ctoken);

        tokio::spawn(enclose!((ctoken) async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            ctoken.cancel();
        }));

        let res = run_fut.await;
        assert!(res.is_err());
        let err = res.err().unwrap();
        assert_eq!(err.to_string(), "cancelled");

        Ok(())
    }

    /// Cancelling a child that ignores SIGINT must escalate through the grace
    /// window to SIGKILL and return `Err("cancelled")` — without ever arming a
    /// `tokio::time` timer in the cancel path. The escalation now lives inside
    /// `proc_exec::Handle::wait_or_cancel` (timer-free), so a Ctrl-C racing
    /// runtime teardown can't poll a timer on a shutting-down runtime (the
    /// "A Tokio 1.x context was found, but it is being shutdown" panic).
    /// Runs on the `multi_thread` flavor — the runtime shape that panicked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_grace_escalates_without_panicking_timer() -> anyhow::Result<()> {
        let driver = Driver::new_bash();
        let ctoken = StdCancellationToken::new();

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                // Ignore SIGINT and hang, forcing the grace → SIGKILL path.
                run: vec!["trap '' INT; sleep 30".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: false,
            hash: vec![],
            transparent: false,
        };

        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };

        let run_fut = driver.run(make_req(req), &ctoken);

        tokio::spawn(enclose!((ctoken) async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            ctoken.cancel();
        }));

        let res = run_fut.await;
        let err = res.err().expect("cancelled run must error");
        assert_eq!(err.to_string(), "cancelled");

        Ok(())
    }

    /// Regression for shell/build deadlock when the child needs concurrent
    /// IO pump progress while the spawned wait task parks a worker. The
    /// stdout drain on a dedicated `std::thread` (inside `proc_exec`) plus
    /// the wait poll on a separate spawn must keep the pipe buffer
    /// draining; without that, the child blocks on `write` and never exits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_run_large_output_does_not_deadlock_multi_thread() -> anyhow::Result<()> {
        let driver = Driver::new_bash();
        let ctoken = StdCancellationToken::new();
        // 256 KiB — bigger than macOS pipe buffers (16-64 KiB), so without
        // a draining stdout pump the child would block on write forever.
        let payload_bytes: usize = 256 * 1024;
        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec![format!("head -c {payload_bytes} /dev/urandom | base64")],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: false,
            hash: vec![],
            transparent: false,
        };

        let mut stdout = Vec::new();
        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: Some(&mut stdout),
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };

        let res = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            driver.run(make_req(req), &ctoken),
        )
        .await
        .context("driver.run deadlocked under multi-thread runtime")?;
        res?;

        assert!(
            stdout.len() >= payload_bytes,
            "expected >= {payload_bytes} bytes, got {}",
            stdout.len()
        );
        Ok(())
    }

    /// The stall report triggers on the two streams' *combined* cost, not
    /// each stream's on its own, and it fires once rather than per chunk.
    ///
    /// Both halves matter: a target throttled 1.2 s on stdout and 1.2 s on
    /// stderr is a target throttled 2.4 s, and a per-stream threshold would
    /// stay silent through it. A per-chunk warn would bury the run in
    /// hundreds of identical lines.
    #[test]
    fn sink_cost_reports_on_combined_stall_exactly_once() {
        let half = SINK_STALL_WARN / 2;
        let mut cost = SinkCost::default();

        assert!(
            !cost.record(proc_exec::StreamId::Stdout, half),
            "half the budget on one stream is not a stall",
        );
        // The other stream carries the rest — neither alone would trip it.
        assert!(
            cost.record(proc_exec::StreamId::Stderr, half),
            "the two streams' cost must be counted together",
        );
        assert_eq!(cost.stdout, half);
        assert_eq!(cost.stderr, half);

        assert!(
            !cost.record(proc_exec::StreamId::Stdout, SINK_STALL_WARN),
            "the report is once per target, not once per chunk past the threshold",
        );
        assert_eq!(cost.stdout, half.saturating_add(SINK_STALL_WARN));
    }

    /// An `AsyncWrite` that records when its first byte landed. Lets a test
    /// distinguish "streamed while the child was running" from "flushed in a
    /// lump once the child exited" — which is the whole difference P7.2 is
    /// about, and is invisible to a test that only inspects final contents.
    #[derive(Clone, Default)]
    struct TimedSink(Arc<std::sync::Mutex<TimedSinkState>>);

    #[derive(Default)]
    struct TimedSinkState {
        first_write: Option<std::time::Instant>,
        bytes: Vec<u8>,
    }

    impl TimedSink {
        fn first_write(&self) -> Option<std::time::Instant> {
            self.0.lock().expect("sink mutex").first_write
        }

        fn bytes(&self) -> Vec<u8> {
            self.0.lock().expect("sink mutex").bytes.clone()
        }
    }

    impl tokio::io::AsyncWrite for TimedSink {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let mut state = self.0.lock().expect("sink mutex");
            state
                .first_write
                .get_or_insert_with(std::time::Instant::now);
            state.bytes.extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Both of a target's output streams must reach their sinks *while it
    /// runs*, not in a burst once it exits.
    ///
    /// This is the regression for the head-of-line block between the two
    /// tees. When stdout and stderr each had their own tee under
    /// `tokio::join!`, the first one polled parked the shared task inside
    /// `block_in_place` until the child exited, and the other stream reached
    /// neither `log.txt` nor the TUI until then. A compile that is quiet on
    /// stdout and chatty on stderr — the common case — showed nothing at all
    /// while it ran.
    ///
    /// Deliberately symmetric: **both** streams speak early and both speak
    /// again at the end. Under the old shape exactly one of them starved, but
    /// which one depended on the order `join!` happened to poll — so a test
    /// that only checked one stream would have been a coin flip. Checking
    /// both fails under either order.
    ///
    /// macOS-only in effect: Linux's reader is a real `AsyncRead` that yields
    /// rather than parking the task, so it streamed both ways already.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_run_streams_both_pipes_while_the_child_runs() -> anyhow::Result<()> {
        /// The child's quiet stretch between its first and last writes.
        const MIDDLE: std::time::Duration = std::time::Duration::from_secs(1);
        /// How much of that stretch a stream must beat to count as streamed.
        /// Half, so a loaded runner cannot turn a real pass into a failure
        /// while a starved stream (delta ~0) stays unambiguously red.
        const EARLY_BY: std::time::Duration = std::time::Duration::from_millis(500);

        let driver = Driver::new_bash();
        let ctoken = StdCancellationToken::new();
        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec![
                    "echo out-first; echo err-first >&2; sleep 1; echo out-last; echo err-last >&2"
                        .to_string(),
                ],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: false,
            hash: vec![],
            transparent: false,
        };

        let out_sink = TimedSink::default();
        let err_sink = TimedSink::default();
        let mut out_handle = out_sink.clone();
        let mut err_handle = err_sink.clone();
        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: Some(&mut out_handle),
            stderr: Some(&mut err_handle),
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };

        tokio::time::timeout(MIDDLE * 10, driver.run(make_req(req), &ctoken))
            .await
            .context("driver.run did not finish")??;
        let finished = std::time::Instant::now();

        for (name, sink) in [("stdout", &out_sink), ("stderr", &err_sink)] {
            let first = sink
                .first_write()
                .unwrap_or_else(|| panic!("{name} sink never received anything"));
            let lead = finished.saturating_duration_since(first);
            assert!(
                lead >= EARLY_BY,
                "{name} only reached its sink {lead:?} before the target exited — it was \
                 buffered until exit rather than streamed",
            );
        }

        assert_eq!(
            String::from_utf8_lossy(&out_sink.bytes()),
            "out-first\nout-last\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&err_sink.bytes()),
            "err-first\nerr-last\n"
        );
        Ok(())
    }

    /// `log.txt` must record the two streams in arrival order.
    ///
    /// It is a returned `Log` artifact and the text `--log-lines` renders in
    /// the failure box, so its byte order is user-visible. Before the merge,
    /// on macOS, it held one stream's entire output followed by the other's —
    /// a failing target's log claimed an ordering that never happened. The
    /// sleeps make the true order unambiguous, so this is not a race.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_run_log_records_both_streams_in_arrival_order() -> anyhow::Result<()> {
        let driver = Driver::new_bash();
        let ctoken = StdCancellationToken::new();
        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec![
                    "echo a; sleep 0.3; echo b >&2; sleep 0.3; echo c; sleep 0.3; echo d >&2"
                        .to_string(),
                ],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: false,
            hash: vec![],
            transparent: false,
        };

        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };

        driver.run(make_req(req), &ctoken).await?;

        let log = std::fs::read_to_string(tmp.path().join("log.txt"))?;
        assert_eq!(
            log, "a\nb\nc\nd\n",
            "log.txt must interleave the streams as they arrived",
        );
        Ok(())
    }

    /// Companion to `test_run_large_output_does_not_deadlock_multi_thread`.
    ///
    /// That one clears the 64 KiB kernel pipe. This one clears the macOS
    /// drain channel's own bound (`STREAM_DRAIN_CHUNKS` × `CHUNK_SIZE`,
    /// 512 KiB) eight times over, on **both** streams at once, which is the
    /// configuration that bounding introduces a deadlock risk into: a drain
    /// thread blocked in `send` stops reading its pipe, so if the consumer
    /// ever stops consuming — including because the *other* stream is
    /// starving it — the child never exits. Every byte must still arrive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_run_output_far_beyond_drain_bound_does_not_deadlock() -> anyhow::Result<()> {
        const PER_STREAM: usize = 4 * 1024 * 1024;

        let driver = Driver::new_bash();
        let ctoken = StdCancellationToken::new();
        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                // Interleaved rather than sequential so both drains are live
                // at once and each can stall the other. No pipeline: the
                // wrapper runs under `pipefail`, and a producer killed by
                // SIGPIPE when `head` exits would fail the target for
                // reasons that have nothing to do with the drain.
                run: vec![format!(
                    "head -c {PER_STREAM} /dev/zero & head -c {PER_STREAM} /dev/zero >&2; wait"
                )],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: false,
            hash: vec![],
            transparent: false,
        };

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: Some(&mut stdout),
            stderr: Some(&mut stderr),
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };

        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            driver.run(make_req(req), &ctoken),
        )
        .await
        .context("driver.run deadlocked past the bounded drain")??;

        assert_eq!(stdout.len(), PER_STREAM, "stdout truncated");
        assert_eq!(stderr.len(), PER_STREAM, "stderr truncated");
        Ok(())
    }

    /// An `AsyncWrite` that inserts a fixed delay before each write lands —
    /// stands in for a human-paced consumer (a TUI repaint, a slow terminal)
    /// draining `tee_output` slower than the child can produce.
    ///
    /// The in-flight `Sleep` is held across polls rather than recreated each
    /// time: a fresh timer on every `poll_write` call would reset its own
    /// deadline on every wake and never elapse.
    struct PacedSink {
        delay: std::time::Duration,
        sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
        bytes: Vec<u8>,
    }

    impl PacedSink {
        fn new(delay: std::time::Duration) -> Self {
            Self {
                delay,
                sleep: None,
                bytes: Vec::new(),
            }
        }
    }

    impl tokio::io::AsyncWrite for PacedSink {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let this = self.get_mut();
            let sleep = this
                .sleep
                .get_or_insert_with(|| Box::pin(tokio::time::sleep(this.delay)));
            match std::future::Future::poll(sleep.as_mut(), cx) {
                std::task::Poll::Pending => std::task::Poll::Pending,
                std::task::Poll::Ready(()) => {
                    this.sleep = None;
                    this.bytes.extend_from_slice(buf);
                    std::task::Poll::Ready(Ok(buf.len()))
                }
            }
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Regression for the cycle `pump_stdin`'s doc comment warns about: a
    /// child that is simultaneously blocked in `write(2)` on its *output*
    /// (because the consumer is slower than it is) and waiting to be read
    /// from on its *input* must still finish, because the stdin write and the
    /// output drain are on independent tasks.
    ///
    /// The script writes far more than the drain bound before it ever reads
    /// stdin, and the sink is deliberately paced slower than the child can
    /// produce — so by the time the child would read stdin, it is already
    /// parked in `write(2)` on stdout. Concurrently, more stdin is queued
    /// than the child's pipe (64 KiB) can hold, so `StdinPump::write_all`
    /// blocks too. If that write ran on `tee_output`'s task (the pre-fix
    /// shape — see `pump_stdin`), neither side could ever unblock the other:
    /// the tee stops draining, the drain channel fills, the child's stdout
    /// write never returns, and it never reaches the `cat` that would read
    /// the input the write is blocked on. With the write on its own task,
    /// `tee_output` keeps draining regardless, the child eventually finishes
    /// writing, reads stdin, and the run completes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_run_slow_sink_does_not_deadlock_with_concurrent_stdin() -> anyhow::Result<()> {
        // Comfortably past STREAM_DRAIN_CHUNKS * CHUNK_SIZE (512 KiB) plus the
        // 64 KiB pipe, so the child is guaranteed to still be blocked writing
        // stdout when it would otherwise have moved on to `cat`.
        const OUT_BYTES: usize = 900 * 1024;
        // Comfortably past the child's 64 KiB stdin pipe, so `StdinPump`'s
        // writer blocks on the real fd rather than finishing in one write.
        const STDIN_BYTES: usize = 200 * 1024;
        // Slow enough that the sink cannot drain OUT_BYTES before the stdin
        // write would need to block too (a handful of ms per 8 KiB chunk is
        // already far above human typing speed; this pushes past it to make
        // the overlap deterministic rather than a race).
        const SINK_DELAY: std::time::Duration = std::time::Duration::from_millis(5);

        let driver = Driver::new_bash();
        let ctoken = StdCancellationToken::new();
        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                // `wc -c`'s count goes to a *file*, not stdout: stdout is
                // deliberately paced slower than the child can produce (see
                // `PacedSink` below) so its trailing bytes are subject to a
                // separate, pre-existing loss window on process exit (see the
                // comment further down) — routing the proof-of-stdin through
                // the same channel would make this assertion flaky for a
                // reason unrelated to what this test checks.
                run: vec![format!(
                    "head -c {OUT_BYTES} /dev/zero; wc -c > stdin_byte_count.txt"
                )],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: false,
            hash: vec![],
            transparent: false,
        };

        let mut stdin = std::io::Cursor::new(vec![b'x'; STDIN_BYTES]);
        let mut stdout = PacedSink::new(SINK_DELAY);
        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: Some(&mut stdin),
            stdout: Some(&mut stdout),
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };

        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            driver.run(make_req(req), &ctoken),
        )
        .await
        .context(
            "driver.run deadlocked: the stdin write and the output drain blocked on each other",
        )??;

        // `head`'s zero bytes land on stdout, paced through the slow sink. A
        // sanity floor, not a byte-exact count: proves the output actually
        // streamed through the bounded channel under backpressure (not "zero
        // bytes arrived, something short-circuited upstream"), without
        // pinning down an exact loss margin — see the note below on why the
        // tail is expected to be short here, and why the real proof this test
        // cares about (did the child see all of stdin) is read from a file
        // instead of stdout.
        let zero_count = stdout.bytes.iter().filter(|&&b| b == 0).count();
        assert!(
            zero_count > OUT_BYTES / 3,
            "only {zero_count} of {OUT_BYTES} output bytes arrived — that's far more loss than \
             the known post-exit grace window accounts for, which points at something else",
        );
        // Not asserted above, but worth recording: `driver.run`'s post-wait IO
        // drain (`pluginexec::mod::Driver::run_inner`, the
        // `pluginexec:post_wait_io_drain` phase) gives a consumer that is
        // still behind only a fixed 50ms after the child exits before giving
        // up on it — pre-existing, not introduced by this PR. Against a sink
        // this slow (5ms/chunk), that window can't drain a full backlog
        // (`STREAM_DRAIN_CHUNKS` chunks take ~320ms at this pace), so stdout's
        // tail — and in one observed run, `wc -c`'s entire count — did not
        // survive to the sink. #245 bounds *how much* can ever be in flight
        // (at most one channel's worth, ~576 KiB) where the prior unbounded
        // drain had no such bound — a real improvement — but the loss itself
        // is not new, and it is why the byte count below is read from a file
        // the child wrote directly rather than from this same stdout.
        let wc_path = tmp.path().join("stdin_byte_count.txt");
        let wc_str = std::fs::read_to_string(&wc_path)
            .with_context(|| format!("read {}", wc_path.display()))?;
        let wc: usize = wc_str
            .trim()
            .parse()
            .with_context(|| format!("wc -c output not a number: {wc_str:?}"))?;
        assert_eq!(
            wc, STDIN_BYTES,
            "child did not see all of stdin — wc -c reported {wc}, expected {STDIN_BYTES}"
        );

        Ok(())
    }

    async fn run_bash_env(
        run_cmd: &str,
        pass_env: BTreeMap<String, String>,
        runtime_pass_env: Vec<String>,
        runtime_env: HashMap<String, String>,
    ) -> anyhow::Result<String> {
        let driver = Driver::new_bash();
        let ctoken = StdCancellationToken::new();
        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec![run_cmd.to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env,
                runtime_pass_env,
                runtime_env,
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: true,
            hash: vec![],
            transparent: false,
        };
        let mut stdout = Vec::new();
        let request_id = "test".to_string();
        let tmp = tempfile::tempdir()?;
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: Some(&mut stdout),
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };
        driver.run(make_req(req), &ctoken).await?;
        Ok(String::from_utf8(stdout)?.trim().to_string())
    }

    #[tokio::test]
    async fn test_run_pass_env_injected() -> anyhow::Result<()> {
        let out = run_bash_env(
            "echo $MY_PASS_VAR",
            BTreeMap::from([("MY_PASS_VAR".to_string(), "pass_value".to_string())]),
            vec![],
            HashMap::new(),
        )
        .await?;
        assert_eq!(out, "pass_value");
        Ok(())
    }

    #[tokio::test]
    async fn test_run_runtime_env_injected() -> anyhow::Result<()> {
        let out = run_bash_env(
            "echo $MY_RUNTIME_ENV",
            BTreeMap::new(),
            vec![],
            HashMap::from([(
                "MY_RUNTIME_ENV".to_string(),
                "runtime_env_value".to_string(),
            )]),
        )
        .await?;
        assert_eq!(out, "runtime_env_value");
        Ok(())
    }

    #[tokio::test]
    async fn test_run_runtime_pass_env_injected() -> anyhow::Result<()> {
        unsafe {
            std::env::set_var("heph_TEST_RUNTIME_PASS", "runtime_pass_value");
        }
        let out = run_bash_env(
            "echo $heph_TEST_RUNTIME_PASS",
            BTreeMap::new(),
            vec!["heph_TEST_RUNTIME_PASS".to_string()],
            HashMap::new(),
        )
        .await?;
        assert_eq!(out, "runtime_pass_value");
        Ok(())
    }

    #[tokio::test]
    async fn test_run_runtime_pass_env_wildcard_passes_all() -> anyhow::Result<()> {
        unsafe {
            std::env::set_var("heph_TEST_WILDCARD_VAR", "wildcard_value");
        }
        // `"*"` passes every host var through without naming it.
        let out = run_bash_env(
            "echo $heph_TEST_WILDCARD_VAR",
            BTreeMap::new(),
            vec!["*".to_string()],
            HashMap::new(),
        )
        .await?;
        assert_eq!(out, "wildcard_value");
        Ok(())
    }

    #[tokio::test]
    async fn test_run_env_not_leaked_from_parent() -> anyhow::Result<()> {
        unsafe {
            std::env::set_var("heph_TEST_PARENT_ONLY", "should_not_see_this");
        }
        let out = run_bash_env(
            "echo ${heph_TEST_PARENT_ONLY:-absent}",
            BTreeMap::new(),
            vec![],
            HashMap::new(),
        )
        .await?;
        assert_eq!(out, "absent");
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_pass_env_resolves_value() -> anyhow::Result<()> {
        unsafe {
            std::env::set_var("heph_TEST_PARSE_PASS", "resolved_value");
        }
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let config = HashMap::from([
            (
                "run".to_string(),
                hcore::htvalue::Value::String("echo".to_string()),
            ),
            (
                "pass_env".to_string(),
                hcore::htvalue::Value::List(vec![hcore::htvalue::Value::String(
                    "heph_TEST_PARSE_PASS".to_string(),
                )]),
            ),
        ]);
        let res = driver
            .parse(
                hplugin::driver::ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: std::sync::Arc::new(hplugin::provider::TargetSpec {
                        addr: Addr::default(),
                        driver: "exec".to_string(),
                        config,
                        ..Default::default()
                    }),
                },
                &ctoken,
            )
            .await?;
        let def = res.target_def.def::<TargetDef>();
        assert_eq!(
            def.pass_env.get("heph_TEST_PARSE_PASS"),
            Some(&"resolved_value".to_string())
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_pass_env_missing_var_skipped() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let config = HashMap::from([
            (
                "run".to_string(),
                hcore::htvalue::Value::String("echo".to_string()),
            ),
            (
                "pass_env".to_string(),
                hcore::htvalue::Value::List(vec![hcore::htvalue::Value::String(
                    "heph_TEST_DEFINITELY_UNSET_99999".to_string(),
                )]),
            ),
        ]);
        let res = driver
            .parse(
                hplugin::driver::ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: std::sync::Arc::new(hplugin::provider::TargetSpec {
                        addr: Addr::default(),
                        driver: "exec".to_string(),
                        config,
                        ..Default::default()
                    }),
                },
                &ctoken,
            )
            .await?;
        let def = res.target_def.def::<TargetDef>();
        assert!(def.pass_env.is_empty());
        Ok(())
    }

    /// Drives `parse()` with the given `extra` config keys merged on top of a
    /// minimal exec spec (just `run`). Returns the resulting EngineTargetDef.
    async fn parse_with(
        extra: HashMap<String, hcore::htvalue::Value>,
    ) -> anyhow::Result<hplugin::driver::targetdef::TargetDef> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let mut config = HashMap::from([(
            "run".to_string(),
            hcore::htvalue::Value::String("echo".to_string()),
        )]);
        config.extend(extra);
        let res = driver
            .parse(
                hplugin::driver::ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: std::sync::Arc::new(hplugin::provider::TargetSpec {
                        addr: Addr::default(),
                        driver: "exec".to_string(),
                        config,
                        ..Default::default()
                    }),
                },
                &ctoken,
            )
            .await?;
        Ok(res.target_def)
    }

    #[tokio::test]
    async fn test_parse_hash_deps_routes_inputs_with_flags() -> anyhow::Result<()> {
        let extra = HashMap::from([(
            "hash_deps".to_string(),
            hcore::htvalue::Value::List(vec![hcore::htvalue::Value::String(
                "//some:dep".to_string(),
            )]),
        )]);
        let td = parse_with(extra).await?;
        let def = td.def::<TargetDef>();

        // hash_deps must not appear in any pluginexec runtime wiring map.
        assert!(def.dep_group_inputs.is_empty());
        assert!(def.runtime_dep_group_inputs.is_empty());

        // They DO show up as engine inputs (so engine resolves them and folds
        // their hashout into hashin) with hashed=true / runtime=false.
        let hash_dep_input = td
            .inputs
            .iter()
            .find(|i| i.origin_id.starts_with("hash_dep|"))
            .expect("hash_dep input present");
        assert!(hash_dep_input.hashed);
        assert!(!hash_dep_input.runtime);
        Ok(())
    }

    /// A scratch reference is the one input with `hashed: false, runtime: false`.
    /// Both halves matter: it materializes no artifacts (its declaration has
    /// none), and it must not touch the consumer's cache key, because a target's
    /// outputs are required to be identical whether its scratch is warm, cold, or
    /// absent.
    #[tokio::test]
    async fn test_parse_scratch_routes_a_non_hashed_non_runtime_input() -> anyhow::Result<()> {
        use hcore::htvalue::Value;
        let extra = HashMap::from([(
            "scratch".to_string(),
            Value::List(vec![Value::String("//build:gocache".to_string())]),
        )]);
        let td = parse_with(extra).await?;
        let def = td.def::<TargetDef>();

        // Not wired into any runtime routing map: a scratch is not a dep, and
        // must never appear in SRC_*/LIST_*.
        assert!(def.dep_group_inputs.is_empty());
        assert!(def.runtime_dep_group_inputs.is_empty());
        assert!(def.tool_group_inputs.is_empty());

        let input = td
            .inputs
            .iter()
            .find(|i| i.origin_id.starts_with("scratch|"))
            .expect("scratch input present");
        assert!(
            !input.hashed,
            "a scratch must not feed the consumer's hashin"
        );
        assert!(!input.runtime, "a scratch materializes no artifacts");
        assert!(
            hdriver_support::scratch::is_scratch(&input.annotations),
            "the host recognizes a scratch by its annotation"
        );
        assert_eq!(input.r#ref.r#ref.format(), "//build:gocache");
        Ok(())
    }

    /// The property the whole design rests on: adding a scratch reference leaves
    /// the def hash untouched, so it cannot reach any consumer's `hashin`.
    #[tokio::test]
    async fn test_parse_scratch_excluded_from_def_hash() -> anyhow::Result<()> {
        use hcore::htvalue::Value;
        let bare = parse_with(HashMap::new()).await?;
        let with = parse_with(HashMap::from([(
            "scratch".to_string(),
            Value::List(vec![Value::String("//build:gocache".to_string())]),
        )]))
        .await?;
        assert_eq!(
            bare.hash, with.hash,
            "a scratch reference must not change the def hash"
        );
        Ok(())
    }

    /// A repeated reference would mount one directory twice and set one variable
    /// twice. Collapsing it silently would hide a BUILD-file mistake.
    #[tokio::test]
    async fn test_parse_scratch_rejects_a_duplicate_reference() -> anyhow::Result<()> {
        use hcore::htvalue::Value;
        let extra = HashMap::from([(
            "scratch".to_string(),
            Value::List(vec![
                Value::String("//build:c".to_string()),
                Value::String("//build:c".to_string()),
            ]),
        )]);
        let err = match parse_with(extra).await {
            Ok(_) => panic!("a duplicate scratch reference must fail"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("twice"));
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_read_only_deps_annotates_only_listed_groups() -> anyhow::Result<()> {
        use hcore::htvalue::Value;
        let extra = HashMap::from([
            (
                "deps".to_string(),
                Value::Map(HashMap::from([
                    (
                        "gosdk".to_string(),
                        Value::List(vec![Value::String("//sdk:go".to_string())]),
                    ),
                    (
                        "src".to_string(),
                        Value::List(vec![Value::String("//pkg:src".to_string())]),
                    ),
                ])),
            ),
            (
                "read_only_deps".to_string(),
                Value::List(vec![Value::String("gosdk".to_string())]),
            ),
        ]);
        let td = parse_with(extra).await?;

        let sdk = td
            .inputs
            .iter()
            .find(|i| i.origin_id.starts_with("dep|gosdk|"))
            .expect("gosdk dep input present");
        assert_eq!(
            sdk.annotations
                .get(hdriver_support::stage::READ_ONLY_ANNOTATION)
                .map(String::as_str),
            Some("true"),
            "listed group must be marked read-only"
        );

        let src = td
            .inputs
            .iter()
            .find(|i| i.origin_id.starts_with("dep|src|"))
            .expect("src dep input present");
        assert!(
            !src.annotations
                .contains_key(hdriver_support::stage::READ_ONLY_ANNOTATION),
            "unlisted group must not be marked read-only"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_runtime_deps_routes_inputs_with_flags() -> anyhow::Result<()> {
        let extra = HashMap::from([(
            "runtime_deps".to_string(),
            hcore::htvalue::Value::List(vec![hcore::htvalue::Value::String(
                "//some:dep".to_string(),
            )]),
        )]);
        let td = parse_with(extra).await?;
        let def = td.def::<TargetDef>();

        // runtime_deps wire into the runtime-only SRC_*/LIST_* map (excluded
        // from def.hash), and are NOT in the hashed dep_group_inputs map.
        assert!(def.dep_group_inputs.is_empty());
        assert_eq!(def.runtime_dep_group_inputs.len(), 1);

        let runtime_dep_input = td
            .inputs
            .iter()
            .find(|i| i.origin_id.starts_with("runtime_dep|"))
            .expect("runtime_dep input present");
        assert!(!runtime_dep_input.hashed);
        assert!(runtime_dep_input.runtime);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_runtime_deps_excluded_from_def_hash() -> anyhow::Result<()> {
        let base = parse_with(HashMap::new()).await?;
        let with_runtime = parse_with(HashMap::from([(
            "runtime_deps".to_string(),
            hcore::htvalue::Value::List(vec![hcore::htvalue::Value::String(
                "//some:dep".to_string(),
            )]),
        )]))
        .await?;
        // Adding a `runtime_deps` entry must NOT change the per-target def
        // hash; otherwise the cache key would depend on runtime-only state.
        assert_eq!(base.hash, with_runtime.hash);
        Ok(())
    }

    // ---- exec runner ----

    fn runner_val(v: &str) -> hcore::htvalue::Value {
        hcore::htvalue::Value::String(v.to_string())
    }

    async fn parse_with_driver_at(
        driver: &Driver,
        addr: Addr,
        extra: HashMap<String, hcore::htvalue::Value>,
    ) -> anyhow::Result<hplugin::driver::targetdef::TargetDef> {
        let ctoken = StdCancellationToken::new();
        let mut config = HashMap::from([(
            "run".to_string(),
            hcore::htvalue::Value::String("echo".to_string()),
        )]);
        config.extend(extra);
        Ok(driver
            .parse(
                hplugin::driver::ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: std::sync::Arc::new(hplugin::provider::TargetSpec {
                        addr,
                        driver: "exec".to_string(),
                        config,
                        ..Default::default()
                    }),
                },
                &ctoken,
            )
            .await?
            .target_def)
    }

    async fn parse_with_driver(
        driver: &Driver,
        extra: HashMap<String, hcore::htvalue::Value>,
    ) -> anyhow::Result<hplugin::driver::targetdef::TargetDef> {
        parse_with_driver_at(driver, Addr::default(), extra).await
    }

    fn runner_inputs(def: &hplugin::driver::targetdef::TargetDef) -> Vec<&Input> {
        def.inputs
            .iter()
            .filter(|i| i.origin_id == "runner")
            .collect()
    }

    #[tokio::test]
    async fn test_runner_absent_by_default() -> anyhow::Result<()> {
        let def = parse_with(HashMap::new()).await?;
        assert!(def.def::<TargetDef>().runner.is_none());
        assert!(runner_inputs(&def).is_empty());
        Ok(())
    }

    /// The runner reaches the cache key through its hashout, exactly as a
    /// `hash_dep` does — so `def.hash` must not move. This is what makes
    /// landing the feature invalidate nobody's cache, and what keeps two runner
    /// targets that emit identical `runner.json` sharing entries.
    #[tokio::test]
    async fn test_runner_excluded_from_def_hash() -> anyhow::Result<()> {
        let base = parse_with(HashMap::new()).await?;
        let with_runner =
            parse_with(HashMap::from([("runner".to_string(), runner_val("//t:r"))])).await?;
        assert_eq!(
            base.hash, with_runner.hash,
            "runner must not enter def.hash; it keys the cache via the hashout \
             of the Input parse emits for it"
        );
        Ok(())
    }

    /// hashed, so it folds into `hashin`; not runtime, so it never reaches the
    /// sandbox and its transitives never merge into the consumer's.
    #[tokio::test]
    async fn test_runner_is_a_hash_dep() -> anyhow::Result<()> {
        let def = parse_with(HashMap::from([("runner".to_string(), runner_val("//t:r"))])).await?;
        let inputs = runner_inputs(&def);
        assert_eq!(inputs.len(), 1);
        let input = inputs.first().expect("one runner input");
        assert!(input.hashed, "must fold into hashin");
        assert!(
            !input.runtime,
            "must never be materialized into the sandbox"
        );
        assert_eq!(input.r#ref.r#ref.format(), "//t:r");

        // And it must not be wired into SRC_*/LIST_* routing.
        let xdef = def.def::<TargetDef>();
        assert!(
            xdef.dep_group_inputs
                .values()
                .flatten()
                .all(|i| i.origin_id != "runner")
        );
        assert!(
            xdef.runtime_dep_group_inputs
                .values()
                .flatten()
                .all(|i| i.origin_id != "runner")
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_runner_local_is_the_opt_out() -> anyhow::Result<()> {
        let def = parse_with(HashMap::from([("runner".to_string(), runner_val("local"))])).await?;
        assert!(def.def::<TargetDef>().runner.is_none());
        assert!(runner_inputs(&def).is_empty());
        Ok(())
    }

    /// A bare word that is not the reserved `local` must say what the field
    /// takes, rather than failing later as "target not found".
    #[tokio::test]
    async fn test_runner_bare_word_is_rejected_with_the_shape() {
        let err =
            match parse_with(HashMap::from([("runner".to_string(), runner_val("locl"))])).await {
                Ok(_) => panic!("bare word must be rejected"),
                Err(e) => e,
            };
        let msg = format!("{err:#}");
        assert!(msg.contains("runner"), "{msg}");
        assert!(msg.contains("target address"), "{msg}");
    }

    /// The driver option is the workspace-wide door, and it must produce
    /// exactly the same Input an explicit field does — a default that reached
    /// the child without reaching the key would serve stale artifacts when
    /// switched on.
    #[tokio::test]
    async fn test_driver_default_runner_hashes_like_an_explicit_field() -> anyhow::Result<()> {
        let mut driver = Driver::new_exec();
        driver.default_runner = Some("//t:r".to_string());
        let defaulted = parse_with_driver(&driver, HashMap::new()).await?;
        let explicit =
            parse_with(HashMap::from([("runner".to_string(), runner_val("//t:r"))])).await?;

        assert_eq!(defaulted.hash, explicit.hash);
        let a = runner_inputs(&defaulted);
        let b = runner_inputs(&explicit);
        assert_eq!(a.len(), 1);
        assert_eq!(a.len(), b.len());
        assert_eq!(a[0].r#ref.r#ref, b[0].r#ref.r#ref);
        assert_eq!(a[0].hashed, b[0].hashed);
        assert_eq!(a[0].runtime, b[0].runtime);
        Ok(())
    }

    #[tokio::test]
    async fn test_target_field_beats_the_driver_default() -> anyhow::Result<()> {
        let mut driver = Driver::new_exec();
        driver.default_runner = Some("//t:default".to_string());
        let def = parse_with_driver(
            &driver,
            HashMap::from([("runner".to_string(), runner_val("//t:explicit"))]),
        )
        .await?;
        let inputs = runner_inputs(&def);
        assert_eq!(inputs[0].r#ref.r#ref.format(), "//t:explicit");
        Ok(())
    }

    #[tokio::test]
    async fn test_target_can_opt_out_of_the_driver_default() -> anyhow::Result<()> {
        let mut driver = Driver::new_exec();
        driver.default_runner = Some("//t:default".to_string());
        let def = parse_with_driver(
            &driver,
            HashMap::from([("runner".to_string(), runner_val("local"))]),
        )
        .await?;
        assert!(def.def::<TargetDef>().runner.is_none());
        Ok(())
    }

    /// The natural way to write a runner is an `exec`/`bash` target, which
    /// would otherwise inherit the workspace-wide default and become its own
    /// runner — the headline configuration cycling on the very first build.
    #[tokio::test]
    async fn test_the_default_runner_does_not_become_its_own_runner() -> anyhow::Result<()> {
        let me = parse_addr("//tools/devenv:runner")?;
        let mut driver = Driver::new_exec();
        driver.default_runner = Some(me.format());
        let def = parse_with_driver_at(&driver, me, HashMap::new()).await?;
        assert!(
            def.def::<TargetDef>().runner.is_none(),
            "a target that IS the default runner must not inherit it"
        );
        Ok(())
    }

    #[test]
    fn test_driver_option_runner_is_accepted_and_empty_means_unset() {
        let mut opts = hplugin::config::Options::new();
        opts.insert(
            "runner".to_string(),
            serde_yaml::from_str("\"//t:r\"").expect("yaml"),
        );
        let d = Driver::from_options_exec(&opts).expect("from_options");
        assert_eq!(d.default_runner.as_deref(), Some("//t:r"));

        let mut empty = hplugin::config::Options::new();
        empty.insert(
            "runner".to_string(),
            serde_yaml::from_str("\"\"").expect("yaml"),
        );
        let d = Driver::from_options_exec(&empty).expect("from_options");
        assert_eq!(d.default_runner, None);
    }

    #[tokio::test]
    async fn test_parse_hash_deps_excluded_from_def_hash() -> anyhow::Result<()> {
        // hash_deps are tracked via their hashout (flows into hashin via the
        // engine), not via the per-target def hash. So adding/changing a
        // hash_dep does not change `def.hash` either — invalidation happens
        // through the engine's input-result mixing.
        let base = parse_with(HashMap::new()).await?;
        let with_hash = parse_with(HashMap::from([(
            "hash_deps".to_string(),
            hcore::htvalue::Value::List(vec![hcore::htvalue::Value::String(
                "//some:dep".to_string(),
            )]),
        )]))
        .await?;
        assert_eq!(base.hash, with_hash.hash);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_input_order_stable_across_group_order() -> anyhow::Result<()> {
        // `deps`/`tools` arrive as `HashMap`s, so two parses of the same spec
        // iterate their groups in different orders. `def.inputs` order must not
        // follow: the engine's transitive collector numbers each merged sandbox
        // by an input's *position* in this list, and that number lands in the
        // merged dep ids — i.e. in the def hash of a target that depends on
        // this one. One group is always position 0; it takes two to show.
        let groups = |prefix: &str| {
            hcore::htvalue::Value::Map(
                ["a", "b", "c", "d", "e", "f", "g", "h"]
                    .iter()
                    .map(|g| {
                        (
                            format!("{prefix}{g}"),
                            hcore::htvalue::Value::List(vec![hcore::htvalue::Value::String(
                                format!("//some:{prefix}{g}"),
                            )]),
                        )
                    })
                    .collect(),
            )
        };
        let parse = || async {
            parse_with(HashMap::from([
                ("deps".to_string(), groups("d")),
                ("tools".to_string(), groups("t")),
            ]))
            .await
        };
        let origin_ids = |def: &hplugin::driver::targetdef::TargetDef| -> Vec<String> {
            def.inputs.iter().map(|i| i.origin_id.clone()).collect()
        };

        let first = parse().await?;
        for _ in 0..8 {
            let again = parse().await?;
            assert_eq!(
                origin_ids(&first),
                origin_ids(&again),
                "input order follows HashMap order"
            );
            assert_eq!(first.hash, again.hash, "def hash moved between parses");
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_deps_change_def_hash() -> anyhow::Result<()> {
        // Plain `deps` are structural — they must invalidate the per-target
        // def hash. (This is the property `runtime_deps` deliberately lacks.)
        let base = parse_with(HashMap::new()).await?;
        let with_deps = parse_with(HashMap::from([(
            "deps".to_string(),
            hcore::htvalue::Value::List(vec![hcore::htvalue::Value::String(
                "//some:dep".to_string(),
            )]),
        )]))
        .await?;
        assert_ne!(base.hash, with_deps.hash);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_env_value_change_def_hash() -> anyhow::Result<()> {
        // Literal `env` values must invalidate the per-target def hash —
        // changing an env value is a semantic change to the target's input.
        let with_v1 = parse_with(HashMap::from([(
            "env".to_string(),
            hcore::htvalue::Value::Map(HashMap::from([(
                "FOO".to_string(),
                hcore::htvalue::Value::String("v1".to_string()),
            )])),
        )]))
        .await?;
        let with_v2 = parse_with(HashMap::from([(
            "env".to_string(),
            hcore::htvalue::Value::Map(HashMap::from([(
                "FOO".to_string(),
                hcore::htvalue::Value::String("v2".to_string()),
            )])),
        )]))
        .await?;
        assert_ne!(with_v1.hash, with_v2.hash);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_env_key_change_def_hash() -> anyhow::Result<()> {
        // Changing an env key (not just its value) must also invalidate the
        // def hash.
        let with_foo = parse_with(HashMap::from([(
            "env".to_string(),
            hcore::htvalue::Value::Map(HashMap::from([(
                "FOO".to_string(),
                hcore::htvalue::Value::String("v".to_string()),
            )])),
        )]))
        .await?;
        let with_bar = parse_with(HashMap::from([(
            "env".to_string(),
            hcore::htvalue::Value::Map(HashMap::from([(
                "BAR".to_string(),
                hcore::htvalue::Value::String("v".to_string()),
            )])),
        )]))
        .await?;
        assert_ne!(with_foo.hash, with_bar.hash);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_env_added_changes_def_hash() -> anyhow::Result<()> {
        // Adding any env entry where there was none must change the def hash.
        let base = parse_with(HashMap::new()).await?;
        let with_env = parse_with(HashMap::from([(
            "env".to_string(),
            hcore::htvalue::Value::Map(HashMap::from([(
                "FOO".to_string(),
                hcore::htvalue::Value::String("v".to_string()),
            )])),
        )]))
        .await?;
        assert_ne!(base.hash, with_env.hash);
        Ok(())
    }

    /// Builds an `out` config value: `{group: [paths...]}`.
    fn out_value(groups: &[(&str, &[&str])]) -> hcore::htvalue::Value {
        hcore::htvalue::Value::Map(
            groups
                .iter()
                .map(|(g, paths)| {
                    (
                        g.to_string(),
                        hcore::htvalue::Value::List(
                            paths
                                .iter()
                                .map(|p| hcore::htvalue::Value::String(p.to_string()))
                                .collect(),
                        ),
                    )
                })
                .collect(),
        )
    }

    #[tokio::test]
    async fn test_parse_out_path_change_def_hash() -> anyhow::Result<()> {
        // Renaming a declared output inside an existing group must invalidate
        // the def hash. The group name is unchanged, so the cache manifest
        // still has a blob for it — without the outputs in the hash this is a
        // stale hit that serves the previously captured artifact and never
        // reruns the target.
        let with_a = parse_with(HashMap::from([(
            "out".to_string(),
            out_value(&[("", &["a.txt"])]),
        )]))
        .await?;
        let with_b = parse_with(HashMap::from([(
            "out".to_string(),
            out_value(&[("", &["b.txt"])]),
        )]))
        .await?;
        assert_ne!(with_a.hash, with_b.hash);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_out_added_changes_def_hash() -> anyhow::Result<()> {
        // Declaring an output where there was none must change the def hash.
        let base = parse_with(HashMap::new()).await?;
        let with_out = parse_with(HashMap::from([(
            "out".to_string(),
            out_value(&[("", &["a.txt"])]),
        )]))
        .await?;
        assert_ne!(base.hash, with_out.hash);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_out_group_rename_changes_def_hash() -> anyhow::Result<()> {
        // Same paths under a different group name — the group is what `$OUT_*`
        // and the cache manifest are keyed on, so it must be hashed too.
        let g1 = parse_with(HashMap::from([(
            "out".to_string(),
            out_value(&[("g1", &["a.txt"])]),
        )]))
        .await?;
        let g2 = parse_with(HashMap::from([(
            "out".to_string(),
            out_value(&[("g2", &["a.txt"])]),
        )]))
        .await?;
        assert_ne!(g1.hash, g2.hash);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_out_hash_stable_across_group_order() -> anyhow::Result<()> {
        // `spec.outputs` is a HashMap: two parses of the same spec can iterate
        // its groups in different orders. The def hash must not depend on that,
        // or identical targets thrash the cache between runs.
        let groups: &[(&str, &[&str])] = &[
            ("a", &["a.txt"]),
            ("b", &["b.txt"]),
            ("c", &["c.txt"]),
            ("d", &["d.txt"]),
            ("e", &["e.txt"]),
            ("f", &["f.txt"]),
            ("g", &["g.txt"]),
            ("h", &["h.txt"]),
        ];
        let first = parse_with(HashMap::from([("out".to_string(), out_value(groups))])).await?;
        for _ in 0..8 {
            let again = parse_with(HashMap::from([("out".to_string(), out_value(groups))])).await?;
            assert_eq!(first.hash, again.hash);
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_codegen_change_def_hash() -> anyhow::Result<()> {
        // `codegen` is baked into every output path's `codegen_tree`, and it
        // decides whether the outputs land back in the tree. Changing it is a
        // semantic change to the target.
        let copy = parse_with(HashMap::from([
            ("out".to_string(), out_value(&[("", &["a.txt"])])),
            (
                "codegen".to_string(),
                hcore::htvalue::Value::String("copy".to_string()),
            ),
        ]))
        .await?;
        let in_place = parse_with(HashMap::from([
            ("out".to_string(), out_value(&[("", &["a.txt"])])),
            (
                "codegen".to_string(),
                hcore::htvalue::Value::String("in_place".to_string()),
            ),
        ]))
        .await?;
        assert_ne!(copy.hash, in_place.hash);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_support_files_change_def_hash() -> anyhow::Result<()> {
        // support_files are packed into the target's artifact set, so changing
        // them changes what a cache hit would serve.
        let with_a = parse_with(HashMap::from([(
            "support_files".to_string(),
            hcore::htvalue::Value::List(vec![hcore::htvalue::Value::String("a.txt".to_string())]),
        )]))
        .await?;
        let with_b = parse_with(HashMap::from([(
            "support_files".to_string(),
            hcore::htvalue::Value::List(vec![hcore::htvalue::Value::String("b.txt".to_string())]),
        )]))
        .await?;
        assert_ne!(with_a.hash, with_b.hash);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_outputs_reach_engine_target_def() -> anyhow::Result<()> {
        // The EngineTargetDef outputs are now derived from the hashed def map —
        // make sure every declared group still reaches the engine.
        let td = parse_with(HashMap::from([(
            "out".to_string(),
            out_value(&[("bin", &["a.txt"]), ("lib", &["b.txt", "c.txt"])]),
        )]))
        .await?;
        let groups: Vec<&str> = td.outputs.iter().map(|o| o.group.as_str()).collect();
        assert_eq!(groups, vec!["bin", "lib"]);
        assert_eq!(td.outputs[1].paths.len(), 2);
        Ok(())
    }

    fn make_tool_binary(
        dir: &std::path::Path,
        name: &str,
        body: &str,
    ) -> anyhow::Result<std::path::PathBuf> {
        let path = dir.join(name);
        // These tools are exec'd by the driver while sibling tests spawn their own
        // subprocesses; a fork racing the write inherits the writable fd and the
        // exec fails with ETXTBSY. `write_executable` takes the barrier that
        // writing by hand would skip.
        hcore::fsutil::write_executable(&path, format!("#!/bin/sh\n{body}").as_bytes())?;
        Ok(path)
    }

    fn make_tool_managed_input(
        origin_id: &str,
        tool_path: &std::path::Path,
        list_dir: &std::path::Path,
    ) -> anyhow::Result<hdriver_support::driver_managed::ManagedRunInput> {
        make_tool_managed_input_full(
            origin_id,
            tool_path,
            list_dir,
            hmodel::htaddr::Addr::default(),
            "",
        )
    }

    fn make_tool_managed_input_with_source(
        origin_id: &str,
        tool_path: &std::path::Path,
        list_dir: &std::path::Path,
        source_addr: hmodel::htaddr::Addr,
    ) -> anyhow::Result<hdriver_support::driver_managed::ManagedRunInput> {
        make_tool_managed_input_full(origin_id, tool_path, list_dir, source_addr, "")
    }

    fn make_tool_managed_input_full(
        origin_id: &str,
        tool_path: &std::path::Path,
        list_dir: &std::path::Path,
        source_addr: hmodel::htaddr::Addr,
        hashout: &str,
    ) -> anyhow::Result<hdriver_support::driver_managed::ManagedRunInput> {
        use hplugin::driver::{RunInput, inputartifact, outputartifact};
        let list_path = list_dir.join(format!("input_{origin_id}.list"));
        std::fs::write(&list_path, format!("{}\n", tool_path.display()))?;
        Ok(hdriver_support::driver_managed::ManagedRunInput {
            input: RunInput {
                artifact: inputartifact::InputArtifact {
                    r#type: inputartifact::Type::Dep,
                    origin_id: origin_id.to_string(),
                    content: Arc::new(outputartifact::OutputArtifact {
                        group: "".to_string(),
                        name: "".to_string(),
                        r#type: outputartifact::Type::Output,
                        content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                            data: vec![],
                            path: "".to_string(),
                            x: false,
                        }),
                        hashout: hashout.to_string(),
                    }),
                },
                origin_id: origin_id.to_string(),
                source_addr,
                filters: vec![],
                annotations: BTreeMap::new(),
            },
            list_path: Some(list_path.clone()),
            unpack_root: list_dir.to_path_buf(),
        })
    }

    fn make_tool_target_def(run: Vec<String>, origin_id: &str, group: &str) -> EngineTargetDef {
        EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run,
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::from([(
                    group.to_string(),
                    vec![Input {
                        r#ref: hplugin::driver::TargetAddr::default(),
                        mode: InputMode::Tool,
                        origin_id: origin_id.to_string(),
                        annotations: BTreeMap::from([(
                            "unpack_root".to_string(),
                            "tools".to_string(),
                        )]),
                        hashed: true,
                        runtime: true,
                    }],
                )]),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: true,
            hash: vec![],
            transparent: false,
        }
    }

    #[tokio::test]
    async fn test_tool_binary_symlinked_in_bin() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        let tool_path = make_tool_binary(tmp.path(), "mytool", "echo mytool_output")?;
        let origin_id = "tool||0";
        let managed_input = make_tool_managed_input(origin_id, &tool_path, tmp.path())?;
        // exec driver: "mytool" is resolved via the child PATH which will be set to bin_dir
        let target_def = make_tool_target_def(vec!["mytool".to_string()], origin_id, "");

        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };
        driver
            .run(
                ManagedRunRequest {
                    sandbox_dir: tmp.path().to_path_buf(),
                    sandbox_ws_dir: tmp.path().to_path_buf(),
                    sandbox_pkg_dir: tmp.path().to_path_buf(),
                    request: req,
                    inputs: vec![managed_input],
                },
                &ctoken,
            )
            .await?;

        let bin_tool = tmp.path().join("bin").join("mytool");
        assert!(bin_tool.exists(), "bin/mytool should exist");
        assert!(
            bin_tool.symlink_metadata()?.file_type().is_symlink(),
            "bin/mytool should be a symlink"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_tool_callable_by_name() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        let tool_path = make_tool_binary(tmp.path(), "mytool", "echo tool_was_called")?;
        let origin_id = "tool||0";
        let managed_input = make_tool_managed_input(origin_id, &tool_path, tmp.path())?;
        let target_def = make_tool_target_def(vec!["mytool".to_string()], origin_id, "");

        let mut stdout = Vec::new();
        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: Some(&mut stdout),
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };
        driver
            .run(
                ManagedRunRequest {
                    sandbox_dir: tmp.path().to_path_buf(),
                    sandbox_ws_dir: tmp.path().to_path_buf(),
                    sandbox_pkg_dir: tmp.path().to_path_buf(),
                    request: req,
                    inputs: vec![managed_input],
                },
                &ctoken,
            )
            .await?;

        assert_eq!(String::from_utf8(stdout)?.trim(), "tool_was_called");
        Ok(())
    }

    #[tokio::test]
    async fn test_tool_bin_prepended_to_existing_path() -> anyhow::Result<()> {
        let driver = Driver::new_bash();
        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        let tool_path = make_tool_binary(tmp.path(), "mytool", "echo ok")?;
        let origin_id = "tool||0";
        let managed_input = make_tool_managed_input(origin_id, &tool_path, tmp.path())?;

        let existing_path = "/usr/bin:/bin";
        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec!["echo $PATH".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::from([(
                    "".to_string(),
                    vec![Input {
                        r#ref: hplugin::driver::TargetAddr::default(),
                        mode: InputMode::Tool,
                        origin_id: origin_id.to_string(),
                        annotations: BTreeMap::from([(
                            "unpack_root".to_string(),
                            "tools".to_string(),
                        )]),
                        hashed: true,
                        runtime: true,
                    }],
                )]),
                pass_env: BTreeMap::from([("PATH".to_string(), existing_path.to_string())]),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let mut stdout = Vec::new();
        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: Some(&mut stdout),
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };
        driver
            .run(
                ManagedRunRequest {
                    sandbox_dir: tmp.path().to_path_buf(),
                    sandbox_ws_dir: tmp.path().to_path_buf(),
                    sandbox_pkg_dir: tmp.path().to_path_buf(),
                    request: req,
                    inputs: vec![managed_input],
                },
                &ctoken,
            )
            .await?;

        let path_out = String::from_utf8(stdout)?;
        let path_out = path_out.trim();
        let bin_dir = tmp.path().join("bin").to_string_lossy().into_owned();
        assert!(
            path_out.starts_with(&bin_dir),
            "PATH should start with bin dir; got: {path_out}"
        );
        assert!(
            path_out.contains(existing_path),
            "PATH should retain existing entries; got: {path_out}"
        );
        Ok(())
    }

    /// End-to-end regression for multi-output tool refs.
    ///
    /// A multi-output tool target (one Output group per program, each with
    /// 1 FilePath) resolves to N `RunInput`s that share one `origin_id`.
    /// `inputs_result_exec` (engine/execute.rs) creates one `RunInput` per
    /// output artifact; the managed bridge then unpacks each one and APPENDS
    /// the produced paths to the same `input_<origin_id>.list`. The tool
    /// symlinker must end up with one symlink per binary in bin/.
    ///
    /// This goes through `ManagedDriverOs::run_inner` (the OS-copy sandbox
    /// path — what the bridge dispatches to with FUSE off) with N real tar
    /// artifacts, not pre-built list files, so it exercises the
    /// unpack-then-symlink flow that production hits.
    #[tokio::test]
    async fn test_multi_output_tool_via_bridge() -> anyhow::Result<()> {
        use hdriver_support::driver_managed_os::ManagedDriverOs;
        use hplugin::driver::{RunInput, inputartifact, outputartifact};

        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        // Pretend nix produced 4 wrapper scripts; each output group ships one.
        let store_dir = tmp.path().join("store");
        std::fs::create_dir_all(&store_dir)?;
        let names = ["node", "npm", "npx", "yarn"];

        // Build 4 tar artifacts, each containing one file at `pkg/bin/<name>`.
        // Mirrors what ManagedDriverOs::run_inner packs at the end of run.
        let tar_dir = tmp.path().join("tars");
        std::fs::create_dir_all(&tar_dir)?;
        let origin_id = "tool||0";
        let mut artifacts: Vec<RunInput> = Vec::new();
        for name in names {
            let src = make_tool_binary(&store_dir, name, "echo ok")?;
            let mut tp = hcore::hartifactcontent::tar::TarPacker::new();
            tp.create_file(
                src.to_string_lossy().into_owned(),
                format!("pkg/bin/{name}"),
            );
            let tar_path = tar_dir.join(format!("{name}.tar"));
            let f = std::fs::File::create(&tar_path)?;
            tp.pack(f)?;

            artifacts.push(RunInput {
                artifact: inputartifact::InputArtifact {
                    r#type: inputartifact::Type::Dep,
                    origin_id: origin_id.to_string(),
                    content: Arc::new(outputartifact::OutputArtifact {
                        group: name.to_string(),
                        name: format!("{name}.tar"),
                        r#type: outputartifact::Type::Output,
                        content: outputartifact::Content::TarPath(
                            outputartifact::ContentPath::borrowed(
                                tar_path.to_string_lossy().into_owned(),
                            ),
                        ),
                        hashout: format!("h_{name}"),
                    }),
                },
                origin_id: origin_id.to_string(),
                source_addr: hmodel::htaddr::Addr::default(),
                filters: vec![],
                annotations: BTreeMap::from([("unpack_root".to_string(), "tools".to_string())]),
            });
        }

        // Exec target with one declared tool input matching the shared origin_id.
        let target_def = EngineTargetDef {
            addr: Addr::new(
                hmodel::htpkg::PkgBuf::from("pkg"),
                "consumer".to_string(),
                BTreeMap::new(),
            ),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec!["true".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::from([(
                    "".to_string(),
                    vec![Input {
                        r#ref: hplugin::driver::TargetAddr::default(),
                        mode: InputMode::Tool,
                        origin_id: origin_id.to_string(),
                        annotations: BTreeMap::from([(
                            "unpack_root".to_string(),
                            "tools".to_string(),
                        )]),
                        hashed: true,
                        runtime: true,
                    }],
                )]),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: false,
            hash: vec![],
            transparent: false,
        };

        let sandbox = tmp.path().join("sandbox");
        std::fs::create_dir_all(&sandbox)?;

        let os = ManagedDriverOs::new(
            Box::new(Driver::new_bash()),
            Driver::default_exec_shell_fallback(),
        );

        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: tmp.path().to_path_buf(),
            inputs: artifacts,
            hashin: "h",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox.clone(),
            scratch: vec![],
        };

        os.run_inner(req, &ctoken, false).await?;

        let bin_dir = sandbox.join("bin");
        let listed: Vec<_> = std::fs::read_dir(&bin_dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        for name in names {
            assert!(
                bin_dir.join(name).exists(),
                "bin/{name} missing; bin/ contents: {:?}",
                listed
            );
        }
        Ok(())
    }

    /// Same shape but at the driver level, bypassing the bridge (the bridge
    /// merges N inputs into one list file; this asserts the symlink loop is
    /// correct given that already-merged state).
    #[tokio::test]
    async fn test_multi_output_tool_symlinks_all_binaries() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        // Four real binary files on disk.
        let tool_dir = tmp.path().join("store");
        std::fs::create_dir_all(&tool_dir)?;
        let names = ["node", "npm", "npx", "yarn"];
        let tool_paths: Vec<std::path::PathBuf> = names
            .iter()
            .map(|n| make_tool_binary(&tool_dir, n, "echo ok").expect("make tool"))
            .collect();

        // One shared list file with all 4 paths (mirrors the bridge's append
        // behavior when N RunInputs share an origin_id).
        let origin_id = "tool||0";
        let list_dir = tmp.path().join("ws");
        std::fs::create_dir_all(&list_dir)?;
        let list_path = list_dir.join(format!("input_{origin_id}.list"));
        let mut contents = String::new();
        for p in &tool_paths {
            contents.push_str(&format!("{}\n", p.display()));
        }
        std::fs::write(&list_path, contents)?;

        // N ManagedRunInput, all sharing the same origin_id and list_path —
        // exactly what `inputs_result_exec` + the managed bridge produce for
        // an N-output tool ref.
        use hplugin::driver::{RunInput, inputartifact, outputartifact};
        let make_managed = || hdriver_support::driver_managed::ManagedRunInput {
            input: RunInput {
                artifact: inputartifact::InputArtifact {
                    r#type: inputartifact::Type::Dep,
                    origin_id: origin_id.to_string(),
                    content: Arc::new(outputartifact::OutputArtifact {
                        group: "".to_string(),
                        name: "".to_string(),
                        r#type: outputartifact::Type::Output,
                        content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                            data: vec![],
                            path: "".to_string(),
                            x: false,
                        }),
                        hashout: "".to_string(),
                    }),
                },
                origin_id: origin_id.to_string(),
                source_addr: hmodel::htaddr::Addr::default(),
                filters: vec![],
                annotations: BTreeMap::from([("unpack_root".to_string(), "tools".to_string())]),
            },
            list_path: Some(list_path.clone()),
            unpack_root: list_dir.clone(),
        };
        let managed_inputs: Vec<_> = (0..names.len()).map(|_| make_managed()).collect();

        let target_def = make_tool_target_def(vec!["true".to_string()], origin_id, "");

        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };
        driver
            .run(
                ManagedRunRequest {
                    sandbox_dir: tmp.path().to_path_buf(),
                    sandbox_ws_dir: tmp.path().to_path_buf(),
                    sandbox_pkg_dir: tmp.path().to_path_buf(),
                    request: req,
                    inputs: managed_inputs,
                },
                &ctoken,
            )
            .await?;

        let bin_dir = tmp.path().join("bin");
        for name in names {
            let bin = bin_dir.join(name);
            assert!(
                bin.exists(),
                "bin/{name} must exist (multi-output tool); got dir: {:?}",
                std::fs::read_dir(&bin_dir)
                    .map(|rd| rd.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
                    .unwrap_or_default()
            );
            assert!(
                bin.symlink_metadata()?.file_type().is_symlink(),
                "bin/{name} must be a symlink"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_no_bin_dir_without_tools() -> anyhow::Result<()> {
        let driver = Driver::new_bash();
        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec!["true".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };
        driver.run(make_req(req), &ctoken).await?;

        assert!(
            !tmp.path().join("bin").exists(),
            "bin/ should not be created when no tools"
        );
        Ok(())
    }

    /// Two tool inputs producing the same `bin/<filename>` are silently
    /// deduped at symlink time — first wins, second is skipped. Address-level
    /// uniqueness (no two engine inputs share `(r#ref, group)`) is enforced
    /// upstream by `Sandbox::merge_sandbox`; here we just avoid EEXIST.
    #[tokio::test]
    async fn overlapping_tool_filenames_dedupe_silently() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        std::fs::create_dir_all(&dir_a)?;
        std::fs::create_dir_all(&dir_b)?;
        let tool_a = make_tool_binary(&dir_a, "node", "echo a")?;
        let tool_b = make_tool_binary(&dir_b, "node", "echo b")?;

        let mi_a = make_tool_managed_input("tool|a|0", &tool_a, tmp.path())?;
        let mi_b = make_tool_managed_input("tool|b|0", &tool_b, tmp.path())?;

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec!["true".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::from([(
                    "".to_string(),
                    vec![
                        Input {
                            r#ref: hplugin::driver::TargetAddr::default(),
                            mode: InputMode::Tool,
                            origin_id: "tool|a|0".to_string(),
                            annotations: BTreeMap::new(),
                            hashed: true,
                            runtime: true,
                        },
                        Input {
                            r#ref: hplugin::driver::TargetAddr::default(),
                            mode: InputMode::Tool,
                            origin_id: "tool|b|0".to_string(),
                            annotations: BTreeMap::new(),
                            hashed: true,
                            runtime: true,
                        },
                    ],
                )]),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };
        driver
            .run(
                ManagedRunRequest {
                    sandbox_dir: tmp.path().to_path_buf(),
                    sandbox_ws_dir: tmp.path().to_path_buf(),
                    sandbox_pkg_dir: tmp.path().to_path_buf(),
                    request: req,
                    inputs: vec![mi_a, mi_b],
                },
                &ctoken,
            )
            .await?;

        assert!(
            tmp.path().join("bin").join("node").exists(),
            "bin/node must exist after dedup"
        );
        Ok(())
    }

    /// Same source target referenced via two tool groups (e.g. `tools = [t]`
    /// in two different groups) must not surface an overlap — symlink the
    /// destination once and continue.
    #[tokio::test]
    async fn same_source_tool_filename_dedupes_silently() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        let tool_path = make_tool_binary(tmp.path(), "node", "echo ok")?;
        let src = Addr::new(
            hmodel::htpkg::PkgBuf::from("pkg"),
            "node_tool".to_string(),
            BTreeMap::new(),
        );

        // Two ManagedRunInputs from the same source target, distinct
        // origin_ids (different tool groups).
        let mi_a =
            make_tool_managed_input_with_source("tool|g1|0", &tool_path, tmp.path(), src.clone())?;
        let mi_b = make_tool_managed_input_with_source("tool|g2|0", &tool_path, tmp.path(), src)?;

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec!["true".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::from([
                    (
                        "g1".to_string(),
                        vec![Input {
                            r#ref: hplugin::driver::TargetAddr::default(),
                            mode: InputMode::Tool,
                            origin_id: "tool|g1|0".to_string(),
                            annotations: BTreeMap::new(),
                            hashed: true,
                            runtime: true,
                        }],
                    ),
                    (
                        "g2".to_string(),
                        vec![Input {
                            r#ref: hplugin::driver::TargetAddr::default(),
                            mode: InputMode::Tool,
                            origin_id: "tool|g2|0".to_string(),
                            annotations: BTreeMap::new(),
                            hashed: true,
                            runtime: true,
                        }],
                    ),
                ]),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };
        driver
            .run(
                ManagedRunRequest {
                    sandbox_dir: tmp.path().to_path_buf(),
                    sandbox_ws_dir: tmp.path().to_path_buf(),
                    sandbox_pkg_dir: tmp.path().to_path_buf(),
                    request: req,
                    inputs: vec![mi_a, mi_b],
                },
                &ctoken,
            )
            .await?;

        assert!(
            tmp.path().join("bin").join("node").exists(),
            "bin/node must exist after dedup",
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_run_creates_output_dirs() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec!["true".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![
                Output {
                    group: "file".to_string(),
                    paths: vec![Path {
                        content: Content::FilePath("nested/dir/out.txt".to_string()),
                        codegen_tree: CodegenMode::None,
                        collect: false,
                    }],
                },
                Output {
                    group: "dir".to_string(),
                    paths: vec![Path {
                        content: Content::DirPath("a/b/c".to_string()),
                        codegen_tree: CodegenMode::None,
                        collect: false,
                    }],
                },
                Output {
                    group: "glob".to_string(),
                    paths: vec![Path {
                        content: Content::Glob("never/created/**/*.txt".to_string()),
                        codegen_tree: CodegenMode::None,
                        collect: false,
                    }],
                },
            ],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let request_id = "test".to_string();
        let tmp = tempfile::tempdir()?;

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };

        driver.run(make_req(req), &ctoken).await?;

        assert!(
            tmp.path().join("nested/dir").is_dir(),
            "FilePath parent must be created",
        );
        assert!(
            !tmp.path().join("nested/dir/out.txt").exists(),
            "FilePath itself must not be created",
        );
        assert!(tmp.path().join("a/b/c").is_dir(), "DirPath must be created",);
        assert!(
            !tmp.path().join("never").exists(),
            "Glob must not trigger dir creation",
        );
        Ok(())
    }

    // ---- builtin coreutils ----

    /// A shim directory that looks like the real one, without depending on the
    /// crate that builds it — the point of the closure this crate takes.
    fn fake_shims(root: &std::path::Path) -> CoreutilsShims {
        let root = root.to_path_buf();
        Arc::new(move || {
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin)?;
            for name in ["cp", "install", "sha256sum"] {
                let link = bin.join(name);
                if !link.exists() {
                    std::fs::write(&link, b"shim")?;
                }
            }
            Ok(bin)
        })
    }

    fn coreutils_opts(on: bool) -> hplugin::config::Options {
        let mut opts = hplugin::config::Options::new();
        opts.insert(
            "coreutils".to_string(),
            serde_yaml::from_str(if on { "true" } else { "false" }).expect("yaml"),
        );
        opts
    }

    #[test]
    fn coreutils_is_on_unless_turned_off() {
        // The default is on: a toolbox nobody enables fixes nothing.
        let on = Driver::from_options_exec(&hplugin::config::Options::new()).expect("from_options");
        assert!(on.coreutils_enabled, "the toolbox must be on by default");

        // And `coreutils: false` must still turn it off, since that is the
        // escape hatch for a workspace that wants the host's tools.
        let off = Driver::from_options_exec(&coreutils_opts(false)).expect("from_options");
        assert!(!off.coreutils_enabled);
        assert!(off.coreutils_version().is_none());
    }

    #[test]
    fn a_driver_built_without_options_starts_off() {
        // `new_exec` is what test harnesses and the shell fallback use. They
        // have no heph home to materialize shims under, so the toolbox must not
        // switch itself on there and then assert about missing shims.
        let d = Driver::new_exec();
        assert!(!d.coreutils_enabled);
        assert!(d.coreutils_version().is_none());
    }

    #[test]
    fn coreutils_is_ignored_while_the_toolbox_is_off() {
        // A host supplies the toolbox unconditionally; policy lives in the
        // option, so an off driver must not resolve anything.
        let tmp = tempfile::tempdir().expect("tempdir");
        let d = Driver::from_options_exec(&coreutils_opts(false))
            .expect("from_options")
            .with_coreutils(7, fake_shims(tmp.path()));
        assert!(d.coreutils.is_none());
        assert!(d.coreutils_dir().expect("no work to do").is_none());
        assert!(
            !tmp.path().join("bin").exists(),
            "nothing may be materialized"
        );
    }

    #[test]
    fn coreutils_on_without_a_supply_is_an_error_not_a_shrug() {
        // Running against the host's utilities while the config says otherwise
        // is the silently-wrong-build case; it has to be loud.
        let d = Driver::from_options_exec(&coreutils_opts(true)).expect("from_options");
        let err = d.coreutils_dir().expect_err("must not degrade quietly");
        assert!(
            err.to_string().contains("no shim directory was supplied"),
            "unhelpful error: {err}"
        );
    }

    #[test]
    fn coreutils_on_materializes_the_shims_once() {
        let home = tempfile::tempdir().expect("tempdir");
        let d = Driver::from_options_exec(&coreutils_opts(true))
            .expect("from_options")
            .with_coreutils(1, fake_shims(home.path()));

        let first = d.coreutils_dir().expect("shim dir").expect("enabled");
        assert!(first.join("cp").exists(), "cp shim is missing");
        assert!(first.join("install").exists(), "install shim is missing");
        // Resolved once and reused — the steady-state cost is a cache read, not
        // a directory walk per target.
        let second = d.coreutils_dir().expect("shim dir").expect("enabled");
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn coreutils_moves_the_def_hash_and_only_when_on() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let off = Driver::new_exec();
        let on = Driver::from_options_exec(&coreutils_opts(true))?
            .with_coreutils(1, fake_shims(home.path()));

        let base = parse_with_driver(&off, HashMap::new()).await?;
        let with_toolbox = parse_with_driver(&on, HashMap::new()).await?;

        // The utilities are on the target's PATH without being declared, so
        // their identity has to reach the key: otherwise an upgrade that
        // changes `cp` keeps serving artifacts the old one built.
        assert_ne!(
            base.hash, with_toolbox.hash,
            "turning the toolbox on must move the def hash"
        );

        // And the off path must be byte-identical to what it hashes today, or
        // shipping this would invalidate every exec target in every workspace
        // that never asked for it.
        let off_again = parse_with_driver(
            &Driver::from_options_exec(&coreutils_opts(false))?,
            HashMap::new(),
        )
        .await?;
        assert_eq!(base.hash, off_again.hash);
        Ok(())
    }

    #[tokio::test]
    async fn coreutils_sits_behind_everything_the_environment_provides() -> anyhow::Result<()> {
        // The precedence the whole design rests on. A target that declares a
        // tool gets that one — `prefix` leads. The builtins are what *heph*
        // supplies rather than what the target asked for, so they go last: they
        // fill a gap the environment leaves and never shadow a binary it
        // deliberately ships.
        let home = tempfile::tempdir()?;
        let driver = Driver::new_bash()
            .with_coreutils_enabled_for_test()
            .with_coreutils(1, fake_shims(home.path()));
        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        let tool_path = make_tool_binary(tmp.path(), "mytool", "echo ok")?;
        let origin_id = "tool||0";
        let managed_input = make_tool_managed_input(origin_id, &tool_path, tmp.path())?;

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                runner: None,
                run: vec!["echo $PATH".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::from([(
                    "".to_string(),
                    vec![Input {
                        r#ref: hplugin::driver::TargetAddr::default(),
                        mode: InputMode::Tool,
                        origin_id: origin_id.to_string(),
                        annotations: BTreeMap::from([(
                            "unpack_root".to_string(),
                            "tools".to_string(),
                        )]),
                        hashed: true,
                        runtime: true,
                    }],
                )]),
                pass_env: BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
                outputs: BTreeMap::new(),
                support_files: vec![],
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: CacheConfig::on(true),
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let mut stdout = Vec::new();
        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: Some(&mut stdout),
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
            scratch: vec![],
        };
        driver
            .run(
                ManagedRunRequest {
                    sandbox_dir: tmp.path().to_path_buf(),
                    sandbox_ws_dir: tmp.path().to_path_buf(),
                    sandbox_pkg_dir: tmp.path().to_path_buf(),
                    request: req,
                    inputs: vec![managed_input],
                },
                &ctoken,
            )
            .await?;

        let path_out = String::from_utf8(stdout)?;
        let path_out = path_out.trim();
        let entries: Vec<&str> = path_out.split(':').collect();
        let bin_dir = tmp.path().join("bin").to_string_lossy().into_owned();
        let shim_dir = driver
            .coreutils_dir()?
            .expect("enabled")
            .to_string_lossy()
            .into_owned();

        assert_eq!(
            entries.first().copied(),
            Some(bin_dir.as_str()),
            "the target's own tools must lead; got: {path_out}"
        );
        assert_eq!(
            entries.last().copied(),
            Some(shim_dir.as_str()),
            "the builtins must come last, behind everything the environment \
             provides; got: {path_out}"
        );
        let declared = entries
            .iter()
            .position(|e| *e == "/usr/bin")
            .expect("what the target declared must still be on PATH");
        assert!(
            declared < entries.len() - 1,
            "what the target declared must come before the builtins; got: {path_out}"
        );
        Ok(())
    }
}
