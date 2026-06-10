//! Cache garbage collection.
//!
//! Two entry points, both keyed per-addr and serialized through
//! [`ResultLock`](crate::engine::result_lock::ResultLock) so GC never deletes a
//! revision another request/process is reading or rebuilding:
//!
//! - [`Engine::gc_all`] — the `heph gc` sweep. For every `(addr, hashin)` group
//!   in the cache: if the target no longer resolves (`get_spec` →
//!   `TargetNotFoundError`) every revision is dropped; otherwise the target is
//!   trimmed to its `cache.history` newest revisions.
//! - [`Engine::try_trim_after_write`] — the post-write trim. Non-blocking: it
//!   trims the just-written target only if its lock is free, and never deletes
//!   the revision that was just written.
//!
//! Revision recency comes from each group's `manifest-v1.borsh`
//! (`created_at_nanos`); the full artifact name list to delete comes from the
//! same manifest.

use crate::engine::Engine;
use crate::engine::error::TargetNotFoundError;
use crate::engine::local_cache::MANIFEST_V1;
use crate::engine::request_state::RequestState;
use crate::engine::result_lock::ResultWriteGuard;
use crate::hmemoizer::downcast_chain_ref;
use crate::htaddr::{Addr, parse_addr};
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::task::JoinSet;

/// Outcome of a [`Engine::gc_all`] sweep.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcStats {
    /// Targets whose spec no longer resolves; all their revisions were removed.
    pub orphan_targets_removed: usize,
    /// Cache revisions deleted (orphan drops + history trims).
    pub revisions_removed: usize,
    /// Cache revisions retained on live targets.
    pub revisions_kept: usize,
    /// Total bytes freed (summed from the manifests of deleted revisions).
    pub bytes_removed: u64,
    /// Targets that could not be processed (resolve/delete failed). GC logs each
    /// and keeps going — a single bad target never aborts the sweep.
    pub errored: usize,
}

/// Per-target result of a GC pass, accumulated into [`GcStats`].
#[derive(Debug, Default)]
struct TargetOutcome {
    removed: usize,
    kept: usize,
    bytes: u64,
    orphan: bool,
}

/// What phase 1 decided for a target; applied under its lock in phase 2.
#[derive(Debug, Clone, Copy)]
enum Decision {
    /// Spec no longer resolves — drop every revision.
    Orphan,
    /// Live target — keep its `history` newest revisions.
    Trim(u32),
    /// Resolution failed — leave the target untouched (counted as errored).
    Skip,
}

impl Engine {
    /// Delete one cache revision: every artifact named in its manifest, then the
    /// manifest itself. Returns the bytes freed (Σ manifest artifact sizes).
    /// Best-effort if the manifest is missing (still removes the manifest key in
    /// case of a partial write; reports 0 bytes).
    fn gc_entry(&self, addr: &Addr, hashin: &str) -> Result<u64> {
        let mut bytes = 0u64;
        if let Some(manifest) = self.read_manifest(addr, hashin)? {
            for a in &manifest.artifacts {
                self.local_cache
                    .delete(addr, hashin, &a.name)
                    .with_context(|| format!("delete cached artifact {} for {addr}", a.name))?;
                bytes = bytes.saturating_add(a.size);
            }
        }
        self.local_cache
            .delete(addr, hashin, MANIFEST_V1)
            .with_context(|| format!("delete manifest for {addr} {hashin}"))?;
        Ok(bytes)
    }

