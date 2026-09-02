//! Host-side handling of scratch references.
//!
//! A scratch cache is declared as a target (`driver = "scratch"`) and referenced
//! by addr from the targets that use it. The reference arrives as an [`Input`]
//! with `hashed: false, runtime: false`, marked by
//! [`SCRATCH_ANNOTATION`](hdriver_support::scratch::SCRATCH_ANNOTATION) — see that
//! module for why the settings travel on the declaration's spec rather than on
//! the edge.
//!
//! This module owns the part the engine must decide: **which declaration a
//! reference resolves to, and whether the set of them a target holds is
//! coherent**. Everything that gives a resolved reference an effect — the slot
//! store, mounting, locking, the lineage — builds on top of this.
//!
//! # Why validation is here and not in the driver
//!
//! A driver sees its own target's config and nothing else. The properties that
//! matter across a *set* of references — two of them wanting the same environment
//! variable, two of them mounting over each other — are only visible once each
//! addr has been resolved to a declaration, which is a host operation. Doing it
//! here also means every driver gets the checks, not just pluginexec.

use crate::engine::Engine;
use crate::engine::driver::targetdef::Input;
use crate::engine::request_state::RequestState;
use crate::engine::result_lock::LockBackend;
use anyhow::Context as _;
use hbuiltins::pluginscratch::{Access, DRIVER_NAME, ScratchDef, parse_declaration};
use hcore::hasync::Cancellable;
use hlock::hlock::{FRWLock, KeyedRWLock, MemRWLock};
use hmodel::htaddr::Addr;
use hplugin::driver::ScratchMount;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A reference resolved against its declaration: what the consuming target asked
/// for, plus everything the declaration says about the cache.
#[derive(Debug, Clone)]
pub struct ResolvedScratch {
    /// The declaring target.
    pub addr: Addr,
    /// The declaration's settings.
    pub def: ScratchDef,
}

impl ResolvedScratch {
    /// Stable id for this cache's storage slot.
    ///
    /// The identity is `(addr, version)` — see
    /// `docs/SCRATCH.md`, "Identity", for what is deliberately *absent*: `path`, `env`,
    /// `access`, `remote` and `max_size` are all policy about how a cache is used
    /// rather than what is in it, and changing one must not throw the contents
    /// away.
    ///
    /// `SLOT_FORMAT` is folded in so a layout change orphans old slots instead of
    /// misreading them. Bump it whenever the on-disk shape changes.
    pub fn slot(&self) -> String {
        use std::hash::{Hash as _, Hasher as _};
        /// On-disk layout version for a slot directory.
        const SLOT_FORMAT: u32 = 1;

        // Addr and `version`, and nothing else. heph contributes no dimension of
        // its own: what a cache's contents depend on is the author's statement,
        // not heph's guess. A closed enum over the host's os/arch — the obvious
        // alternative — cannot express a toolchain release, a target triple or a
        // set of build tags, and for a cross-compiled toolchain it would key on
        // the *host* while the contents depend on the *target*.
        //
        // `heph.core.os()` and `heph.core.arch()` are Starlark builtins, so a
        // host-specific cache says so in userland; an empty `version` means
        // portable.
        let mut h = xxhash_rust::xxh3::Xxh3Default::new();
        SLOT_FORMAT.hash(&mut h);
        self.addr.format().hash(&mut h);
        self.def.version.hash(&mut h);
        format!("{:016x}", h.finish())
    }
}

/// The per-slot cross-process lock guarding a scratch directory.
///
/// A [`KeyedRWLock`] rather than the transformable lock `result_lock` uses: a
/// scratch is only ever taken for read (`access = "shared"`) or write
/// (`access = "exclusive"`) and never upgraded mid-run, so the extra machinery
/// would buy nothing.
pub enum ScratchLock {
    /// `flock(2)` files under `<home>/lock/scratch/`. Mutually exclusive across
    /// processes on the same machine — which is the point, since two `heph`
    /// invocations share one slot directory.
    Fs {
        lock: KeyedRWLock<String, FRWLock>,
        /// The directory the per-slot lock files live in. Kept so
        /// [`holder_pid`](ScratchLock::holder_pid) can find a slot's file by
        /// path, the same way `ResultLock::Fs` keeps its own.
        dir: PathBuf,
    },
    /// In-process only. Tests, and anything that has opted out of file locking.
    Mem(KeyedRWLock<String, MemRWLock>),
}

