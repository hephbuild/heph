//! Latency-based ordering of remote caches. Reads probe caches one-by-one in
//! ascending-latency order, so the fastest cache that holds an entry serves it.
//!
//! Probing every cache on every run would add a round-trip to startup, so the
//! measured order is persisted to `<home>/cache/remote-latency.json` and reused
//! until the cache definitions change. The file is keyed by a hash of the
//! definitions; a mismatch (a cache added/removed/re-pointed) forces a
//! re-measure, satisfying "measure once per repo, or when the definition
//! changes".

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Persisted latency ordering. `order` lists cache names fastest-first; `config_hash`
/// ties the measurement to the exact set of definitions it was taken against.
#[derive(Debug, Serialize, Deserialize)]
struct StoredOrder {
    config_hash: String,
    /// Cache names, fastest first.
    order: Vec<String>,
}

fn latency_file(home: &Path) -> PathBuf {
    home.join("cache").join("remote-latency.json")
}

/// Load a previously-measured order if it matches `config_hash`. Returns the
/// stored name ordering, or `None` when absent/stale/unreadable (any failure
/// just triggers a fresh measurement — the file is a cache, not a source of
/// truth).
pub(crate) fn load_order(home: &Path, config_hash: &str) -> Option<Vec<String>> {
    let bytes = std::fs::read(latency_file(home)).ok()?;
    let stored: StoredOrder = serde_json::from_slice(&bytes).ok()?;
    if stored.config_hash == config_hash {
        Some(stored.order)
    } else {
        None
    }
}

/// Persist a freshly-measured order. Best-effort: a write failure only costs a
/// re-measure next run, so errors are swallowed by the caller.
pub(crate) fn store_order(home: &Path, config_hash: &str, order: &[String]) -> anyhow::Result<()> {
    let file = latency_file(home);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stored = StoredOrder {
        config_hash: config_hash.to_string(),
        order: order.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&stored)?;
    std::fs::write(&file, bytes)?;
    Ok(())
}

/// Sentinel for an unreachable cache during probing: sorts last so a flaky cache
/// is tried after every healthy one rather than dropped.
pub(crate) const UNREACHABLE: Duration = Duration::from_secs(u64::MAX / 2);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_roundtrips_when_hash_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path();
        assert!(load_order(home, "h1").is_none());

        store_order(home, "h1", &["fast".into(), "slow".into()]).expect("store");
        assert_eq!(
            load_order(home, "h1"),
            Some(vec!["fast".to_string(), "slow".to_string()])
        );
    }

    #[test]
    fn stale_hash_forces_remeasure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path();
        store_order(home, "h1", &["a".into()]).expect("store");
        // A changed definition set (different hash) must be treated as absent.
        assert!(load_order(home, "h2").is_none());
    }
}
