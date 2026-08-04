//! Targeted cache eviction — the `heph clean` command.
//!
//! Where [`Engine::gc_all`](crate::engine::Engine::gc_all) decides *for* the
//! user (drop what no longer resolves, trim the rest to `cache.history`),
//! `clean` does exactly what it is told: every cached revision of every target
//! the matcher selects is deleted, whether or not the target still resolves and
//! regardless of its history budget. It is the "I don't trust this cache entry,
//! throw it away" lever.
//!
//! Two consequences follow, and they are what make `clean` cheap where GC is
//! not:
//!
//! - **Nothing is resolved.** GC has to ask each provider whether a target still
//!   exists and each driver what its `cache.history` is — a full spec/def walk
//!   that can run Starlark and execute uncacheable deps. `clean` deletes
//!   everything it matches, so it needs neither answer and never leaves the
//!   cache. A cache entry for a target whose `BUILD` file has since been deleted
//!   is therefore still cleanable, which is precisely when it is wanted.
//! - **Selection is addr-only.** With no def in hand there is no label set and no
//!   output paths, so `label()` / `tree_output()` cannot be evaluated. Those are
//!   rejected up front by [`ensure_addr_only`] rather than silently matching
//!   nothing — a `clean` that quietly deleted zero entries is the worst possible
//!   outcome for a command whose whole job is to make a stale entry go away.
//!
//! Deletion is serialized through each addr's
//! [`ResultLock`](crate::engine::result_lock::ResultLock) write lock, exactly as
//! GC's phase 2 is, so `clean` can never pull a revision out from under a
//! concurrent reader or builder in this or another process.

use crate::engine::Engine;
use crate::engine::request_state::RequestState;
use anyhow::{Context, Result};
use hmodel::htaddr::{Addr, parse_addr};
use hmodel::htmatcher::{MatchResult, Matcher};
use std::sync::Arc;
use tokio::task::JoinSet;

/// Outcome of an [`Engine::clean`] run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CleanStats {
    /// Cached targets the matcher selected. Counts the targets *considered*,
    /// including any that turned out to have no revisions left to delete — a
    /// zero here is the "your matcher selected nothing" signal, which a caller
    /// wants to distinguish from "it matched, and there was nothing to free".
    pub targets_matched: usize,
    /// Matched targets that actually lost at least one revision.
    pub targets_cleaned: usize,
    /// Cache revisions deleted.
    pub revisions_removed: usize,
    /// Total bytes freed (summed from the manifests of the deleted revisions).
    pub bytes_removed: u64,
    /// Matched targets whose deletion failed. Each is logged and the run keeps
    /// going — one unwritable entry never aborts the rest.
    pub errored: usize,
}

/// What cleaning one target did.
#[derive(Debug, Default)]
struct TargetOutcome {
    removed: usize,
    bytes: u64,
}

/// Reject a matcher whose evaluation would need a target's spec or def.
///
/// `clean` sees only the addrs the local cache knows about, so
/// [`Matcher::Label`] and [`Matcher::TreeOutputTo`] — both of which answer
/// [`MatchResult::MatchShrug`] on an addr alone — cannot be decided here. They
/// are an error, not a silent non-match: the user asked for entries to be
/// deleted, and "0 removed" would read as "there was nothing to clean".
fn ensure_addr_only(m: &Matcher) -> Result<()> {
    match m {
        Matcher::Label(_) => anyhow::bail!(
            "`clean` selects by address or package only; `label(...)` needs each target's definition, which a cached entry does not carry"
        ),
        Matcher::TreeOutputTo(_) => anyhow::bail!(
            "`clean` selects by address or package only; `tree_output(...)` needs each target's definition, which a cached entry does not carry"
        ),
        Matcher::Or(terms) | Matcher::And(terms) => terms.iter().try_for_each(ensure_addr_only),
        Matcher::Not(inner) => ensure_addr_only(inner),
        Matcher::Addr(_) | Matcher::Package(_) | Matcher::PackagePrefix(_) => Ok(()),
    }
}

