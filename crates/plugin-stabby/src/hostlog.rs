//! Re-emit a plugin's forwarded `tracing` events on the host subscriber under the
//! plugin's **own** target.
//!
//! `tracing`'s macros take a `'static` target, fixed at the call site, so the host
//! cannot write `debug!(target: <string from the plugin>, ..)`. Re-emitting under
//! one fixed target (`heph::plugin`, as this used to) breaks every per-crate
//! directive: `HEPH_LOG=oci_client=debug` never matches an event from the OCI
//! plugin's `oci_client`, and its debug events fall to the default `info` level.
//!
//! Instead each distinct `(target, level)` gets a callsite built at runtime and
//! registered like a macro's static one, so `EnvFilter` (and every other layer)
//! sees the plugin's module path exactly as if the crate were linked into the
//! host. Callsites are leaked: `tracing` requires `'static` metadata, and the set
//! is bounded by the targets compiled into the loaded plugins — capped regardless
//! at [`MAX_TARGETS`] per level, past which events fall back to `heph::plugin`.
//!
//! The other direction: [`set_plugin_log_filter`] records the host's filter as a
//! portable `pb::LogFilter`, which the loader hands each plugin so it can drop what
//! the host would discard before formatting and forwarding it.

use plugin_abi::pb;
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

/// The host's log filter as encoded `pb::LogFilter`, handed to each plugin at load
/// (see [`crate::abi::SET_LOG_FILTER_SYMBOL`]). `None` until the host sets one, and
/// then no plugin is sent a filter — it forwards everything.
static LOG_FILTER: RwLock<Option<Vec<u8>>> = RwLock::new(None);

/// Record the host's log filter for every plugin loaded from now on. `directives`
/// is the `EnvFilter` directive string in effect, `max` that filter's
/// `max_level_hint`. Plugins already loaded are not re-sent it.
pub fn set_plugin_log_filter(directives: &str, max: Option<LevelFilter>) {
    use prost::Message;
    let bytes = log_filter(directives, max).encode_to_vec();
    *LOG_FILTER.write().unwrap_or_else(PoisonError::into_inner) = Some(bytes);
}

