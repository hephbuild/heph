//! Plugin option parsing: the small slice of config every provider/driver
//! factory uses to read its `options:` map. Lives in the foundational config
//! crate; `hplugin::config` re-exports it so plugin authors keep their import.

use anyhow::Context as _;
use serde::Deserialize;
use std::collections::BTreeMap;

/// A plugin's `options:` map as parsed from the config YAML: arbitrary keys to
/// raw YAML values, decoded on demand by each plugin via [`decode_opt`].
pub type Options = BTreeMap<String, serde_yaml::Value>;

/// Decode a single typed option `key` from `opts`, if present.
pub fn decode_opt<T: for<'de> Deserialize<'de>>(
    opts: &Options,
    plugin: &str,
    key: &str,
) -> anyhow::Result<Option<T>> {
    match opts.get(key) {
        Some(v) => {
            let parsed = serde_yaml::from_value::<T>(v.clone())
                .with_context(|| format!("{plugin}: invalid value for option `{key}`"))?;
            Ok(Some(parsed))
        }
        None => Ok(None),
    }
}

/// Verify that `opts` contains only the keys listed in `allowed`. Errors
/// otherwise, naming the nearest allowed key (D4, CLI-27):
/// "unknown option `pattern`; did you mean `patterns`? (allowed: …)".
pub fn deny_unknown(plugin: &str, opts: &Options, allowed: &[&str]) -> anyhow::Result<()> {
    for key in opts.keys() {
        if !allowed.iter().any(|k| k == key) {
            let dym = hcore::suggest::did_you_mean_text(&hcore::suggest::suggest(
                key,
                allowed.iter().copied(),
            ))
            .map(|d| format!("; {d}"))
            .unwrap_or_default();
            anyhow::bail!(
                "{plugin}: unknown option `{}`{dym} (allowed: {})",
                hcore::diagnostic::escape_control(key),
                allowed.join(", ")
            );
        }
    }
    Ok(())
}
