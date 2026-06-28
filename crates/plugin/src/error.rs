use hcore::hartifactcontent::Content;
use hmodel::htaddr::Addr;
use std::fmt;
use std::sync::Arc;

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

/// A target's own process exited non-zero. Carries the exit status string and a
/// lazy [`Content`] handle to the full process log, rather than the log bytes:
/// the diagnostic reads only the last N lines on demand (see
/// [`last_n_lines_with_start`]), where N is the request's `--log-lines`. The log
/// file persists in the sandbox until the target's next run, so the handle stays
/// readable through end-of-run rendering. Surfaced as the `source` of a
/// [`TargetFailure`].
#[derive(Debug, Clone)]
pub struct ProcessFailed {
    pub status: String,
    pub log: Arc<dyn Content>,
}

impl fmt::Display for ProcessFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "process exited with status: {}", self.status)
    }
}

impl std::error::Error for ProcessFailed {}

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
    pub source: std::sync::Arc<anyhow::Error>,
}

impl TargetFailure {
    pub fn new(addr: Addr, log_tail: Option<LogTail>, source: anyhow::Error) -> Self {
        Self {
            addr,
            log_tail,
            source: std::sync::Arc::new(source),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