impl Engine {
    /// Delete every locally cached revision of every target `matcher` selects.
    ///
    /// `matcher` is evaluated against the addr alone (see [`ensure_addr_only`]);
    /// pass `//...` ([`Matcher::PackagePrefix`] of the root package) to clean the
    /// whole local cache.
    ///
    /// Only [`MatchResult::MatchYes`] selects. A `MatchShrug` is unreachable once
    /// `ensure_addr_only` has passed, and treating it as a non-match keeps this
    /// side of the invariant safe even if a new matcher variant lands: `clean`
    /// deletes, so an undecidable predicate must skip, never guess.
    ///
    /// Per-target failures are logged and counted in
    /// [`CleanStats::errored`] — the run never aborts partway and leaves the user
    /// wondering which half of their cache is gone.
    pub async fn clean(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        matcher: &Matcher,
    ) -> Result<CleanStats> {
        ensure_addr_only(matcher)?;

        let targets = self
            .local_cache
            .list_targets()
            .context("clean: listing cache targets")?;

        let limit = self.max_workers.max(1);
        let mut set: JoinSet<(Addr, Result<TargetOutcome>)> = JoinSet::new();
        let mut stats = CleanStats::default();

        for target in targets {
            let addr_key = match target {
                Ok(k) => k,
                Err(e) => {
                    // The stream failed mid-way. Clean what was already seen
                    // rather than throwing it away — a partial clean is still a
                    // clean, and the error is surfaced.
                    tracing::warn!(error = %format!("{e:#}"), "clean: listing targets failed mid-stream, cleaning what was seen");
                    break;
                }
            };
            let addr = match parse_addr(&addr_key) {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(addr = %addr_key, error = %format!("{e:#}"), "clean: skip unparseable cache addr");
                    continue;
                }
            };
            if matcher.matches_addr(&addr) != MatchResult::MatchYes {
                continue;
            }
            stats.targets_matched += 1;

            while set.len() >= limit {
                Self::drain_one_clean(&mut set, &rs, &mut stats).await;
            }
            let engine = Arc::clone(&self);
            let crs = rs.clone();
            set.spawn(async move {
                let out = engine.clean_addr(crs, &addr).await;
                (addr, out)
            });
        }
        while !set.is_empty() {
            Self::drain_one_clean(&mut set, &rs, &mut stats).await;
        }

        Ok(stats)
    }

    /// Delete every revision of one target, under its write lock.
    ///
    /// Cheap unlocked pre-check first, mirroring
    /// [`gc_apply`](Engine::gc_apply): a target with no entries has nothing to
    /// delete, so it never pays for the lock's file open/`unlink`/close. Skipping
    /// deletes nothing, so it needs no lock to be correct; the authoritative
    /// enumeration is re-taken under the lock below.
    async fn clean_addr(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> Result<TargetOutcome> {
        let pre = self
            .local_cache
            .list_target_entries(addr)
            .with_context(|| format!("clean: list entries for {addr}"))?;
        if pre.is_empty() {
            return Ok(TargetOutcome::default());
        }

        let guard = self
            .acquire_with_notice(&rs, addr, self.result_lock().write(addr, rs.ctoken()))
            .await?;

        let hashins = self
            .local_cache
            .list_target_entries(addr)
            .with_context(|| format!("clean: list entries for {addr}"))?;
        let removed = hashins.len();

        // `LocalCache::delete` parks the calling thread until the sqlite writer
        // commits its batch, and `limit` of these run at once — parking that many
        // runtime workers would take the reactor, the timer wheel and the TUI with
        // them. Onto the blocking pool, with the write guard moved in so the lock
        // spans every delete it covers.
        let engine = Arc::clone(&self);
        let addr_owned = addr.clone();
        let bytes = hcore::blocking::run(move || {
            let _guard = guard;
            let mut bytes = 0u64;
            for hashin in &hashins {
                bytes = bytes.saturating_add(
                    engine
                        .gc_entry(&addr_owned, hashin)
                        .with_context(|| format!("clean: drop revision of {addr_owned}"))?,
                );
            }
            anyhow::Ok(bytes)
        })
        .await?;

        Ok(TargetOutcome { removed, bytes })
    }

    /// Await one finished clean task and fold it into `stats`. Per-target errors
    /// and task panics are logged and counted, never propagated. Emits one
    /// `GcTargetSwept` per target — including failures — so the TUI's explored
    /// count advances in step with the work.
    async fn drain_one_clean(
        set: &mut JoinSet<(Addr, Result<TargetOutcome>)>,
        rs: &Arc<RequestState>,
        stats: &mut CleanStats,
    ) {
        let Some(joined) = set.join_next().await else {
            return;
        };
        match joined {
            Ok((_, Ok(o))) => {
                if o.removed > 0 {
                    stats.targets_cleaned += 1;
                }
                stats.revisions_removed += o.removed;
                stats.bytes_removed = stats.bytes_removed.saturating_add(o.bytes);
                emit_clean_target_swept(rs, o.removed, o.bytes);
            }
            Ok((addr, Err(e))) => {
                tracing::warn!(%addr, error = %format!("{e:#}"), "clean: target failed, continuing");
                stats.errored += 1;
                emit_clean_target_swept(rs, 0, 0);
            }
            Err(join_err) => {
                tracing::warn!(error = %join_err, "clean: target task panicked, continuing");
                stats.errored += 1;
            }
        }
    }
}

