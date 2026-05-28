pub mod app;
mod backend;
pub mod log_sink;
pub mod panic;
pub mod paused;
pub mod stdout_buffer;
pub mod tty;

use std::io::{self, IsTerminal};

pub use app::{App, AppContext, PauseGuard};
pub use log_sink::LogSink;
pub use paused::paused;
pub use stdout_buffer::BufferedStdout;

pub fn should_use_tui(force_off: bool) -> bool {
    !force_off && io::stderr().is_terminal()
}

pub async fn run_app<A: App + 'static>(
    app: A,
    sink: LogSink,
    interactive: bool,
    shutdown: crate::commands::bootstrap::ShutdownTrigger,
) -> anyhow::Result<A::Output> {
    // Establish a stable identity for the runtime's outer `block_on`
    // future so the memoizer's cross-task wait-for graph doesn't collide
    // unrelated callers on the sentinel id 0. No-op when the cycle
    // detector is disabled (the common case).
    crate::hmemoizer::with_cycle_ctx(async move {
        if interactive {
            backend::interactive::run(app, sink, shutdown).await
        } else {
            backend::ci::run(app, sink, shutdown).await
        }
    })
    .await
}
