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
use tracing_log::NormalizeEvent;
use tracing_subscriber::layer::{Context, Layer};

/// Renders an event into one line: its `message`, then ` name=value` for every
/// other field — the host event carries only a message, so a field left out here
/// (`debug!(%url, "pulling")`) is lost. The `log.*` fields `tracing-log` adds to a
/// bridged `log` record are its metadata, already carried by the target.
#[derive(Default)]
struct MsgVisitor {
    /// The event is a bridged `log` record, whose `log.*` fields are skipped.
    bridged: bool,
    message: String,
    fields: String,
}

impl MsgVisitor {
    fn skip(&self, name: &str) -> bool {
        self.bridged && name.starts_with("log.")
    }

    fn line(mut self) -> String {
        if self.message.is_empty() {
            self.fields.trim_start().to_string()
        } else {
            self.message.push_str(&self.fields);
            self.message
        }
    }
}

impl Visit for MsgVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `push_str(&format!(..))` rather than `write!` — the latter returns an
        // infallible-here `Result` that trips both the drop-copy and
        // let-underscore lints.
        match field.name() {
            "message" => self.message.push_str(&format!("{value:?}")),
            name if self.skip(name) => {}
            name => self.fields.push_str(&format!(" {name}={value:?}")),
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message.push_str(value),
            name if self.skip(name) => {}
            name => self.fields.push_str(&format!(" {name}={value:?}")),
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
        // A `log` record bridged by `tracing-log` (reqwest logs through `log`)
        // has target `log`; its real target is in the `log.target` field.
        let normalized = event.normalized_metadata();
        let meta = normalized.as_ref().unwrap_or_else(|| event.metadata());
        // `tracing::Level` as `1=ERROR .. 5=TRACE` — the wire encoding the host
        // decodes back into a `Level` for the event it re-emits.
        let level = match *meta.level() {
            Level::ERROR => 1u8,
            Level::WARN => 2,
            Level::INFO => 3,
            Level::DEBUG => 4,
            Level::TRACE => 5,
        };
        let mut v = MsgVisitor {
            bridged: normalized.is_some(),
            ..MsgVisitor::default()
        };
        event.record(&mut v);
        self.sink.log(
            level,
            SString::from(meta.target()),
            SString::from(v.line().as_str()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use hplugin_stabby::abi::StableLogSink;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    type Seen = Arc<Mutex<Vec<(u8, String, String)>>>;

    /// Stands in for the host: records what crossed the seam.
    struct RecordingSink(Seen);

    impl StableLogSink for RecordingSink {
        extern "C" fn log(&self, level: u8, target: SString, message: SString) {
            self.0
                .lock()
                .expect("lock")
                .push((level, target.to_string(), message.to_string()));
        }
    }

    fn forwarded(f: impl FnOnce()) -> Vec<(u8, String, String)> {
        let seen = Seen::default();
        let sink: DynLogSink =
            hplugin_stabby::vtable::dynify(stabby::boxed::Box::new(RecordingSink(seen.clone())));
        let sub = tracing_subscriber::registry().with(ForwardLayer { sink });
        tracing::subscriber::with_default(sub, f);
        seen.lock().expect("lock").clone()
    }

    #[test]
    fn forwards_target_level_and_fields() {
        let seen = forwarded(|| {
            tracing::debug!(target: "oci_client::client", url = %"ghcr.io/x", n = 3, "pulling");
            tracing::warn!(target: "plug", only = "fields");
        });
        assert_eq!(
            seen,
            vec![
                (
                    4,
                    "oci_client::client".to_string(),
                    "pulling url=ghcr.io/x n=3".to_string()
                ),
                (2, "plug".to_string(), "only=\"fields\"".to_string()),
            ]
        );
    }

    /// A dep logging through the `log` crate reaches tracing with target `log`;
    /// the host must see the record's own target, or `RUST_LOG=reqwest=debug`
    /// cannot select it.
    #[test]
    fn bridged_log_record_keeps_its_target() {
        let seen = forwarded(|| {
            let record = log::Record::builder()
                .level(log::Level::Debug)
                .target("reqwest::connect")
                .args(format_args!("starting new connection"))
                .build();
            tracing_log::format_trace(&record).expect("bridge");
        });
        assert_eq!(
            seen,
            vec![(
                4,
                "reqwest::connect".to_string(),
                "starting new connection".to_string()
            )]
        );
    }
}
