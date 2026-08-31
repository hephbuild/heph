//! Inspecting and reclaiming the scratch store.
//!
//! The store is `<home>/scratch/<slot>/<scope>/head`, plus a small `slot.meta`
//! per slot recording which declaration produced it. Everything here is about
//! answering "what is in there and what can go", which is a different question
//! from resolving and locking a slot for a build (`scratch.rs`).
//!
//! # Why a meta file
//!
//! A slot id is a hash, so without one, `heph tool scratch ls` could only print
//! hashes — or would have to resolve the whole graph to map them back. Writing 100
//! bytes once per slot buys a listing that works with no BUILD files read at all,
//! which is the same property `heph tool clean` has for addr-only matchers and for
//! the same reason: a cache should stay inspectable when the targets that made it
//! have been deleted.
//!
//! # Nothing bounds a slot but this
//!
//! There is no `hashin` to age out and no `cache.history` to trim against — a
//! scratch is mutable state keyed by a declaration, so it grows until something
//! removes it. That makes eviction a requirement rather than a nicety, and it is
//! why the sweep is wired into `heph tool gc` rather than living behind a flag
//! nobody sets.

use crate::engine::Engine;
use anyhow::Context as _;
use borsh::{BorshDeserialize, BorshSerialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// On-disk format version for [`SlotMeta`]. A slot whose meta does not
/// deserialize is reported as unknown rather than failing the command — it is a
/// cache, and a listing that dies on one bad file is worse than one that says so.
const SLOT_META_FORMAT: u32 = 1;

const SLOT_META_FILE: &str = "slot.meta";

/// What a slot was created from. Written once per slot, purely so the store can
/// describe itself.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SlotMeta {
    pub format: u32,
    /// The declaring target, formatted (`//build:gocache`).
    pub addr: String,
    /// Mount point in a consumer's sandbox.
    pub path: String,
    /// Environment variable the directory is announced through.
    pub env: String,
    /// `"exclusive"` / `"shared"`.
    pub access: String,
    /// The declaration's bust handle.
    pub version: String,
    /// Whether the declaration opted into the remote lineage.
    pub remote: bool,
}

/// One slot as the store sees it.
#[derive(Debug, Clone)]
pub struct SlotEntry {
    pub slot: String,
    /// `None` when the meta is missing or unreadable — an orphan from a previous
    /// format, or a partially-created slot. Still listable, still removable.
    pub meta: Option<SlotMeta>,
    /// Lineages present, by sanitized scope directory name.
    pub scopes: Vec<String>,
    /// Total bytes across every lineage.
    pub bytes: u64,
    /// Most recent modification across every lineage — what LRU eviction orders
    /// by. `None` if nothing could be stat'd.
    pub last_used: Option<SystemTime>,
}

/// Root of the scratch store.
pub fn store_root(home: &Path) -> PathBuf {
    home.join("scratch")
}

fn meta_path(home: &Path, slot: &str) -> PathBuf {
    store_root(home).join(slot).join(SLOT_META_FILE)
}

/// Record what a slot came from, if it is not already recorded.
///
/// Best-effort by design: this is a diagnostic, and failing a build because a
/// descriptive file could not be written would trade a listing for a build.
/// Idempotent, so the common (warm) path does one `exists` and stops.
pub fn write_slot_meta(home: &Path, slot: &str, meta: &SlotMeta) {
    let path = meta_path(home, slot);
    if path.exists() {
        return;
    }
    let write = || -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = borsh::to_vec(meta)?;
        // Write-then-rename, so a concurrent reader never sees a half file.
        let tmp = path.with_extension("meta.partial");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    };
    if let Err(err) = write() {
        tracing::debug!(slot, error = %err, "could not record scratch slot meta");
    }
}

fn read_slot_meta(home: &Path, slot: &str) -> Option<SlotMeta> {
    let bytes = std::fs::read(meta_path(home, slot)).ok()?;
    let meta = SlotMeta::try_from_slice(&bytes).ok()?;
    (meta.format == SLOT_META_FORMAT).then_some(meta)
}

