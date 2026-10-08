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
    // either via `HEPH_LOG=fuser=debug` / `HEPH_LOG=object_store=info`.
    //
    // `HEPH_LOG`, not `RUST_LOG`: heph's own variable, so a `RUST_LOG` set for
    // some other Rust tool in the same shell does not reconfigure heph.
    const LOG_ENV: &str = "HEPH_LOG";
    const DEFAULT_FILTER: &str = "info,fuser=error,object_store=warn";
    let (filter, directives) = match EnvFilter::try_from_env(LOG_ENV) {
        Ok(filter) => (filter, std::env::var(LOG_ENV).unwrap_or_default()),
        Err(_) => (EnvFilter::new(DEFAULT_FILTER), DEFAULT_FILTER.to_string()),
    };
    // Plugins get the same filter, so they drop what this subscriber would before
    // paying to forward it across the seam.
    hplugin_stabby::host::set_plugin_log_filter(&directives, filter.max_level_hint());

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