/// Emit one `GcTargetSwept` so the TUI's "targets explored" count advances.
fn emit_clean_target_swept(rs: &RequestState, revisions_removed: usize, bytes_removed: u64) {
    rs.emit(crate::engine::event::BuildEventKind::GcTargetSwept {
        revisions_removed,
        bytes_removed,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::cache_test_support::{addr, addr_in, present, test_engine, write_revision};
    use hmodel::htpkg::PkgBuf;
    use std::time::Duration;

    /// `//...` — the no-argument selection.
    fn everything() -> Matcher {
        Matcher::PackagePrefix(PkgBuf::from(""))
    }

    #[tokio::test]
    async fn cleans_every_revision_of_the_selected_target() {
        // Not just the ones GC would trim: `clean` ignores `cache.history`
        // entirely, so even the newest revision goes.
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["out_x.tar"]);
        write_revision(&engine, &a, "h2", 200, &["out_x.tar"]);
        write_revision(&engine, &a, "h3", 300, &["out_x.tar"]);

        let rs = engine.new_state();
        let stats = Arc::clone(&engine)
            .clean(rs, &Matcher::Addr(a.clone()))
            .await
            .expect("clean");

        assert_eq!(stats.targets_matched, 1);
        assert_eq!(stats.targets_cleaned, 1);
        assert_eq!(stats.revisions_removed, 3);
        assert_eq!(stats.errored, 0);
        // Three revisions × one 4-byte artifact each.
        assert_eq!(stats.bytes_removed, 12);
        for h in ["h1", "h2", "h3"] {
            assert!(!present(&engine, &a, h), "{h} must be gone");
        }
        // The artifacts go with the manifest, not just the manifest key.
        assert!(
            !engine
                .local_cache
                .exists(&a, "h3", "out_x.tar")
                .expect("exists")
        );
    }

    #[tokio::test]
    async fn an_unmatched_target_is_left_alone() {
        let (engine, _dir) = test_engine();
        let victim = addr("victim");
        let bystander = addr("bystander");
        write_revision(&engine, &victim, "h1", 100, &["out.tar"]);
        write_revision(&engine, &bystander, "h1", 100, &["out.tar"]);

        let rs = engine.new_state();
        let stats = Arc::clone(&engine)
            .clean(rs, &Matcher::Addr(victim.clone()))
            .await
            .expect("clean");

        assert_eq!(stats.targets_matched, 1, "exactly one addr selected");
        assert_eq!(stats.revisions_removed, 1);
        assert!(!present(&engine, &victim, "h1"));
        assert!(present(&engine, &bystander, "h1"), "bystander survives");
    }

    #[tokio::test]
    async fn a_package_prefix_selects_the_whole_subtree() {
        let (engine, _dir) = test_engine();
        let inside = addr_in("cmd/server", "bin");
        let deeper = addr_in("cmd/server/sub", "lib");
        let outside = addr_in("cmdx", "bin");
        for a in [&inside, &deeper, &outside] {
            write_revision(&engine, a, "h1", 100, &["out.tar"]);
        }

        let rs = engine.new_state();
        let stats = Arc::clone(&engine)
            .clean(rs, &Matcher::PackagePrefix(PkgBuf::from("cmd")))
            .await
            .expect("clean");

        assert_eq!(stats.targets_matched, 2);
        assert_eq!(stats.revisions_removed, 2);
        assert!(!present(&engine, &inside, "h1"));
        assert!(!present(&engine, &deeper, "h1"));
        // `cmdx` shares a *string* prefix with `cmd` but not a path prefix.
        assert!(
            present(&engine, &outside, "h1"),
            "//cmdx is not under //cmd"
        );
    }

    #[tokio::test]
    async fn variants_of_one_target_are_distinct_addrs() {
        // Cache keys carry the variant args, and so does the matcher: cleaning
        // `//pkg:t` must not take `//pkg:t@variant=race` with it. A package
        // matcher is the way to clean every variant at once.
        let (engine, _dir) = test_engine();
        let plain = addr("t");
        let variant = Addr::new(
            PkgBuf::from("pkg"),
            "t".to_string(),
            [("variant".to_string(), "race".to_string())]
                .into_iter()
                .collect(),
        );
        write_revision(&engine, &plain, "h1", 100, &["out.tar"]);
        write_revision(&engine, &variant, "h1", 100, &["out.tar"]);

        let rs = engine.new_state();
        let stats = Arc::clone(&engine)
            .clean(rs, &Matcher::Addr(plain.clone()))
            .await
            .expect("clean");

        assert_eq!(stats.revisions_removed, 1);
        assert!(!present(&engine, &plain, "h1"));
        assert!(
            present(&engine, &variant, "h1"),
            "the variant is its own addr"
        );

        // …and the package matcher does take both.
        let rs = engine.new_state();
        let stats = Arc::clone(&engine)
            .clean(rs, &Matcher::Package(PkgBuf::from("pkg")))
            .await
            .expect("clean");
        assert_eq!(stats.revisions_removed, 1, "only the variant was left");
        assert!(!present(&engine, &variant, "h1"));
    }

    #[tokio::test]
    async fn no_argument_clears_the_whole_cache() {
        let (engine, _dir) = test_engine();
        let a = addr_in("a", "t");
        let b = addr_in("b/c", "t");
        write_revision(&engine, &a, "h1", 100, &["out.tar"]);
        write_revision(&engine, &a, "h2", 200, &["out.tar"]);
        write_revision(&engine, &b, "h1", 100, &["out.tar"]);

        let rs = engine.new_state();
        let stats = Arc::clone(&engine)
            .clean(rs, &everything())
            .await
            .expect("clean");

        assert_eq!(stats.targets_matched, 2);
        assert_eq!(stats.revisions_removed, 3);
        assert!(!present(&engine, &a, "h1"));
        assert!(!present(&engine, &a, "h2"));
        assert!(!present(&engine, &b, "h1"));
    }

    #[tokio::test]
    async fn is_idempotent() {
        // Cleaning an already-clean selection is a success that removed nothing
        // — the postcondition holds either way. It must not error, or every
        // scripted `heph clean` would have to special-case its second run.
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["out.tar"]);

        let rs = engine.new_state();
        Arc::clone(&engine)
            .clean(rs, &everything())
            .await
            .expect("first clean");
        let rs = engine.new_state();
        let stats = Arc::clone(&engine)
            .clean(rs, &everything())
            .await
            .expect("second clean");

        // The addr is still a known cache target (its rows are gone, the key is
        // not necessarily), so `targets_matched` may or may not count it — what
        // must hold is that nothing was deleted and nothing failed.
        assert_eq!(stats.revisions_removed, 0);
        assert_eq!(stats.targets_cleaned, 0);
        assert_eq!(stats.errored, 0);
    }

    #[tokio::test]
    async fn a_matcher_selecting_nothing_is_not_an_error() {
        // Distinguishable from "matched, freed nothing" by `targets_matched`,
        // which is what lets the CLI say "no cached entries match …" instead of
        // printing a zero-shaped success.
        let (engine, _dir) = test_engine();
        write_revision(&engine, &addr("t"), "h1", 100, &["out.tar"]);

        let rs = engine.new_state();
        let stats = Arc::clone(&engine)
            .clean(rs, &Matcher::Package(PkgBuf::from("nowhere")))
            .await
            .expect("clean");

        assert_eq!(stats.targets_matched, 0);
        assert_eq!(stats.revisions_removed, 0);
        assert!(present(&engine, &addr("t"), "h1"), "nothing was touched");
    }

    #[tokio::test]
    async fn a_matcher_needing_a_def_is_rejected_rather_than_matching_nothing() {
        let (engine, _dir) = test_engine();
        write_revision(&engine, &addr("t"), "h1", 100, &["out.tar"]);

        for m in [
            Matcher::Label("test".to_string()),
            Matcher::Not(Box::new(Matcher::Label("test".to_string()))),
            Matcher::And(vec![everything(), Matcher::TreeOutputTo(PkgBuf::from("g"))]),
        ] {
            let rs = engine.new_state();
            let err = Arc::clone(&engine)
                .clean(rs, &m)
                .await
                .expect_err("must be rejected");
            let chain = format!("{err:#}");
            assert!(
                chain.contains("address or package only"),
                "expected a selection error, got: {chain}"
            );
        }
        // Rejected before anything was deleted.
        assert!(present(&engine, &addr("t"), "h1"));
    }

    #[tokio::test]
    async fn a_nested_addr_only_matcher_is_accepted() {
        // The guard rejects by *node kind*, so a composite of allowed kinds must
        // still pass — otherwise `!//vendor/...`-shaped selections would be
        // rejected along with the label ones.
        let (engine, _dir) = test_engine();
        let keep = addr_in("vendor/x", "t");
        let drop = addr_in("app", "t");
        write_revision(&engine, &keep, "h1", 100, &["out.tar"]);
        write_revision(&engine, &drop, "h1", 100, &["out.tar"]);

        let m = Matcher::And(vec![
            everything(),
            Matcher::Not(Box::new(Matcher::PackagePrefix(PkgBuf::from("vendor")))),
        ]);
        let rs = engine.new_state();
        let stats = Arc::clone(&engine).clean(rs, &m).await.expect("clean");

        assert_eq!(stats.revisions_removed, 1);
        assert!(present(&engine, &keep, "h1"));
        assert!(!present(&engine, &drop, "h1"));
    }

    #[tokio::test]
    async fn deleting_waits_for_a_concurrent_holder_of_the_write_lock() {
        // `clean` deletes under the addr's write lock, so a revision another
        // request is reading or rebuilding cannot vanish mid-flight. Held here
        // by taking the lock first and releasing it only once `clean` is
        // demonstrably parked on it.
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["out.tar"]);

        let guard = crate::engine::cache_test_support::wlock(&engine, &a).await;
        let rs = engine.new_state();
        let cleaner = tokio::spawn({
            let engine = Arc::clone(&engine);
            let m = Matcher::Addr(a.clone());
            async move { engine.clean(rs, &m).await }
        });

        // While the lock is held the revision must survive. A sleep is the only
        // observation available (the block is the absence of an event); it
        // cannot produce a false failure — a `clean` that wrongly deleted
        // without the lock would have done so by now.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            present(&engine, &a, "h1"),
            "deleted while the lock was held"
        );

        drop(guard);
        let stats = cleaner.await.expect("join").expect("clean");
        assert_eq!(stats.revisions_removed, 1);
        assert!(!present(&engine, &a, "h1"));
    }
}
