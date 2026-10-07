//! Re-emit a plugin's forwarded `tracing` events on the host subscriber under the
//! plugin's **own** target.
//!
//! `tracing`'s macros take a `'static` target, fixed at the call site, so the host
//! cannot write `debug!(target: <string from the plugin>, ..)`. Re-emitting under
//! one fixed target (`heph::plugin`, as this used to) breaks every per-crate
//! directive: `RUST_LOG=oci_client=debug` never matches an event from the OCI
//! plugin's `oci_client`, and its debug events fall to the default `info` level.
//!
//! Instead each distinct `(target, level)` gets a callsite built at runtime and
//! registered like a macro's static one, so `EnvFilter` (and every other layer)
//! sees the plugin's module path exactly as if the crate were linked into the
//! host. Callsites are leaked: `tracing` requires `'static` metadata, and the set
//! is bounded by the targets compiled into the loaded plugins — capped regardless
//! at [`MAX_TARGETS`] per level, past which events fall back to `heph::plugin`.

use std::collections::HashMap;
use std::sync::{OnceLock, PoisonError, RwLock};
use tracing::field::{FieldSet, Value, display};
use tracing::level_filters::LevelFilter;
use tracing::subscriber::Interest;
use tracing::{Event, Level, Metadata};
// tracing-core, not tracing's hidden re-exports: building a callsite at runtime
// is tracing-core's documented API and tracing's copy of it is not stable.
use tracing_core::Kind;
use tracing_core::callsite::Callsite;

/// Distinct targets interned per level before falling back to [`FALLBACK_TARGET`].
/// Generous for real plugins (module paths of the plugin and its deps); a bound
/// on what a plugin sending made-up targets can leak.
const MAX_TARGETS: usize = 4096;

/// Target used once [`MAX_TARGETS`] is reached; the plugin's target is then
/// carried in the message instead.
const FALLBACK_TARGET: &str = "heph::plugin";

/// The fields every re-emitted event carries. `plugin` repeats the target: the
/// host's formatter hides targets, and without it a plugin's line reads like
/// heph's own.
const FIELDS: &[&str] = &["message", "plugin"];

/// Metadata [`PluginCallsite::metadata`] returns if its own is unset, which the
/// construction order rules out; it exists to keep that path panic-free.
struct FallbackCallsite;
static FALLBACK_CALLSITE: FallbackCallsite = FallbackCallsite;
static FALLBACK_META: Metadata<'static> = tracing_core::metadata! {
    name: "plugin event",
    target: FALLBACK_TARGET,
    level: Level::TRACE,
    fields: FIELDS,
    callsite: &FALLBACK_CALLSITE,
    kind: Kind::EVENT,
};

impl Callsite for FallbackCallsite {
    fn set_interest(&self, _: Interest) {}

    fn metadata(&self) -> &Metadata<'_> {
        &FALLBACK_META
    }
}

/// A runtime-built event callsite. `meta` is set once, right after the callsite
/// is leaked (its `FieldSet` must name the callsite's own `'static` address), and
/// before it is registered or published — so it is never observed unset.
struct PluginCallsite {
    meta: OnceLock<Metadata<'static>>,
}

impl Callsite for PluginCallsite {
    fn set_interest(&self, _: Interest) {
        // Interest is not cached: `emit` asks the dispatcher's `enabled` each time.
    }

    fn metadata(&self) -> &Metadata<'_> {
        self.meta.get().unwrap_or(&FALLBACK_META)
    }
}

/// Per-level map from target to its interned metadata.
type Interned = HashMap<Box<str>, &'static Metadata<'static>>;

fn level_index(level: Level) -> usize {
    match level {
        Level::ERROR => 0,
        Level::WARN => 1,
        Level::INFO => 2,
        Level::DEBUG => 3,
        Level::TRACE => 4,
    }
}

fn tables() -> &'static [RwLock<Interned>; 5] {
    static TABLES: OnceLock<[RwLock<Interned>; 5]> = OnceLock::new();
    TABLES.get_or_init(Default::default)
}

/// Build, leak and register a callsite for `(target, level)`.
fn new_callsite(target: &str, level: Level) -> &'static Metadata<'static> {
    let target: &'static str = Box::leak(target.into());
    let cs: &'static PluginCallsite = Box::leak(Box::new(PluginCallsite {
        meta: OnceLock::new(),
    }));
    let meta = cs.meta.get_or_init(|| {
        Metadata::new(
            "plugin event",
            target,
            level,
            None,
            None,
            Some(target),
            FieldSet::new(FIELDS, tracing_core::identify_callsite!(cs)),
            Kind::EVENT,
        )
    });
    tracing_core::callsite::register(cs);
    meta
}