    /// Trim `addr`'s revisions to the `keep` newest (by `created_at_nanos`),
    /// deleting the rest. `protect` is never deleted regardless of age. Returns
    /// `(removed, kept, bytes_freed)`.
    ///
    /// Takes `_guard` as proof the caller holds `addr`'s write lock: deleting a
    /// revision out from under a concurrent reader/builder would corrupt its
    /// result. The guard is held by the caller (not acquired here) so the lock
    /// also covers the `get_spec`/enumerate that decided what to trim — closing
    /// the window where a racing build could write a fresh revision between the
    /// decision and the delete.
    fn trim_addr_history(
        &self,
        _guard: &ResultWriteGuard,
        addr: &Addr,
        hashins: &[String],
        keep: u32,
        protect: Option<&str>,
    ) -> Result<(usize, usize, u64)> {
        let mut with_ts: Vec<(&str, i64)> = Vec::with_capacity(hashins.len());
        for hashin in hashins {
            // A revision whose manifest is unreadable sorts oldest (ts 0) so it
            // is the first to be reclaimed.
            let ts = self
                .read_manifest(addr, hashin)?
                .map(|m| m.created_at_nanos)
                .unwrap_or(0);
            with_ts.push((hashin.as_str(), ts));
        }
        // Newest first.
        with_ts.sort_by_key(|(_, ts)| std::cmp::Reverse(*ts));

        let keep = keep as usize;
        let mut removed = 0;
        let mut kept = 0;
        let mut bytes = 0u64;
        for (i, (hashin, _)) in with_ts.iter().enumerate() {
            if i < keep || protect == Some(*hashin) {
                kept += 1;
                continue;
            }
            bytes = bytes.saturating_add(
                self.gc_entry(addr, hashin)
                    .with_context(|| format!("trim revision {hashin} of {addr}"))?,
            );
            removed += 1;
        }
        Ok((removed, kept, bytes))
    }

