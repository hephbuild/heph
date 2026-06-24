//! Guest side of plugin-log forwarding: install a `tracing` subscriber that
//! funnels every event to the host's [`DynLogSink`] over the ABI.
//!
//! A loaded cdylib statically links its OWN `tracing`, whose global subscriber is
//! never set — so a plugin author's `tracing::info!(...)` would be dropped on the
//! floor. The host hands the plugin a sink (via the `heph_plugin_set_log_sink`
//! symbol); [`install_log_sink`] installs a subscriber forwarding all events to it,
//! and the host re-emits each on its own `tracing`. The guest forwards every level
//! and lets the host's subscriber do the filtering — the host owns log config.

use hplugin_stabby::abi::{DynLogSink, StableLogSinkDyn};
use stabby::string::String as SString;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// Collects an event's `message` field into a string (the rendered log line).
struct MsgVisitor(String);

impl Visit for MsgVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            // `push_str(&format!(..))` rather than `write!` — the latter returns an
            // infallible-here `Result` that trips both the drop-copy and
            // let-underscore lints.
            self.0.push_str(&format!("{value:?}"));
        }
    }
}

/// A `tracing` layer that forwards each event to the host sink. Holds the
/// ABI-stable [`DynLogSink`] (`Send + Sync`), so it satisfies the global
/// subscriber's bounds.
struct ForwardLayer {
    sink: DynLogSink,
}

impl<S: Subscriber> Layer<S> for ForwardLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        // `tracing::Level` as `1=ERROR .. 5=TRACE` — the wire encoding the host
        // re-expands back into the matching macro.
        let level = match *meta.level() {
            Level::ERROR => 1u8,
            Level::WARN => 2,
            Level::INFO => 3,
            Level::DEBUG => 4,
            Level::TRACE => 5,
        };
        let mut v = MsgVisitor(String::new());
        event.record(&mut v);
        self.sink.log(
            level,
            SString::from(meta.target()),
            SString::from(v.0.as_str()),
        );
    }
}

/// Install the host log sink as this plugin's global `tracing` subscriber. Idempotent
/// and best-effort: only the first call wins (the global subscriber is set once per
/// process), and a failure to set it (already set) is ignored — log forwarding is
/// never load-fatal.
pub fn install_log_sink(sink: DynLogSink) {
    use std::sync::OnceLock;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    static INSTALLED: OnceLock<()> = OnceLock::new();
    if INSTALLED.set(()).is_err() {
        return;
    }
    let subscriber = tracing_subscriber::registry().with(ForwardLayer { sink });
    // `try_init` returns `Err` only if a global subscriber is already set; ignore.
    drop(subscriber.try_init());
}
