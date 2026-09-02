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

/// The directory a slot's contents live in, for one lineage.
///
/// Scope-structured, so a branch's work stays on that branch: two scopes of one
/// slot are two directories, and nothing a feature branch does can reach the one
/// `master` builds from. `sanitize_scope` is what keeps a branch name like
/// `feature/x` from silently nesting an extra level.
pub fn scope_dir(home: &Path, slot: &str, scope: &str) -> PathBuf {
    crate::engine::scratch_store::head_dir(home, slot, scope)
}

/// Remove audit directories belonging to processes that are gone.
///
/// Best-effort and never fatal: a leftover directory costs disk, and failing a
/// build over one would be worse than the disk. A live pid is left alone, which
/// is what lets two concurrent audits coexist.
fn sweep_dead_audit_dirs(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        // `kill(pid, 0)` asks "may I signal this?" without signalling. ESRCH
        // means no such process; EPERM means it exists and is someone else's,
        // which still counts as alive.
        // SAFETY: `kill` with signal 0 performs the permission check and
        // returns, sending nothing. It takes a plain `pid_t` and touches no
        // memory we own, so there is no pointer or lifetime obligation.
        let alive = unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !alive {
            drop(std::fs::remove_dir_all(entry.path()));
        }
    }
}

/// Resolve which lineage to use, and seed it if it is new.
///
/// The behaviour is *try this branch, then fall back* — the current scope first,
/// then each configured `restore_scopes` entry in order. Returns the directory to
/// mount, always inside the **current** scope: a fallback is a place to copy
/// *from*, never a place to write to, and that asymmetry is the whole of the
/// isolation. A PR build cannot advance `master`'s cache, and a broken experiment
/// on a branch cannot corrupt the one you go back to.
///
/// Seeding is the only copy in this design. It happens once per (slot, scope) —
/// on the first build after a branch switch — and is measured against a cold
/// rebuild, not against nothing. `scratch.seedOnFork: false` turns it off for a
/// large slot on a filesystem without reflink, at the cost of every new branch
/// starting cold.
async fn resolve_scope_dir(
    home: &Path,
    slot: &str,
    opts: &crate::engine::config::ScratchOptions,
    addr: &Addr,
) -> anyhow::Result<PathBuf> {
    let own = scope_dir(home, slot, &opts.scope);
    let exists = |p: PathBuf| async move {
        hcore::blocking::run(move || Ok::<bool, std::io::Error>(p.is_dir()))
            .await
            .unwrap_or(false)
    };

    if exists(own.clone()).await {
        return Ok(own);
    }

    // Cold in this lineage. Seed from the first fallback that has anything, so a
    // branch switch costs a copy rather than a rebuild.
    if opts.seed_on_fork {
        for fallback in &opts.restore_scopes {
            if *fallback == opts.scope {
                continue;
            }
            let from = scope_dir(home, slot, fallback);
            if !exists(from.clone()).await {
                continue;
            }
            let (src, dst) = (from.clone(), own.clone());
            let seeded = hcore::blocking::run(move || copy_tree(&src, &dst)).await;
            match seeded {
                Ok(()) => {
                    tracing::debug!(
                        %addr, slot, from = %from.display(), to = %own.display(),
                        "seeded scratch from fallback scope",
                    );
                    return Ok(own);
                }
                // A failed seed is a cold cache, never a failed build: by the
                // scratch contract, losing one is a slowdown and nothing more.
                // Leave no partial tree behind for the next run to mistake for a
                // warm one.
                Err(err) => {
                    tracing::debug!(
                        %addr, slot, error = %err,
                        "could not seed scratch from fallback scope; starting cold",
                    );
                    let dead = own.clone();
                    drop(hcore::blocking::run(move || std::fs::remove_dir_all(&dead)).await);
                }
            }
        }
    }
    Ok(own)
}