    /// Synchronous post-write trim: keep the target's `keep` newest revisions,
    /// always preserving `written_hashin`. Runs only if `addr`'s lock is free —
    /// returns immediately (trimming nothing) when contended, never blocking.
    /// Fire-and-forget from the background cleaner; errors are logged, not
    /// propagated.
    pub(crate) fn try_trim_after_write(
        self: &Arc<Self>,
        addr: &Addr,
        keep: u32,
        written_hashin: &str,
    ) {
        match self.result_lock().try_write(addr) {
            Ok(Some(guard)) => {
                // Barrier on the just-written manifest so enumeration observes it
                // (the sqlite write may still be in flight) and we don't leave the
                // fresh revision uncounted.
                if let Err(e) = self.local_cache.exists(addr, written_hashin, MANIFEST_V1) {
                    tracing::debug!(error = %format!("{e:#}"), %addr, "post-write gc barrier");
                    return;
                }
                let hashins = match self.local_cache.list_target_entries(addr) {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::debug!(error = %format!("{e:#}"), %addr, "post-write gc enumerate");
                        return;
                    }
                };
                if let Err(e) =
                    self.trim_addr_history(&guard, addr, &hashins, keep, Some(written_hashin))
                {
                    tracing::debug!(error = %format!("{e:#}"), %addr, "post-write gc trim");
                }
            }
            // Contended — another request holds the lock; skip without blocking.
            Ok(None) => {}
            Err(e) => tracing::debug!(error = %format!("{e:#}"), %addr, "post-write gc try_write"),
        }
    }

    /// Sweep the whole local cache in two phases.
    ///
    /// **Phase 1 (resolve)** decides each target's fate — `Orphan` (spec no
    /// longer resolves), `Trim(history)`, or `Skip` (resolution failed) — under a
    /// *single shared* request state. Resolution executes shared, uncacheable
    /// deps (e.g. `_golist`, `go.mod`); memoizing across the whole sweep runs
    /// each a handful of times instead of once per dependent target — otherwise
    /// GC would re-create the very (tmp-keyed) revisions it is trying to reclaim.
    /// The read guards resolution takes live in that request state and are
    /// released when it drops at the end of phase 1.
    ///
    /// **Phase 2 (apply)** acquires each target's write lock and trims/deletes.
    /// It does *no* resolution and holds no request state, so a per-addr write
    /// lock can never invert against a read lock the sweep still holds (the
    /// deadlock that a one-state sweep would hit). Targets are independent
    /// (distinct addrs → distinct locks) and processed with bounded concurrency.
    ///
    /// A per-target failure is logged and counted (`GcStats::errored`), never
    /// aborts the sweep, and the JoinSet is always fully drained.
    pub async fn gc_all(self: Arc<Self>, rs: Arc<RequestState>) -> Result<GcStats> {
        let decisions = Arc::clone(&self).gc_resolve_decisions(&rs).await;

        // Targets whose resolution failed are left untouched but still counted.
        let mut stats = GcStats {
            errored: decisions
                .iter()
                .filter(|(_, d)| matches!(d, Decision::Skip))
                .count(),
            ..GcStats::default()
        };

        let limit = self.max_workers.max(1);
        let mut set: JoinSet<(Addr, Result<TargetOutcome>)> = JoinSet::new();
        for (addr, decision) in decisions {
            let (Decision::Orphan | Decision::Trim(_)) = decision else {
                // Skipped target: count already recorded; advance the explored
                // count without taking its lock.
                emit_gc_target_swept(&rs, 0, 0);
                continue;
            };
            while set.len() >= limit {
                Self::drain_one(&mut set, &rs, &mut stats).await;
            }
            let engine = Arc::clone(&self);
            let crs = rs.clone();
            set.spawn(async move {
                let out = engine.gc_apply(crs, &addr, decision).await;
                (addr, out)
            });
        }
        while !set.is_empty() {
            Self::drain_one(&mut set, &rs, &mut stats).await;
        }

        Ok(stats)
    }

    /// Phase 1: resolve every cached target's [`Decision`] under one shared,
    /// memoizing request state, with bounded concurrency. Resolution failures
    /// become [`Decision::Skip`] (logged), never aborting. The shared state — and
    /// every read lock resolution took — is dropped before this returns, so phase
    /// 2's write locks are safe.
    async fn gc_resolve_decisions(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
    ) -> Vec<(Addr, Decision)> {
        let resolve_rs = self.new_state_full(false, rs.events_sender(), rs.bg_pending());
        let targets = match self.local_cache.list_targets() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "gc: listing cache targets failed");
                return Vec::new();
            }
        };

        let limit = self.max_workers.max(1);
        let mut set: JoinSet<(Addr, Decision)> = JoinSet::new();
        let mut decisions: Vec<(Addr, Decision)> = Vec::new();

        for target in targets {
            let addr_key = match target {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "gc: listing targets failed mid-stream, resolving what was seen");
                    break;
                }
            };
            let addr = match parse_addr(&addr_key) {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(addr = %addr_key, error = %format!("{e:#}"), "gc: skip unparseable cache addr");
                    continue;
                }
            };
            while set.len() >= limit {
                push_decision(&mut decisions, set.join_next().await);
            }
            let engine = Arc::clone(&self);
            let rrs = resolve_rs.clone();
            set.spawn(async move {
                let d = engine.gc_decide(rrs, &addr).await;
                (addr, d)
            });
        }
        while !set.is_empty() {
            push_decision(&mut decisions, set.join_next().await);
        }

        decisions
        // `resolve_rs` drops here → releases every read lock resolution took.
    }

    /// Resolve one target's [`Decision`]. Never errors: a resolution failure maps
    /// to [`Decision::Skip`] (logged) so the sweep keeps going.
    async fn gc_decide(self: Arc<Self>, rrs: Arc<RequestState>, addr: &Addr) -> Decision {
        match Arc::clone(&self).get_spec(rrs.clone(), addr).await {
            Err(e) if downcast_chain_ref::<TargetNotFoundError>(&e).is_some() => Decision::Orphan,
            Err(e) => {
                tracing::warn!(%addr, error = %format!("{e:#}"), "gc: get_spec failed, skipping target");
                Decision::Skip
            }
            Ok(_) => match self.get_direct_def(rrs, addr).await {
                Ok(def) => Decision::Trim(def.target_def.cache.history),
                Err(e) => {
                    tracing::warn!(%addr, error = %format!("{e:#}"), "gc: get_def failed, skipping target");
                    Decision::Skip
                }
            },
        }
    }

    /// Phase 2: apply a resolved [`Decision`].
    ///
    /// First does a *cheap, unlocked* revision count. A target that is already
    /// within budget — a live target with `count <= history`, or an orphan with
    /// no entries — has nothing to delete, so it is returned without ever taking
    /// the per-addr write lock. That lock's file open/`unlink`/close churn is the
    /// dominant cost of a steady-state sweep (profiled ~31% of gc CPU when
    /// nothing was freed), so skipping it for already-clean targets is the big
    /// lever. The skip is race-safe: skipping deletes nothing, so no lock is
    /// needed; a revision a build adds after the check is reclaimed next sweep.
    ///
    /// Only when work is actually needed is the write lock acquired
    /// (`acquire_with_notice` surfaces a contended lock as a TUI notice) and the
    /// entry set re-listed under it (authoritative — it may have changed since
    /// the unlocked pre-check).
    async fn gc_apply(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        decision: Decision,
    ) -> Result<TargetOutcome> {
        let pre = self
            .local_cache
            .list_target_entries(addr)
            .with_context(|| format!("gc: list entries for {addr}"))?;
        let needs_lock = match decision {
            Decision::Orphan => !pre.is_empty(),
            Decision::Trim(history) => pre.len() as u32 > history,
            Decision::Skip => false,
        };
        if !needs_lock {
            // Nothing to delete — retained as-is, no lock taken.
            let kept = if matches!(decision, Decision::Orphan) {
                0
            } else {
                pre.len()
            };
            return Ok(TargetOutcome {
                removed: 0,
                kept,
                bytes: 0,
                orphan: false,
            });
        }

        let guard = self
            .acquire_with_notice(&rs, addr, self.result_lock().write(addr, rs.ctoken()))
            .await?;

        let hashins = self
            .local_cache
            .list_target_entries(addr)
            .with_context(|| format!("gc: list entries for {addr}"))?;

        match decision {
            Decision::Orphan => {
                let mut bytes = 0u64;
                for hashin in &hashins {
                    bytes = bytes.saturating_add(
                        self.gc_entry(addr, hashin)
                            .with_context(|| format!("gc: drop orphan {addr}"))?,
                    );
                }
                Ok(TargetOutcome {
                    removed: hashins.len(),
                    kept: 0,
                    bytes,
                    orphan: true,
                })
            }
            Decision::Trim(history) => {
                let (removed, kept, bytes) =
                    self.trim_addr_history(&guard, addr, &hashins, history, None)?;
                Ok(TargetOutcome {
                    removed,
                    kept,
                    bytes,
                    orphan: false,
                })
            }
            // Skipped targets never reach phase 2 (filtered in gc_all).
            Decision::Skip => Ok(TargetOutcome::default()),
        }
    }

    /// Await one finished phase-2 task and fold it into `stats`. Per-target errors
    /// (and task panics) are logged and counted, never propagated, so the sweep
    /// continues. Emits one `GcTargetSwept` per target so the TUI's "targets
    /// explored" count advances even for failures.
    async fn drain_one(
        set: &mut JoinSet<(Addr, Result<TargetOutcome>)>,
        rs: &Arc<RequestState>,
        stats: &mut GcStats,
    ) {
        let Some(joined) = set.join_next().await else {
            return;
        };
        match joined {
            Ok((_, Ok(o))) => {
                stats.revisions_removed += o.removed;
                stats.revisions_kept += o.kept;
                stats.bytes_removed = stats.bytes_removed.saturating_add(o.bytes);
                if o.orphan {
                    stats.orphan_targets_removed += 1;
                }
                emit_gc_target_swept(rs, o.removed, o.bytes);
            }
            Ok((addr, Err(e))) => {
                tracing::warn!(%addr, error = %format!("{e:#}"), "gc: target failed, continuing");
                stats.errored += 1;
                emit_gc_target_swept(rs, 0, 0);
            }
            Err(join_err) => {
                // A panicked task must not take down the sweep.
                tracing::warn!(error = %join_err, "gc: target task panicked, continuing");
                stats.errored += 1;
            }
        }
    }
}

