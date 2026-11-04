//! `heph tool gc`'s sweep of `<home>/failures/` (see [`failure_slot`]).
//!
//! A slot is cleared when its target next resolves, so the slots that stay are
//! those of targets nobody builds again: deleted, renamed, or simply not
//! rebuilt. The sweep removes:
//!
//! - a slot whose address no longer resolves to a target — the same
//!   `get_spec` → [`TargetNotFoundError`] test [`Engine::gc_all`] drops cache
//!   revisions on. Any *other* resolution error keeps the slot: "can't resolve"
//!   is not "dead";
//! - a slot older than the max age, measured from its manifest's `failed_at`.
//!   An age of zero removes every slot;
//! - litter: a slot with no manifest, an unknown version, a manifest for
//!   another key, an unreadable directory, and leftover `.tmp-` directories of
//!   a fill or clear that never finished. Litter is removed only once it is
//!   older than [`LITTER_MIN_AGE`] by mtime, so a fill in flight — whose
//!   temporary directory is renamed into place whole — is never taken.
//!
//! A slot with a manifest is removed under its target's execute lock, taken
//! without waiting: a run holding it is about to clear or refill the slot, so
//! the slot is skipped. Under the lock the manifest is read again, and a slot a
//! run refilled since the scan is kept.
//!
//! Never fatal: every error is a `warn!` and a count in [`FailureGcStats`].
//!
//! [`failure_slot`]: crate::engine::failure_slot

use crate::engine::Engine;
use crate::engine::error::TargetNotFoundError;
use crate::engine::failure_slot::{TEMP_PREFIX, addr_key, read_any_manifest};
use crate::engine::request_state::RequestState;
use hcore::hmemoizer::downcast_chain_ref;
use hmodel::htaddr::{Addr, parse_addr};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::task::JoinSet;

/// How old litter (a `.tmp-` directory, a slot without a valid manifest) must
/// be before the sweep removes it. A fill builds its slot in a `.tmp-`
/// directory and renames it into place within milliseconds; a minute leaves a
/// wide margin for a slow disk.
pub const LITTER_MIN_AGE: Duration = Duration::from_secs(60);

/// What one sweep of `<home>/failures/` did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FailureGcStats {
    /// Slots and litter directories removed.
    pub removed: usize,
    /// Bytes they held.
    pub bytes_removed: u64,
    /// Slots of live, recent targets, and slots kept because their target
    /// could not be resolved.
    pub kept: usize,
    /// Slots skipped because a run of their target held its execute lock.
    pub locked: usize,
    /// Errors: listing `failures/`, resolving a target, taking a lock or
    /// removing a directory. Each is logged.
    pub errored: usize,
}

/// A slot with a readable manifest, found by the scan.
struct Slot {
    dir: PathBuf,
    addr: Addr,
    failed_at: u64,
}

/// What the scan found.
#[derive(Default)]
struct Scan {
    slots: Vec<Slot>,
    litter: Vec<PathBuf>,
    errored: usize,
}

impl Engine {
    /// Sweep `<home>/failures/`, removing what the module doc lists. Emits one
    /// [`GcFailureLogsSwept`](crate::engine::event::BuildEventKind::GcFailureLogsSwept)
    /// on `rs`.
    pub async fn gc_failure_slots(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        max_age: Duration,
    ) -> FailureGcStats {
        self.gc_failure_slots_at(rs, max_age, SystemTime::now())
            .await
    }

