//! Guest side of plugin-log forwarding: install a `tracing` subscriber that
//! funnels every event to the host's [`DynLogSink`] over the ABI.
//!
//! A loaded cdylib statically links its OWN `tracing`, whose global subscriber is
//! never set — so a plugin author's `tracing::info!(...)` would be dropped on the
//! floor. The host hands the plugin a sink (via the `heph_plugin_set_log_sink`
//! symbol); [`install_log_sink`] installs a subscriber forwarding all events to it,
//! and the host re-emits each on its own `tracing`.
//!
//! The host owns log config. It hands the plugin its filter (via
//! `heph_plugin_set_log_filter`, [`install_log_filter`]) only so events it would
//! discard are not formatted and forwarded first; it still filters what arrives.
//! Without one, every event is forwarded.

use hplugin_stabby::abi::{DynLogSink, StableLogSinkDyn};
use stabby::string::String as SString;
use std::sync::{PoisonError, RwLock};
use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::subscriber::Interest;
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_log::{AsLog, NormalizeEvent};
use tracing_subscriber::filter::Targets;
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

/// The host's log filter (`pb::LogFilter`) as this plugin applies it. `None` until
/// the host sends one: then every event is forwarded.
type HostFilter = RwLock<Option<Targets>>;

/// The filter [`install_log_filter`] sets and the installed [`ForwardLayer`] reads.
static HOST_FILTER: HostFilter = RwLock::new(None);

/// A `tracing` layer that forwards each event to the host sink. Holds the
/// ABI-stable [`DynLogSink`] (`Send + Sync`), so it satisfies the global
/// subscriber's bounds.
struct ForwardLayer {
    sink: DynLogSink,
    filter: &'static HostFilter,
}

impl ForwardLayer {
    fn would_forward(&self, meta: &Metadata<'_>) -> bool {
        self.filter
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_none_or(|t| t.would_enable(meta.target(), meta.level()))
    }
}

impl<S: Subscriber> Layer<S> for ForwardLayer {
    // Interest is cached per callsite; `install_log_filter` rebuilds it, so a
    // filter arriving after a callsite was first hit still applies to it.
    fn register_callsite(&self, meta: &'static Metadata<'static>) -> Interest {
        // tracing-log's per-level callsites carry target `log` for every bridged
        // record; its `LogTracer` asks `enabled` with the record's real target.
        // tracing-log 0.2 ignores the interest it is given, so this only guards
        // against a version that caches it: never decide by the placeholder.
        if meta.target() == "log" {
            return Interest::sometimes();
        }
        if self.would_forward(meta) {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn enabled(&self, meta: &Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        self.would_forward(meta)
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        self.filter
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(max_level)
    }

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
    let subscriber = tracing_subscriber::registry().with(ForwardLayer {
        sink,
        filter: &HOST_FILTER,
    });
    // `try_init` returns `Err` only if a global subscriber is already set; ignore.
    drop(subscriber.try_init());
}

/// Apply the host's log filter: prost-encoded `pb::LogFilter` bytes, as the host
/// hands them to `heph_plugin_set_log_filter`. Events it rejects are dropped here
/// instead of being formatted and sent to a host that would discard them. May be
/// called again (the latest wins), before or after [`install_log_sink`]. Bytes that
/// do not decode leave the filter as it was — over-forwarding is the safe failure,
/// since the host filters again.
pub fn install_log_filter(bytes: &[u8]) {
    use prost::Message;
    let filter = match plugin_abi::pb::LogFilter::decode(bytes) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "ignoring undecodable host log filter");
            return;
        }
    };
    set_filter(&HOST_FILTER, targets(&filter));
}

fn set_filter(slot: &HostFilter, targets: Targets) {
    let max = max_level(&targets);
    *slot.write().unwrap_or_else(PoisonError::into_inner) = Some(targets);
    // A dep logging through `log` checks `log::max_level()` before reaching
    // tracing-log at all; lower it so a disabled `log::trace!` costs one compare.
    tracing_log::log::set_max_level(max.as_log());
    // Re-ask every registered callsite, so ones cached before the filter arrived
    // pick it up (and the global max level drops with it).
    tracing_core::callsite::rebuild_interest_cache();
}

