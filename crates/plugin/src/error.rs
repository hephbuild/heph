use crate::driver::outputartifact::{
    Content as OaContent, ContentFile, ContentPath, ContentRaw, OutputArtifact,
    Type as ArtifactType,
};
use hmodel::htaddr::Addr;
use std::fmt;

#[derive(Debug, Clone)]
pub struct TargetNotFoundError {
    pub addr: Addr,
}

impl fmt::Display for TargetNotFoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "target not found: {}", self.addr.format())
    }
}

impl std::error::Error for TargetNotFoundError {}

/// Returned the instant a request's cancellation token is observed set, so a
/// cancelled build stops resolving and executing new targets immediately
/// rather than draining the entire matched set. Callers match on this to
/// suppress it from the "N targets failed" tally.
#[derive(Debug, Clone)]
pub struct CancelledError;

impl fmt::Display for CancelledError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cancelled")
    }
}

impl std::error::Error for CancelledError {}

/// True if `e` (anywhere in its source chain) is a `CancelledError`.
pub fn is_cancelled(e: &anyhow::Error) -> bool {
    hcore::hmemoizer::downcast_chain_ref::<CancelledError>(e).is_some()
}

/// A hash-only request reached a target it would have had to *build* to answer.
///
/// Hash-only requests exist to re-read the tree from inside an in-flight
/// resolution (the in_place write-back guard and the fixpoint recompute). They
/// are forbidden from taking the exclusive per-addr result lock, because the
/// request they are nested inside is already holding riding **read** guards that
/// it will not release until the nested call returns — so a write acquire is a
/// self-deadlock, not contention.
///
/// So a cache miss under such a request answers "unknown" instead of building.
/// Callers decide what that means: the fixpoint recompute skips (it is an
/// optimization), the write-back guard refuses to write (it cannot confirm).
#[derive(Debug, Clone)]
pub struct HashUnknownError {
    pub addr: Addr,
}

impl fmt::Display for HashUnknownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is not cached at its current inputs, and this hash-only resolution may not build it",
            self.addr.format()
        )
    }
}

impl std::error::Error for HashUnknownError {}

/// A revision this request had already resolved changed or disappeared while
/// nothing in the request was holding it.
///
/// A request keeps a revision's read lock only while artifacts of it are alive.
/// Between two uses the read can lapse, and another `heph` may then `clean` the
/// revision or `--force` a rebuild of it. When the request needs the revision
/// again it re-acquires the read and checks the on-disk manifest still carries
/// the hashouts it already handed out (and that dependents may already have
/// folded into their own keys). If it does not, handing the memoized artifacts
/// out would mean dangling blobs or bytes that disagree with those hashouts, so
/// the resolution fails with this instead. Re-running resolves afresh.
#[derive(Debug, Clone)]
pub struct RevisionChangedError {
    pub addr: Addr,
    pub hashin: String,
}

impl fmt::Display for RevisionChangedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: its cached output (revision {}) changed or was removed by another heph \
             process while this build was using it; re-run",
            self.addr.format(),
            self.hashin
        )
    }
}

impl std::error::Error for RevisionChangedError {}

/// How many group members the `--shell` diagnostic lists before eliding.
const SHELL_MEMBERS_SHOWN: usize = 5;

/// `--shell` was refused: either the request does not resolve to a single
/// executing target (a multi-target selection, or a transparent group that is
/// not an alias for one target), or it does, but there is no terminal to hand
/// it to.
///
/// A property of the *request*, not a failure of any target: nothing ran, and
/// nothing was going to. So `classify_failure` propagates it unchanged rather
/// than recording it against whichever frame happened to raise it — a nested
/// group would otherwise render the same user error as a failed-target box for
/// a group that never executed.
///
/// The fields stay structured so a caller can render or serialize them; the
/// `Display` is the human-facing form and always names the next command to run,
/// so the message is recoverable on its own by a human or an agent.
#[derive(Debug, Clone)]
pub enum ShellNeedsSingleTarget {
    /// A query or label/package matcher that selects more than one target.
    Selection {
        /// The selection as the user wrote it.
        query: String,
    },
    /// A transparent group whose inputs are not exactly one distinct member.
    Group {
        addr: Addr,
        /// The group's distinct members, in declaration order.
        members: Vec<Addr>,
    },
    /// `addr` is a single target — the shape is fine — but this run has
    /// nothing to attach a shell to: no tty (CI, a redirected stdin/stdout),
    /// or a client that never offered one.
    NotInteractive { addr: Addr },
}

