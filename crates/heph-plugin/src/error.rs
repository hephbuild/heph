use heph_model::htaddr::Addr;
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
/// bounded tail of the process log (the full log is still saved as the `log.txt`
/// artifact). Surfaced as the `source` of a `TargetFailure`.
#[derive(Debug, Clone)]
pub struct ProcessFailed {
    pub status: String,
    pub log_tail: String,
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
    /// as the `log.txt` artifact). Rendered as a framed `log` box.
    pub log_tail: Option<String>,
    pub source: std::sync::Arc<anyhow::Error>,
}

impl TargetFailure {
    pub fn new(addr: Addr, log_tail: Option<String>, source: anyhow::Error) -> Self {
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

/// Returns the last `n` lines of `s`, joined with `\n`. A trailing newline does
/// not count as an extra empty line (`str::lines` already drops it).
pub fn last_n_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines.get(start..).unwrap_or(&[]).join("\n")
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
}