    /// [`gc_failure_slots`](Self::gc_failure_slots) as of `now`.
    pub(crate) async fn gc_failure_slots_at(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        max_age: Duration,
        now: SystemTime,
    ) -> FailureGcStats {
        let root = self.failure_slots.root().to_path_buf();
        let scan = hcore::blocking::run(move || scan(&root, now)).await;
        let mut stats = FailureGcStats {
            errored: scan.errored,
            ..FailureGcStats::default()
        };

        let now_ms = u64::try_from(
            now.duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        let max_age_ms = u64::try_from(max_age.as_millis()).unwrap_or(u64::MAX);
        let aged = |s: &Slot| now_ms.saturating_sub(s.failed_at) >= max_age_ms;
        let (mut doomed, recent): (Vec<Slot>, Vec<Slot>) = scan.slots.into_iter().partition(aged);

        // Only the recent ones need resolving: an aged slot goes whatever its
        // target's state.
        let (dead, kept, errored) = Arc::clone(&self).resolve_dead(rs, recent).await;
        doomed.extend(dead);
        stats.kept += kept;
        stats.errored += errored;

        let engine = Arc::clone(&self);
        let litter = scan.litter;
        let removed =
            hcore::blocking::run(move || engine.remove_failure_slots(litter, doomed, now)).await;
        stats.removed += removed.removed;
        stats.bytes_removed = stats.bytes_removed.saturating_add(removed.bytes_removed);
        stats.kept += removed.kept;
        stats.locked += removed.locked;
        stats.errored += removed.errored;

        rs.emit(crate::engine::event::BuildEventKind::GcFailureLogsSwept {
            removed: stats.removed,
            bytes_removed: stats.bytes_removed,
        });
        stats
    }

    /// Split `slots` into those whose target no longer resolves, and count the
    /// rest: `(dead, kept, errored)`. Resolved under one shared request state,
    /// as [`Engine::gc_all`]'s phase 1 does, dropped before this returns.
    async fn resolve_dead(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        slots: Vec<Slot>,
    ) -> (Vec<Slot>, usize, usize) {
        let mut dead = Vec::new();
        let (mut kept, mut errored) = (0, 0);
        if slots.is_empty() {
            return (dead, kept, errored);
        }
        let resolve_rs = self.new_state_full(
            false,
            rs.events_sender(),
            rs.bg_pending(),
            Self::DEFAULT_LOG_TAIL_LINES,
            None,
        );
        resolve_rs.suppress_deferred_trims();

        let limit = self.max_workers.max(1);
        let mut set: JoinSet<(Slot, Verdict)> = JoinSet::new();
        let mut fold = |joined: Option<Result<(Slot, Verdict), tokio::task::JoinError>>| {
            match joined {
                Some(Ok((slot, Verdict::Dead))) => dead.push(slot),
                Some(Ok((_, Verdict::Live))) => kept += 1,
                Some(Ok((_, Verdict::Unknown))) => {
                    kept += 1;
                    errored += 1;
                }
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "gc: failure slot resolve task panicked, keeping it");
                    errored += 1;
                }
                None => {}
            }
        };
        for slot in slots {
            while set.len() >= limit {
                fold(set.join_next().await);
            }
            let engine = Arc::clone(&self);
            let rrs = Arc::clone(&resolve_rs);
            set.spawn(async move {
                let verdict = match engine.get_spec(rrs, &slot.addr).await {
                    Ok(_) => Verdict::Live,
                    Err(e) if downcast_chain_ref::<TargetNotFoundError>(&e).is_some() => {
                        Verdict::Dead
                    }
                    Err(e) => {
                        tracing::warn!(
                            addr = %slot.addr,
                            error = %format!("{e:#}"),
                            "gc: resolving a failure slot's target failed, keeping it",
                        );
                        Verdict::Unknown
                    }
                };
                (slot, verdict)
            });
        }
        while !set.is_empty() {
            fold(set.join_next().await);
        }
        (dead, kept, errored)
    }

    /// Remove `litter` and each of `doomed` under its execute lock. Blocking.
    fn remove_failure_slots(
        &self,
        litter: Vec<PathBuf>,
        doomed: Vec<Slot>,
        now: SystemTime,
    ) -> FailureGcStats {
        let mut stats = FailureGcStats::default();
        let remove = |stats: &mut FailureGcStats, dir: &Path| match remove_dir(dir) {
            Ok(Some(bytes)) => {
                stats.removed += 1;
                stats.bytes_removed = stats.bytes_removed.saturating_add(bytes);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "gc: removing a failure slot");
                stats.errored += 1;
            }
        };
        for dir in &litter {
            // Re-checked: a fill may have renamed a fresh slot into this name
            // since the scan.
            if is_litter(dir, now) {
                remove(&mut stats, dir);
            }
        }
        for slot in doomed {
            let _guard = match self.result_lock().try_lock_execute(&slot.addr) {
                Ok(Some(g)) => g,
                Ok(None) => {
                    tracing::debug!(addr = %slot.addr, "gc: failure slot's target is running, kept");
                    stats.locked += 1;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(addr = %slot.addr, error = %format!("{e:#}"), "gc: locking a failure slot's target");
                    stats.errored += 1;
                    continue;
                }
            };
            // A run between the scan and the lock cleared or refilled it.
            match read_any_manifest(&slot.dir) {
                Some(m) if m.failed_at == slot.failed_at => remove(&mut stats, &slot.dir),
                Some(_) => stats.kept += 1,
                None => {}
            }
        }
        stats
    }
}