impl fmt::Display for ShellNeedsSingleTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Selection { query } => write!(
                f,
                "--shell needs exactly one target; {query} selects many\n  \
                 try: heph run --shell //pkg:name with one address"
            ),
            Self::Group { addr, members } => {
                write!(
                    f,
                    "--shell needs exactly one target; {} is a group with {} members",
                    addr.format(),
                    members.len()
                )?;
                let Some(first) = members.first() else {
                    return f.write_str(
                        "\n  the group is empty — there is nothing to shell into\
                         \n  try: heph run --shell //pkg:name with a target address",
                    );
                };
                f.write_str("\n  members: ")?;
                for (i, member) in members.iter().take(SHELL_MEMBERS_SHOWN).enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(&member.format())?;
                }
                let rest = members.len().saturating_sub(SHELL_MEMBERS_SHOWN);
                if rest > 0 {
                    write!(f, " … and {rest} more")?;
                }
                write!(f, "\n  try: heph run --shell {}", first.format())
            }
            Self::NotInteractive { addr } => write!(
                f,
                "--shell needs a terminal to attach to; {} would run in a \
                 non-interactive session (CI, a redirected stdin/stdout, or --no-tui)\n  \
                 try: run `heph run --shell {}` from an interactive terminal",
                addr.format(),
                addr.format(),
            ),
        }
    }
}

impl std::error::Error for ShellNeedsSingleTarget {}

#[derive(Debug, Clone)]
pub struct CycleError {
    pub from: Addr,
    pub to: Addr,
}

impl fmt::Display for CycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cyclic dependency detected: {} → {}",
            self.from.format(),
            self.to.format()
        )
    }
}

impl std::error::Error for CycleError {}

/// Aggregates multiple errors from a concurrent fanout when `RequestState::fail_fast`
/// is disabled. Each inner error is rendered with anyhow's alternate `{:#}` so the
/// full context chain shows up.
#[derive(Debug)]
pub struct MultiError(pub Vec<anyhow::Error>);

impl fmt::Display for MultiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} errors:", self.0.len())?;
        for (i, e) in self.0.iter().enumerate() {
            writeln!(f, "  [{}] {:#}", i, e)?;
        }
        Ok(())
    }
}

impl std::error::Error for MultiError {}

/// Lightweight propagation marker meaning "I failed only because a dependency
/// failed." Propagates UP the graph and is **never** rendered. It NEVER wraps a
/// cause — collateral hops replace their incoming error with a fresh
/// `UpstreamFailed`, keeping chain depth O(1) on any graph. `root` identifies the
/// genuinely-failing target whose `TargetFailure` is recorded in the registry.
#[derive(Debug, Clone)]
pub struct UpstreamFailed {
    pub root: Addr,
}

impl fmt::Display for UpstreamFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dependency failed (root: {})", self.root.format())
    }
}

impl std::error::Error for UpstreamFailed {}

/// A target's own process exited non-zero. Carries the exit status only: the
/// process's log rides the same error as a [`FailureArtifacts`] context (see
/// [`ProcessFailed::from_output`] and [`with_failure_artifacts`]), which the
/// diagnostic reads the last N lines of on demand. Surfaced as the `source` of
/// a [`TargetFailure`].
///
/// `status` is the human text (std's `ExitStatus` display, which is not a
/// contract and must never be parsed); `exit` is the same status as data, for
/// a caller that mirrors it (an entrypoint's code becomes heph's exit code, its
/// signal is re-raised). `None` when the driver did not report one — every
/// driver before the field existed, and a failure that is not a child's exit.
///
/// `#[non_exhaustive]`: built with [`ProcessFailed::new`] outside this crate,
/// so a later field is not a source break.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ProcessFailed {
    pub status: String,
    pub exit: Option<Exit>,
}

impl fmt::Display for ProcessFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "process exited with status: {}", self.status)
    }
}

impl std::error::Error for ProcessFailed {}

pub use hcore::process::Exit;

/// A run failure that crossed the plugin seam with a structured exit status
/// ([`ProcessFailed::exit`] on the plugin's side).
///
/// The wire carries the failure (`RunFailure`) as its rendered message, the
/// status and the failure's artifacts, so the host does not rebuild the
/// plugin's [`ProcessFailed`]: its `status` text is already folded into the
/// message, which is shown unchanged. The artifacts are rebuilt as the same
/// [`FailureArtifacts`] context they were on the plugin's side. The status
/// stays readable through [`process_exit`].
///
/// `#[non_exhaustive]`: built with [`RemoteProcessFailed::new`] outside this
/// crate, so a later field is not a source break.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RemoteProcessFailed {
    pub message: String,
    pub exit: Exit,
}

impl RemoteProcessFailed {
    pub fn new(message: String, exit: Exit) -> Self {
        Self { message, exit }
    }
}

impl ProcessFailed {
    pub fn new(status: impl Into<String>, exit: Option<Exit>) -> Self {
        Self {
            status: status.into(),
            exit,
        }
    }

    /// The failure of a child that exited with `status`.
    pub fn from_status(status: std::process::ExitStatus) -> Self {
        Self::new(status.to_string(), Exit::from_std(status))
    }

