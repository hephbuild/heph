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
    /// `docs/SCRATCH.md` §6.1 for what is deliberately *absent*: `path`, `env`,
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
    Fs(KeyedRWLock<String, FRWLock>),
    /// In-process only. Tests, and anything that has opted out of file locking.
    Mem(KeyedRWLock<String, MemRWLock>),
}

/// An opaque RAII guard on a slot. Held for the target's execute and dropped
/// after; the concrete guard type is erased because a target may hold a mix of
/// read and write guards and this code only ever holds and drops them.
pub type ScratchGuard = Box<dyn std::any::Any + Send>;

impl ScratchLock {
    pub fn new(backend: LockBackend, dir: PathBuf) -> Self {
        match backend {
            LockBackend::Fs => Self::Fs(KeyedRWLock::new(move |slot: &String| {
                FRWLock::new(dir.join(format!("{slot}.scratch.lock")))
            })),
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
            (Self::Fs(l), Access::Shared) => Box::new(l.read(slot, ctoken).await?) as ScratchGuard,
            (Self::Fs(l), Access::Exclusive) => {
                Box::new(l.write(slot, ctoken).await?) as ScratchGuard
            }
            (Self::Mem(l), Access::Shared) => Box::new(l.read(slot, ctoken).await?) as ScratchGuard,
            (Self::Mem(l), Access::Exclusive) => {
                Box::new(l.write(slot, ctoken).await?) as ScratchGuard
            }
        })
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
        resolved: &[ResolvedScratch],
    ) -> anyhow::Result<(Vec<ScratchMount>, Vec<ScratchGuard>)> {
        if resolved.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        let mut ordered: Vec<(String, &ResolvedScratch)> =
            resolved.iter().map(|r| (r.slot(), r)).collect();
        ordered.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        let mut guards = Vec::with_capacity(ordered.len());
        let mut mounts = Vec::with_capacity(ordered.len());
        for (slot, r) in ordered {
            let guard = self
                .scratch_lock
                .acquire(slot.clone(), r.def.access, rs.ctoken())
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
}
