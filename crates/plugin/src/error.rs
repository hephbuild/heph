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

/// A target names a driver no plugin registered. Typed so a diagnostic can
/// point at the target's `driver` option ([`DriverNotFound::FIELD`]), with
/// the registered drivers nearest the name (D4) and, for a name that is a
/// plugin's, that plugin's drivers.
#[derive(Debug, Clone)]
pub struct DriverNotFound {
    pub driver: String,
    /// The nearest registered driver names, best first.
    pub nearest: Vec<String>,
    /// The registered drivers of the plugin `driver` names (`archive` →
    /// `archive.create`, `archive.extract`), sorted.
    pub of_plugin: Vec<String>,
}

/// The most of a plugin's drivers an unknown-driver error lists.
const PLUGIN_DRIVERS_SHOWN: usize = 5;

impl DriverNotFound {
    /// The option of the declaring call that names the driver.
    pub const FIELD: &'static str = "driver";

    /// `driver` is not among `registered`, the registered driver names.
    pub fn new<'a>(driver: &str, registered: impl IntoIterator<Item = &'a str> + Clone) -> Self {
        let nearest = hcore::suggest::suggest(driver, registered.clone())
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut of_plugin: Vec<String> = registered
            .into_iter()
            .filter(|n| {
                n.strip_prefix(driver)
                    .is_some_and(|rest| rest.starts_with('.'))
            })
            .map(str::to_owned)
            .collect();
        of_plugin.sort_unstable();
        Self {
            driver: driver.to_owned(),
            nearest,
            of_plugin,
        }
    }

    /// The error, with a [`Diagnostic`](crate::diagnostic::Diagnostic)
    /// carrying the suggestions as hints over it.
    pub fn into_error(self) -> anyhow::Error {
        use hcore::diagnostic::{Diagnostic, Hint, HintKind, Stage};
        let names: Vec<&str> = self.nearest.iter().map(String::as_str).collect();
        let mut d = Diagnostic::new(Stage::Parse, self.headline());
        d.hints = hcore::suggest::did_you_mean(&names);
        if let Some(text) = self.plugin_drivers() {
            d.hints.push(Hint::new(HintKind::ValidValues, text));
        }
        anyhow::Error::new(self).context(d)
    }

    fn headline(&self) -> String {
        format!(
            "driver not found: {}",
            hcore::diagnostic::escape_control(&self.driver)
        )
    }

    /// "plugin `archive` has drivers: archive.create, archive.extract".
    fn plugin_drivers(&self) -> Option<String> {
        (!self.of_plugin.is_empty()).then(|| {
            format!(
                "plugin `{}` has drivers: {}",
                hcore::diagnostic::escape_control(&self.driver),
                hcore::suggest::listed(
                    self.of_plugin.iter().map(String::as_str),
                    PLUGIN_DRIVERS_SHOWN
                )
            )
        })
    }
}

impl fmt::Display for DriverNotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.headline())?;
        let names: Vec<&str> = self.nearest.iter().map(String::as_str).collect();
        if let Some(text) = hcore::suggest::did_you_mean_text(&names) {
            write!(f, "; {text}")?;
        }
        if let Some(text) = self.plugin_drivers() {
            write!(f, " ({text})")?;
        }
        Ok(())
    }
}

impl std::error::Error for DriverNotFound {}

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

/// True if `e` (anywhere in its source chain) is a `CancelledError`, or a
/// surfaced [`TargetFailure`] whose record says it was a stop: the failure it
/// stands for is no longer on the chain (D7b), only its record's facts.
pub fn is_cancelled(e: &anyhow::Error) -> bool {
    hcore::hmemoizer::downcast_chain_ref::<CancelledError>(e).is_some()
        || hcore::hmemoizer::downcast_chain_ref::<TargetFailure>(e)
            .is_some_and(|t| t.record.cancelled)
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

impl RevisionChangedError {
    /// What happened, without the address: the message of a failure whose
    /// record already names [`addr`](Self::addr).
    pub fn what(&self) -> String {
        format!(
            "its cached output (revision {}) changed or was removed by another heph process \
             while this build was using it; re-run",
            self.hashin
        )
    }
}

impl fmt::Display for RevisionChangedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.addr.format(), self.what())
    }
}

impl std::error::Error for RevisionChangedError {}

/// How many group members the `--shell` diagnostic lists before eliding.
const SHELL_MEMBERS_SHOWN: usize = 5;