    /// The failure of a child whose output was captured whole rather than
    /// streamed: appends its stdout then stderr to `log_path` (the target's
    /// `log.txt`) and returns the failure with that log attached as a `Log`
    /// [`FailureArtifacts`] entry, so the output is rendered as the target's
    /// log tail instead of being pasted into the message.
    ///
    /// The log is best-effort: if it cannot be written, the output is attached
    /// inline (`Content::Raw`) instead, so it is not lost.
    pub fn from_output(log_path: &std::path::Path, output: &std::process::Output) -> anyhow::Error {
        use std::io::Write as _;
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .and_then(|mut f| {
                f.write_all(&output.stdout)?;
                f.write_all(&output.stderr)
            });
        let log = match written.ok().and_then(|()| log_file_artifact(log_path)) {
            Some(log) => log,
            None => {
                let mut data = Vec::with_capacity(output.stdout.len() + output.stderr.len());
                data.extend_from_slice(&output.stdout);
                data.extend_from_slice(&output.stderr);
                log_artifact(OaContent::Raw(ContentRaw {
                    data,
                    path: LOG_NAME.to_string(),
                    x: false,
                }))
            }
        };
        with_failure_artifacts(
            anyhow::Error::new(Self::from_status(output.status)),
            vec![log],
        )
    }
}

/// The name a run's log artifact has: the target's `log.txt`.
const LOG_NAME: &str = "log.txt";

fn log_artifact(content: OaContent) -> OutputArtifact {
    OutputArtifact {
        group: String::new(),
        name: LOG_NAME.to_string(),
        r#type: ArtifactType::Log,
        content,
        hashout: String::new(),
    }
}

/// The `Log` artifact for the run log at `path` (normally `<sandbox>/log.txt`),
/// for a driver to attach to its failure with [`with_failure_artifacts`].
/// `None` when the path is not UTF-8, which an artifact cannot name.
pub fn log_file_artifact(path: &std::path::Path) -> Option<OutputArtifact> {
    Some(log_artifact(OaContent::File(ContentFile {
        source_path: path.to_str()?.to_string(),
        out_path: LOG_NAME.to_string(),
        x: false,
        // The log lives in the sandbox: never a durable workspace file.
        passthrough: false,
    })))
}

/// What a failed run left behind for the diagnostic — normally its `Log`
/// artifact — riding the error as a context.
///
/// Data, not a cause: its `Display` is empty, and every renderer of a chain
/// (`hcore::hmemoizer::write_chain`, the CLI's cause chain) skips an empty
/// frame. Attach it with [`with_failure_artifacts`] and read it with
/// [`failure_artifacts`]; a plain `err.chain().find_map(downcast_ref)` never
/// finds a context.
pub struct FailureArtifacts(pub Vec<OutputArtifact>);

impl fmt::Debug for FailureArtifacts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(
                self.0
                    .iter()
                    .map(|a| (a.group.as_str(), a.name.as_str(), &a.r#type)),
            )
            .finish()
    }
}

