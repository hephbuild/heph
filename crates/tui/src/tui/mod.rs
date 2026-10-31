pub mod app;
pub mod approval;
mod backend;
pub mod color;
pub mod log_sink;
pub mod panic;
pub mod paused;
pub mod progress;
pub mod pty_session;
pub mod stderr_backend;
pub mod stdout_buffer;
pub mod tty;

use std::io::{self, IsTerminal};

pub use app::{App, AppContext, CIAppView, PauseFor, PauseGuard, TUIAppView};
pub use approval::{ApprovalCenter, ApprovalNoticeView, ApprovalView};
pub use log_sink::LogSink;
pub use paused::paused;
pub use progress::{
    BuildHeader, BuildState, CiProgressView, CountScope, GcCiView, GcHeader, HeaderItem,
    MIN_PROGRESS_ROWS, ProgressHeader, TuiProgressView, ViewMode, human_bytes, rows_for_height,
};
pub use pty_session::{PtyEnds, PtySession};
pub use stdout_buffer::BufferedStdout;

pub fn should_use_tui(force_off: bool) -> bool {
    // SAFETY: no preconditions; a fd with no terminal is an error return.
    let foreground = unsafe { libc::tcgetpgrp(libc::STDERR_FILENO) };
    // SAFETY: no preconditions.
    let own = unsafe { libc::getpgrp() };
    let interactive = tui_allowed(force_off, io::stderr().is_terminal(), foreground, own);
    // Record the decision here, at the one place that makes it.
    htelemetry::telemetry::record_interactive(interactive);
    interactive
}

/// The decision behind [`should_use_tui`]. `foreground` is `tcgetpgrp(2)`, and
/// `own` this process's group.
///
/// It looks at fd 2, the fd the TUI draws on, and never at fd 0: `echo x | heph
/// run //t` is still in the foreground and keeps the TUI. A background job is
/// what disqualifies, because the TUI would go raw and stop on the terminal
/// before the run ever got to hand it over. A failing `tcgetpgrp` (a terminal
/// that is not this session's) says nothing about being in the background.
fn tui_allowed(
    force_off: bool,
    stderr_is_terminal: bool,
    foreground: libc::pid_t,
    own: libc::pid_t,
) -> bool {
    !force_off && stderr_is_terminal && (foreground < 0 || foreground == own)
}

pub async fn run_app<A: App + 'static>(
    app: A,
    sink: LogSink,
    interactive: bool,
    shutdown: hcore::shutdown::ShutdownTrigger,
) -> anyhow::Result<A::Output> {
    // Establish a stable identity for the runtime's outer `block_on`
    // future so the memoizer's cross-task wait-for graph doesn't collide
    // unrelated callers on the sentinel id 0. No-op when the cycle
    // detector is disabled (the common case).
    //
    // Each backend owns the build-event channel: it creates the (sender,
    // receiver) pair, hands the sender to the app via `AppContext`, and consumes
    // the receiver for rendering. This auto-plumbs the bus into every command.
    hcore::hmemoizer::with_cycle_ctx(async move {
        if interactive {
            backend::interactive::run(app, sink, shutdown).await
        } else {
            backend::ci::run(app, sink, shutdown).await
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::tui_allowed;

    #[test]
    fn should_use_tui_foreground_table() {
        const OWN: libc::pid_t = 100;
        const OTHER: libc::pid_t = 200;
        // (case, force_off, stderr is a terminal, tcgetpgrp(2), expected)
        let cases = [
            ("foreground", false, true, OWN, true),
            ("backgrounded job", false, true, OTHER, false),
            ("not this session's terminal", false, true, -1, true),
            ("stderr redirected", false, false, -1, false),
            ("stderr redirected, stale group", false, false, OTHER, false),
            ("forced off", true, true, OWN, false),
            ("forced off in the background", true, true, OTHER, false),
        ];
        for (case, force_off, terminal, foreground, expected) in cases {
            assert_eq!(
                tui_allowed(force_off, terminal, foreground, OWN),
                expected,
                "{case}"
            );
        }
    }
}