/// Recursive size and newest mtime of a directory tree.
///
/// Walks rather than caching: a size is only ever asked for by `ls`, `gc` and
/// the per-slot cap, none of which is on a build's hot path, and a cached total
/// would be wrong the moment a target wrote into the slot — which is constantly.
fn measure(dir: &Path) -> (u64, Option<SystemTime>) {
    let (mut bytes, mut newest) = (0u64, None::<SystemTime>);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (bytes, newest);
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let (b, t) = measure(&entry.path());
            bytes += b;
            newest = newest.max(t);
        } else if let Ok(md) = entry.metadata() {
            // Symlinks contribute their own size, not their target's — a scratch
            // may contain links out of the tree and following them would both
            // over-count and, on a cycle, never finish.
            if !ft.is_symlink() {
                bytes += md.len();
            }
            newest = newest.max(md.modified().ok());
        }
    }
    (bytes, newest)
}

impl Engine {
    /// Every slot in the store, newest first.
    ///
    /// Reads no BUILD files: a slot describes itself, so a cache stays inspectable
    /// after the targets that made it are gone.
    pub fn scratch_slots(&self) -> anyhow::Result<Vec<SlotEntry>> {
        let root = store_root(&self.home);
        let Ok(dir) = std::fs::read_dir(&root) else {
            // No store yet is not an error — it is an empty one.
            return Ok(Vec::new());
        };

        let mut out = Vec::new();
        for entry in dir.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let slot = entry.file_name().to_string_lossy().into_owned();
            let slot_dir = entry.path();

            // Measured per *scope*, not over the whole slot dir, so `bytes` is
            // the cache's own footprint and not that plus heph's bookkeeping.
            // A user asking how big a cache is does not mean `slot.meta`, and a
            // size that silently included it would make every eviction threshold
            // slightly wrong in a way nobody could see.
            let (mut scopes, mut bytes, mut last_used) = (Vec::new(), 0u64, None);
            if let Ok(inner) = std::fs::read_dir(&slot_dir) {
                for s in inner.flatten() {
                    if !s.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    scopes.push(s.file_name().to_string_lossy().into_owned());
                    let (b, t) = measure(&s.path());
                    bytes += b;
                    last_used = last_used.max(t);
                }
            }
            scopes.sort();
            out.push(SlotEntry {
                meta: read_slot_meta(&self.home, &slot),
                slot,
                scopes,
                bytes,
                last_used,
            });
        }
        // Newest first: the interesting ones are the ones in use.
        out.sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.slot.cmp(&b.slot)));
        Ok(out)
    }

    /// Drop slots whose declaring addr matches `addr`, or all of them when `None`.
    ///
    /// Returns how many went and how many bytes came back. A slot with no readable
    /// meta matches only the `None` (everything) selection — it cannot be named,
    /// so naming must not silently miss it, and clearing everything must not
    /// silently leave it.
    pub fn scratch_remove(&self, addr: Option<&str>) -> anyhow::Result<(usize, u64)> {
        let (mut removed, mut bytes) = (0usize, 0u64);
        for slot in self.scratch_slots()? {
            let matches = match addr {
                None => true,
                Some(want) => slot.meta.as_ref().is_some_and(|m| m.addr == want),
            };
            if !matches {
                continue;
            }
            let dir = store_root(&self.home).join(&slot.slot);
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("remove scratch slot {dir:?}"))?;
            removed += 1;
            bytes += slot.bytes;
        }
        Ok((removed, bytes))
    }

    /// Reclaim scratch storage: drop anything older than `max_age`, then drop
    /// least-recently-used slots until the store fits `max_bytes`.
    ///
    /// **Whole slots, never partial trims.** heph cannot know which of a foreign
    /// tool's entries are hot, and guessing would quietly degrade a cache while
    /// claiming to manage it. Dropping is honest and self-correcting: the next
    /// build repopulates what it actually needs.
    pub fn scratch_sweep(
        &self,
        max_bytes: Option<u64>,
        max_age: Option<std::time::Duration>,
    ) -> anyhow::Result<(usize, u64)> {
        let mut slots = self.scratch_slots()?;
        let (mut removed, mut freed) = (0usize, 0u64);
        let now = SystemTime::now();

        if let Some(max_age) = max_age {
            slots.retain(|s| {
                let stale = s
                    .last_used
                    .and_then(|t| now.duration_since(t).ok())
                    .is_some_and(|age| age > max_age);
                if !stale {
                    return true;
                }
                let dir = store_root(&self.home).join(&s.slot);
                if std::fs::remove_dir_all(&dir).is_ok() {
                    removed += 1;
                    freed += s.bytes;
                }
                false
            });
        }

        if let Some(max_bytes) = max_bytes {
            let mut total: u64 = slots.iter().map(|s| s.bytes).sum();
            // `scratch_slots` is newest-first, so evict from the back.
            while total > max_bytes {
                let Some(victim) = slots.pop() else { break };
                let dir = store_root(&self.home).join(&victim.slot);
                if std::fs::remove_dir_all(&dir).is_ok() {
                    removed += 1;
                    freed += victim.bytes;
                }
                total = total.saturating_sub(victim.bytes);
            }
        }
        Ok((removed, freed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(addr: &str) -> SlotMeta {
        SlotMeta {
            format: SLOT_META_FORMAT,
            addr: addr.to_string(),
            path: ".cache/x".to_string(),
            env: "C".to_string(),
            access: "exclusive".to_string(),
            version: String::new(),
            remote: false,
        }
    }

    /// Build a slot with one scope holding `bytes` bytes.
    fn make_slot(home: &Path, slot: &str, addr: &str, bytes: usize) {
        let d = store_root(home).join(slot).join("_").join("head");
        std::fs::create_dir_all(&d).expect("mkdir");
        std::fs::write(d.join("blob"), vec![0u8; bytes]).expect("write");
        write_slot_meta(home, slot, &meta(addr));
    }

    #[test]
    fn slot_meta_round_trips_and_rejects_a_foreign_format() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_slot_meta(tmp.path(), "s", &meta("//a:b"));
        assert_eq!(read_slot_meta(tmp.path(), "s"), Some(meta("//a:b")));

        // A meta from a future layout is not guessed at.
        let mut future = meta("//a:b");
        future.format = SLOT_META_FORMAT + 1;
        let p = meta_path(tmp.path(), "s2");
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(&p, borsh::to_vec(&future).expect("ser")).expect("write");
        assert_eq!(read_slot_meta(tmp.path(), "s2"), None);
    }

    /// Writing is idempotent, so the warm path never rewrites and a slot's
    /// recorded origin cannot drift.
    #[test]
    fn writing_meta_twice_keeps_the_first() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_slot_meta(tmp.path(), "s", &meta("//first:one"));
        write_slot_meta(tmp.path(), "s", &meta("//second:two"));
        assert_eq!(
            read_slot_meta(tmp.path(), "s").expect("meta").addr,
            "//first:one"
        );
    }

    #[test]
    fn measure_counts_bytes_and_does_not_follow_symlinks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let d = tmp.path().join("t");
        std::fs::create_dir_all(d.join("sub")).expect("mkdir");
        std::fs::write(d.join("a"), vec![0u8; 100]).expect("write");
        std::fs::write(d.join("sub").join("b"), vec![0u8; 50]).expect("write");
        // A link to a big file outside the tree must not be counted as its target.
        std::fs::write(tmp.path().join("big"), vec![0u8; 10_000]).expect("write");
        std::os::unix::fs::symlink(tmp.path().join("big"), d.join("link")).expect("symlink");

        let (bytes, newest) = measure(&d);
        assert_eq!(bytes, 150, "symlink target must not be counted");
        assert!(newest.is_some());
    }

    #[test]
    fn an_absent_store_lists_empty_rather_than_erroring() {
        let (engine, _tmp) = crate::engine::cache_test_support::test_engine();
        assert!(engine.scratch_slots().expect("list").is_empty());
    }

    #[test]
    fn slots_are_listed_with_their_declaration_and_size() {
        let (engine, _tmp) = crate::engine::cache_test_support::test_engine();
        make_slot(&engine.home, "aaa", "//build:gocache", 500);
        make_slot(&engine.home, "bbb", "//build:cargo", 100);

        let slots = engine.scratch_slots().expect("list");
        assert_eq!(slots.len(), 2);
        let by_addr: std::collections::HashMap<_, _> = slots
            .iter()
            .map(|s| (s.meta.as_ref().expect("meta").addr.clone(), s))
            .collect();
        assert_eq!(by_addr["//build:gocache"].bytes, 500);
        assert_eq!(by_addr["//build:cargo"].bytes, 100);
        assert_eq!(by_addr["//build:gocache"].scopes, vec!["_".to_string()]);
    }

    /// Every lineage of a slot counts toward it — the store is bounded per slot,
    /// not per branch, or a developer with many branches would never hit a cap.
    #[test]
    fn a_slots_size_spans_all_its_scopes() {
        let (engine, _tmp) = crate::engine::cache_test_support::test_engine();
        make_slot(&engine.home, "aaa", "//build:c", 100);
        let other = store_root(&engine.home)
            .join("aaa")
            .join("feat")
            .join("head");
        std::fs::create_dir_all(&other).expect("mkdir");
        std::fs::write(other.join("blob"), vec![0u8; 250]).expect("write");

        let slots = engine.scratch_slots().expect("list");
        assert_eq!(slots[0].bytes, 350);
        assert_eq!(slots[0].scopes, vec!["_".to_string(), "feat".to_string()]);
    }

    #[test]
    fn removing_by_addr_takes_only_that_slot() {
        let (engine, _tmp) = crate::engine::cache_test_support::test_engine();
        make_slot(&engine.home, "aaa", "//build:gocache", 500);
        make_slot(&engine.home, "bbb", "//build:cargo", 100);

        let (n, freed) = engine.scratch_remove(Some("//build:gocache")).expect("rm");
        assert_eq!((n, freed), (1, 500));

        let left = engine.scratch_slots().expect("list");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].meta.as_ref().expect("meta").addr, "//build:cargo");
    }

    /// A slot whose meta is unreadable cannot be *named*, so naming must not
    /// silently miss it — and clearing everything must not silently leave it.
    #[test]
    fn an_unnameable_slot_survives_a_named_removal_and_dies_with_the_rest() {
        let (engine, _tmp) = crate::engine::cache_test_support::test_engine();
        let orphan = store_root(&engine.home)
            .join("orphan")
            .join("_")
            .join("head");
        std::fs::create_dir_all(&orphan).expect("mkdir");
        std::fs::write(orphan.join("blob"), vec![0u8; 10]).expect("write");
        make_slot(&engine.home, "aaa", "//build:c", 10);

        assert_eq!(engine.scratch_remove(Some("//build:c")).expect("rm").0, 1);
        assert_eq!(
            engine.scratch_slots().expect("list").len(),
            1,
            "orphan stays"
        );

        assert_eq!(engine.scratch_remove(None).expect("rm all").0, 1);
        assert!(engine.scratch_slots().expect("list").is_empty());
    }

    #[test]
    fn the_sweep_drops_whole_slots_until_the_store_fits() {
        let (engine, _tmp) = crate::engine::cache_test_support::test_engine();
        for (slot, bytes) in [("aaa", 400), ("bbb", 400), ("ccc", 400)] {
            make_slot(&engine.home, slot, &format!("//build:{slot}"), bytes);
            // Distinct mtimes so LRU order is defined.
            std::thread::sleep(std::time::Duration::from_millis(15));
        }

        let (removed, freed) = engine.scratch_sweep(Some(900), None).expect("sweep");
        assert_eq!(removed, 1, "one slot is enough to get under 900");
        assert_eq!(freed, 400);
        // The oldest went; the two most recent stayed.
        let left: Vec<_> = engine
            .scratch_slots()
            .expect("list")
            .into_iter()
            .map(|s| s.slot)
            .collect();
        assert!(!left.contains(&"aaa".to_string()), "oldest must go first");
        assert_eq!(left.len(), 2);
    }

    #[test]
    fn the_sweep_drops_stale_slots_by_age() {
        let (engine, _tmp) = crate::engine::cache_test_support::test_engine();
        make_slot(&engine.home, "aaa", "//build:c", 10);
        // Everything is younger than an hour, so nothing goes.
        assert_eq!(
            engine
                .scratch_sweep(None, Some(std::time::Duration::from_secs(3600)))
                .expect("sweep")
                .0,
            0
        );
        // Everything is older than zero, so it all goes.
        assert_eq!(
            engine
                .scratch_sweep(None, Some(std::time::Duration::ZERO))
                .expect("sweep")
                .0,
            1
        );
    }

    #[test]
    fn a_sweep_with_no_limits_removes_nothing() {
        let (engine, _tmp) = crate::engine::cache_test_support::test_engine();
        make_slot(&engine.home, "aaa", "//build:c", 10);
        assert_eq!(engine.scratch_sweep(None, None).expect("sweep"), (0, 0));
        assert_eq!(engine.scratch_slots().expect("list").len(), 1);
    }
}