impl fmt::Display for FailureArtifacts {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

impl std::error::Error for FailureArtifacts {}

/// Attach `artifacts` to `err` as a [`FailureArtifacts`] context.
///
/// Merges with the artifacts already on the chain: one with the same path
/// (`group` and `name`) as a new one is replaced — the outermost attachment is
/// authoritative — and the rest are kept after the new ones.
pub fn with_failure_artifacts(err: anyhow::Error, artifacts: Vec<OutputArtifact>) -> anyhow::Error {
    if artifacts.is_empty() {
        return err;
    }
    let merged = merge_artifacts(artifacts, failure_artifacts(&err).map(|p| p.0.as_slice()));
    err.context(FailureArtifacts(merged))
}

/// `new`, then each of `prev` whose path (`group` and `name`) no artifact in
/// `new` names: the outermost attachment is authoritative.
fn merge_artifacts(
    mut new: Vec<OutputArtifact>,
    prev: Option<&[OutputArtifact]>,
) -> Vec<OutputArtifact> {
    let Some(prev) = prev else {
        return new;
    };
    let kept: Vec<OutputArtifact> = prev
        .iter()
        .filter(|p| !new.iter().any(|m| m.group == p.group && m.name == p.name))
        .cloned()
        .collect();
    new.extend(kept);
    new
}

/// The outermost [`FailureArtifacts`] on `err`'s chain, through memoizer
/// wrappers too — which, by [`with_failure_artifacts`]'s merge, holds them all.
pub fn failure_artifacts(err: &anyhow::Error) -> Option<&FailureArtifacts> {
    hcore::hmemoizer::downcast_chain_ref::<FailureArtifacts>(err)
}

/// The fixes a failed `in_place` run left behind, for the engine to write back
/// over the tree while the target still fails — riding the error as a context
/// beside, never inside, [`FailureArtifacts`].
///
/// Its own context rather than another artifact type, so nothing can mistake
/// a fix for the diagnostics, and neither can reach a cache manifest: what a
/// failed run produced is never cached. Data, not a cause: its `Display` is
/// empty, like [`FailureArtifacts`]'s. Attach it with
/// [`with_failure_write_back`] and read it with [`failure_write_back`].
pub struct FailureWriteBack(pub Vec<OutputArtifact>);

impl fmt::Debug for FailureWriteBack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(
                self.0
                    .iter()
                    .map(|a| (a.group.as_str(), a.name.as_str(), &a.r#type)),
            )
            .finish()
    }
}

impl fmt::Display for FailureWriteBack {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

impl std::error::Error for FailureWriteBack {}

/// Attach `artifacts` to `err` as a [`FailureWriteBack`] context, merged with
/// the ones already on the chain exactly as [`with_failure_artifacts`] merges:
/// a new artifact replaces one with the same `group` and `name`, the rest are
/// kept after the new ones. Nothing to attach adds no frame.
pub fn with_failure_write_back(
    err: anyhow::Error,
    artifacts: Vec<OutputArtifact>,
) -> anyhow::Error {
    if artifacts.is_empty() {
        return err;
    }
    let merged = merge_artifacts(artifacts, failure_write_back(&err).map(|p| p.0.as_slice()));
    err.context(FailureWriteBack(merged))
}

/// The outermost [`FailureWriteBack`] on `err`'s chain, through memoizer
/// wrappers too — which, by [`with_failure_write_back`]'s merge, holds them all.
pub fn failure_write_back(err: &anyhow::Error) -> Option<&FailureWriteBack> {
    hcore::hmemoizer::downcast_chain_ref::<FailureWriteBack>(err)
}

/// The failure's `Log` artifact, if it carries one.
pub fn failure_log(err: &anyhow::Error) -> Option<&OutputArtifact> {
    failure_artifacts(err)?
        .0
        .iter()
        .find(|a| a.r#type == ArtifactType::Log)
}

/// The name of the [`FailureArtifacts`] entry that carries a failed run's
/// `failure_out` matches: a tar packed in the sandbox, its members
/// workspace-relative paths.
pub const FAILURE_OUT_NAME: &str = "failure_out";

/// The [`FailureArtifacts`] entry for the `failure_out` tar at `tar_path`:
/// an `Output` artifact named [`FAILURE_OUT_NAME`] whose content is the tar,
/// owned (the engine unpacks it into the failure slot and removes it). No new
/// artifact type and no new wire kind — an `OutputArtifact` with `TarPath`
/// content already crosses the plugin ABI on a `RunFailure`, and only
/// [`failure_out_tar`] reads it back.
pub fn failure_out_artifact(tar_path: String, hashout: String) -> OutputArtifact {
    OutputArtifact {
        group: String::new(),
        name: FAILURE_OUT_NAME.to_string(),
        r#type: ArtifactType::Output,
        content: OaContent::TarPath(ContentPath::owned(tar_path)),
        hashout,
    }
}

/// The `failure_out` tar the failure carries, if any (see
/// [`failure_out_artifact`]).
pub fn failure_out_tar(err: &anyhow::Error) -> Option<&OutputArtifact> {
    failure_artifacts(err)?.0.iter().find(|a| {
        a.r#type == ArtifactType::Output
            && a.name == FAILURE_OUT_NAME
            && matches!(a.content, OaContent::TarPath(_))
    })
}

impl fmt::Display for RemoteProcessFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RemoteProcessFailed {}

/// The structured exit status anywhere in `err`'s chain: a [`ProcessFailed`]
/// from an in-process driver, or a [`RemoteProcessFailed`] from a cdylib.
/// `None` when no process exit is behind the failure, or the driver that
/// produced it predates [`ProcessFailed::exit`].
pub fn process_exit(err: &anyhow::Error) -> Option<Exit> {
    err.chain().find_map(|e| {
        if let Some(pf) = e.downcast_ref::<ProcessFailed>() {
            pf.exit
        } else {
            e.downcast_ref::<RemoteProcessFailed>().map(|r| r.exit)
        }
    })
}

/// The rich, human-facing diagnostic for a genuinely-failing target. Recorded
/// once per addr in the per-request failure registry and rendered (custom, see
/// `commands::errors`) at the end of execution.
///
/// `source` is held behind an `Arc` so the whole diagnostic is `Clone`: the
/// outermost `result_addr` frame surfaces a clone of the recorded failure to its
/// direct caller (in place of the lightweight `UpstreamFailed` marker that
/// propagates internally), while the registry keeps the original for rendering.
#[derive(Debug, Clone)]
pub struct TargetFailure {
    pub addr: Addr,
    /// The last lines of the target's process log (the full log is still saved
    /// as the `log.txt` artifact), with the real starting line number so the box
    /// renders true file positions. Rendered as a framed `log` box.
    pub log_tail: Option<LogTail>,
    /// The log `log_tail` was read from: the engine's copy in the target's
    /// failure slot (open, with its path for the box's `full log:` footer), or
    /// the tail in memory when no copy is kept.
    pub log: Option<FailureLog>,
    /// The `out/` directory of the target's failure slot, when the failed run
    /// kept `failure_out` matches there: what the box prints as `outputs:`.
    pub outputs: Option<std::path::PathBuf>,
    /// What the `in_place` write-back on failure did, for a target with a
    /// `write_on_failure` output that failed with a process exit: the fixes
    /// it wrote, or why it wrote none. `None` for every other failure.
    pub fixes: Option<FailureFixes>,
    pub source: std::sync::Arc<anyhow::Error>,
}

/// What a failed `in_place` run's write-back did to the tree (D20, D21).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureFixes {
    /// The fixes written back over the tree while the target still failed.
    Written {
        /// Every path written, workspace-relative, sorted. Empty when the
        /// run's fixes matched the tree already.
        paths: Vec<String>,
        /// The slot's `fixes.txt` listing `paths`, one per line, when it was
        /// kept: the full list, past the box's truncation.
        list: Option<std::path::PathBuf>,
    },
    /// Nothing was written, and why.
    NotWritten(FixesNotWritten),
}

/// Why a failed `in_place` run wrote nothing back (D20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixesNotWritten {
    /// The run was killed by a signal: a fixer cut off mid-write.
    Signal(i32),
    /// The run exited 128 or above, a shell's report of a killed child
    /// (137 for an OOM kill, 143 for a timeout).
    HighExit(i32),
    /// An `in_place` source moved while the run read it (I5): writing would
    /// discard the newer bytes. A guard that could not run at all is not
    /// this; it is reported as a failed write on the error instead.
    MovedSource,
    /// `--frozen`: the run verifies the tree, it never writes it.
    Frozen,
}

impl TargetFailure {
    pub fn new(addr: Addr, log_tail: Option<LogTail>, source: anyhow::Error) -> Self {
        Self {
            addr,
            log_tail,
            log: None,
            outputs: None,
            fixes: None,
            source: std::sync::Arc::new(source),
        }
    }

