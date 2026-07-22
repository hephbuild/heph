use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::tui::log_sink::{LogSink, MakeLogSink};

pub fn init() -> LogSink {
    let sink = LogSink::new_direct();

    // fuser is chatty at info!/warn! during mount lifecycle. Cap it at
    // error! by default so genuine failures surface but lifecycle noise
    // is silenced. object_store emits an info! ("fetching token from
    // metadata server") on every GCS token fetch — cap it at warn! so the
    // noise is silenced but real credential failures surface. Users raise
    // either via `RUST_LOG=fuser=debug` / `RUST_LOG=object_store=info`.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,fuser=error,object_store=warn"));

    // tracing_subscriber defaults ANSI on regardless of where the writer points.
    // Our writer is stderr, so gate color on stderr's capability — otherwise a
    // redirected/piped stderr gets raw escape codes like `^[[32m INFO^[[0m`.
    let fmt_layer = fmt::layer()
        .with_target(false)
        .without_time()
        .with_ansi(sink.color_enabled())
        .with_writer(MakeLogSink::new(sink.clone()));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();

    // Bridge `log` crate records into the tracing subscriber so dependencies
    // that emit via `log` are captured. Error means it was already initialized.
    drop(tracing_log::LogTracer::init());

    sink
}
