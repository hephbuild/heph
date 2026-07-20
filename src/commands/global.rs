use std::path::PathBuf;

use clap::Args;

/// Global options shared by every subcommand. Flattened into the top-level CLI
/// with `global = true` so the flags are accepted before or after the
/// subcommand, then plumbed to each command's `execute`.
#[derive(Args, Clone, Debug, Default)]
pub struct GlobalOptions {
    /// Write CPU pprof on exit
    #[arg(long = "pprof-cpu", value_name = "PATH", global = true)]
    pub pprof_cpu: Option<PathBuf>,
    /// Install a SIGUSR1 handler that appends the signalled thread's backtrace to
    /// FILE — for diagnosing hangs where ptrace/perf/core dumps are blocked
    /// (locked-down CI containers). Off unless passed; bare `--diag-backtrace`
    /// writes to /tmp/heph-backtrace.log. See the `diag` module for the
    /// sweep-all-threads recipe.
    #[arg(
        long = "diag-backtrace",
        value_name = "FILE",
        num_args = 0..=1,
        default_missing_value = "/tmp/heph-backtrace.log",
        global = true,
        hide = true
    )]
    pub diag_backtrace: Option<PathBuf>,
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

    #[test]
    fn diag_backtrace_is_opt_in() {
        use std::path::Path;
        // Absent → off.
        assert!(parse(&["heph"]).diag_backtrace.is_none());
        // Bare flag → default file.
        assert_eq!(
            parse(&["heph", "--diag-backtrace"])
                .diag_backtrace
                .as_deref(),
            Some(Path::new("/tmp/heph-backtrace.log"))
        );
        // Explicit path honored.
        assert_eq!(
            parse(&["heph", "--diag-backtrace", "/tmp/x.log"])
                .diag_backtrace
                .as_deref(),
            Some(Path::new("/tmp/x.log"))
        );
    }
}