/// Recursively copy `src` to `dst`, following no symlinks out of the tree.
///
/// Deliberately plain: a reflink would make this near-free on APFS/btrfs and is
/// the obvious next step, but it is a per-platform optimization and this has to
/// be correct on every filesystem first. Symlinks are recreated as symlinks
/// rather than followed, so a slot that has acquired a link to somewhere else
/// does not silently duplicate that somewhere else into the new scope.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else if ft.is_symlink() {
            let target = std::fs::read_link(&from)?;
            std::os::unix::fs::symlink(target, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
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

    /// A throwaway directory standing in for a slot's head, for this run only.
    ///
    /// `--no-scratch` withholds *carried-over state*, not the cache itself: the
    /// target still gets a directory at the usual mount point, announced through
    /// the usual variable, locked the usual way. Only its contents start empty.
    ///
    /// Per **process**, not per target, so targets sharing a declaration still
    /// share a directory within the run and `access` still means what it says.
    /// Per process rather than per slot-lineage so a concurrent ordinary build
    /// cannot see it, and an audit cannot see that build's state.
    async fn scratch_audit_dir(self: &Arc<Self>, slot: &str) -> anyhow::Result<PathBuf> {
        let root = self
            .scratch_audit_ready
            .get_or_try_init(|| {
                let root = crate::engine::scratch_store::audit_root(&self.home);
                let mine = root.join(std::process::id().to_string());
                async move {
                    let m = mine.clone();
                    hcore::blocking::run(move || {
                        // A killed run cannot clean up after itself, so sweep
                        // any pid directory that is not a live process before
                        // creating ours.
                        sweep_dead_audit_dirs(&root);
                        std::fs::create_dir_all(&m)
                            .with_context(|| format!("create audit dir {}", m.display()))?;
                        anyhow::Ok(())
                    })
                    .await?;
                    anyhow::Ok(mine)
                }
            })
            .await?;
        Ok(root.join(slot).join("head"))
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

            // Resolved and created under the guard, so two processes racing a
            // cold lineage cannot both decide it is absent and seed it twice.
            // Both creation and seeding are idempotent, which is what makes the
            // shared-access case safe.
            //
            // The audit (`--no-scratch`) takes the other branch: everything is
            // set up exactly as normal — resolved, locked, created, mounted,
            // announced through the env var — and only the *carried-over state*
            // is withheld. That is what makes it an audit of the contract rather
            // than of the target's shell: "the same outputs from a cold cache",
            // not "survives having its cache ripped out".
            let dir = if self.cfg.scratch.enabled {
                resolve_scope_dir(&self.home, &slot, &self.cfg.scratch, &r.addr)
                    .await
                    .with_context(|| format!("resolve scratch lineage for {}", r.addr))?
            } else {
                self.scratch_audit_dir(&slot)
                    .await
                    .with_context(|| format!("audit scratch dir for {}", r.addr))?
            };
            let d = dir.clone();
            hcore::blocking::run(move || std::fs::create_dir_all(&d))
                .await
                .with_context(|| format!("create scratch dir {dir:?} for {}", r.addr))?;

            // Still cold after the local fallbacks? Try the remote lineage. This
            // is the CI case and the reason `remote` exists: a fresh runner is
            // cold in every lineage, so without it every job starts from nothing.
            //
            // Pull is automatic — and safe to be, because it is read-only, costs
            // one list plus one fetch, and every way it can fail degrades to a
            // cold build. Publishing is the opposite on all three counts and is
            // therefore a command (`heph tool scratch push`), never a side effect
            // of building.
            if r.def.remote && self.cfg.scratch.enabled {
                self.pull_if_cold(&slot, &dir, &r.addr).await;
            }

            // Record what this slot came from, so the store can describe itself
            // without resolving the graph (see `scratch_store`). Idempotent and
            // best-effort: it is a diagnostic, not part of the build.
            crate::engine::scratch_store::write_slot_meta(
                &self.home,
                &slot,
                &crate::engine::scratch_store::SlotMeta {
                    format: 1,
                    addr: r.addr.format(),
                    path: r.def.path.clone(),
                    env: r.def.env.clone(),
                    access: r.def.access.as_str().to_string(),
                    version: r.def.version.clone(),
                    remote: r.def.remote,
                },
            );

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

impl Engine {
    /// Seed a locally-cold lineage from the remote, if there is anything to seed
    /// from.
    ///
    /// Never fails a build. A remote that is down, a head that will not decode, a
    /// transfer that dies halfway — every one of them means "cold", which by the
    /// scratch contract is a slowdown and nothing more. A partial unpack is
    /// cleared rather than left for the next run to mistake for a warm cache.
    async fn pull_if_cold(&self, slot: &str, dir: &Path, addr: &Addr) {
        // Unreadable counts as cold: if the directory cannot be listed, nothing
        // useful is in it either way.
        let cold = std::fs::read_dir(dir).map_or(true, |mut d| d.next().is_none());
        if !cold {
            return;
        }
        let opts = &self.cfg.scratch;
        let Some(head) = self
            .scratch_remote_head(slot, &opts.scope, &opts.restore_scopes)
            .await
        else {
            return;
        };

        match self.scratch_pull(&head, dir).await {
            Ok(bytes) => {
                // A cache whose entries embed absolute paths restores fine at a
                // different path and is then inert — present, and useless, which
                // looks exactly like a hit. Naming both paths is what makes that
                // diagnosable rather than mysterious.
                if head.meta.produced_at != dir.to_string_lossy() {
                    tracing::debug!(
                        %addr, produced_at = %head.meta.produced_at, restored_at = %dir.display(),
                        "scratch snapshot restored at a different path than it was produced at; \
                         a path-sensitive cache will be inert",
                    );
                }
                tracing::debug!(
                    %addr, slot, scope = %head.meta.scope, generation = head.meta.generation, bytes,
                    "pulled scratch snapshot",
                );
                crate::engine::scratch_remote::write_local_meta(
                    &self.home,
                    slot,
                    &opts.scope,
                    &head.meta,
                );
            }
            Err(err) => {
                tracing::debug!(%addr, slot, error = %err, "could not pull scratch snapshot");
                let dead = dir.to_path_buf();
                drop(hcore::blocking::run(move || std::fs::remove_dir_all(&dead)).await);
                drop(std::fs::create_dir_all(dir));
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
mod scope_tests {
    use super::*;
    use crate::engine::config::ScratchOptions;
    use hmodel::htpkg::PkgBuf;

    fn addr() -> Addr {
        Addr::new(PkgBuf::from("build"), "c".to_string(), Default::default())
    }

    fn opts(scope: &str, fallbacks: &[&str], seed: bool) -> ScratchOptions {
        ScratchOptions {
            enabled: true,
            scope: scope.to_string(),
            restore_scopes: fallbacks.iter().map(|s| s.to_string()).collect(),
            seed_on_fork: seed,
        }
    }

    /// Two lineages of one slot are two directories. Without this, "branch
    /// isolation" is a word rather than a behaviour.
    #[test]
    fn scopes_are_separate_directories() {
        let home = Path::new("/h");
        assert_ne!(
            scope_dir(home, "abc", "master"),
            scope_dir(home, "abc", "feat")
        );
        assert_eq!(
            scope_dir(home, "abc", "master"),
            scope_dir(home, "abc", "master")
        );
    }

    /// A branch name with a `/` must not nest an extra level, or two branches
    /// collide the moment one is a path prefix of another.
    #[test]
    fn a_slash_in_a_branch_name_stays_one_component() {
        let d = scope_dir(Path::new("/h"), "abc", "feature/x");
        let comps: Vec<_> = d.components().collect();
        assert_eq!(
            comps.len(),
            6,
            "/h + scratch + abc + <scope> + head is 5 components plus root, got {d:?}"
        );
        assert!(!d.to_string_lossy().contains("feature/x"), "{d:?}");
    }

    /// The empty scope is still a directory name — it is the default (one shared
    /// lineage), so it cannot resolve to the slot dir itself.
    #[test]
    fn the_empty_scope_has_a_directory_of_its_own() {
        let d = scope_dir(Path::new("/h"), "abc", "");
        assert!(d.ends_with("head"));
        assert!(d.to_string_lossy().contains("/abc/_/"), "{d:?}");
    }

    #[tokio::test]
    async fn a_cold_lineage_with_no_fallback_resolves_to_its_own_empty_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let got = resolve_scope_dir(tmp.path(), "s", &opts("feat", &[], true), &addr())
            .await
            .expect("resolve");
        assert_eq!(got, scope_dir(tmp.path(), "s", "feat"));
    }

    /// The branch-switch story: `master` is warm, `feat` has never been built, so
    /// `feat` starts from a copy of `master` rather than from nothing.
    #[tokio::test]
    async fn a_new_scope_seeds_from_its_fallback() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let master = scope_dir(tmp.path(), "s", "master");
        std::fs::create_dir_all(master.join("sub")).expect("mkdir");
        std::fs::write(master.join("sub").join("f"), b"warm").expect("write");

        let got = resolve_scope_dir(tmp.path(), "s", &opts("feat", &["master"], true), &addr())
            .await
            .expect("resolve");

        assert_eq!(got, scope_dir(tmp.path(), "s", "feat"));
        assert_eq!(
            std::fs::read(got.join("sub").join("f")).expect("read seeded"),
            b"warm"
        );
    }

    /// Writes land in the branch's own lineage. A PR build must not be able to
    /// advance the cache its base builds from.
    #[tokio::test]
    async fn seeding_leaves_the_fallback_untouched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let master = scope_dir(tmp.path(), "s", "master");
        std::fs::create_dir_all(&master).expect("mkdir");
        std::fs::write(master.join("f"), b"base").expect("write");

        let got = resolve_scope_dir(tmp.path(), "s", &opts("feat", &["master"], true), &addr())
            .await
            .expect("resolve");
        std::fs::write(got.join("f"), b"branch work").expect("write");

        assert_eq!(std::fs::read(master.join("f")).expect("read"), b"base");
    }

    #[tokio::test]
    async fn seed_on_fork_off_starts_cold() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let master = scope_dir(tmp.path(), "s", "master");
        std::fs::create_dir_all(&master).expect("mkdir");
        std::fs::write(master.join("f"), b"warm").expect("write");

        let got = resolve_scope_dir(tmp.path(), "s", &opts("feat", &["master"], false), &addr())
            .await
            .expect("resolve");
        assert!(!got.join("f").exists(), "must not have seeded");
    }

    /// An already-warm lineage is used as-is; nothing re-seeds over live work.
    #[tokio::test]
    async fn a_warm_scope_is_never_reseeded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for (scope, body) in [("master", b"base".as_slice()), ("feat", b"mine".as_slice())] {
            let d = scope_dir(tmp.path(), "s", scope);
            std::fs::create_dir_all(&d).expect("mkdir");
            std::fs::write(d.join("f"), body).expect("write");
        }
        let got = resolve_scope_dir(tmp.path(), "s", &opts("feat", &["master"], true), &addr())
            .await
            .expect("resolve");
        assert_eq!(std::fs::read(got.join("f")).expect("read"), b"mine");
    }

    /// Fallbacks are ordered, so a three-level convention works without
    /// special-casing — and an entry naming a lineage that never existed is
    /// skipped rather than erroring.
    #[tokio::test]
    async fn fallbacks_are_tried_in_order_and_missing_ones_are_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let develop = scope_dir(tmp.path(), "s", "develop");
        std::fs::create_dir_all(&develop).expect("mkdir");
        std::fs::write(develop.join("f"), b"develop").expect("write");

        let got = resolve_scope_dir(
            tmp.path(),
            "s",
            &opts("feat", &["nonexistent", "develop", "master"], true),
            &addr(),
        )
        .await
        .expect("resolve");
        assert_eq!(std::fs::read(got.join("f")).expect("read"), b"develop");
    }

    /// A slot listing its own scope as a fallback must not try to seed from
    /// itself — that is a no-op at best and a self-copy at worst.
    #[tokio::test]
    async fn a_scope_does_not_seed_from_itself() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let got = resolve_scope_dir(tmp.path(), "s", &opts("feat", &["feat"], true), &addr())
            .await
            .expect("resolve");
        assert_eq!(got, scope_dir(tmp.path(), "s", "feat"));
    }

    #[test]
    fn copy_tree_recreates_symlinks_rather_than_following_them() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (src, dst) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(src.join("d")).expect("mkdir");
        std::fs::write(src.join("d").join("f"), b"x").expect("write");
        std::os::unix::fs::symlink("/somewhere/else", src.join("link")).expect("symlink");

        copy_tree(&src, &dst).expect("copy");

        assert_eq!(std::fs::read(dst.join("d").join("f")).expect("read"), b"x");
        let md = std::fs::symlink_metadata(dst.join("link")).expect("stat");
        assert!(md.file_type().is_symlink(), "must stay a symlink");
        assert_eq!(
            std::fs::read_link(dst.join("link")).expect("readlink"),
            Path::new("/somewhere/else")
        );
    }
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