/// `--shell` was refused: the request does not resolve to a single executing
/// target (a multi-target selection, or a transparent group that is not an
/// alias for one target — including a group named directly by
/// `--shell=ADDR`, which never executes and so has no sandbox to enter), or
/// it does, but there is no terminal to hand it to.
///
/// A property of the *request*, not a failure of any target: nothing ran, and
/// nothing was going to. So `classify_failure` propagates it unchanged rather
/// than recording it against whichever frame happened to raise it — a nested
/// group would otherwise render the same user error as a failed-target box for
/// a group that never executed.
///
/// The fields stay structured so a caller can render or serialize them; the
/// `Display` is the human-facing form and always names the next command to run,
/// so the message is recoverable on its own by a human or an agent. The action
/// it names is `--shell=<addr>`, which is the form that actually resolves the
/// ambiguity being reported: it keeps the run the user asked for and only says
/// which of its targets gets the terminal.
#[derive(Debug, Clone)]
pub enum ShellNeedsSingleTarget {
    /// A query or label/package matcher that selects more than one target.
    Selection {
        /// The selection as the user wrote it.
        query: String,
    },
    /// A transparent group whose inputs are not exactly one distinct member, or
    /// any group named directly by `--shell=ADDR`.
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
                "--shell needs exactly one target; {query} selects many\
                 \n  name the one to shell into: heph run --shell=<address> {query}\
                 \n  try: heph query {query}"
            ),
            Self::Group { addr, members } => {
                let n = members.len();
                write!(
                    f,
                    "--shell needs exactly one target; {} is a group with {n} member{}",
                    addr.format(),
                    if n == 1 { "" } else { "s" }
                )?;
                let Some(first) = members.first() else {
                    // No member to name, so the action has to be discovery. Every
                    // `try:` line in this error is runnable verbatim — a template
                    // there is a command an agent will execute for real.
                    return write!(
                        f,
                        "\n  the group is empty — there is nothing to shell into\
                         \n  try: heph query //{}",
                        addr.package
                    );
                };
                f.write_str("\n  members: ")?;
                for (i, member) in members.iter().take(SHELL_MEMBERS_SHOWN).enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(&member.format())?;
                }
                let rest = n.saturating_sub(SHELL_MEMBERS_SHOWN);
                if rest > 0 {
                    write!(f, " … and {rest} more")?;
                }
                write!(
                    f,
                    "\n  name the member to shell into — {1} still builds as asked\
                     \n  try: heph run --shell={0} {1}",
                    first.format(),
                    addr.format()
                )
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

/// A dependency cycle.
///
/// `path` follows the dependency edges and ends on the member it starts
/// from: `[w, x, y, z, w]`, or `[a, a]` for a target that depends on itself.
/// A path of one member, [`CycleError::through`], is a cycle reported across
/// the plugin seam, which carries no path: that member is in it, the rest is
/// not known.
#[derive(Debug, Clone)]
pub struct CycleError {
    pub path: Vec<Addr>,
}

impl CycleError {
    /// A cycle known only to pass through `addr`.
    pub fn through(addr: Addr) -> Self {
        Self { path: vec![addr] }
    }
}

impl fmt::Display for CycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cyclic dependency detected")?;
        match self.path.as_slice() {
            [] => Ok(()),
            [only] => write!(f, " through {}", only.format()),
            [first, rest @ ..] => {
                write!(f, ": {}", first.format())?;
                for addr in rest {
                    write!(f, " → {}", addr.format())?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for CycleError {}

/// Lives next to the error walker, which visits its members.
pub use hcore::hmemoizer::MultiError;

/// Lightweight propagation marker meaning "I failed only because a dependency
/// failed." Propagates UP the graph and is **never** rendered. It NEVER wraps a
/// cause — collateral hops replace their incoming error with a fresh
/// `UpstreamFailed`, keeping chain depth O(1) on any graph. `root` identifies the
/// culprit whose `TargetFailure` is recorded in the registry.
#[derive(Debug, Clone)]
pub struct UpstreamFailed {
    pub root: Culprit,
}

impl fmt::Display for UpstreamFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dependency failed (root: {})", self.root)
    }
}

impl std::error::Error for UpstreamFailed {}

/// What a failure is recorded against: the one thing that is actually wrong.
///
/// A target whose own spec, def or run failed, or that declared a dependency
/// that does not exist, is a [`Culprit::Target`]. A source file a provider
/// reported as not evaluating ([`FileEvalFailed`]; e.g. the buildfile
/// provider's BUILD file) is a [`Culprit::File`]: none of its targets was ever
/// defined, so none of them is to blame. The file is the workspace-relative
/// path, and it never goes into an [`Addr`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Culprit {
    Target(Addr),
    File(std::sync::Arc<str>),
}

impl Culprit {
    /// The target, for a target culprit.
    pub fn target(&self) -> Option<&Addr> {
        match self {
            Self::Target(addr) => Some(addr),
            Self::File(_) => None,
        }
    }