/// The interned metadata for `(target, level)`, or `None` once the per-level cap
/// is reached.
fn intern(target: &str, level: Level) -> Option<&'static Metadata<'static>> {
    let table = tables().get(level_index(level))?;
    if let Some(meta) = table
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(target)
    {
        return Some(meta);
    }
    let mut w = table.write().unwrap_or_else(PoisonError::into_inner);
    // Re-check: another thread may have interned it between the two locks.
    if let Some(meta) = w.get(target) {
        return Some(meta);
    }
    if w.len() >= MAX_TARGETS {
        return None;
    }
    let meta = new_callsite(target, level);
    w.insert(target.into(), meta);
    Some(meta)
}

/// Re-emit one plugin event on the host's current dispatcher under `target`.
pub(crate) fn emit(level: Level, target: &str, message: &str) {
    // Cheap global reject before interning: nothing is listening at this level.
    if level > LevelFilter::current() {
        return;
    }
    let Some(meta) = intern(target, level) else {
        match level {
            Level::ERROR => tracing::error!(target: FALLBACK_TARGET, plugin = %target, "{message}"),
            Level::WARN => tracing::warn!(target: FALLBACK_TARGET, plugin = %target, "{message}"),
            Level::INFO => tracing::info!(target: FALLBACK_TARGET, plugin = %target, "{message}"),
            Level::DEBUG => tracing::debug!(target: FALLBACK_TARGET, plugin = %target, "{message}"),
            Level::TRACE => tracing::trace!(target: FALLBACK_TARGET, plugin = %target, "{message}"),
        }
        return;
    };
    tracing::dispatcher::get_default(|dispatch| {
        if !dispatch.enabled(meta) {
            return;
        }
        let fields = meta.fields();
        let (Some(msg_field), Some(plugin_field)) =
            (fields.field("message"), fields.field("plugin"))
        else {
            return;
        };
        let plugin = display(target);
        let values = [
            (&msg_field, Some(&message as &dyn Value)),
            (&plugin_field, Some(&plugin as &dyn Value)),
        ];
        dispatch.event(&Event::new(meta, &fields.value_set(&values)));
    });
}

#[cfg(test)]
mod tests {
    use crate::abi::StableLogSink;
    use crate::host::HostLogSink;
    use stabby::string::String as SString;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::{Event, Level, Subscriber};
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

    type Seen = Arc<Mutex<Vec<(String, Level, String)>>>;

    struct Capture(Seen);

    /// Renders an event as `<message> plugin=<plugin>`.
    #[derive(Default)]
    struct Msg {
        message: String,
        plugin: String,
    }
    impl Visit for Msg {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "message" {
                self.message = value.to_string();
            }
        }
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            match field.name() {
                "message" => self.message = format!("{value:?}"),
                "plugin" => self.plugin = format!("{value:?}"),
                _ => {}
            }
        }
    }

    impl<S: Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
            let mut m = Msg::default();
            event.record(&mut m);
            let meta = event.metadata();
            self.0.lock().expect("lock").push((
                meta.target().to_string(),
                *meta.level(),
                format!("{} plugin={}", m.message, m.plugin),
            ));
        }
    }

    /// What a loaded plugin calls across the seam.
    fn log(level: u8, target: &str, message: &str) {
        HostLogSink.log(level, SString::from(target), SString::from(message));
    }

    fn capture(filter: &str, f: impl FnOnce()) -> Vec<(String, Level, String)> {
        let seen = Seen::default();
        let sub = tracing_subscriber::registry()
            .with(EnvFilter::new(filter))
            .with(Capture(seen.clone()));
        tracing::subscriber::with_default(sub, f);
        seen.lock().expect("lock").clone()
    }

    /// The reported bug: `RUST_LOG=info,oci_client=debug` showed no `oci_client`
    /// line, because every plugin event was re-emitted as `heph::plugin`.
    #[test]
    fn per_crate_directive_matches_the_plugins_target() {
        let seen = capture("info,oci_client=debug", || {
            log(4, "oci_client::client", "pulling manifest");
            log(4, "reqwest::connect", "filtered out");
            log(3, "reqwest::connect", "kept at info");
            log(5, "oci_client::client", "below oci_client=debug");
        });
        assert_eq!(
            seen,
            vec![
                (
                    "oci_client::client".to_string(),
                    Level::DEBUG,
                    "pulling manifest plugin=oci_client::client".to_string()
                ),
                (
                    "reqwest::connect".to_string(),
                    Level::INFO,
                    "kept at info plugin=reqwest::connect".to_string()
                ),
            ]
        );
    }

    /// A target is interned once per level; a later subscriber with a different
    /// filter still gets to decide for the already-interned callsite.
    #[test]
    fn interned_callsite_is_reused_under_a_different_filter() {
        let on = capture("plugtest_reuse=debug", || {
            log(4, "plugtest_reuse", "a");
            log(4, "plugtest_reuse", "b");
        });
        assert_eq!(on.len(), 2);
        let off = capture("plugtest_reuse=info", || {
            log(4, "plugtest_reuse", "c");
        });
        assert_eq!(off, vec![]);
    }
}
