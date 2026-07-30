//! Latency-based ordering of remote caches. Reads probe caches one-by-one in
//! ascending-latency order, so the fastest cache that holds an entry serves it.
//!
//! Probing every cache on every run would add a round-trip to startup, so the
//! measured order is persisted to `<home>/cache/remote-latency.json` and reused
//! until the cache definitions change. The file is keyed by a hash of the
//! definitions; a mismatch (a cache added/removed/re-pointed) forces a
//! re-measure, satisfying "measure once per repo, or when the definition
//! changes".

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::io::Write;
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
///
/// Written via temp-file-then-rename rather than a direct `fs::write`: two
/// heph processes racing a re-measure at the same time would otherwise be
/// able to interleave their writes into a corrupt (or silently
/// last-writer-mixed) JSON file. `rename` within the same directory is
/// atomic, so a concurrent reader always sees either the old or the new
/// content, never a partial one. Using `tempfile::NamedTempFile` rather than
/// a hand-named `fs::write` + `fs::rename` also gets crash safety for free:
/// if the process is killed between the write and the rename, `Drop` unlinks
/// the temp file instead of leaking it under `<home>/cache/` forever.
pub(crate) fn store_order(home: &Path, config_hash: &str, order: &[String]) -> anyhow::Result<()> {
    let file = latency_file(home);
    let parent = file
        .parent()
        .context("latency file path has no parent directory")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let stored = StoredOrder {
        config_hash: config_hash.to_string(),
        order: order.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&stored)?;

    let mut tmp = tempfile::Builder::new()
        .prefix(
            &file
                .file_name()
                .context("latency file path has no file name")?,
        )
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| format!("creating temp file in {}", parent.display()))?;
    tmp.write_all(&bytes)
        .with_context(|| format!("writing {}", tmp.path().display()))?;
    tmp.persist(&file)
        .map_err(|e| e.error)
        .with_context(|| format!("renaming temp file to {}", file.display()))?;
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

    /// Concurrent writers must never leave the file in a torn/interleaved
    /// state — each `store_order` call is temp-file-then-rename, so a
    /// racing reader always observes one writer's complete content, never a
    /// byte-level mix of two. A plain `fs::write` from two processes at once
    /// could interleave and produce invalid JSON; this would surface as a
    /// `load_order` parse failure below.
    #[test]
    fn concurrent_writers_never_produce_a_torn_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().to_path_buf();

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let home = home.clone();
                std::thread::spawn(move || {
                    let order: Vec<String> = (0..50).map(|n| format!("cache-{i}-{n}")).collect();
                    store_order(&home, "h1", &order).expect("store");
                })
            })
            .collect();
        for h in handles {
            h.join().expect("writer thread panicked");
        }

        // Whichever writer landed last, the file must parse cleanly and
        // contain one writer's full, uninterleaved order — never a partial
        // or mixed one.
        let order = load_order(&home, "h1").expect("file parses and hash matches");
        assert_eq!(order.len(), 50, "order must be one writer's complete set");
        let prefix = order[0].rsplit_once('-').expect("cache-N-M").0;
        assert!(
            order.iter().all(|name| name.starts_with(prefix)),
            "order must not interleave entries from different writers: {order:?}"
        );

        // No leftover temp files: every writer's `persist` must have consumed
        // its own temp file, leaving only the final `remote-latency.json`.
        let entries: Vec<_> = std::fs::read_dir(home.join("cache"))
            .expect("read cache dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("remote-latency.json")],
            "leftover files in cache dir: {entries:?}"
        );
    }
}
