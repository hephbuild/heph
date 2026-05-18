use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let fmt_layer = fmt::layer().with_target(false).without_time();

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();

    // Bridge `log` crate records into the tracing subscriber so dependencies
    // that emit via `log` are captured. Error means it was already initialized.
    drop(tracing_log::LogTracer::init());
}