    /// The workspace-relative path, for a file culprit.
    pub fn file(&self) -> Option<&str> {
        match self {
            Self::Target(_) => None,
            Self::File(file) => Some(file),
        }
    }
}

impl From<Addr> for Culprit {
    fn from(addr: Addr) -> Self {
        Self::Target(addr)
    }
}

impl fmt::Display for Culprit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Target(addr) => write!(f, "{}", addr.format()),
            Self::File(file) => f.write_str(file),
        }
    }
}

/// Context: `declarer` was resolving `input`'s def, named by its option
/// `origin_id` (deps, tools, a runner, a group member, …), and the def failed.
///
/// A context, holding no error: `downcast_chain_ref` looks through contexts
/// and the memoizer's shared wrapper, never into a struct's field, so a
/// `source` field would hide a `CancelledError` or `CycleError` beneath it.
/// The failure classifier blames the innermost such frame's `input` (or a
/// deeper `get spec:`/`get_def:` [`StageFrame`]'s address), never the
/// declarer.
///
/// [`StageFrame`]: crate::diagnostic::StageFrame
#[derive(Debug, Clone)]
pub struct InputDefFailed {
    pub input: Addr,
    pub declarer: Addr,
    pub origin_id: String,
}

impl fmt::Display for InputDefFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (`{}` of {})",
            self.input.format(),
            self.origin_id,
            self.declarer.format()
        )
    }
}

impl std::error::Error for InputDefFailed {}

/// Context over a [`TargetNotFoundError`]: `declarer` names `dep` in
/// `origin_id`, and no target `dep` exists. The declarer is the culprit.
///
/// Raised at a site that resolves a declared ref only when the not-found
/// names that very ref (output group aside): a not-found for a deeper
/// address passes through and is blamed where *that* ref is declared. The
/// `TargetNotFoundError` stays underneath, downcastable.
#[derive(Debug, Clone)]
pub struct MissingDep {
    pub dep: Addr,
    pub declarer: Addr,
    pub origin_id: String,
    /// Where the declarer declares it: its [`SpecSource`]'s call, or finer
    /// when its provider can locate the declaration's field. `None` when the
    /// declarer's spec carries no source (a provider that reports none).
    /// The message says when the file changed since it was evaluated
    /// ([`DeclAt::changed`]).
    ///
    /// [`SpecSource`]: crate::provider::SpecSource
    /// [`DeclAt::changed`]: crate::provider::DeclAt::changed
    pub at: Option<crate::provider::DeclAt>,
}

impl fmt::Display for MissingDep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} depends on {}, which does not exist (`{}`, declared ",
            self.declarer.format(),
            self.dep.format(),
            self.origin_id,
        )?;
        if let Some(at) = &self.at {
            return write!(f, "at {at})");
        }
        // Without a source, the package is the closest the message can
        // point to: whatever source its provider declared it from is that
        // package's.
        f.write_str("in ")?;
        let pkg = self.declarer.package.as_str();
        if pkg.is_empty() {
            f.write_str("the root package)")
        } else {
            write!(f, "package //{pkg})")
        }
    }
}

impl std::error::Error for MissingDep {}

impl MissingDep {
    /// Whether `e` is a not-found of `dep` itself, as a site resolving the
    /// ref `dep` must check before raising a [`MissingDep`]. A not-found for
    /// any other address (`//p:a` while resolving `//p:ab`, a dep of `dep`)
    /// is not.
    pub fn names(e: &anyhow::Error, dep: &Addr) -> bool {
        hcore::hmemoizer::downcast_chain_ref::<TargetNotFoundError>(e)
            .is_some_and(|nf| nf.addr == *dep)
    }

    /// `e` with a [`MissingDep`] when it is a not-found of `dep`, else `e` as
    /// `otherwise` wraps it (a context, or a [`StageFrame`] through
    /// [`with_stage`](crate::diagnostic::with_stage)).
    ///
    /// `at` is asked, with the origin `origin_id` gave, where the declarer
    /// declares it ([`MissingDep::at`]); only on the not-found path.
    ///
    /// [`StageFrame`]: crate::diagnostic::StageFrame
    pub fn wrap_or(
        e: anyhow::Error,
        dep: &Addr,
        declarer: &Addr,
        origin_id: impl FnOnce() -> String,
        at: impl FnOnce(&str) -> Option<crate::provider::DeclAt>,
        otherwise: impl FnOnce(anyhow::Error) -> anyhow::Error,
    ) -> anyhow::Error {
        if Self::names(&e, dep) {
            let origin_id = origin_id();
            return e.context(Self {
                dep: dep.clone(),
                declarer: declarer.clone(),
                at: at(&origin_id),
                origin_id,
            });
        }
        otherwise(e)
    }
}

