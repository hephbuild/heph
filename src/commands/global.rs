use std::path::PathBuf;

use clap::Args;

/// Parse `--stall-notice`: a duration, or `off` to disable.
///
/// Zero is the "off" sentinel rather than `Option`, because clap reads a field of
/// type `Option<T>` as "this flag is optional" and expects the parser to yield
/// `T` — returning an `Option` from the parser panics at access time.
fn parse_stall_notice(s: &str) -> Result<std::time::Duration, String> {
    if matches!(s.trim(), "off" | "none") {
        return Ok(std::time::Duration::ZERO);
    }
    humantime::parse_duration(s).map_err(|e| format!("invalid duration {s:?}: {e}"))
}

/// Global options shared by every subcommand. Flattened into the top-level CLI
/// with `global = true` so the flags are accepted before or after the
/// subcommand, then plumbed to each command's `execute`.
#[derive(Args, Clone, Debug, Default)]
pub struct GlobalOptions {
    /// Sample CPU and write a pprof profile to PATH. `kill -USR2 <pid>`
    /// snapshots the profile so far without stopping the run — this is the point
    /// of the flag, since a hung build never reaches exit. A filtered final
    /// report is also written at exit.
    #[arg(long = "pprof-cpu", value_name = "PATH", global = true)]
    pub pprof_cpu: Option<PathBuf>,
    /// Write a diagnostic when a run makes no progress for this long
    ///
    /// heph watches its own event stream and, if nothing at all advances for the
    /// given duration while work is outstanding, appends one paragraph naming
    /// what is open, for how long, and whether any bytes are moving to
    /// `<home>/diag/stall-<pid>.log`, and logs the path at `warn`. Off with
    /// `--stall-notice=off`. Default: 60s.
    ///
    /// The text is a diagnostic, not a stable interface — parse the JSON surface
    /// instead.
    #[arg(
        long = "stall-notice",
        value_name = "DURATION",
        default_value = "60s",
        value_parser = parse_stall_notice,
        global = true
    )]
    pub stall_notice: std::time::Duration,
    /// Disable the interactive TUI (force CI/log-only output)
    #[arg(long = "no-tui", global = true)]
    pub no_tui: bool,
    /// Fail fast: stop at the first target failure instead of running every
    /// matched target and reporting all failures at the end
    #[arg(long = "fail-fast", visible_alias = "ff", global = true)]
    pub fail_fast: bool,
    /// Approve every `approval`-gated target without prompting. The notice (if
    /// any) is still printed in non-TUI mode.
    #[arg(long = "auto-approve", global = true)]
    pub auto_approve: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        global: GlobalOptions,
    }

    fn parse(args: &[&str]) -> GlobalOptions {
        TestCli::parse_from(args).global
    }

    #[test]
    fn fail_fast_is_opt_in() {
        // Default: fail-fast off — run every matched target, report all failures.
        assert!(!parse(&["heph"]).fail_fast);
        // Opt in with the long flag or its `ff` alias.
        assert!(parse(&["heph", "--fail-fast"]).fail_fast);
        assert!(parse(&["heph", "--ff"]).fail_fast);
    }

    #[test]
    fn no_tui_defaults_off() {
        assert!(!parse(&["heph"]).no_tui);
        assert!(parse(&["heph", "--no-tui"]).no_tui);
    }

    #[test]
    fn auto_approve_is_opt_in() {
        assert!(!parse(&["heph"]).auto_approve);
        assert!(parse(&["heph", "--auto-approve"]).auto_approve);
    }
}