/// The encoded filter to hand a plugin being loaded, if the host set one.
pub(crate) fn plugin_log_filter() -> Option<Vec<u8>> {
    LOG_FILTER
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Express the host's filter in the portable `pb::LogFilter` shape. A plain
/// `target=level` list maps over directive for directive. Anything else — span or
/// field directives, which no plugin can evaluate, or syntax this reader is not
/// sure it reads as `EnvFilter` does — becomes just the filter's overall maximum:
/// over-forwarding is the safe direction, since the host filters again.
fn log_filter(directives: &str, max: Option<LevelFilter>) -> pb::LogFilter {
    plain_directives(directives).unwrap_or_else(|| pb::LogFilter {
        default_level: pb_level(max.unwrap_or(LevelFilter::TRACE)).into(),
        directives: Vec::new(),
    })
}

/// `directives` read as `EnvFilter` reads a plain list, or `None` if it is not
/// one. Not `Targets::from_str`: that accepts some `EnvFilter` strings and reads
/// them more narrowly — `info,` as ERROR, `x=` as `x` at ERROR, `[span]=trace` as
/// a literal target — which would make a plugin drop what the host shows.
///
/// `EnvFilter`'s rules: empty pieces are skipped; a bare piece is the default
/// level if it parses as one, else a target at TRACE; `target=` is TRACE. A
/// repeated target or default keeps the most verbose level, never narrower than
/// either reading of the repeat.
fn plain_directives(directives: &str) -> Option<pb::LogFilter> {
    fn merge(slot: &mut Option<LevelFilter>, level: LevelFilter) {
        *slot = Some(slot.map_or(level, |l| l.max(level)));
    }
    let mut default = None;
    let mut targets: HashMap<&str, Option<LevelFilter>> = HashMap::new();
    for piece in directives.split(',') {
        if piece.is_empty() {
            continue;
        }
        if piece.contains(|c: char| matches!(c, '[' | ']' | '{' | '}') || c.is_whitespace()) {
            return None;
        }
        match piece.split_once('=') {
            None => match piece.parse::<LevelFilter>() {
                Ok(level) => merge(&mut default, level),
                Err(_) => merge(targets.entry(piece).or_default(), LevelFilter::TRACE),
            },
            Some((target, level)) => {
                if target.is_empty() || level.contains('=') {
                    return None;
                }
                let level = if level.is_empty() {
                    LevelFilter::TRACE
                } else {
                    level.parse().ok()?
                };
                merge(targets.entry(target).or_default(), level);
            }
        }
    }
    Some(pb::LogFilter {
        default_level: pb_level(default.unwrap_or(LevelFilter::OFF)).into(),
        directives: targets
            .into_iter()
            .filter_map(|(target, level)| {
                Some(pb::log_filter::Directive {
                    target_prefix: target.to_string(),
                    level: pb_level(level?).into(),
                })
            })
            .collect(),
    })
}

fn pb_level(level: LevelFilter) -> pb::log_filter::Level {
    use pb::log_filter::Level as L;
    match level.into_level() {
        None => L::Off,
        Some(Level::ERROR) => L::Error,
        Some(Level::WARN) => L::Warn,
        Some(Level::INFO) => L::Info,
        Some(Level::DEBUG) => L::Debug,
        Some(Level::TRACE) => L::Trace,
    }
}

#[cfg(test)]
mod filter_tests {
    use super::{LevelFilter, log_filter, pb};
    use crate::abi::StableLogSink;
    use pb::log_filter::{Directive, Level as L};

    fn dir(prefix: &str, level: L) -> Directive {
        Directive {
            target_prefix: prefix.to_string(),
            level: level.into(),
        }
    }

    #[test]
    fn target_directives_map_one_to_one() {
        let mut f = log_filter(
            "info,fuser=error,oci_client=debug",
            Some(LevelFilter::DEBUG),
        );
        f.directives
            .sort_by(|a, b| a.target_prefix.cmp(&b.target_prefix));
        assert_eq!(f.default_level, i32::from(L::Info));
        assert_eq!(
            f.directives,
            vec![dir("fuser", L::Error), dir("oci_client", L::Debug)]
        );
    }

    /// No bare level: `EnvFilter` drops whatever no directive names, so must the plugin.
    #[test]
    fn no_default_level_is_off() {
        let f = log_filter("oci_client=debug", Some(LevelFilter::DEBUG));
        assert_eq!(f.default_level, i32::from(L::Off));
    }

    /// A span directive cannot be evaluated by a plugin: send only the maximum.
    #[test]
    fn span_directive_falls_back_to_the_maximum() {
        let f = log_filter("info,[build]=trace", Some(LevelFilter::TRACE));
        assert_eq!(f.default_level, i32::from(L::Trace));
        assert_eq!(f.directives, vec![]);
    }

    /// The proto's matching rule, written out independently of `Targets`: the
    /// longest matching prefix decides, else the default.
    fn admits(filter: &pb::LogFilter, target: &str, level: tracing::Level) -> bool {
        let to_filter = |l: i32| match L::try_from(l) {
            Ok(L::Off) => LevelFilter::OFF,
            Ok(L::Error) => LevelFilter::ERROR,
            Ok(L::Warn) => LevelFilter::WARN,
            Ok(L::Info) => LevelFilter::INFO,
            Ok(L::Debug) => LevelFilter::DEBUG,
            _ => LevelFilter::TRACE,
        };
        let allowed = filter
            .directives
            .iter()
            .filter(|d| target.starts_with(d.target_prefix.as_str()))
            .max_by_key(|d| d.target_prefix.len())
            .map_or(filter.default_level, |d| d.level);
        level <= to_filter(allowed)
    }

    /// The confirmed invariant — a plugin may forward more than the host shows,
    /// never less — checked against the real `EnvFilter`: every event the host
    /// shows, at every level, for every target, inside a span and out, is one the
    /// filter sent to the plugin admits.
    #[test]
    fn plugin_never_drops_what_the_host_shows() {
        use std::sync::{Arc, Mutex};
        use tracing::{Event, Level, Subscriber};
        use tracing_subscriber::EnvFilter;
        use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

        struct Shown(Arc<Mutex<Vec<(String, Level)>>>);
        impl<S: Subscriber> Layer<S> for Shown {
            fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
                let meta = event.metadata();
                self.0
                    .lock()
                    .expect("lock")
                    .push((meta.target().to_string(), *meta.level()));
            }
        }

        const INPUTS: &[&str] = &[
            "",
            "info",
            "info,",
            ",info",
            "debug,,oci_client=trace",
            "oci_client=",
            "info,oci_client=",
            "info,[build]=trace",
            "info,oci_client[build]=trace",
            "info,[{x}]=off",
            "info,oci_client[{x}]=off",
            "oci_client[{x}]=trace,oci_client=off",
            "3",
            "INFO,oci_client=Debug",
            "oci_client",
            "off,oci_client=error",
            "warn,oci=error,oci_client=debug",
            "info,oci_client=debug,oci_client=warn",
            "info,fuser=error,object_store=warn",
        ];
        const TARGETS: &[&str] = &[
            "oci_client::client",
            "oci_spec",
            "reqwest",
            "fuser",
            "other",
        ];
        const LEVELS: &[u8] = &[1, 2, 3, 4, 5];

        for input in INPUTS {
            // The host falls back to its default for a string EnvFilter rejects.
            let Ok(env) = EnvFilter::try_new(input) else {
                continue;
            };
            let sent = log_filter(input, env.max_level_hint());
            let shown = Arc::new(Mutex::new(Vec::new()));
            let sub = tracing_subscriber::registry()
                .with(env)
                .with(Shown(shown.clone()));
            tracing::subscriber::with_default(sub, || {
                let all = || {
                    for target in TARGETS {
                        for level in LEVELS {
                            crate::host::HostLogSink.log(*level, (*target).into(), "m".into());
                        }
                    }
                };
                all();
                tracing::error_span!("build", x = 1).in_scope(all);
            });
            for (target, level) in shown.lock().expect("lock").iter() {
                assert!(
                    admits(&sent, target, *level),
                    "HEPH_LOG={input:?}: host shows {target} at {level}, plugin filter {sent:?} drops it"
                );
            }
        }
    }
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