/// Context: a provider's evaluation of this source file failed (e.g. the
/// buildfile provider's `d/BUILD`). `file` is workspace-relative, as the
/// provider reported it.
///
/// Data riding the chain, not text: its `Display` is empty, so the message
/// reads as the file's own [`Diagnostic`](crate::diagnostic::Diagnostic)
/// does (which already names the file, workspace-relative).
///
/// The provider attaches it once, for the file that is itself broken: a
/// file that failed only through another (one it loads, say) adds none of
/// its own, so the one
/// `FileEvalFailed` on a chain is the innermost: the file that is actually
/// broken.
#[derive(Debug, Clone)]
pub struct FileEvalFailed {
    pub file: std::sync::Arc<str>,
}

impl fmt::Display for FileEvalFailed {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

impl std::error::Error for FileEvalFailed {}

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

/// A run whose process exited zero, failing in a step past it: collecting
/// its declared outputs (one missing, or of the wrong kind). The host's
/// record reads it as `FailedIn::AfterRun`, as it does a failure while
/// hashing the artifacts: the process went fine, so a run that streamed its
/// output still gets its box, and that box says so rather than repeating the
/// log. Empty `Display`: it marks a chain, it adds no words to it.
///
/// In-process only: it does not cross the plugin seam, where the failure is
/// a message, so a cdylib driver's collection failure stays `BeforeRun`.
#[derive(Debug, Clone, Copy, Default)]
pub struct AfterProcess;

impl AfterProcess {
    /// `e`, marked as failing past a process that went fine.
    pub fn mark(e: anyhow::Error) -> anyhow::Error {
        e.context(Self)
    }
}

impl fmt::Display for AfterProcess {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

impl std::error::Error for AfterProcess {}

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