/// What resolving a slot's address said.
enum Verdict {
    Live,
    Dead,
    /// Resolution failed some other way: keep the slot.
    Unknown,
}

/// List `root`: slots with a readable manifest, and litter old enough to go.
fn scan(root: &Path, now: SystemTime) -> Scan {
    let mut scan = Scan::default();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return scan,
        Err(e) => {
            tracing::warn!(dir = %root.display(), error = %e, "gc: listing failure slots");
            scan.errored += 1;
            return scan;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(dir = %root.display(), error = %e, "gc: listing failure slots");
                scan.errored += 1;
                continue;
            }
        };
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            // Not a name heph writes.
            if is_litter(&path, now) {
                scan.litter.push(path);
            }
            continue;
        };
        if name.starts_with(TEMP_PREFIX) {
            if is_litter(&path, now) {
                scan.litter.push(path);
            }
            continue;
        }
        let slot = read_any_manifest(&path).and_then(|m| {
            let addr = parse_addr(&m.addr).ok()?;
            // A manifest under another key reads as absent for its address.
            (addr_key(&addr) == name).then_some(Slot {
                dir: path.clone(),
                addr,
                failed_at: m.failed_at,
            })
        });
        match slot {
            Some(s) => scan.slots.push(s),
            None if is_litter(&path, now) => scan.litter.push(path),
            None => {}
        }
    }
    scan
}

/// Whether `path` is old enough to be litter by its own mtime. An entry whose
/// mtime cannot be read is not.
fn is_litter(path: &Path, now: SystemTime) -> bool {
    std::fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .is_ok_and(|t| now.duration_since(t).unwrap_or_default() >= LITTER_MIN_AGE)
}