    /// The same failure with what its `in_place` write-back did.
    pub fn with_fixes(mut self, fixes: Option<FailureFixes>) -> Self {
        self.fixes = fixes;
        self
    }

    /// The same failure with the log its tail came from.
    pub fn with_log(mut self, log: Option<FailureLog>) -> Self {
        self.log = log;
        self
    }

    /// The same failure with the slot's `out/` directory, when one was kept.
    pub fn with_outputs(mut self, outputs: Option<std::path::PathBuf>) -> Self {
        self.outputs = outputs;
        self
    }
}

impl fmt::Display for TargetFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "target failed: {}", self.addr.format())
    }
}

impl std::error::Error for TargetFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref().as_ref())
    }
}

/// Raised by `--frozen` codegen verification when a target's generated output
/// does not match what is currently on disk in the workspace tree. Carries the
/// offending target's addr and a concatenated unified-diff body (one diff per
/// differing file, tree=old / generated=new). Nothing is written in frozen mode.
#[derive(Debug)]
pub struct FrozenCheckError {
    pub addr: Addr,
    pub diff: String,
}

impl fmt::Display for FrozenCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "frozen check failed: {} — generated output differs from tree:",
            self.addr.format()
        )?;
        write!(f, "{}", self.diff)
    }
}

impl std::error::Error for FrozenCheckError {}

/// A target gated by `approval` was denied: the user rejected it interactively,
/// or (in a non-interactive context with no `--auto-approve`) no decision could
/// be obtained. Surfaced as the target's failure so the run exits non-zero.
#[derive(Debug, Clone)]
pub struct ApprovalDeniedError {
    pub addr: Addr,
}

impl fmt::Display for ApprovalDeniedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "approval denied for {}", self.addr.format())
    }
}

impl std::error::Error for ApprovalDeniedError {}

/// A bounded slice of a process log prepared for display: the last lines of the
/// full log, plus the real 1-based line number its first line had in the full log
/// so the renderer can show true file positions (e.g. 91–100 for the last 10 of a
/// 100-line log) instead of renumbering from 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogTail {
    pub text: String,
    /// 1-based line number of `text`'s first line within the full log.
    pub start_line: usize,
}

/// Returns the last `n` lines of `s`, joined with `\n`. A trailing newline does
/// not count as an extra empty line (`str::lines` already drops it).
pub fn last_n_lines(s: &str, n: usize) -> String {
    last_n_lines_with_start(s, n).0
}

/// Like [`last_n_lines`] but also returns the 1-based line number of the first
/// returned line within `s`, so callers can render real file positions rather
/// than renumbering the tail from 1.
pub fn last_n_lines_with_start(s: &str, n: usize) -> (String, usize) {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    let text = lines.get(start..).unwrap_or(&[]).join("\n");
    (text, start + 1)
}

/// How much of a log [`read_tail_at`] reads per backward step.
const TAIL_CHUNK: u64 = 8 * 1024;

/// How much of the part before the tail [`read_tail_at`] reads per step to
/// count its lines.
const COUNT_CHUNK: usize = 64 * 1024;