    /// [`from_output`](Self::from_output) for a driver that has the run's
    /// request: under [`RunRequest::log_capture`] the output goes to the run's
    /// own log — stdout to `req.stdout`, stderr to `req.stderr`, where the
    /// host scrubs and stores it — and the failure carries no `Log` artifact.
    /// Otherwise it is `from_output(log_path, output)`, whose log (a file, or
    /// inline bytes when the file cannot be written) the host scrubs like any
    /// returned `Log` artifact.
    ///
    /// A write to the request's writers that fails falls back to an inline
    /// `Log` artifact holding the whole output, so the output is not lost.
    pub async fn from_run_output(
        req: &mut crate::driver::RunRequest<'_, '_>,
        log_path: &std::path::Path,
        output: &std::process::Output,
    ) -> anyhow::Error {
        use tokio::io::AsyncWriteExt as _;
        if !req.log_capture {
            return Self::from_output(log_path, output);
        }
        let mut streamed = true;
        for (writer, bytes) in [
            (req.stdout.as_deref_mut(), &output.stdout),
            (req.stderr.as_deref_mut(), &output.stderr),
        ] {
            if bytes.is_empty() {
                continue;
            }
            let Some(w) = writer else {
                streamed = false;
                break;
            };
            let wrote = match w.write_all(bytes).await {
                Ok(()) => w.flush().await,
                Err(e) => Err(e),
            };
            if wrote.is_err() {
                streamed = false;
                break;
            }
        }
        let err = anyhow::Error::new(Self::from_status(output.status));
        if streamed {
            return err;
        }
        let mut data = Vec::with_capacity(output.stdout.len() + output.stderr.len());
        data.extend_from_slice(&output.stdout);
        data.extend_from_slice(&output.stderr);
        with_failure_artifacts(
            err,
            vec![log_artifact(OaContent::Raw(ContentRaw {
                data,
                path: LOG_NAME.to_string(),
                x: false,
            }))],
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
/// produced it predates [`ProcessFailed::exit`]. A surfaced
/// [`TargetFailure`] answers from its record's exit.
pub fn process_exit(err: &anyhow::Error) -> Option<Exit> {
    // Through the walker: a failure a memoizer shared, or one under a stage
    // frame, has its process behind a `SharedAnyhow` that `chain()` stops at.
    hcore::hmemoizer::walk(err, &mut |f| {
        let exit = if let Some(pf) = f.downcast_ref::<ProcessFailed>() {
            pf.exit
        } else if let Some(r) = f.downcast_ref::<RemoteProcessFailed>() {
            Some(r.exit)
        } else {
            f.downcast_ref::<TargetFailure>()
                .and_then(|t| t.record.exit.as_ref())
                .and_then(|x| match (x.code, x.signal) {
                    (Some(code), _) => Some(Exit::Code(code)),
                    (None, Some(sig)) => Some(Exit::Signal(sig)),
                    (None, None) => None,
                })
        };
        match exit {
            Some(exit) => std::ops::ControlFlow::Break(exit),
            None => std::ops::ControlFlow::Continue(()),
        }
    })
    .break_value()
}

/// The rich, human-facing diagnostic for a genuinely-failing target. Recorded
/// once per addr in the per-request failure registry and rendered (custom, see
/// `commands::errors`) at the end of execution.
///
/// It holds no error chain (D7b): what a renderer, `is_cancelled` or
/// `process_exit` reads of the failure is its [`record`](Self::record). The
/// outermost `result_addr` frame surfaces a clone of the recorded failure to
/// its direct caller (in place of the lightweight `UpstreamFailed` marker that
/// propagates internally), while the registry keeps the original for rendering.
#[derive(Debug, Clone)]
pub struct TargetFailure {
    /// The culprit target; for a file culprit, the address that led into the
    /// file (a target the file would have defined).
    pub addr: Addr,
    /// The workspace-relative source file a provider reported as not
    /// evaluating, when the culprit is that file rather than a target ([`Culprit::File`]).
    pub file: Option<std::sync::Arc<str>>,
    /// The shortest path from a requested target to [`addr`](Self::addr),
    /// inclusive (`[addr]` when it was itself requested), read off the
    /// request's dependency graph when the failure was recorded, and read
    /// again off the complete graph when the request hands its failures out
    /// (`take_failures` / `failures`), so the reported path does not depend
    /// on scheduling. Empty when no requested target reaches it, or when
    /// nobody computed it.
    pub path: Vec<Addr>,
    /// Where the target's log is: the engine's copy in its failure slot, for
    /// the box's `full log:` footer. Never its text: a renderer reads the
    /// tail through the record's [`LogRef`](hcore::failure::LogRef), which
    /// holds the scrubbed in-memory tail of a log kept nowhere on disk.
    pub log: Option<FailureLog>,
    /// The `out/` directory of the target's failure slot, when the failed run
    /// kept `failure_out` matches there: what the box prints as `outputs:`.
    pub outputs: Option<std::path::PathBuf>,
    /// What the `in_place` write-back on failure did, for a target with a
    /// `write_on_failure` output that failed with a process exit: the fixes
    /// it wrote, or why it wrote none. `None` for every other failure.
    pub fixes: Option<FailureFixes>,
    /// What became of the failed run's sandbox: the box's `sandbox:` line.
    /// `None` when there is nothing to say — the run had no sandbox (a plain
    /// driver, a failure before its tree existed), or its removal was not a
    /// choice (credential material without `--keep-failed-sandboxes`).
    pub sandbox: Option<SandboxFate>,
    /// Whether the engine can open a log for this target without fetching
    /// (its log API over local sources), asked once when the run ends: what
    /// decides between `heph log <addr>` and "none kept". `None` when nobody
    /// asked; a renderer then falls back to whether `log` has a path.
    pub log_readable: Option<bool>,
    /// The run's output reached a terminal while it ran: heph wrote at least
    /// one byte of it to a TTY. The log and its tail are kept regardless; a
    /// renderer leaves the tail out of the box and points at the log instead
    /// (D6.1). False for a target that printed nothing, and for output that
    /// went to a file or a pipe.
    pub streamed: bool,
    /// The failure compacted (D7b): scrubbed, capped, with the typed facts a
    /// renderer reads. The engine sets it when it records the culprit
    /// (`engine::failure_record::compact`); one built here with
    /// [`TargetFailure::new`] and never set says only which target failed,
    /// so no unscrubbed text can reach it.
    pub record: std::sync::Arc<FailureRecord>,
    /// The failure was a revision this request resolved changing under it
    /// ([`RevisionChangedError`]): a re-run resolves afresh. Kept typed
    /// because the chain it was read from is not (D7b); it holds an address
    /// and a hash, no text a scrub would have to see.
    pub revision_changed: Option<RevisionChangedError>,
    /// [`record`](Self::record) was set by [`with_record`](Self::with_record):
    /// the failure's own compacted record, not the placeholder `new` makes.
    compacted: bool,
}

pub use hcore::events::FailureRecord;

/// What became of a failed run's sandbox, as the failure box reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxFate {
    /// Removed once the failure slot was filled: the default, which
    /// `--keep-failed-sandboxes` would have changed. Neither the box nor the
    /// report says anything about it.
    Removed,
    /// Left on disk under `--keep-failed-sandboxes`, at this path, until the
    /// target's next run.
    Kept(std::path::PathBuf),
    /// Removed in spite of `--keep-failed-sandboxes`: the run's outputs are
    /// credential material, which no diagnostic is worth leaving on disk.
    RemovedMaterial,
    /// Left on disk because the run timed out and was abandoned: its process
    /// may still be writing to the tree, so nothing removes it under that
    /// writer. The target's next run reclaims it.
    LeftAbandoned,
}

impl fmt::Display for SandboxFate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Removed => f.write_str("removed"),
            Self::Kept(path) => write!(f, "{} (kept)", path.display()),
            Self::RemovedMaterial => f.write_str("removed (credential material)"),
            Self::LeftAbandoned => f.write_str("left (run abandoned after timeout)"),
        }
    }
}