/// Path of a slot's lock file. One definition, so the file `new` locks and the
/// file [`holder_pid`](ScratchLock::holder_pid) probes cannot drift apart.
fn slot_lock_path(dir: &Path, slot: &str) -> PathBuf {
    dir.join(format!("{slot}.scratch.lock"))
}

/// An opaque RAII guard on a slot. Held for the target's execute and dropped
/// after; the concrete guard type is erased because a target may hold a mix of
/// read and write guards and this code only ever holds and drops them.
pub type ScratchGuard = Box<dyn std::any::Any + Send>;

impl ScratchLock {
    pub fn new(backend: LockBackend, dir: PathBuf) -> Self {
        match backend {
            LockBackend::Fs => Self::Fs {
                lock: KeyedRWLock::new({
                    let dir = dir.clone();
                    move |slot: &String| FRWLock::new(slot_lock_path(&dir, slot))
                }),
                dir,
            },
            LockBackend::Mem => Self::Mem(KeyedRWLock::new(|_| MemRWLock::default())),
        }
    }

    /// Acquire the guard `access` calls for, waiting until available.
    async fn acquire(
        &self,
        slot: String,
        access: Access,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ScratchGuard> {
        Ok(match (self, access) {
            (Self::Fs { lock, .. }, Access::Shared) => {
                Box::new(lock.read(slot, ctoken).await?) as ScratchGuard
            }
            (Self::Fs { lock, .. }, Access::Exclusive) => {
                let guard = lock.write(slot, ctoken).await?;
                stamp_pid(guard.get());
                Box::new(guard) as ScratchGuard
            }
            (Self::Mem(l), Access::Shared) => Box::new(l.read(slot, ctoken).await?) as ScratchGuard,
            (Self::Mem(l), Access::Exclusive) => {
                Box::new(l.write(slot, ctoken).await?) as ScratchGuard
            }
        })
    }

    /// A process holding `slot` exclusively right now, when one can be named.
    ///
    /// Best-effort by nature, and in exactly the two ways the result lock's
    /// probe is: it is a snapshot (the holder may release the instant after),
    /// and a pid is a hint for a human, never something the engine acts on.
    ///
    /// `None` for a slot held by *readers*: `flock(2)` shared holders are not
    /// stamped and `FLock::is_path_held` reports them as unheld. That leaves the
    /// case worth naming — one `exclusive` consumer, very often in another
    /// `heph` process, blocking everyone else — which is the whole reason a
    /// waiter cannot answer this question by looking inward.
    fn holder_pid(&self, slot: &str) -> Option<u32> {
        match self {
            Self::Fs { dir, .. } => {
                let path = slot_lock_path(dir, slot);
                match hlock::hlock::FLock::is_path_held(&path) {
                    // Our own pid means this build is serialized against itself
                    // — many targets sharing one `exclusive` cache is the
                    // dominant shape. Reporting "held by pid <us>" would send
                    // the reader hunting a rogue process when the fix is the
                    // `access` on the declaration, so say nothing and let the
                    // renderer fall back to naming the access mode.
                    Ok(true) => crate::engine::result_lock::read_pid(&path)
                        .filter(|pid| *pid != std::process::id()),
                    Ok(false) => None,
                    Err(err) => {
                        // Without this the diagnostic path is itself
                        // undiagnosable — same reasoning as the result lock's.
                        tracing::debug!(error = %err, slot, "probing scratch lock liveness");
                        None
                    }
                }
            }
            // In-process only, so the holder is always this process and naming
            // it would tell a reader nothing they did not already know.
            Self::Mem(_) => None,
        }
    }
}

/// Stamp this process's pid into a slot's lock file, so a blocked waiter in
/// another process can name the holder.
///
/// Written through the held guard's already-open file description, and
/// newline-framed for the same reason the result lock frames its own: a shorter
/// pid written over a longer stale one would otherwise leave `<new><stale tail>`
/// — all digits, perfectly parseable, naming a process that is nobody. See
/// `result_lock::read_pid`, which is the reader for both.
///
/// Best-effort: a failed stamp costs a waiter the holder's name and nothing else.
fn stamp_pid(guard: &hlock::hlock::FWriteGuard) {
    if let Err(err) = guard.write_contents(format!("{}\n", std::process::id()).as_bytes()) {
        tracing::debug!(error = %err, "could not stamp pid into scratch lock");
    }
}

/// The directory a slot's contents live in.
pub fn slot_dir(home: &Path, slot: &str) -> PathBuf {
    home.join("scratch").join(slot)
}

/// True when this input is a scratch reference rather than an ordinary dep.
pub(crate) fn is_scratch_input(input: &Input) -> bool {
    hdriver_support::scratch::is_scratch(&input.annotations)
}

impl Engine {
    /// Resolve every scratch reference on a def, and reject an incoherent set.
    ///
    /// Returns them in the order the target declared them, which is the order the
    /// locks are *not* taken in — lock ordering is by addr, deliberately, so two
    /// targets referencing the same pair in different orders cannot deadlock.
    ///
    /// Cheap on the overwhelmingly common path: a target with no scratch
    /// references returns immediately without resolving anything, and one with
    /// references pays a `get_spec` each — memoized per request, and never a
    /// `get_def`, because a declaration's config is all that is needed.
    pub(crate) async fn resolve_scratch(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        consumer: &Addr,
        inputs: &[Input],
    ) -> anyhow::Result<Vec<ResolvedScratch>> {
        let refs: Vec<&Input> = inputs.iter().filter(|i| is_scratch_input(i)).collect();
        if refs.is_empty() {
            return Ok(Vec::new());
        }

        let mut resolved = Vec::with_capacity(refs.len());
        for input in refs {
            let addr = input.r#ref.r#ref.clone();
            let spec = Arc::clone(self)
                .get_spec(rs.clone(), &addr)
                .await
                .with_context(|| {
                    format!("{consumer} references scratch {addr}, which does not resolve")
                })?;

            // A reference to the wrong kind of target is a BUILD-file mistake
            // that would otherwise surface much later as a mount that does
            // nothing. Name both ends: the author is looking at the consumer, and
            // the problem is the thing it named.
            if spec.driver != DRIVER_NAME {
                anyhow::bail!(
                    "{consumer} lists {addr} under `scratch`, but {addr} is a `{}` target — \
                     `scratch` takes addresses of `{DRIVER_NAME}` targets, which declare a cache \
                     directory. Did you mean to put it in `deps`?",
                    spec.driver
                );
            }

            let def = parse_declaration(&spec)
                .with_context(|| format!("{consumer} references scratch {addr}"))?;
            resolved.push(ResolvedScratch { addr, def });
        }

        check_env_collisions(consumer, &resolved)?;
        check_mount_overlaps(consumer, &resolved)?;
        Ok(resolved)
    }

