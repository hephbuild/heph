//! The shared `cache` target attribute.
//!
//! Any driver that lets a target configure its own caching exposes a `cache`
//! field of type [`TargetSpecCache`]. It accepts either a bare bool (`cache =
//! True/False`, toggling local **and** remote) or a `{enabled, remote, history}`
//! dict, and lowers into the engine's [`CacheConfig`]. Defined once here so
//! `exec`, `http_fetch`, and every future driver parse the knob identically.

use anyhow::Context as _;
use hcore::htvalue::Value;
use hcore::htvalue::signature::ParamType;

use crate::driver::targetdef::CacheConfig;
use crate::htspec::{FromSpecValue, SpecStruct};

/// A parsed `cache` attribute. `local`/`remote` gate the two cache tiers;
/// `history` is how many cache revisions to retain. Defaults to both tiers on
/// with history 1 — the same as an absent `cache`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetSpecCache {
    pub local: bool,
    pub remote: bool,
    /// How many cache revisions to retain for this target. Default 1.
    pub history: u32,
}

impl Default for TargetSpecCache {
    fn default() -> Self {
        TargetSpecCache {
            local: true,
            remote: true,
            history: 1,
        }
    }
}

impl From<TargetSpecCache> for CacheConfig {
    fn from(c: TargetSpecCache) -> Self {
        CacheConfig {
            enabled: c.local,
            remote_enabled: c.remote,
            history: c.history,
        }
    }
}

/// The dict form of `cache`: `{"enabled": bool, "remote": bool, "history": int}`,
/// each key optional and defaulting (enabled/remote true, history 1). The
/// `SpecStruct` derive parses the map and rejects unknown keys.
#[derive(SpecStruct)]
struct CacheDict {
    #[spec(rename = "enabled", default = true)]
    local: bool,
    #[spec(default = true)]
    remote: bool,
    #[spec(default = 1u32, parse = parse_cache_history)]
    history: u32,
}

impl From<CacheDict> for TargetSpecCache {
    fn from(d: CacheDict) -> Self {
        TargetSpecCache {
            local: d.local,
            remote: d.remote,
            history: d.history,
        }
    }
}

/// The `cache` attribute accepts either a bare bool (`cache = True/False`
/// toggles both local and remote, history 1) or the [`CacheDict`] form. This is
/// shape-dispatch (not `SpecUnion`): a map *commits* to the dict arm so its
/// specific parse errors (unknown key, bad `history`) surface, rather than being
/// masked by a generic "expected bool | map" union error.
impl FromSpecValue for TargetSpecCache {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        match v {
            // A bare bool toggles both local and remote; history stays at 1.
            Value::Bool(b) => Ok(TargetSpecCache {
                local: *b,
                remote: *b,
                history: 1,
            }),
            Value::Map(_) => CacheDict::from_spec_value(v).map(TargetSpecCache::from),
            _ => anyhow::bail!("`cache` must be a bool or a dict"),
        }
    }

    fn spec_param_type() -> ParamType {
        ParamType::union(vec![ParamType::Bool, CacheDict::spec_param_type()])
    }
}

fn parse_cache_history(v: &Value) -> anyhow::Result<u32> {
    let n: i64 = match v {
        Value::Int(i) => *i,
        Value::Uint(u) => i64::try_from(*u).context("`cache.history` too large")?,
        _ => anyhow::bail!("`cache.history` must be an integer"),
    };
    if n < 1 {
        anyhow::bail!("`cache.history` must be >= 1, got {n}");
    }
    u32::try_from(n).context("`cache.history` too large")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: Value) -> anyhow::Result<TargetSpecCache> {
        TargetSpecCache::from_spec_value(&v)
    }

    fn dict(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        Value::Map(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    #[test]
    fn default_is_both_tiers_on() {
        let c = TargetSpecCache::default();
        assert!(c.local && c.remote);
        assert_eq!(c.history, 1);
    }

    #[test]
    fn bool_true_toggles_both() {
        let c = parse(Value::Bool(true)).expect("parse");
        assert!(c.local && c.remote);
        assert_eq!(c.history, 1);
    }

    #[test]
    fn bool_false_disables_both() {
        let c = parse(Value::Bool(false)).expect("parse");
        assert!(!c.local && !c.remote);
    }

    #[test]
    fn dict_partial_keys_default_the_rest() {
        // Only `remote` set → local defaults on, remote honored off.
        let c = parse(dict([("remote", Value::Bool(false))])).expect("parse");
        assert!(c.local, "enabled defaults to true");
        assert!(!c.remote);
        assert_eq!(c.history, 1);
    }

    #[test]
    fn dict_history_honored() {
        let c = parse(dict([("history", Value::Int(5))])).expect("parse");
        assert_eq!(c.history, 5);
        assert!(c.local && c.remote);
    }

    #[test]
    fn dict_zero_history_errors() {
        let err = parse(dict([("history", Value::Int(0))])).expect_err("zero");
        assert!(format!("{err:#}").contains(">= 1"), "got: {err:#}");
    }

    #[test]
    fn dict_unknown_key_errors() {
        let err = parse(dict([("nope", Value::Bool(true))])).expect_err("unknown");
        assert!(format!("{err:#}").contains("unknown"), "got: {err:#}");
    }

    #[test]
    fn non_bool_non_map_errors() {
        let err = parse(Value::Int(1)).expect_err("bad type");
        assert!(
            format!("{err:#}").contains("bool or a dict"),
            "got: {err:#}"
        );
    }

    #[test]
    fn lowers_into_cache_config() {
        let cfg: CacheConfig = TargetSpecCache {
            local: true,
            remote: false,
            history: 3,
        }
        .into();
        assert!(cfg.enabled);
        assert!(!cfg.remote_enabled);
        assert_eq!(cfg.history, 3);
    }
}