/// What a failed `in_place` run's write-back did to the tree (D20, D21).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureFixes {
    /// The fixes written back over the tree while the target still failed.
    Written {
        /// The workspace root the fixes were written under: what `paths` are
        /// relative to, and where the box's restore hint runs git.
        root: std::path::PathBuf,
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

/// Why nothing was written, as the box and the report say it.
impl fmt::Display for FixesNotWritten {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Signal(sig) => write!(f, "killed by signal {sig}"),
            Self::HighExit(code) => write!(f, "exit {code} (128+, treated as a signal)"),
            Self::MovedSource => f.write_str("an in_place source changed while the target ran"),
            Self::Frozen => f.write_str("--frozen checks the tree, it never writes it"),
        }
    }
}

impl TargetFailure {
    pub fn new(addr: Addr) -> Self {
        let formatted = addr.format();
        let mut record = FailureRecord::new(
            crate::diagnostic::Diagnostic::new(
                crate::diagnostic::Stage::Run,
                format!("target failed: {formatted}"),
            )
            .with_addr(formatted.clone()),
        );
        record.addr = Some(formatted.clone());
        record.entry = formatted;
        Self {
            addr,
            file: None,
            path: Vec::new(),
            log: None,
            outputs: None,
            fixes: None,
            sandbox: None,
            log_readable: None,
            streamed: false,
            record: std::sync::Arc::new(record),
            revision_changed: None,
            compacted: false,
        }
    }

    /// The same failure, saying whether it was a revision changing under
    /// the request.
    pub fn with_revision_changed(mut self, changed: Option<RevisionChangedError>) -> Self {
        self.revision_changed = changed;
        self
    }

    /// The same failure with its compacted record.
    pub fn with_record(mut self, record: std::sync::Arc<FailureRecord>) -> Self {
        self.record = record;
        self.compacted = true;
        self
    }

    /// The same failure, saying whether its output streamed to a terminal.
    pub fn with_streamed(mut self, streamed: bool) -> Self {
        self.streamed = streamed;
        self
    }

    /// The same failure, blamed on `file` (which did not evaluate) rather
    /// than on [`addr`](Self::addr), the address that led into it.
    pub fn with_file(mut self, file: Option<std::sync::Arc<str>>) -> Self {
        self.file = file;
        self
    }

    /// The same failure, reached from a requested target along `path`.
    pub fn with_path(mut self, path: Vec<Addr>) -> Self {
        self.path = path;
        self
    }

    /// What this failure is recorded against.
    pub fn culprit(&self) -> Culprit {
        match &self.file {
            Some(file) => Culprit::File(std::sync::Arc::clone(file)),
            None => Culprit::Target(self.addr.clone()),
        }
    }

    /// The same failure, saying whether the engine can open its log.
    pub fn with_log_readable(mut self, readable: bool) -> Self {
        self.log_readable = Some(readable);
        self
    }

    /// Whether to point at `heph log <addr>`: the engine's answer when it
    /// gave one, else whether the slot kept the log on disk.
    pub fn points_at_log(&self) -> bool {
        self.log_readable
            .unwrap_or_else(|| self.log.as_ref().and_then(|l| l.path()).is_some())
    }

    /// The same failure, saying what became of its sandbox.
    pub fn with_sandbox(mut self, sandbox: Option<SandboxFate>) -> Self {
        self.sandbox = sandbox;
        self
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

    /// What this failure left behind, as the failure box's last rows read it.
    pub fn left_behind(&self) -> hcore::events::LeftBehindData {
        left_behind(
            self.log.as_ref(),
            self.sandbox.as_ref(),
            self.outputs.as_deref(),
            self.fixes.as_ref(),
            self.log_readable,
        )
    }
}

/// What a failed run left behind, in the event's terms: the one conversion
/// both the recorded failure ([`TargetFailure::left_behind`]) and the
/// engine's `ResultEnd` (from the failure slot) go through, so the box and a
/// live printer say the same thing.
pub fn left_behind(
    log: Option<&FailureLog>,
    sandbox: Option<&SandboxFate>,
    outputs: Option<&std::path::Path>,
    fixes: Option<&FailureFixes>,
    log_readable: Option<bool>,
) -> hcore::events::LeftBehindData {
    hcore::events::LeftBehindData {
        log_path: log
            .and_then(FailureLog::path)
            .map(|p| p.display().to_string()),
        sandbox: sandbox.and_then(|s| match s {
            // The default: neither the box nor the report says anything.
            SandboxFate::Removed => None,
            other => Some(other.to_string()),
        }),
        outputs: outputs.map(|p| p.display().to_string()),
        fixes: fixes.map(|f| match f {
            FailureFixes::Written { root, paths, list } => hcore::events::FixesData::Written {
                root: root.to_string_lossy().into_owned(),
                paths: paths.clone(),
                list: list.as_ref().map(|l| l.to_string_lossy().into_owned()),
                paths_more: 0,
            },
            FailureFixes::NotWritten(reason) => hcore::events::FixesData::NotWritten {
                reason: reason.to_string(),
            },
        }),
        log_readable,
        // Counted when the slot is filled, and carried by its record.
        kept: Vec::new(),
        kept_more: 0,
        // The slot's, which only the engine knows.
        log_fill_id: None,
    }
}

impl fmt::Display for TargetFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.file {
            Some(file) => write!(f, "{file} could not be evaluated"),
            None => write!(f, "target failed: {}", self.addr.format()),
        }
    }
}

