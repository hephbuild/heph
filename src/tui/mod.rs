pub mod app;
mod backend;
pub mod log_sink;
pub mod panic;
pub mod stdout_buffer;
pub mod tty;

use std::io::{self, IsTerminal};

pub use app::{App, AppContext, PauseGuard};
pub use log_sink::LogSink;
pub use stdout_buffer::BufferedStdout;

pub fn should_use_tui(force_off: bool) -> bool {
    !force_off && io::stderr().is_terminal()
}

pub async fn run_app<A: App>(
    app: A,
    sink: LogSink,
    interactive: bool,
) -> anyhow::Result<A::Output> {
    if interactive {
        backend::interactive::run(app, sink).await
    } else {
        backend::ci::run(app, sink).await
    }
}