/// Remove `path` and return the bytes it held; `None` when it was already
/// gone. A directory heph cannot read (mode `000`) is made readable by its
/// owner first: it is heph's own.
fn remove_dir(path: &Path) -> io::Result<Option<u64>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        let bytes = meta.len();
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        };
    }
    let mut bytes = tree_size(path);
    let res = match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
            bytes = tree_size(path);
            std::fs::remove_dir_all(path)
        }
        res => res,
    };
    match res {
        Ok(()) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Bytes of the regular files under `dir`, best-effort. Slots are flat; this
/// recurses anyway for litter of unknown shape.
fn tree_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => tree_size(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .fold(0u64, u64::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::failure_slot::write_test_slot;
    use crate::engine::fault_provider::{FaultProvider, Faults};
    use hcore::hasync::StdCancellationToken;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn addr(s: &str) -> Addr {
        parse_addr(s).expect("addr")
    }

    /// An engine whose provider resolves `//live:t` and fails to resolve
    /// `//broken:t`; everything else is not found.
    fn engine() -> (Arc<Engine>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let _rt = crate::engine::test_rt_enter();
        let mut engine = Engine::new(Config {
            parallelism: None,
            ..Config::for_tests(dir.path())
        })
        .expect("engine");
        let provider = FaultProvider::new(
            vec![hbuiltins::pluginstatictarget::Target {
                addr: "//live:t".to_string(),
                driver: "bash".to_string(),
                ..Default::default()
            }],
            Faults {
                broken: vec![addr("//broken:t")],
                ..Default::default()
            },
        )
        .expect("provider");
        engine
            .register_provider(move |_| Box::new(provider))
            .expect("register");
        (Arc::new(engine), dir)
    }

    fn root(engine: &Engine) -> PathBuf {
        engine.failure_slots().root().to_path_buf()
    }

    fn ms(t: SystemTime) -> u64 {
        t.duration_since(UNIX_EPOCH).expect("epoch").as_millis() as u64
    }

    /// Set `path`'s mtime to `t`.
    fn set_mtime(path: &Path, t: SystemTime) {
        std::fs::File::open(path)
            .expect("open")
            .set_modified(t)
            .expect("set mtime");
    }

    async fn sweep(engine: &Arc<Engine>, max_age: Duration, now: SystemTime) -> FailureGcStats {
        let rs = engine.new_state();
        Arc::clone(engine)
            .gc_failure_slots_at(&rs, max_age, now)
            .await
    }

    #[tokio::test]
    async fn gc_removes_slot_of_dead_target() {
        let (engine, _dir) = engine();
        let now = SystemTime::now();
        let ghost = write_test_slot(&root(&engine), &addr("//ghost:t"), ms(now), b"boom");
        let stats = sweep(&engine, 7 * DAY, now).await;
        assert!(!ghost.exists(), "a slot whose target is gone is removed");
        assert_eq!(stats.removed, 1, "{stats:?}");
        assert!(stats.bytes_removed >= 4, "{stats:?}");
        assert_eq!(stats.errored, 0, "{stats:?}");
    }

    #[tokio::test]
    async fn gc_removes_aged_slot() {
        let (engine, _dir) = engine();
        let now = SystemTime::now();
        let max_age = 7 * DAY;
        // Exactly at the boundary goes; a millisecond younger stays.
        let at = write_test_slot(&root(&engine), &addr("//live:t"), ms(now - max_age), b"x");
        let stats = sweep(&engine, max_age, now).await;
        assert!(
            !at.exists(),
            "a slot exactly max_age old is removed: {stats:?}"
        );

        let young = write_test_slot(
            &root(&engine),
            &addr("//live:t"),
            ms(now - max_age) + 1,
            b"x",
        );
        let stats = sweep(&engine, max_age, now).await;
        assert!(
            young.exists(),
            "a slot younger than max_age stays: {stats:?}"
        );
        assert_eq!(stats.kept, 1, "{stats:?}");

        // `Duration::ZERO`: a slot that failed at `now` is zero old, and goes.
        let fresh = write_test_slot(&root(&engine), &addr("//live:u"), ms(now), b"x");
        let stats = sweep(&engine, Duration::ZERO, now).await;
        assert!(!young.exists(), "max age zero removes it: {stats:?}");
        assert!(!fresh.exists(), "even one zero old: {stats:?}");
    }

    #[tokio::test]
    async fn gc_keeps_live_recent_slot() {
        let (engine, _dir) = engine();
        let now = SystemTime::now();
        let live = write_test_slot(&root(&engine), &addr("//live:t"), ms(now), b"x");
        let stats = sweep(&engine, 7 * DAY, now).await;
        assert!(live.exists());
        assert_eq!(
            stats,
            FailureGcStats {
                kept: 1,
                ..Default::default()
            }
        );
    }

    #[tokio::test]
    async fn gc_removes_partial_and_temp_slots() {
        let (engine, _dir) = engine();
        let root = root(&engine);
        let now = SystemTime::now();
        let old = now - 2 * LITTER_MIN_AGE;

        let partial = root.join("partial-0000");
        std::fs::create_dir_all(&partial).expect("mkdir");
        std::fs::write(partial.join("log.txt"), b"x").expect("log");
        set_mtime(&partial, old);

        let old_tmp = root.join(format!("{TEMP_PREFIX}old"));
        std::fs::create_dir_all(&old_tmp).expect("mkdir");
        std::fs::write(old_tmp.join("log.txt"), b"xyz").expect("log");
        set_mtime(&old_tmp, old);

        let fresh_tmp = root.join(format!("{TEMP_PREFIX}fresh"));
        std::fs::create_dir_all(&fresh_tmp).expect("mkdir");

        let unreadable = root.join("unreadable-0000");
        std::fs::create_dir_all(&unreadable).expect("mkdir");
        std::fs::write(unreadable.join("log.txt"), b"x").expect("log");
        set_mtime(&unreadable, old);
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))
                .expect("chmod");
        }

        let stats = sweep(&engine, 7 * DAY, now).await;
        assert!(!partial.exists(), "no manifest: removed");
        assert!(!old_tmp.exists(), "an old temp dir: removed");
        assert!(!unreadable.exists(), "an unreadable dir: removed");
        assert!(fresh_tmp.exists(), "a fill in flight is never taken");
        assert_eq!(stats.removed, 3, "{stats:?}");
        assert_eq!(stats.errored, 0, "{stats:?}");
    }

    #[tokio::test]
    async fn gc_keeps_slots_on_provider_error() {
        let (engine, _dir) = engine();
        let now = SystemTime::now();
        let broken = write_test_slot(&root(&engine), &addr("//broken:t"), ms(now), b"x");
        let stats = sweep(&engine, 7 * DAY, now).await;
        assert!(broken.exists(), "can't resolve is not dead");
        assert_eq!(
            (stats.kept, stats.errored, stats.removed),
            (1, 1, 0),
            "{stats:?}"
        );
    }

    #[tokio::test]
    async fn gc_skips_slot_with_held_lock() {
        let (engine, _dir) = engine();
        let now = SystemTime::now();
        let a = addr("//ghost:t");
        let slot = write_test_slot(&root(&engine), &a, ms(now), b"x");
        let held = engine
            .result_lock()
            .lock_execute(&a, &StdCancellationToken::new())
            .await
            .expect("execute lock");
        let stats = sweep(&engine, Duration::ZERO, now).await;
        assert!(slot.exists(), "a running target's slot is left to the run");
        assert_eq!((stats.locked, stats.removed), (1, 0), "{stats:?}");

        drop(held);
        let stats = sweep(&engine, Duration::ZERO, now).await;
        assert!(!slot.exists(), "{stats:?}");
    }

    #[tokio::test]
    async fn gc_without_failures_dir() {
        let (engine, _dir) = engine();
        assert!(!root(&engine).exists());
        let stats = sweep(&engine, 7 * DAY, SystemTime::now()).await;
        assert_eq!(stats, FailureGcStats::default());
    }

    #[tokio::test]
    async fn gc_max_age_zero_removes_all() {
        let (engine, _dir) = engine();
        let now = SystemTime::now();
        let root = root(&engine);
        let slots = [
            write_test_slot(&root, &addr("//live:t"), ms(now), b"x"),
            write_test_slot(&root, &addr("//broken:t"), ms(now), b"x"),
            write_test_slot(&root, &addr("//ghost:t"), ms(now), b"x"),
            // Failed "in the future" (clock skew): its age is zero, and still goes.
            write_test_slot(&root, &addr("//live:u"), ms(now) + 10_000, b"x"),
        ];
        let stats = sweep(&engine, Duration::ZERO, now).await;
        for s in &slots {
            assert!(!s.exists(), "{} survived: {stats:?}", s.display());
        }
        assert_eq!(stats.removed, 4, "{stats:?}");
        assert_eq!(stats.errored, 0, "zero age never resolves: {stats:?}");
    }
}