/// Fold one resolved decision into the list, tolerating a panicked resolve task
/// (logged, dropped — the target is simply left untouched in phase 2).
fn push_decision(
    decisions: &mut Vec<(Addr, Decision)>,
    joined: Option<std::result::Result<(Addr, Decision), tokio::task::JoinError>>,
) {
    match joined {
        Some(Ok(entry)) => decisions.push(entry),
        Some(Err(join_err)) => {
            tracing::warn!(error = %join_err, "gc: resolve task panicked, skipping target")
        }
        None => {}
    }
}

/// Emit one `GcTargetSwept` so the TUI's "targets explored" count advances.
fn emit_gc_target_swept(rs: &RequestState, revisions_removed: usize, bytes_removed: u64) {
    rs.emit(crate::engine::event::BuildEventKind::GcTargetSwept {
        revisions_removed,
        bytes_removed,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::local_cache::{
        MANIFEST_V1, Manifest, ManifestArtifact, ManifestArtifactContentType,
        ManifestArtifactEncoding, ManifestArtifactType,
    };
    use crate::hasync::StdCancellationToken;
    use crate::htpkg::PkgBuf;
    use std::collections::BTreeMap;
    use std::io::Write as _;

    fn test_engine() -> (Arc<Engine>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })
        .expect("engine");
        (Arc::new(engine), dir)
    }

    /// Engine whose durable cache spills any blob over `spill` bytes to the FS
    /// store, so GC tests can exercise reclaiming filesystem-spilled blobs
    /// without writing megabytes.
    fn test_engine_spill(spill: u64) -> (Arc<Engine>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            spill_threshold_bytes: spill,
            ..Default::default()
        })
        .expect("engine");
        (Arc::new(engine), dir)
    }

    fn addr(name: &str) -> Addr {
        Addr::new(PkgBuf::from("pkg"), name.to_string(), BTreeMap::new())
    }

    /// Write a cache revision with a controlled `created_at` and artifact set,
    /// so recency ordering is deterministic (real writes stamp wall-clock time).
    fn write_revision(
        engine: &Engine,
        addr: &Addr,
        hashin: &str,
        created: i64,
        artifacts: &[&str],
    ) {
        for name in artifacts {
            let mut w = engine
                .local_cache
                .writer(addr, hashin, name)
                .expect("writer");
            w.write_all(b"data").expect("write artifact");
            drop(w);
        }
        let manifest = Manifest {
            version: "1.0.0".to_string(),
            target: addr.format(),
            created_at_nanos: created,
            hashin: hashin.to_string(),
            artifacts: artifacts
                .iter()
                .map(|name| ManifestArtifact {
                    hashout: "ho".to_string(),
                    group: String::new(),
                    name: (*name).to_string(),
                    size: 4,
                    r#type: ManifestArtifactType::Output,
                    content_type: ManifestArtifactContentType::Tar,
                    encoding: ManifestArtifactEncoding::None,
                })
                .collect(),
        };
        let mut w = engine
            .local_cache
            .writer(addr, hashin, MANIFEST_V1)
            .expect("manifest writer");
        borsh::to_writer(&mut w, &manifest).expect("write manifest");
        drop(w);
        // Barrier: ensure the write landed before callers enumerate.
        assert!(
            engine
                .local_cache
                .exists(addr, hashin, MANIFEST_V1)
                .expect("exists")
        );
    }

    fn present(engine: &Engine, addr: &Addr, hashin: &str) -> bool {
        engine
            .local_cache
            .exists(addr, hashin, MANIFEST_V1)
            .expect("exists")
    }

    async fn wlock(engine: &Engine, addr: &Addr) -> ResultWriteGuard {
        engine
            .result_lock()
            .write(addr, &StdCancellationToken::new())
            .await
            .expect("write lock")
    }

    #[tokio::test]
    async fn trim_keeps_newest_and_deletes_artifacts() {
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["out_x.tar"]);
        write_revision(&engine, &a, "h2", 200, &["out_x.tar"]);
        write_revision(&engine, &a, "h3", 300, &["out_x.tar"]);

        let guard = wlock(&engine, &a).await;
        let hashins = engine.local_cache.list_target_entries(&a).expect("hashins");
        let (removed, kept, bytes) = engine
            .trim_addr_history(&guard, &a, &hashins, 1, None)
            .expect("trim");

        assert_eq!((removed, kept), (2, 1));
        // Each trimmed revision held one 4-byte artifact (manifest size).
        assert_eq!(bytes, 8);
        assert!(present(&engine, &a, "h3"), "newest revision kept");
        assert!(!present(&engine, &a, "h1"));
        assert!(!present(&engine, &a, "h2"));
        // The trimmed revision's artifacts are deleted too, not just the manifest.
        assert!(
            !engine
                .local_cache
                .exists(&a, "h1", "out_x.tar")
                .expect("exists")
        );
    }

    #[tokio::test]
    async fn trim_history_two_keeps_two_newest() {
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);
        write_revision(&engine, &a, "h3", 300, &["o.tar"]);

        let guard = wlock(&engine, &a).await;
        let hashins = engine.local_cache.list_target_entries(&a).expect("hashins");
        let (removed, kept, _bytes) = engine
            .trim_addr_history(&guard, &a, &hashins, 2, None)
            .expect("trim");

        assert_eq!((removed, kept), (1, 2));
        assert!(present(&engine, &a, "h3"));
        assert!(present(&engine, &a, "h2"));
        assert!(!present(&engine, &a, "h1"), "oldest dropped");
    }

    #[tokio::test]
    async fn trim_protects_named_revision() {
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "old", 100, &["o.tar"]);
        write_revision(&engine, &a, "new", 300, &["o.tar"]);

        // keep=1 would normally drop "old", but protecting it keeps both.
        let guard = wlock(&engine, &a).await;
        let hashins = engine.local_cache.list_target_entries(&a).expect("hashins");
        let (removed, kept, _bytes) = engine
            .trim_addr_history(&guard, &a, &hashins, 1, Some("old"))
            .expect("trim");

        assert_eq!((removed, kept), (0, 2));
        assert!(present(&engine, &a, "new"));
        assert!(present(&engine, &a, "old"), "protected revision survives");
    }

    #[tokio::test]
    async fn try_trim_after_write_trims_when_lock_free() {
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);

        engine.try_trim_after_write(&a, 1, "h2");

        assert!(present(&engine, &a, "h2"), "just-written revision kept");
        assert!(!present(&engine, &a, "h1"), "stale revision trimmed");
    }

    #[tokio::test]
    async fn try_trim_after_write_skips_when_lock_held() {
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);

        // Hold the addr's write lock; the non-blocking trim must skip.
        let ctoken = StdCancellationToken::new();
        let _held = engine
            .result_lock()
            .write(&a, &ctoken)
            .await
            .expect("write lock");

        engine.try_trim_after_write(&a, 1, "h2");

        assert!(
            present(&engine, &a, "h1"),
            "contended lock → nothing trimmed"
        );
        assert!(present(&engine, &a, "h2"));
    }

    #[tokio::test]
    async fn gc_all_drops_orphan_target_entries() {
        let (engine, _dir) = test_engine();
        // No provider knows this addr → get_spec yields TargetNotFoundError.
        let a = addr("ghost");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);

        let rs = engine.new_state();
        let stats = Arc::clone(&engine).gc_all(rs).await.expect("gc_all");

        assert_eq!(stats.orphan_targets_removed, 1);
        assert_eq!(stats.revisions_removed, 2);
        assert_eq!(stats.revisions_kept, 0);
        // Two revisions × one 4-byte artifact each.
        assert_eq!(stats.bytes_removed, 8);
        assert!(!present(&engine, &a, "h1"));
        assert!(!present(&engine, &a, "h2"));
    }

    #[tokio::test]
    async fn gc_all_processes_many_targets_concurrently() {
        let (engine, _dir) = test_engine();
        for i in 0..8 {
            let a = addr(&format!("ghost{i}"));
            write_revision(&engine, &a, "h1", 100, &["o.tar"]);
            write_revision(&engine, &a, "h2", 200, &["o.tar"]);
        }

        let rs = engine.new_state();
        let stats = Arc::clone(&engine).gc_all(rs).await.expect("gc_all");

        // Every target is visited and reclaimed regardless of fan-out order.
        assert_eq!(stats.orphan_targets_removed, 8);
        assert_eq!(stats.revisions_removed, 16);
        assert_eq!(stats.errored, 0);
    }

    #[tokio::test]
    async fn gc_all_keeps_going_when_a_target_fails() {
        let (engine, _dir) = test_engine();
        // Healthy orphan: must still be reclaimed.
        let good = addr("good");
        write_revision(&engine, &good, "h1", 100, &["o.tar"]);

        // Broken orphan: a corrupt manifest makes its deletion fail. The sweep
        // must record the error and carry on, not abort.
        let bad = addr("bad");
        {
            let mut w = engine
                .local_cache
                .writer(&bad, "h1", MANIFEST_V1)
                .expect("writer");
            w.write_all(b"not a valid borsh manifest").expect("write");
            drop(w);
            assert!(
                engine
                    .local_cache
                    .exists(&bad, "h1", MANIFEST_V1)
                    .expect("exists")
            );
        }

        let rs = engine.new_state();
        let stats = Arc::clone(&engine)
            .gc_all(rs)
            .await
            .expect("gc_all completes despite a failing target");

        assert!(
            stats.errored >= 1,
            "expected the bad target counted: {stats:?}"
        );
        assert!(
            !present(&engine, &good, "h1"),
            "healthy orphan reclaimed despite the other target failing"
        );
    }

    /// End-to-end: a revision whose artifact spilled to the FS blob store is
    /// fully reclaimed by an orphan sweep — the large blob is no longer readable
    /// through the cache after GC, proving `delete` reached the FS backend.
    #[tokio::test]
    async fn gc_reclaims_fs_spilled_blob() {
        // Threshold 16B; the artifact below is well over it, so it spills to FS.
        let (engine, _dir) = test_engine_spill(16);
        let a = addr("ghost"); // unknown to any provider → orphan on sweep
        let big = vec![0xABu8; 4096];

        let name = "out_big.tar";
        {
            let mut w = engine.local_cache.writer(&a, "h1", name).expect("writer");
            w.write_all(&big).expect("write blob");
            drop(w);
        }
        let manifest = Manifest {
            version: "1.0.0".to_string(),
            target: a.format(),
            created_at_nanos: 100,
            hashin: "h1".to_string(),
            artifacts: vec![ManifestArtifact {
                hashout: "ho".to_string(),
                group: String::new(),
                name: name.to_string(),
                size: big.len() as u64,
                r#type: ManifestArtifactType::Output,
                content_type: ManifestArtifactContentType::Tar,
                encoding: ManifestArtifactEncoding::None,
            }],
        };
        {
            let mut w = engine
                .local_cache
                .writer(&a, "h1", MANIFEST_V1)
                .expect("manifest writer");
            borsh::to_writer(&mut w, &manifest).expect("write manifest");
            drop(w);
        }
        // Barrier + precondition: the spilled blob is present before GC.
        assert!(engine.local_cache.exists(&a, "h1", name).expect("exists"));

        let rs = engine.new_state();
        let stats = Arc::clone(&engine).gc_all(rs).await.expect("gc_all");

        assert_eq!(stats.orphan_targets_removed, 1);
        assert_eq!(stats.revisions_removed, 1);
        assert_eq!(stats.bytes_removed, big.len() as u64);
        // The FS-spilled blob and its manifest are gone from the cache.
        assert!(!engine.local_cache.exists(&a, "h1", name).expect("exists"));
        assert!(!present(&engine, &a, "h1"));
    }

    #[tokio::test]
    async fn gc_apply_skips_when_within_history() {
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);

        // 1 revision, history 2 → nothing to trim. The lock is never taken; the
        // revision is retained.
        let rs = engine.new_state();
        let out = Arc::clone(&engine)
            .gc_apply(rs, &a, Decision::Trim(2))
            .await
            .expect("apply");

        assert_eq!((out.removed, out.kept), (0, 1));
        assert!(present(&engine, &a, "h1"), "revision within budget kept");
    }

    #[tokio::test]
    async fn gc_apply_trims_when_over_history() {
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);
        write_revision(&engine, &a, "h3", 300, &["o.tar"]);

        // 3 revisions, history 1 → over budget, lock taken, trims to newest.
        let rs = engine.new_state();
        let out = Arc::clone(&engine)
            .gc_apply(rs, &a, Decision::Trim(1))
            .await
            .expect("apply");

        assert_eq!((out.removed, out.kept), (2, 1));
        assert!(present(&engine, &a, "h3"), "newest kept");
        assert!(!present(&engine, &a, "h1"));
    }
}