    /// Take every slot lock this target needs, create the directories, and return
    /// the mounts to hand the driver.
    ///
    /// # Lock ordering
    ///
    /// Guards are acquired in **sorted slot order**, not the order the target
    /// declared its references. Two targets naming the same pair in opposite
    /// orders would otherwise deadlock, each holding what the other waits for;
    /// sorting is the standard fix and costs one `sort_unstable` on a list that is
    /// essentially always ≤ 2.
    ///
    /// # Where this sits in `execute`
    ///
    /// **After dependency resolution and before the worker permit.** Both halves
    /// matter, and both mirror rules `execute` already follows:
    ///
    /// * After deps, because holding a slot across dep resolution has the same
    ///   shape as the diamond deadlock the worker permit is already ordered to
    ///   avoid — a dep needing the same slot could never get it.
    /// * Before the permit, like the approval gate, so a target queued on a
    ///   contended slot holds no worker. Since the lock is always taken first, no
    ///   permit holder is ever blocked on a slot, so a slot holder always
    ///   eventually gets a permit: the wait is bounded, not circular.
    pub(crate) async fn acquire_scratch(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        consumer: &Addr,
        resolved: &[ResolvedScratch],
    ) -> anyhow::Result<(Vec<ScratchMount>, Vec<ScratchGuard>)> {
        if resolved.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        let mut ordered: Vec<(String, &ResolvedScratch)> =
            resolved.iter().map(|r| (r.slot(), r)).collect();
        ordered.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        let consumer_str = consumer.format();
        let mut guards = Vec::with_capacity(ordered.len());
        let mut mounts = Vec::with_capacity(ordered.len());
        for (slot, r) in ordered {
            let guard = self
                .acquire_slot_with_notice(rs, &consumer_str, &slot, r)
                .await
                .with_context(|| format!("acquire scratch lock for {}", r.addr))?;
            guards.push(guard);

            // Created under the guard, so two processes racing a cold slot cannot
            // both decide it is absent and clobber each other's setup. Creation
            // is idempotent, which is what makes the shared-access case safe.
            let dir = slot_dir(&self.home, &slot);
            let d = dir.clone();
            hcore::blocking::run(move || std::fs::create_dir_all(&d))
                .await
                .with_context(|| format!("create scratch dir {dir:?} for {}", r.addr))?;

            mounts.push(ScratchMount {
                addr: r.addr.clone(),
                path: r.def.path.clone(),
                env: r.def.env.clone(),
                dir,
            });
        }
        Ok((mounts, guards))
    }
}

/// How long a scratch slot may be contended before the wait is announced.
///
/// The same five seconds the result lock uses, and for the same reason: below
/// it, contention is normal scheduling and a notice would be noise; above it,
/// the user is waiting and deserves to know what for.
#[cfg(not(test))]
const SCRATCH_LOCK_NOTICE: std::time::Duration = std::time::Duration::from_secs(5);

/// Shortened under `cfg(test)` so the contended path can be exercised without
/// putting a five-second sleep in a suite that runs on every push. The gate
/// itself — that an immediate acquire stays silent and a blocked one does not —
/// is what the tests assert; the threshold is a product decision, fixed above.
#[cfg(test)]
const SCRATCH_LOCK_NOTICE: std::time::Duration = std::time::Duration::from_millis(50);

impl Engine {
    /// Acquire one slot, announcing the wait if it outlasts
    /// [`SCRATCH_LOCK_NOTICE`]. The notice is informational; the wait continues
    /// until the slot is acquired or the request is cancelled.
    ///
    /// Gated on the threshold rather than emitted unconditionally because an
    /// uncontended acquire is the overwhelmingly common case and must stay
    /// silent — hundreds of targets sharing one cache would otherwise emit two
    /// events each saying only "nothing was wrong".
    async fn acquire_slot_with_notice(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        consumer: &str,
        slot: &str,
        r: &ResolvedScratch,
    ) -> anyhow::Result<ScratchGuard> {
        let lock_fut = self
            .scratch_lock
            .acquire(slot.to_string(), r.def.access, rs.ctoken());
        tokio::pin!(lock_fut);

        match tokio::time::timeout(SCRATCH_LOCK_NOTICE, &mut lock_fut).await {
            Ok(res) => res,
            Err(_elapsed) => {
                // Probed only once the wait is already known to be long, so the
                // common path never pays for the filesystem check.
                let holder_pid = self.scratch_lock.holder_pid(slot);
                let (addr, scratch) = (consumer.to_string(), r.addr.format());
                let end = (addr.clone(), scratch.clone());
                crate::engine::event::emit_scope(
                    rs,
                    crate::engine::event::BuildEventKind::ScratchLockWaitStart {
                        addr,
                        scratch,
                        access: r.def.access.as_str().to_string(),
                        holder_pid,
                    },
                    move |_| crate::engine::event::BuildEventKind::ScratchLockWaitEnd {
                        addr: end.0,
                        scratch: end.1,
                    },
                    async { (&mut lock_fut).await },
                )
                .await
            }
        }
    }
}

/// Reject two references that would claim the same environment variable.
///
/// The default name is derived from the target *name* alone (so packages do not
/// disambiguate it) through a deliberately lossy sanitizer, which makes
/// `//a:go-cache` and `//b:go.cache` collide — and an explicit `env` on two
/// declarations collides outright. Either way one of two real caches becomes
/// unreachable, silently, which is exactly the failure worth spending an error
/// on. The fix is on a *declaration*, not here, because the reference configures
/// nothing.
fn check_env_collisions(consumer: &Addr, resolved: &[ResolvedScratch]) -> anyhow::Result<()> {
    for (i, a) in resolved.iter().enumerate() {
        for b in resolved.iter().skip(i + 1) {
            if a.def.env == b.def.env {
                anyhow::bail!(
                    "{consumer} references two scratches that both set `{}`: {} and {}. One \
                     would shadow the other, so neither is safe to mount. Set an explicit `env` \
                     on one of those declarations.",
                    a.def.env,
                    a.addr,
                    b.addr
                );
            }
        }
    }
    Ok(())
}

/// Reject two references whose mount points overlap.
///
/// Mounting one scratch inside another means the outer one's directory contains a
/// symlink to the inner, so whatever writes the outer cache also writes into the
/// inner — two caches, one set of bytes, with lifetimes and eviction policies that
/// disagree. Equal paths are the degenerate case of the same thing.
fn check_mount_overlaps(consumer: &Addr, resolved: &[ResolvedScratch]) -> anyhow::Result<()> {
    for (i, a) in resolved.iter().enumerate() {
        for b in resolved.iter().skip(i + 1) {
            if hcore::paths::paths_overlap(&a.def.path, &b.def.path) {
                anyhow::bail!(
                    "{consumer} references two scratches whose mount points overlap: {} at {:?} \
                     and {} at {:?}. One cache would be written through the other; give them \
                     disjoint paths.",
                    a.addr,
                    a.def.path,
                    b.addr,
                    b.def.path
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmodel::htpkg::PkgBuf;

    fn addr(name: &str) -> Addr {
        Addr::new(PkgBuf::from("build"), name.to_string(), Default::default())
    }

    fn res(name: &str, env: &str, path: &str) -> ResolvedScratch {
        ResolvedScratch {
            addr: addr(name),
            def: ScratchDef {
                path: path.to_string(),
                env: env.to_string(),
                access: hbuiltins::pluginscratch::Access::Exclusive,
                version: String::new(),
                remote: false,
                max_size: None,
            },
        }
    }

    #[test]
    fn distinct_envs_and_paths_are_fine() {
        let r = [res("a", "A", "ca"), res("b", "B", "cb")];
        check_env_collisions(&addr("c"), &r).expect("no env collision");
        check_mount_overlaps(&addr("c"), &r).expect("no overlap");
    }

    #[test]
    fn two_scratches_claiming_one_variable_name_both_declarations() {
        let r = [res("a", "GOCACHE", "ca"), res("b", "GOCACHE", "cb")];
        let err = check_env_collisions(&addr("c"), &r).expect_err("collision must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("GOCACHE"), "{msg}");
        assert!(
            msg.contains("//build:a") && msg.contains("//build:b"),
            "{msg}"
        );
        // The fix is on a declaration; say so, because the author is looking at
        // the consumer and there is nothing to change there.
        assert!(msg.contains("declarations"), "{msg}");
    }

    #[test]
    fn a_mount_nested_inside_another_is_rejected() {
        let r = [res("a", "A", ".cache"), res("b", "B", ".cache/go")];
        let err = check_mount_overlaps(&addr("c"), &r).expect_err("nesting must fail");
        assert!(format!("{err:#}").contains("overlap"));
    }

    #[test]
    fn identical_mounts_are_rejected_however_they_are_spelled() {
        for (x, y) in [("c", "c"), ("c", "./c"), ("c/", "c"), ("a/b", "a/./b")] {
            let r = [res("a", "A", x), res("b", "B", y)];
            check_mount_overlaps(&addr("c"), &r)
                .expect_err(&format!("{x:?} vs {y:?} must collide"));
        }
    }

    /// A string `starts_with` would call these an overlap and emit an error the
    /// author cannot act on, because there is nothing wrong.
    #[test]
    fn a_shared_name_prefix_is_not_an_overlap() {
        for (x, y) in [
            (".cache/go", ".cache/golang"),
            ("build", "buildkit"),
            ("a/bc", "a/bcd"),
        ] {
            let r = [res("a", "A", x), res("b", "B", y)];
            check_mount_overlaps(&addr("c"), &r)
                .unwrap_or_else(|e| panic!("{x:?} vs {y:?} must not collide: {e:#}"));
        }
    }

    #[test]
    fn sibling_paths_do_not_overlap() {
        assert!(!hcore::paths::paths_overlap("a/b", "a/c"));
        assert!(hcore::paths::paths_overlap("a", "a/b"));
        assert!(hcore::paths::paths_overlap("a/b", "a"));
    }

    /// Records every event a run emits, so a test can assert on the stream the
    /// TUI, the GHA hook and the stall watchdog all actually consume.
    #[derive(Default)]
    struct Rec {
        seen: parking_lot::Mutex<Vec<crate::engine::event::BuildEventKind>>,
    }

    impl crate::engine::hook::Hook for Rec {
        fn name(&self) -> String {
            "rec".into()
        }
        fn on_event(&self, ev: &hcore::events::BuildEvent) {
            self.seen.lock().push(ev.kind.clone());
        }
        fn on_close(&self) {}
    }

    /// An engine over `root` with a recording hook attached.
    fn recording_engine(root: &Path) -> (Arc<Engine>, Arc<Rec>) {
        let mut e = Engine::new(crate::engine::Config {
            root: root.to_path_buf(),
            home_dir: PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })
        .expect("engine");
        let rec = Arc::new(Rec::default());
        e.register_hook(Arc::clone(&rec) as Arc<dyn crate::engine::hook::Hook>)
            .expect("register hook");
        (Arc::new(e), rec)
    }

    /// An uncontended acquire emits nothing.
    ///
    /// The notice is threshold-gated because the quiet case is overwhelmingly
    /// the common one: hundreds of targets sharing a cache would otherwise emit
    /// two events each saying "nothing was wrong".
    #[tokio::test]
    async fn an_uncontended_slot_emits_no_wait_notice() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _rt = crate::engine::test_rt_enter();
        let (engine, rec) = recording_engine(tmp.path());
        let rs = engine.new_state_with_events(true, None);
        let r = res("c", "C", ".cache/x");

        let guard = engine
            .acquire_slot_with_notice(&rs, "//app:a", &r.slot(), &r)
            .await
            .expect("acquire");
        drop(guard);

        assert!(
            !rec.seen.lock().iter().any(|k| matches!(
                k,
                crate::engine::event::BuildEventKind::ScratchLockWaitStart { .. }
            )),
            "an immediate acquire must stay silent",
        );
    }

    /// A slot that is genuinely held announces the wait, names the cache, and
    /// closes the span on acquire.
    ///
    /// This is the whole point: before it, a build serialized on a shared cache
    /// showed only an open `result` span — indistinguishable from a target
    /// queued for a worker — and neither the TUI nor the stall report could say
    /// what it was waiting for.
    #[tokio::test]
    async fn a_contended_slot_announces_the_wait_and_names_the_cache() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _rt = crate::engine::test_rt_enter();
        let (engine, rec) = recording_engine(tmp.path());
        let rs = engine.new_state_with_events(true, None);
        let r = res("c", "C", ".cache/x");
        let slot = r.slot();

        // Hold the slot, then race a second acquire past the notice threshold.
        let held = engine
            .acquire_slot_with_notice(&rs, "//app:holder", &slot, &r)
            .await
            .expect("first acquire");

        let waiter = {
            let (engine, rs, r, slot) = (
                Arc::clone(&engine),
                Arc::clone(&rs),
                r.clone(),
                slot.clone(),
            );
            tokio::spawn(async move {
                engine
                    .acquire_slot_with_notice(&rs, "//app:waiter", &slot, &r)
                    .await
                    .map(drop)
            })
        };

        tokio::time::sleep(SCRATCH_LOCK_NOTICE * 4).await;
        drop(held);
        waiter.await.expect("join").expect("second acquire");

        let seen = rec.seen.lock();
        assert!(
            seen.iter().any(|k| matches!(
                k,
                crate::engine::event::BuildEventKind::ScratchLockWaitStart {
                    addr, scratch, access, ..
                } if addr == "//app:waiter"
                    && scratch == &r.addr.format()
                    && access == "exclusive"
            )),
            "the blocked waiter must announce itself: {seen:?}",
        );
        assert!(
            seen.iter().any(|k| matches!(
                k,
                crate::engine::event::BuildEventKind::ScratchLockWaitEnd { addr, .. }
                    if addr == "//app:waiter"
            )),
            "the span must close on acquire, or the row never clears: {seen:?}",
        );
    }

    /// Contention *within* one process reports no holder pid.
    ///
    /// The stamp is this process's own, so naming it would point the reader at a
    /// rogue process when the fix is the `access` on the declaration. The
    /// renderer falls back to naming the access mode instead.
    #[tokio::test]
    async fn self_contention_names_no_holder_process() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _rt = crate::engine::test_rt_enter();
        let (engine, _rec) = recording_engine(tmp.path());
        let rs = engine.new_state_with_events(true, None);
        let r = res("c", "C", ".cache/x");
        let slot = r.slot();

        let held = engine
            .acquire_slot_with_notice(&rs, "//app:holder", &slot, &r)
            .await
            .expect("acquire");
        assert_eq!(
            engine.scratch_lock.holder_pid(&slot),
            None,
            "our own pid is not a useful holder to report",
        );
        drop(held);
    }
}