/// The last `n` lines of a log of `len` bytes, read through `read_at` (a
/// positional read: `read_at(buf, offset)`), with the same text and start
/// line [`last_n_lines_with_start`] gives for the whole log, lossily decoded.
///
/// Seek-based: it reads backwards from the end in [`TAIL_CHUNK`] steps until
/// it holds `n` line breaks, so only the tail is ever held in memory. The
/// start line still needs the number of lines before the tail, which is
/// counted in [`COUNT_CHUNK`] reads that are never kept — constant memory,
/// one pass over the head of the file.
pub fn read_tail_at(
    len: u64,
    mut read_at: impl FnMut(&mut [u8], u64) -> std::io::Result<usize>,
    n: usize,
) -> std::io::Result<LogTail> {
    let (cut, window) = tail_window(len, &mut read_at, n)?;
    let mut prefix_lines = 0usize;
    let mut buf = vec![0u8; COUNT_CHUNK];
    let mut off = 0u64;
    while off < cut {
        let want = usize::try_from((cut - off).min(COUNT_CHUNK as u64)).unwrap_or(COUNT_CHUNK);
        let got = buf.get_mut(..want).unwrap_or_default();
        read_exact_at(&mut read_at, got, off)?;
        prefix_lines += got.iter().filter(|&&b| b == b'\n').count();
        off += want as u64;
    }
    if n == 0 {
        // No lines shown; the "next" line is one past the last, which an
        // unterminated last line counts as.
        let mut last = [0u8; 1];
        let open = len > 0 && {
            read_exact_at(&mut read_at, &mut last, len - 1)?;
            last != *b"\n"
        };
        return Ok(LogTail {
            text: String::new(),
            start_line: prefix_lines + usize::from(open) + 1,
        });
    }
    let text = String::from_utf8_lossy(&window);
    let (text, local_start) = last_n_lines_with_start(&text, n);
    Ok(LogTail {
        text,
        start_line: prefix_lines + local_start,
    })
}

/// The backward half of [`read_tail_at`]: the offset the tail starts at (just
/// past a line break, or 0) and the bytes from there to `len`.
pub fn tail_window(
    len: u64,
    read_at: &mut impl FnMut(&mut [u8], u64) -> std::io::Result<usize>,
    n: usize,
) -> std::io::Result<(u64, Vec<u8>)> {
    if n == 0 {
        return Ok((len, Vec::new()));
    }
    let mut window: Vec<u8> = Vec::new();
    let mut start = len;
    loop {
        // A line break that ends the log closes its last line rather than
        // starting another, as `str::lines` has it.
        let trailing = usize::from(window.last() == Some(&b'\n'));
        let body = window.get(..window.len() - trailing).unwrap_or_default();
        // The n-th break from the end precedes the first line of the tail.
        if let Some((i, _)) = body
            .iter()
            .enumerate()
            .rev()
            .filter(|&(_, &b)| b == b'\n')
            .nth(n - 1)
        {
            let tail = window.split_off(i + 1);
            return Ok((start + i as u64 + 1, tail));
        }
        if start == 0 {
            return Ok((0, window));
        }
        let take = start.min(TAIL_CHUNK);
        let off = start - take;
        let mut chunk = vec![0u8; usize::try_from(take).unwrap_or(0)];
        read_exact_at(read_at, &mut chunk, off)?;
        chunk.extend_from_slice(&window);
        window = chunk;
        start = off;
    }
}

fn read_exact_at(
    read_at: &mut impl FnMut(&mut [u8], u64) -> std::io::Result<usize>,
    mut buf: &mut [u8],
    mut off: u64,
) -> std::io::Result<()> {
    while !buf.is_empty() {
        match read_at(buf, off) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "log shrank while its tail was read",
                ));
            }
            Ok(k) => {
                buf = buf.get_mut(k..).unwrap_or_default();
                off += k as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// A failed run's log as the diagnostic holds it: the engine's copy in the
/// target's failure slot, kept **open** so a later rerun replacing the slot
/// cannot swap the log this run's box shows; or, for a target whose log may
/// not be kept on disk (its outputs are credential material) or whose copy
/// could not be made, the tail already read into memory.
#[derive(Clone)]
pub struct FailureLog {
    path: Option<std::path::PathBuf>,
    source: FailureLogSource,
    lines: usize,
}

#[derive(Clone)]
enum FailureLogSource {
    Open(std::sync::Arc<std::fs::File>),
    Tail(LogTail),
}

impl fmt::Debug for FailureLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.source {
            FailureLogSource::Open(_) => "open",
            FailureLogSource::Tail(_) => "tail",
        };
        f.debug_struct("FailureLog")
            .field("path", &self.path)
            .field("source", &kind)
            .finish()
    }
}

impl FailureLog {
    /// The slot's log at `path`, through `file` opened on it; its tail is the
    /// last `lines` lines.
    pub fn open(path: std::path::PathBuf, file: std::fs::File, lines: usize) -> Self {
        Self {
            path: Some(path),
            source: FailureLogSource::Open(std::sync::Arc::new(file)),
            lines,
        }
    }

    /// A tail with no file behind it.
    pub fn in_memory(tail: LogTail) -> Self {
        Self {
            path: None,
            source: FailureLogSource::Tail(tail),
            lines: 0,
        }
    }

