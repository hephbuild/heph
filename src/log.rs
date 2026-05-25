use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::tui::log_sink::{LogSink, MakeLogSink};

pub fn init() -> LogSink {
    let sink = LogSink::new_direct();

    // fuser is chatty at info!/warn! during mount lifecycle. Cap it at
    // error! by default so genuine failures surface but lifecycle noise
    // is silenced. User raises via `RUST_LOG=fuser=debug` for details.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,fuser=error"));

    let fmt_layer = fmt::layer()
        .with_target(false)
        .without_time()
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