/// The chain the failure was recorded from is not kept (D7b): its source is
/// the record's diagnostic, already scrubbed and bounded, so `{:#}` of a
/// surfaced failure still says its cause. A failure never compacted has
/// none (its diagnostic only repeats [`Display`](fmt::Display)).
impl std::error::Error for TargetFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.compacted
            .then_some(&self.record.diagnostic as &(dyn std::error::Error + 'static))
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

/// The tail of a log, as [`hcore::failure_log`] reads it: the one reader of a
/// failure slot's format, re-exported here for the drivers and renderers
/// that always named it from this module.
pub use hcore::failure_log::{
    LogTail, TAIL_CUT_MARK, TAIL_LINES_CAP, TAIL_MAX_BYTES, last_n_lines, last_n_lines_with_start,
    read_tail_at, tail_window,
};

/// A failed run's log as the diagnostic holds it: the path of the engine's
/// copy in the target's failure slot, never an open file (E2: a rerun
/// replacing the slot is told apart by the record's fill id, not by holding
/// the old inode); or, for a target whose log may not be kept on disk (its
/// outputs are credential material) or whose copy could not be made, the
/// tail already read into memory, within [`RECORD_CAP`](hcore::failure::RECORD_CAP).
#[derive(Debug, Clone, Default)]
pub struct FailureLog {
    path: Option<std::path::PathBuf>,
    memory: Option<LogTail>,
    /// The in-memory tail was cut to fit the record cap.
    memory_cut: bool,
    lines: usize,
}

impl FailureLog {
    /// The slot's log at `path`; its tail is the last `lines` lines.
    pub fn on_disk(path: std::path::PathBuf, lines: usize) -> Self {
        Self {
            path: Some(path),
            memory: None,
            memory_cut: false,
            lines,
        }
    }

    /// A tail with no file behind it; `cut` when it was cut to fit the
    /// record cap, which the record then says (`truncated`, E2).
    pub fn in_memory(tail: LogTail, cut: bool) -> Self {
        Self {
            path: None,
            memory: Some(tail),
            memory_cut: cut,
            lines: 0,
        }
    }

    /// The same log without its in-memory tail: what a holder that never
    /// prints the tail keeps (the record holds the scrubbed copy).
    pub fn without_memory(&self) -> Self {
        Self {
            path: self.path.clone(),
            memory: None,
            memory_cut: false,
            lines: self.lines,
        }
    }

    /// Whether the in-memory tail was cut to fit the record cap.
    pub fn memory_cut(&self) -> bool {
        self.memory_cut
    }

    /// Where the full log is, when there is one on disk.
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    /// The tail held in memory, for a log with no file behind it; `None`
    /// for one on disk. No IO.
    pub fn memory_tail(&self) -> Option<&LogTail> {
        self.memory.as_ref()
    }

    /// The last lines of the log: the tail held in memory, else read from
    /// the slot's file at [`path`](Self::path) (seek-based, see
    /// [`read_tail_at`]), whatever is there now.
    pub fn tail(&self) -> std::io::Result<LogTail> {
        if let Some(t) = &self.memory {
            return Ok(t.clone());
        }
        let Some(path) = &self.path else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no log kept",
            ));
        };
        use std::os::unix::fs::FileExt as _;
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        read_tail_at(len, |buf, off| file.read_at(buf, off), self.lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D4: an unknown driver names the nearest registered one, in its text
    /// (what a `{:#}` log shows) and as a hint on its diagnostic; a name
    /// nothing is near gets neither.
    #[test]
    fn driver_not_found_suggests_the_nearest_driver() {
        use hcore::diagnostic::{Hint, HintKind};
        let registered = ["bash", "exec", "fs", "group"];
        let e = DriverNotFound::new("bsh", registered.iter().copied()).into_error();
        assert_eq!(
            format!("{e:#}"),
            "driver not found: bsh: driver not found: bsh; did you mean `bash`?"
        );
        let d = crate::diagnostic::find_diagnostic(&e).expect("a diagnostic");
        assert_eq!(d.message, "driver not found: bsh");
        assert_eq!(d.hints, vec![Hint::new(HintKind::DidYouMean, "bash")]);
        assert!(
            hcore::hmemoizer::downcast_chain_ref::<DriverNotFound>(&e).is_some(),
            "the typed error stays on the chain, for its location"
        );

        let e = DriverNotFound::new("zzzz", registered.iter().copied());
        assert_eq!(e.to_string(), "driver not found: zzzz");
    }

    /// A name that is a plugin's, not a driver's, lists that plugin's
    /// drivers; a name that only starts like one (`arch` for `archive.x`)
    /// does not.
    #[test]
    fn driver_not_found_lists_a_plugins_drivers() {
        let registered = ["archive.extract", "archive.create", "archived.x", "bash"];
        let e = DriverNotFound::new("archive", registered.iter().copied());
        assert_eq!(
            e.to_string(),
            "driver not found: archive (plugin `archive` has drivers: archive.create, archive.extract)"
        );
        let e = DriverNotFound::new("arch", registered.iter().copied());
        assert!(e.of_plugin.is_empty(), "{e}");
    }

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

    fn sh_output(script: &str) -> std::process::Output {
        std::process::Command::new("sh")
            .args(["-c", script])
            .output()
            .expect("run sh")
    }

    /// Under `log_capture` the output is the run's log: each stream goes to its
    /// own writer, the failure carries no `Log` artifact, and no file is written.
    #[tokio::test]
    async fn from_run_output_under_log_capture_writes_the_writers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join("log.txt");
        let output = sh_output("echo on-stdout; echo on-stderr >&2; exit 3");
        let (id, target) = (
            String::from("r"),
            crate::driver::targetdef::TargetDef::default(),
        );
        let (mut out, mut errw) = (Vec::<u8>::new(), Vec::<u8>::new());
        let mut req = crate::driver::RunRequest::new(&id, &target, dir.path());
        req.stdout = Some(&mut out);
        req.stderr = Some(&mut errw);
        req.log_capture = true;

        let err = ProcessFailed::from_run_output(&mut req, &log_path, &output).await;
        drop(req);
        assert_eq!(process_exit(&err), Some(Exit::Code(3)));
        assert!(failure_log(&err).is_none(), "the writers are the log");
        assert!(!log_path.exists(), "no own log file under log_capture");
        assert_eq!(out, b"on-stdout\n");
        assert_eq!(errw, b"on-stderr\n");
    }

    /// Without `log_capture` it is `from_output`: the log file, attached.
    #[tokio::test]
    async fn from_run_output_without_log_capture_writes_its_own_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join("log.txt");
        let output = sh_output("echo kept; exit 2");
        let (id, target) = (
            String::from("r"),
            crate::driver::targetdef::TargetDef::default(),
        );
        let mut out = Vec::<u8>::new();
        let mut req = crate::driver::RunRequest::new(&id, &target, dir.path());
        req.stdout = Some(&mut out);

        let err = ProcessFailed::from_run_output(&mut req, &log_path, &output).await;
        drop(req);
        assert!(out.is_empty(), "the writers are not the log");
        assert_eq!(std::fs::read_to_string(&log_path).expect("log"), "kept\n");
        assert!(matches!(
            failure_log(&err).map(|a| &a.content),
            Some(OaContent::File(_))
        ));
    }

    /// A writer that fails does not lose the output: it rides inline instead.
    #[tokio::test]
    async fn from_run_output_falls_back_inline_when_a_writer_fails() {
        struct Broken;
        impl tokio::io::AsyncWrite for Broken {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Ready(Err(std::io::ErrorKind::StorageFull.into()))
            }
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let output = sh_output("echo a; echo b >&2; exit 1");
        let (id, target) = (
            String::from("r"),
            crate::driver::targetdef::TargetDef::default(),
        );
        let (mut out, mut errw) = (Vec::<u8>::new(), Broken);
        let mut req = crate::driver::RunRequest::new(&id, &target, dir.path());
        req.stdout = Some(&mut out);
        req.stderr = Some(&mut errw);
        req.log_capture = true;

        let err =
            ProcessFailed::from_run_output(&mut req, &dir.path().join("log.txt"), &output).await;
        let log = failure_log(&err).expect("an inline log");
        assert!(matches!(&log.content, OaContent::Raw(r) if r.data == b"a\nb\n"));
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