    /// Where the full log is, when there is one on disk.
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    /// The last lines of the log, read from the open file (seek-based, see
    /// [`read_tail_at`]) or the tail held in memory.
    pub fn tail(&self) -> std::io::Result<LogTail> {
        match &self.source {
            FailureLogSource::Tail(t) => Ok(t.clone()),
            FailureLogSource::Open(file) => {
                use std::os::unix::fs::FileExt as _;
                let len = file.metadata()?.len();
                read_tail_at(len, |buf, off| file.read_at(buf, off), self.lines)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A captured child's failure lands in the log (appended, both streams),
    /// and the error carries that log and the exit — not the output itself.
    #[test]
    fn from_output_writes_the_log_and_carries_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join("log.txt");
        std::fs::write(&log_path, "earlier step\n").expect("seed log");
        let output = std::process::Command::new("sh")
            .args(["-c", "echo on-stdout; echo on-stderr >&2; exit 3"])
            .output()
            .expect("run sh");

        let err = ProcessFailed::from_output(&log_path, &output).context("go compile failed");
        assert_eq!(process_exit(&err), Some(Exit::Code(3)));
        assert_eq!(
            hcore::hmemoizer::chain_message(&err),
            "go compile failed: process exited with status: exit status: 3"
        );
        let log = std::fs::read_to_string(&log_path).expect("read log");
        assert_eq!(log, "earlier step\non-stdout\non-stderr\n");
        let artifact = failure_log(&err).expect("a log artifact");
        assert_eq!(artifact.name, "log.txt");
        assert!(
            matches!(&artifact.content, OaContent::File(f)
                if f.source_path == log_path.to_str().expect("utf-8")),
            "the log is the file"
        );
    }

    /// A log that cannot be written does not lose the output.
    #[test]
    fn from_output_keeps_the_output_when_the_log_cannot_be_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = std::process::Command::new("sh")
            .args(["-c", "echo kept >&2; exit 1"])
            .output()
            .expect("run sh");
        let err = ProcessFailed::from_output(&dir.path().join("missing/log.txt"), &output);
        let log = failure_log(&err).expect("a log artifact");
        assert!(matches!(log.content, OaContent::Raw(_)), "inline fallback");
        let mut text = String::new();
        std::io::Read::read_to_string(
            &mut hcore::hartifactcontent::Content::reader(log).expect("reader"),
            &mut text,
        )
        .expect("read");
        assert_eq!(text, "kept\n");
    }

    fn raw(name: &str, data: &str) -> OutputArtifact {
        OutputArtifact {
            group: String::new(),
            name: name.to_string(),
            r#type: ArtifactType::Log,
            content: OaContent::Raw(ContentRaw {
                data: data.as_bytes().to_vec(),
                path: name.to_string(),
                x: false,
            }),
            hashout: String::new(),
        }
    }

    fn data(a: &OutputArtifact) -> &[u8] {
        match &a.content {
            OaContent::Raw(r) => &r.data,
            _ => &[],
        }
    }

    /// A `FailureArtifacts` frame renders as nothing: no `: :` in any message,
    /// wherever it sits in the chain, and through a memoizer's shared wrapper.
    #[test]
    fn empty_context_not_rendered() {
        let inner = with_failure_artifacts(
            anyhow::Error::new(ProcessFailed::new("exit status: 1", Some(Exit::Code(1)))),
            vec![raw("log.txt", "x")],
        )
        .context("go compile failed");
        let outer = with_failure_artifacts(inner, vec![raw("other.txt", "y")]).context("run");
        let want = "run: go compile failed: process exited with status: exit status: 1";
        assert_eq!(hcore::hmemoizer::chain_message(&outer), want);

        let arc = std::sync::Arc::new(outer);
        let shared =
            hcore::hmemoizer::unwrap_arc_err(std::sync::Arc::clone(&arc)).context("execute //p:t");
        assert_eq!(
            hcore::hmemoizer::chain_message(&shared),
            format!("execute //p:t: {want}")
        );
        // Outermost, with nothing above it: skipped, no separator invented.
        let bare = with_failure_artifacts(anyhow::anyhow!("boom"), vec![raw("log.txt", "x")]);
        assert_eq!(hcore::hmemoizer::chain_message(&bare), "boom");
    }

    /// Attaching twice merges: the outer attachment wins a path both name,
    /// the inner one's other artifacts are kept, and the accessor finds the
    /// merged set through contexts and a memoizer wrapper.
    #[test]
    fn with_failure_artifacts_merges() {
        let inner = with_failure_artifacts(
            anyhow::anyhow!("boom"),
            vec![raw("log.txt", "inner"), raw("trace.txt", "t")],
        )
        .context("step");
        let outer = with_failure_artifacts(inner, vec![raw("log.txt", "outer")]);
        let arc = std::sync::Arc::new(outer);
        let shared =
            hcore::hmemoizer::unwrap_arc_err(std::sync::Arc::clone(&arc)).context("execute");
        let got = failure_artifacts(&shared).expect("found");
        let names: Vec<&str> = got.0.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["log.txt", "trace.txt"]);
        assert_eq!(data(failure_log(&shared).expect("log")), b"outer");
        assert!(failure_artifacts(&anyhow::anyhow!("none")).is_none());
        // Nothing to attach adds no frame.
        let untouched = with_failure_artifacts(anyhow::anyhow!("plain"), vec![]);
        assert!(failure_artifacts(&untouched).is_none());
    }