/// `pb::LogFilter` as a `Targets`, whose matching is the proto's: the longest
/// target prefix wins, else the default. An unspecified or unknown level reads as
/// TRACE, and a prefix sent twice keeps its more verbose level — forward rather
/// than drop.
fn targets(filter: &plugin_abi::pb::LogFilter) -> Targets {
    let mut merged: std::collections::HashMap<&str, LevelFilter> = Default::default();
    for d in &filter.directives {
        let level = level_filter(d.level);
        merged
            .entry(d.target_prefix.as_str())
            .and_modify(|l| *l = (*l).max(level))
            .or_insert(level);
    }
    Targets::new()
        .with_default(level_filter(filter.default_level))
        .with_targets(merged.into_iter().map(|(t, l)| (t.to_string(), l)))
}

fn level_filter(level: i32) -> LevelFilter {
    use plugin_abi::pb::log_filter::Level as L;
    match L::try_from(level) {
        Ok(L::Off) => LevelFilter::OFF,
        Ok(L::Error) => LevelFilter::ERROR,
        Ok(L::Warn) => LevelFilter::WARN,
        Ok(L::Info) => LevelFilter::INFO,
        Ok(L::Debug) => LevelFilter::DEBUG,
        Ok(L::Trace | L::Unspecified) | Err(_) => LevelFilter::TRACE,
    }
}

