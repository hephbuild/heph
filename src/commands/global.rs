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
    /// Run against a fresh, empty scratch cache instead of the stored one.
    ///
    /// **Deletes nothing.** The stored cache is not touched, read, or emptied —
    /// the run is simply pointed at a throwaway directory instead, and that
    /// directory is discarded afterwards. A later ordinary build finds its cache
    /// exactly as it left it.
    ///
    /// This is how the scratch contract gets audited: a target's outputs must be
    /// identical whether its scratch is warm or cold, so a build with
    /// `--no-scratch` should produce the same `hashout`s as one without. If it
    /// does not, the target depends on carried-over state and is already broken.
    /// It implies `--force`: a scratch never reaches `hashin`, so without a
    /// rebuild the run would just replay the result built *with* a warm cache.
    ///
    /// Everything else is set up as normal — the directory is created and
    /// mounted, the environment variable is announced, the slot is locked — so a
    /// target reading `$MYCACHE` runs cold rather than failing on an unset
    /// variable. The audit is of your build, not of your shell.
    ///
    /// A build flag rather than a `heph tool scratch` subcommand — it modifies a
    /// build rather than being one, so it belongs next to `--force`. And a bool
    /// rather than `--scratch=on|off`: there are exactly two states, `off` is the
    /// only one anyone types, and a valued flag named `scratch` collides with
    /// every subcommand that wants a `--scratch` of its own (see
    /// `tool::clean`'s test).
    #[arg(long = "no-scratch", global = true)]
    pub no_scratch: bool,
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
    /// Also dump every thread's backtrace on `SIGQUIT`. Can deadlock the process
    ///
    /// `SIGQUIT` always writes the in-flight report (what work is open and what
    /// it is waiting on), which is the half that names the stuck work and is
    /// written by an ordinary thread. This flag adds per-thread backtraces,
    /// which are captured *inside a signal handler* — that calls the DWARF
    /// unwinder and the allocator, neither of which is async-signal-safe. A
    /// thread interrupted inside `malloc` re-enters it from its own handler and
    /// deadlocks holding the arena lock, taking the whole process with it.
    ///
    /// Off by default because that outcome is the opposite of what a hang
    /// diagnostic is for. Turn it on only when the in-flight report was not
    /// enough, on a run you are willing to lose.
    #[arg(long = "diag-backtrace", global = true)]
    pub diag_backtrace: bool,
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