    /// D6: a `FailureWriteBack` frame renders as nothing, like
    /// `FailureArtifacts`; it is found through contexts and a memoizer wrapper,
    /// merges like `FailureArtifacts`, and never reaches `failure_artifacts` or
    /// `failure_log` — a fix is not a diagnostic, and the log stays the log.
    #[test]
    fn failure_write_back_is_its_own_silent_context() {
        let failed = with_failure_artifacts(
            anyhow::Error::new(ProcessFailed::new("exit status: 1", Some(Exit::Code(1)))),
            vec![raw("log.txt", "the log")],
        );
        let inner = with_failure_write_back(failed, vec![raw("a.go", "inner"), raw("b.go", "b")])
            .context("go fmt failed");
        let outer = with_failure_write_back(inner, vec![raw("a.go", "outer")]).context("run");
        let want = "run: go fmt failed: process exited with status: exit status: 1";
        assert_eq!(hcore::hmemoizer::chain_message(&outer), want);

        let arc = std::sync::Arc::new(outer);
        let shared =
            hcore::hmemoizer::unwrap_arc_err(std::sync::Arc::clone(&arc)).context("execute //p:t");
        assert_eq!(
            hcore::hmemoizer::chain_message(&shared),
            format!("execute //p:t: {want}")
        );

        let wb = failure_write_back(&shared).expect("found");
        let names: Vec<&str> = wb.0.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["a.go", "b.go"]);
        assert_eq!(data(&wb.0[0]), b"outer");

        // The diagnostics are untouched by the write-back beside them.
        let fa = failure_artifacts(&shared).expect("diagnostics");
        let names: Vec<&str> = fa.0.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["log.txt"]);
        assert_eq!(data(failure_log(&shared).expect("log")), b"the log");

        // A write-back alone is no log, even an artifact typed `Log` (all of
        // `raw`'s are): the context, not the type, tells them apart. Nothing
        // to attach adds no frame.
        let only_fixes = with_failure_write_back(anyhow::anyhow!("boom"), vec![raw("a.go", "x")]);
        assert_eq!(hcore::hmemoizer::chain_message(&only_fixes), "boom");
        assert!(failure_log(&only_fixes).is_none());
        assert!(failure_artifacts(&only_fixes).is_none());
        let untouched = with_failure_write_back(anyhow::anyhow!("plain"), vec![]);
        assert!(failure_write_back(&untouched).is_none());
    }

    /// E5: the status is data next to an unchanged message.
    #[test]
    fn process_failed_exit_is_structured() {
        for (status, exit) in [
            ("exit status: 3", Some(Exit::Code(3))),
            ("signal: 15 (SIGTERM)", Some(Exit::Signal(15))),
            ("exit status: 1", None),
        ] {
            let pf = ProcessFailed::new(status, exit);
            // Byte-identical to the message before the field existed.
            assert_eq!(
                pf.to_string(),
                format!("process exited with status: {status}")
            );
            let wrapped = anyhow::Error::new(pf)
                .context("driver run")
                .context("execute //pkg:a");
            assert_eq!(process_exit(&wrapped), exit, "{status}");
        }

        let remote = anyhow::Error::new(RemoteProcessFailed::new(
            "running //p:t: process exited with status: exit status: 3".to_string(),
            Exit::Code(3),
        ))
        .context("driver run");
        assert_eq!(process_exit(&remote), Some(Exit::Code(3)));
        assert_eq!(
            format!("{:#}", remote),
            "driver run: running //p:t: process exited with status: exit status: 3",
            "the remote failure renders as its message, nothing added"
        );
        assert_eq!(process_exit(&anyhow::anyhow!("boom")), None);
    }

    #[test]
    fn last_n_lines_empty() {
        assert_eq!(last_n_lines("", 10), "");
    }

    #[test]
    fn last_n_lines_fewer_than_n() {
        assert_eq!(last_n_lines("a\nb", 10), "a\nb");
    }

    #[test]
    fn last_n_lines_more_than_n() {
        assert_eq!(last_n_lines("a\nb\nc\nd\ne", 3), "c\nd\ne");
    }

    #[test]
    fn last_n_lines_trailing_newline() {
        // The trailing newline is dropped by `lines()`, so the last real line wins.
        assert_eq!(last_n_lines("a\nb\nc\n", 2), "b\nc");
    }

    #[test]
    fn last_n_lines_with_start_reports_real_first_line() {
        // 5 lines, last 3 → first shown line is line 3 (1-based).
        assert_eq!(
            last_n_lines_with_start("a\nb\nc\nd\ne", 3),
            ("c\nd\ne".to_string(), 3)
        );
    }

    #[test]
    fn last_n_lines_with_start_fewer_than_n_starts_at_one() {
        assert_eq!(last_n_lines_with_start("a\nb", 10), ("a\nb".to_string(), 1));
    }

    #[test]
    fn last_n_lines_with_start_empty_starts_at_one() {
        assert_eq!(last_n_lines_with_start("", 10), (String::new(), 1));
    }
}