/// The most verbose level `targets` admits anywhere.
fn max_level(targets: &Targets) -> LevelFilter {
    targets
        .iter()
        .map(|(_, level)| level)
        .chain(targets.default_level())
        .max()
        .unwrap_or(LevelFilter::OFF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hplugin_stabby::abi::StableLogSink;
    use plugin_abi::pb;
    use plugin_abi::pb::log_filter::Level as L;
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
        forwarded_through(new_slot(), f)
    }

    /// A filter slot of the test's own, so tests don't share [`HOST_FILTER`].
    fn new_slot() -> &'static HostFilter {
        Box::leak(Box::new(RwLock::new(None)))
    }

    fn forwarded_through(
        filter: &'static HostFilter,
        f: impl FnOnce(),
    ) -> Vec<(u8, String, String)> {
        let seen = Seen::default();
        let sink: DynLogSink =
            hplugin_stabby::vtable::dynify(stabby::boxed::Box::new(RecordingSink(seen.clone())));
        let sub = tracing_subscriber::registry().with(ForwardLayer { sink, filter });
        tracing::subscriber::with_default(sub, f);
        seen.lock().expect("lock").clone()
    }

    /// A filter as a host in any language would send it.
    fn pb_filter(default: L, directives: &[(&str, L)]) -> pb::LogFilter {
        pb::LogFilter {
            default_level: default.into(),
            directives: directives
                .iter()
                .map(|(prefix, level)| pb::log_filter::Directive {
                    target_prefix: (*prefix).to_string(),
                    level: (*level).into(),
                })
                .collect(),
        }
    }

    fn slot_with(filter: &pb::LogFilter) -> &'static HostFilter {
        let slot = new_slot();
        set_filter(slot, targets(filter));
        slot
    }

    fn targets_of(seen: &[(u8, String, String)]) -> Vec<&str> {
        seen.iter().map(|(_, t, _)| t.as_str()).collect()
    }

    #[test]
    fn host_filter_drops_what_the_host_would() {
        let slot = slot_with(&pb_filter(L::Info, &[("oci_client", L::Debug)]));
        let seen = forwarded_through(slot, || {
            tracing::debug!(target: "oci_client::client", "kept: oci_client=debug");
            tracing::debug!(target: "reqwest::connect", "dropped: default info");
            tracing::info!(target: "reqwest::connect", "kept: default info");
            tracing::trace!(target: "oci_client::client", "dropped: above debug");
        });
        assert_eq!(
            targets_of(&seen),
            vec!["oci_client::client", "reqwest::connect"]
        );
    }

    /// The proto's rule: the longest matching prefix wins, whatever the order sent.
    #[test]
    fn longest_target_prefix_wins() {
        let slot = slot_with(&pb_filter(
            L::Off,
            &[("oci_client", L::Debug), ("oci", L::Warn)],
        ));
        let seen = forwarded_through(slot, || {
            tracing::debug!(target: "oci_client::client", "kept: oci_client=debug");
            tracing::info!(target: "oci_spec::image", "dropped: oci=warn");
            tracing::warn!(target: "oci_spec::image", "kept: oci=warn");
            tracing::error!(target: "reqwest", "dropped: default off");
        });
        assert_eq!(
            targets_of(&seen),
            vec!["oci_client::client", "oci_spec::image"]
        );
    }

    /// A prefix sent twice keeps the more verbose level, whatever the order.
    #[test]
    fn duplicate_prefix_keeps_the_more_verbose_level() {
        let slot = slot_with(&pb_filter(L::Off, &[("x", L::Trace), ("x", L::Off)]));
        let seen = forwarded_through(slot, || tracing::trace!(target: "x::y", "kept"));
        assert_eq!(targets_of(&seen), vec!["x::y"]);
    }

    /// A level a newer host sends that this plugin doesn't know, or none at all,
    /// must forward rather than drop.
    #[test]
    fn unknown_or_unspecified_level_forwards() {
        let mut filter = pb_filter(L::Unspecified, &[]);
        filter.directives.push(pb::log_filter::Directive {
            target_prefix: "future".to_string(),
            level: 99,
        });
        let seen = forwarded_through(slot_with(&filter), || {
            tracing::trace!(target: "anything", "kept");
            tracing::trace!(target: "future::x", "kept");
        });
        assert_eq!(targets_of(&seen), vec!["anything", "future::x"]);
    }

    /// The filter arrives after the callsite's interest was cached as "always":
    /// the rebuild must make that callsite honour it.
    #[test]
    fn filter_arriving_late_applies_to_cached_callsites() {
        // One dispatcher throughout: installing a new one would rebuild interest
        // by itself and hide a missing rebuild in `set_filter`.
        let slot = new_slot();
        let emit = || tracing::debug!(target: "late_filter_test", "hello");
        let seen = forwarded_through(slot, || {
            emit();
            set_filter(slot, targets(&pb_filter(L::Info, &[])));
            emit();
        });
        assert_eq!(seen.len(), 1);
    }

    /// A `log` record goes through `LogTracer`, which asks `enabled` with the
    /// record's real target: the host filter must apply to it by that target.
    #[test]
    fn host_filter_applies_to_bridged_log_records() {
        use tracing_log::log::Log;
        let slot = slot_with(&pb_filter(L::Info, &[("reqwest", L::Debug)]));
        let record = |target: &'static str| {
            let tracer = tracing_log::LogTracer::new();
            tracer.log(
                &log::Record::builder()
                    .level(log::Level::Debug)
                    .target(target)
                    .args(format_args!("connecting"))
                    .build(),
            );
        };
        let seen = forwarded_through(slot, || {
            record("reqwest::connect");
            record("hyper::client");
        });
        assert_eq!(targets_of(&seen), vec!["reqwest::connect"]);
    }

    /// The exported entry: valid bytes set the filter; garbage leaves it alone.
    #[test]
    fn install_log_filter_decodes_and_ignores_garbage() {
        use prost::Message;
        install_log_filter(&[0xff, 0xff, 0xff]);
        assert!(HOST_FILTER.read().expect("lock").is_none());
        install_log_filter(&pb_filter(L::Warn, &[]).encode_to_vec());
        let set = HOST_FILTER.read().expect("lock").clone();
        assert_eq!(set.and_then(|t| t.default_level()), Some(LevelFilter::WARN));
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
    /// the host must see the record's own target, or `HEPH_LOG=reqwest=debug`
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
