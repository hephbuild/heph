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
    let interactive = tui_allowed(
        force_off,
        io::stderr().is_terminal(),
        stdout_is_pipe(),
        foreground,
        own,
    );
    // Record the decision here, at the one place that makes it.
    htelemetry::telemetry::record_interactive(interactive);
    interactive
}

/// Whether fd 1 feeds another process: a pipe (`| grep`, `>(…)`) or a socket.
/// A failing `fstat` (fd 1 closed) is not a reader.
fn stdout_is_pipe() -> bool {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `fstat` only writes into `st`, which is read only on success.
    if unsafe { libc::fstat(libc::STDOUT_FILENO, st.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: `fstat` returned 0, so it initialised `st`.
    let kind = unsafe { st.assume_init() }.st_mode & libc::S_IFMT;
    kind == libc::S_IFIFO || kind == libc::S_IFSOCK
}

/// The decision behind [`should_use_tui`]. `foreground` is `tcgetpgrp(2)`, and
/// `own` this process's group.
///
/// It looks at fd 2, the fd the TUI draws on, and never at fd 0: `echo x | heph
/// run //t` is still in the foreground and keeps the TUI. A background job is
/// what disqualifies, because the TUI would go raw and stop on the terminal
/// before the run ever got to hand it over. A failing `tcgetpgrp` (a terminal
/// that is not this session's) says nothing about being in the background.
///
/// Stdout into a pipe disqualifies too. The reader (`heph q //p | grep x`)
/// usually prints to the same terminal the TUI draws on, and nothing tells heph
/// when it does: its lines land in the middle of the inline viewport while the
/// terminal is raw, so newlines lose their carriage return and stair-step, and
/// the TUI's redraws are interleaved into the output. Stdout into a file keeps
/// the TUI, since nothing else writes to the terminal then.
fn tui_allowed(
    force_off: bool,
    stderr_is_terminal: bool,
    stdout_is_pipe: bool,
    foreground: libc::pid_t,
    own: libc::pid_t,
) -> bool {
    !force_off && stderr_is_terminal && !stdout_is_pipe && (foreground < 0 || foreground == own)
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
        // (case, force_off, stderr is a terminal, stdout is a pipe, tcgetpgrp(2), expected)
        let cases = [
            ("foreground", false, true, false, OWN, true),
            ("backgrounded job", false, true, false, OTHER, false),
            ("not this session's terminal", false, true, false, -1, true),
            ("stderr redirected", false, false, false, -1, false),
            (
                "stderr redirected, stale group",
                false,
                false,
                false,
                OTHER,
                false,
            ),
            ("forced off", true, true, false, OWN, false),
            (
                "forced off in the background",
                true,
                true,
                false,
                OTHER,
                false,
            ),
            ("stdout piped to a reader", false, true, true, OWN, false),
            (
                "stdout piped, stderr redirected",
                false,
                false,
                true,
                OWN,
                false,
            ),
        ];
        for (case, force_off, terminal, piped, foreground, expected) in cases {
            assert_eq!(
                tui_allowed(force_off, terminal, piped, foreground, OWN),
                expected,
                "{case}"
            );
        }
    }
}
