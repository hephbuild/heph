//! A target's last failure, kept for the human reading the failure box.
//!
//! When a target's run fails, the engine copies the log the driver returned
//! (its `Log` failure artifact, [`hplugin::error::failure_log`]) into
//! `<home>/failures/<addr-key>/`: a `log.txt`, and a `manifest.json` written
//! **last** naming the address, the revision (none for a failure before the
//! run), when it failed and the fill's own id (P12). The failure box
//! renders the tail from the copy and points at it (`full log: <path>`).
//!
//! When the run kept `failure_out` matches (the managed layer packed them into
//! a tar in the sandbox, [`hplugin::error::failure_out_tar`]), the tar is
//! **unpacked** into the slot as `out/`, laid out by workspace-relative path,
//! and removed when the driver handed it over (`owned`). Only a regular file
//! inside the sandbox is read or removed. Only file members are unpacked,
//! never a link, and only at a plain relative path, so nothing lands outside
//! `out/`. `out/` is an additive sibling of
//! `log.txt` under the same version-1 manifest: a slot holds a log, an `out/`,
//! or both. A failed unpack of `out/` keeps the log slot and records why
//! ([`FailureSlot::out_error`]).
//!
//! When a failed `in_place` run's fixes were written back over the tree, the
//! slot also holds `fixes.txt` ([`FIXES`]): every path written, one per line,
//! workspace-relative. Another additive sibling under the same manifest.
//!
//! **Diagnostics only.** Nothing that hashes, selects content or stages into a
//! sandbox ever reads a slot, and a slot is never a cache entry: it is not in the
//! local cache, never uploaded, never part of any key. A slot that changes or
//! disappears changes what a human reads, and nothing else.
//!
//! - **One slot per address** (variant included), filled once per `(addr,
//!   hashin)` run: in `execute`'s failure arm, under the target's execute lock.
//! - **Copied, never moved.** The driver's file stays where it is, and the
//!   failed sandbox is removed only once the fill is done (or left on disk
//!   under `--keep-failed-sandboxes`). `std::fs::copy` reflinks
//!   where the filesystem can (`clonefile` on APFS, `copy_file_range` on btrfs and
//!   XFS). A log outside the sandbox — a symlink pointing out of it included — is
//!   not copied.
//! - **Atomic.** The slot is built in a `.tmp-` directory beside it and renamed
//!   into place, so a reader sees the old slot, the new one or none, never a
//!   partial one. A slot without a manifest, or with an unknown version, or
//!   naming another address, reads as absent.
//! - **Cleared when the target next resolves**: on a cache hit, under its result
//!   lock, and when its next run starts, under its execute lock. To keep a warm
//!   run free of a syscall per target, an engine lists `failures/` once and
//!   clears only the slots it saw or filled.
//! - **A slot for every fresh claim** ([`write_claim`]). The claim's
//!   compacted record is written as `diagnostic.json`: into the slot the run
//!   filled, under that fill's id, or into a slot of its own (manifest and
//!   `diagnostic.json`, no revision) for a failure that kept nothing — one
//!   before the run, or a source file that did not evaluate (keyed by
//!   [`file_key`]). In the background, counted on the request's `bg_pending`;
//!   a fill or a clear of the same slot waits for it. Not for credential
//!   material, nor for a record another request surfaced.
//! - **No log for credential material.** A target whose outputs are material
//!   gets its tail read into memory instead; its `failure_out` matches still
//!   get an `out/` (what a target lists there is its author's to judge).
//! - **A failed fill never hides the failure**: it warns, and the target's own
//!   error surfaces as it would have.

use crate::engine::driver::outputartifact::{Content, OutputArtifact, Type as ArtifactType};
use crate::engine::sandbox_cleaner::PendingCounter;
use hmodel::htaddr::Addr;
use hplugin::error::{FailureLog, LogTail};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The directory under the home that holds every slot.
pub(crate) const FAILURES_DIR: &str = "failures";
/// Name prefix of a slot being built or being discarded. A key never starts
/// with a dot, so no key is ever taken for one.
pub(crate) const TEMP_PREFIX: &str = ".tmp-";
/// The kept log, inside a slot.
pub use hcore::failure_log::LOG;
use hcore::failure_log::{DIAGNOSTIC, MANIFEST};
/// The kept `failure_out` matches, inside a slot, by workspace-relative path.
pub const OUT: &str = "out";
/// Every path a failed `in_place` run wrote back over the tree, one per line,
/// workspace-relative (D21): the full list past the failure box's truncation.
pub const FIXES: &str = "fixes.txt";
const MANIFEST_VERSION: u32 = 1;
/// Bytes of the readable part of a key.
const READABLE_MAX: usize = 48;

/// The directory name of `addr`'s slot: a short readable part for a human
/// browsing `failures/`, then a 128-bit digest of the address.
///
/// The digest makes the name injective (up to a collision of the digest, and a
/// reader checks the manifest's address anyway); the readable part is only
/// lowercase `[a-z0-9_]`, so the name is filesystem-safe, cannot be `.` or
/// `..`, never starts with a dot, never differs from another only by case, and
/// stays far under `NAME_MAX`. Flat: never nested by package.
///
/// Not [`Addr::hash_str`]: that feeds the package, name and arguments into
/// the hasher back to back with no separator, so `//a:bc` and `//ab:c` get
/// the same digest. The fields are length-prefixed here.
pub fn addr_key(addr: &Addr) -> String {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut field = |s: &str| {
        h.update(&(s.len() as u64).to_le_bytes());
        h.update(s.as_bytes());
    };
    field(addr.package.as_str());
    field(&addr.name);
    for (k, v) in &addr.args {
        field(k);
        field(v);
    }
    let digest = h.digest128();
    key_name(
        addr.package
            .as_str()
            .chars()
            .chain(std::iter::once(':'))
            .chain(addr.name.chars()),
        digest,
    )
}

/// The prefix of a source file's slot address in its manifest: the slot of the
/// file at workspace-relative `rel` names `file:<rel>`.
pub const FILE_ADDR_PREFIX: &str = "file:";

/// The directory name of the slot of the source file at workspace-relative
/// `rel`, which a provider reported as not evaluating: as [`addr_key`], over `file:<rel>`. Its
/// digest never meets an address's: an address's starts with a length, and
/// `file:` read as one is far past any name.
pub fn file_key(rel: &str) -> String {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(FILE_ADDR_PREFIX.as_bytes());
    h.update(rel.as_bytes());
    key_name(FILE_ADDR_PREFIX.chars().chain(rel.chars()), h.digest128())
}

/// A slot's directory name: the readable part `chars` gives, then `digest`.
fn key_name(chars: impl Iterator<Item = char>, digest: u128) -> String {
    let mut readable = String::with_capacity(READABLE_MAX);
    for c in chars {
        if readable.len() >= READABLE_MAX {
            break;
        }
        let c = c.to_ascii_lowercase();
        readable.push(if c.is_ascii_lowercase() || c.is_ascii_digit() {
            c
        } else {
            '_'
        });
    }
    format!("{readable}-{digest:032x}")
}

/// What a slot's `manifest.json` holds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SlotManifest {
    pub version: u32,
    /// The full address, variant included — the only place a reader takes it
    /// from; the directory name is not parsed.
    pub addr: String,
    /// The run's revision; `None` for a failure before the run, which has
    /// none, and for an invocation, which is never hashed.
    #[serde(default)]
    pub hashin: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub failed_at: u64,
    /// This fill's id, a uuid minted per fill (P12): what a record's log
    /// reference is compared against to tell the slot was since replaced.
    /// `None` in a slot from an older heph. Never hashed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_id: Option<String>,
    /// For a source file's slot (`addr` is `file:<path>`): the packages whose
    /// targets were reached through it, so `inspect failure //d:t` finds the
    /// file's slot when `//d:t` has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
}

/// The engine's record of a failure's log on the error chain: what
/// `classify_failure` reads the tail from.
///
/// Data, not a cause: its `Display` is empty, so every chain renderer skips it
/// (as for [`hplugin::error::FailureArtifacts`]).
#[derive(Debug, Clone, Default)]
pub struct FailureSlot {
    pub log: Option<FailureLog>,
    /// The slot's `out/`, when the run's `failure_out` matches were kept.
    pub out: Option<PathBuf>,
    /// What `out` holds at the target's package, grouped ([`kept_groups`]):
    /// counted once here, so no renderer walks the slot.
    pub kept: Vec<hcore::events::KeptGroup>,
    /// How many groups past `kept` were left out.
    pub kept_more: u32,
    /// Why the run's `failure_out` matches were not kept, when keeping them
    /// failed: the target's error carries it as one context line.
    pub out_error: Option<String>,
    /// What the run's `in_place` write-back on failure did, with the slot's
    /// `fixes.txt` filled in when it was kept.
    pub fixes: Option<hplugin::error::FailureFixes>,
    /// What becomes of the run's sandbox once the fill is done, for the
    /// failure box ([`SandboxFate`](hplugin::error::SandboxFate)). Set by the
    /// engine ([`with_sandbox`](Self::with_sandbox)), never by the fill. A
    /// failure after the run, while hashing its artifacts, fills no slot and
    /// carries a default one with only this set.
    pub sandbox: Option<hplugin::error::SandboxFate>,
    /// The run's output reached a terminal as it ran ([`TerminalUse`]): the
    /// log is kept all the same, and a renderer leaves its tail out of the
    /// box. Set by the engine ([`with_streamed`](Self::with_streamed)).
    ///
    /// [`TerminalUse`]: crate::engine::TerminalUse
    pub streamed: bool,
    /// The run itself went fine and a step past it failed (hashing its
    /// artifacts, checking the host paths it declared): the record is
    /// [`FailedIn::AfterRun`](hcore::failure::FailedIn::AfterRun), so a run
    /// that streamed still gets its box (D6.1c). Set by the engine
    /// ([`after_run`](Self::after_run)), never by the fill.
    pub after_run: bool,
    /// The id of the fill that put the slot in place, when one did: what the
    /// failure's record names its log by ([`hcore::failure::LogRef`]).
    pub fill_id: Option<String>,
    /// How many lines the slot's `log.txt` has, counted after the copy; 0
    /// with no log on disk.
    pub log_lines: u64,
    /// The target's outputs are credential material ([`Fill::material`]):
    /// the claim's `diagnostic.json` is not written for it.
    pub material: bool,
}

impl FailureSlot {
    /// The same slot, saying what becomes of the run's sandbox.
    #[must_use]
    pub fn with_sandbox(mut self, sandbox: Option<hplugin::error::SandboxFate>) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// The same slot, saying whether the run's output streamed to a terminal.
    #[must_use]
    pub fn with_streamed(mut self, streamed: bool) -> Self {
        self.streamed = streamed;
        self
    }

    /// The same slot, saying the failure came past a run that went fine.
    #[must_use]
    pub fn after_run(mut self) -> Self {
        self.after_run = true;
        self
    }
}

impl fmt::Display for FailureSlot {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

impl std::error::Error for FailureSlot {}

/// A failure after a run whose output reached a terminal, where no
/// [`FailureSlot`] rides the chain: the cache store, the codegen write-back
/// (D6.1c). Its own type, not an empty slot: a slot on the chain says
/// `execute` decided the run failed and kept it (`ErrorDetail::kept`), and
/// whether a failure is a stop must not depend on a TTY being attached.
/// Empty `Display`, like the slot.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunStreamed;

impl RunStreamed {
    /// `e`, marked as following a run that streamed when `streamed`.
    pub(crate) fn mark(e: anyhow::Error, streamed: bool) -> anyhow::Error {
        if streamed { e.context(Self) } else { e }
    }

    /// Whether `e`'s run streamed: its slot says so, or a [`RunStreamed`]
    /// marks it.
    pub(crate) fn on(e: &anyhow::Error) -> bool {
        hcore::hmemoizer::downcast_chain_ref::<FailureSlot>(e).is_some_and(|s| s.streamed)
            || hcore::hmemoizer::downcast_chain_ref::<Self>(e).is_some()
    }
}

impl fmt::Display for RunStreamed {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

impl std::error::Error for RunStreamed {}

/// The bytes of the log to keep.
enum LogSource {
    File(PathBuf),
    Bytes(Vec<u8>),
    /// A run's live log, read through the descriptor its writer opened, so
    /// it is this run's bytes whatever happens to the path.
    Fd(std::fs::File),
}

/// One failed run, as [`FailureSlots::fill`] needs it.
pub(crate) struct Fill<'a> {
    pub addr: &'a Addr,
    pub hashin: &'a str,
    /// Where the driver's log may be: the run's sandbox, and its FUSE
    /// counterparts when the sandbox was redirected. A log anywhere else is
    /// not read.
    pub roots: Vec<PathBuf>,
    pub err: &'a anyhow::Error,
    pub lines: usize,
    /// The target's outputs are credential material: the log's tail stays in
    /// memory and the slot gets `out/` only.
    pub material: bool,
    /// What the run's `in_place` write-back on failure did. Written paths
    /// become the slot's `fixes.txt`.
    pub fixes: Option<hplugin::error::FailureFixes>,
    /// Keep the failure's `Log` artifact. False for an entrypoint's
    /// invocation, which is never logged (R22): its slot holds `out/` only,
    /// whatever its driver attached.
    pub keep_log: bool,
    /// What the run's record is scrubbed of. A material run's memory tail
    /// is scrubbed of it *before* it is cut to the record's cap (H-M1): a
    /// cut first could leave a secret's suffix at the tail's head, where no
    /// later scrub recognises it.
    pub scrub: crate::engine::failure_record::Scrub,
}

/// The slots of one home, as one engine sees them.
pub struct FailureSlots {
    root: PathBuf,
    /// Slot names this engine knows exist: what `failures/` held when it was
    /// first listed, plus the slots filled since. Listed on first use.
    known: tokio::sync::OnceCell<std::sync::Mutex<HashSet<String>>>,
    /// Of the known slots, each source file's (`file:` manifests): the
    /// packages it lists. Filled with `known`; a target of one of those
    /// packages resolving means the file evaluated again, and its slot is
    /// cleared with the target's ([`clear`](Self::clear)).
    file_packages: std::sync::Mutex<HashMap<String, Vec<String>>>,
    /// Claim writes ([`write_claim`]) registered and not yet done.
    writes: PendingCounter,
    /// One exclusion per slot key, held by a fill, a claim write or a clear
    /// while it changes that slot: a clear or a fill never lands under a
    /// claim write registered before it. The entries no one holds are pruned
    /// when the map outgrows twice what it held after the last prune
    /// (FQ-1): amortized O(1) per lookup, where pruning on every lookup was
    /// O(n) each and O(n²) over a burst of failures.
    slot_locks: std::sync::Mutex<SlotLocks>,
    #[cfg(any(test, feature = "test-support"))]
    fills: std::sync::atomic::AtomicU64,
}

/// The slot exclusions ([`FailureSlots::slot_lock`]): one per key, the ones
/// no one holds pruned once the map doubles past its last pruned size.
#[derive(Default)]
struct SlotLocks {
    map: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// The size past which the next lookup prunes.
    prune_at: usize,
    /// How many entries the prunes walked, in all.
    #[cfg(test)]
    walked: usize,
}

impl SlotLocks {
    /// The fewest entries a prune waits for.
    const PRUNE_MIN: usize = 64;

    /// The exclusion of `key`, pruning first when the map is due.
    fn get(&mut self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        if self.map.len() >= self.prune_at.max(Self::PRUNE_MIN) {
            #[cfg(test)]
            {
                self.walked += self.map.len();
            }
            self.map.retain(|_, l| Arc::strong_count(l) > 1);
            self.prune_at = self.map.len().saturating_mul(2);
        }
        Arc::clone(self.map.entry(key.to_owned()).or_default())
    }
}

impl fmt::Debug for FailureSlots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FailureSlots")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl FailureSlots {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(FAILURES_DIR),
            known: tokio::sync::OnceCell::new(),
            file_packages: std::sync::Mutex::default(),
            writes: PendingCounter::default(),
            slot_locks: std::sync::Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            fills: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Resolve once every claim write registered so far is done. For tests:
    /// a claim's `diagnostic.json` lands in the background.
    pub async fn writes_drained(&self) {
        self.writes.wait_drained().await;
    }

    /// The exclusion of the slot `key`.
    fn slot_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.slot_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
    }

    /// Whether someone holds or awaits the exclusion of the slot `key`.
    fn slot_busy(&self, key: &str) -> bool {
        self.slot_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map
            .get(key)
            .is_some_and(|l| Arc::strong_count(l) > 1)
    }

    /// `<home>/failures`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory `addr`'s slot lives in, whether or not it exists.
    pub fn slot_dir(&self, addr: &Addr) -> PathBuf {
        self.root.join(addr_key(addr))
    }

    /// The manifest of `addr`'s slot, `None` when there is no complete slot
    /// for it (see [`read_manifest`]).
    pub fn read(&self, addr: &Addr) -> Option<SlotManifest> {
        read_manifest(&self.slot_dir(addr), addr)
    }

    /// The directory the slot of the source file at workspace-relative `rel`
    /// lives in, whether or not it exists ([`file_key`]).
    pub fn file_slot_dir(&self, rel: &str) -> PathBuf {
        self.root.join(file_key(rel))
    }

    /// The manifest of the slot of the source file at `rel`, `None` when there
    /// is no complete slot for it.
    pub fn read_file(&self, rel: &str) -> Option<SlotManifest> {
        read_any_manifest(&self.file_slot_dir(rel))
            .filter(|m| m.addr.strip_prefix(FILE_ADDR_PREFIX) == Some(rel))
    }

    /// The newest complete slot of a source file whose manifest lists
    /// `package`, with its directory: what `inspect failure //d:t` answers
    /// when `d/BUILD` did not evaluate and `//d:t` has no slot of its own.
    /// Lists `failures/`; for a command, not a build.
    pub fn read_file_of_package(&self, package: &str) -> Option<(PathBuf, SlotManifest)> {
        std::fs::read_dir(&self.root)
            .ok()?
            .filter_map(|e| {
                let name = e.ok()?.file_name().into_string().ok()?;
                name.starts_with("file_").then(|| self.root.join(name))
            })
            .filter_map(|dir| {
                let m = read_any_manifest(&dir)?;
                (m.addr.starts_with(FILE_ADDR_PREFIX) && m.packages.iter().any(|p| p == package))
                    .then_some((dir, m))
            })
            .max_by_key(|(_, m)| m.failed_at)
    }

    /// How many slots this engine has filled.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fills(&self) -> u64 {
        self.fills.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The slots this engine knows of, listing `failures/` on first use.
    async fn known(&self) -> &std::sync::Mutex<HashSet<String>> {
        self.known
            .get_or_init(|| async {
                let root = self.root.clone();
                let (slots, files) = hcore::blocking::run(move || list_slots(&root)).await;
                self.lock_file_packages().extend(files);
                std::sync::Mutex::new(slots)
            })
            .await
    }

    fn lock_file_packages(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<String>>> {
        self.file_packages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Take out of `known` (and the file map) every source file slot that
    /// lists `package`: its targets resolve, so the file evaluated again.
    fn take_files_of(&self, known: &mut HashSet<String>, package: &str) -> Vec<String> {
        let mut files = self.lock_file_packages();
        if files.is_empty() {
            return Vec::new();
        }
        let keys: Vec<String> = files
            .iter()
            .filter(|(_, pkgs)| pkgs.iter().any(|p| p == package))
            .map(|(k, _)| k.clone())
            .collect();
        for k in &keys {
            files.remove(k);
            known.remove(k);
        }
        keys
    }

    /// Remove `addr`'s slot, if this engine knows of one. The first call lists
    /// `failures/`; afterwards a target with no slot costs a hash and a set
    /// lookup, never a syscall. Best-effort: a failure to remove is logged.
    ///
    /// A claim write registered for the slot before this call
    /// ([`write_claim`]) lands first, and is then removed with it.
    ///
    /// The slot of each source file that lists `addr`'s package goes too: the
    /// target resolved, so its package's files evaluated (D5, "a target
    /// fixed and rebuilt green clears its slot").
    pub async fn clear(&self, addr: &Addr) {
        let known = self.known().await;
        let pending = self.writes.load(std::sync::atomic::Ordering::Acquire) > 0;
        let keys = {
            let mut set = known
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // The common case, a home with no failures: not even the key.
            if set.is_empty() && !pending {
                return;
            }
            let mut keys = self.take_files_of(&mut set, addr.package.as_str());
            let key = addr_key(addr);
            if set.remove(&key) || (pending && self.slot_busy(&key)) {
                keys.push(key);
            }
            keys
        };
        for key in keys {
            let _excl = self.slot_lock(&key).lock_owned().await;
            let dir = self.root.join(&key);
            let removed = hcore::blocking::run(move || match std::fs::remove_dir_all(&dir) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                res => res.map_err(|e| (dir, e)),
            })
            .await;
            if let Err((dir, e)) = removed {
                tracing::warn!(addr = %addr, dir = %dir.display(), error = %e, "clearing a failure slot");
            }
        }
    }

    /// Keep the failed run's log and return the context `classify_failure`
    /// reads it back from. Never fails: whatever goes wrong is logged, and the
    /// target's own error is what surfaces.
    pub(crate) async fn fill(&self, fill: Fill<'_>) -> FailureSlot {
        self.fill_with(fill, None, SlotIo::REAL).await
    }

    /// [`fill`](Self::fill) from the run's live log
    /// ([`crate::engine::run_log`]), read through its open descriptor: when
    /// the run had one, it is the log, and any `Log` the failure carries was
    /// already imported into it or dropped. An empty live log, or none, keeps
    /// an empty `log.txt` (RUN-19): the slot says the run printed nothing.
    pub(crate) async fn fill_live(
        &self,
        fill: Fill<'_>,
        live: Option<std::fs::File>,
    ) -> FailureSlot {
        self.fill_with(fill, Some(live), SlotIo::REAL).await
    }

    /// [`fill`](Self::fill), with the file operations injected. `live` is
    /// `Some` for a run that had a live log, and then replaces the failure's
    /// own `Log`.
    async fn fill_with(
        &self,
        fill: Fill<'_>,
        live: Option<Option<std::fs::File>>,
        ops: SlotIo,
    ) -> FailureSlot {
        let source = match live {
            // RUN-19: a run that printed nothing keeps an empty log. Material
            // keeps only a memory tail, and an empty one is no tail.
            Some(file) => file
                .filter(|f| f.metadata().is_ok_and(|m| m.len() > 0))
                .map(LogSource::Fd)
                .or_else(|| (!fill.material).then(|| LogSource::Bytes(Vec::new()))),
            None if fill.keep_log => select_log(fill.addr, fill.err),
            None => None,
        };
        let out = hplugin::error::failure_out_tar(fill.err).cloned();
        let mut fixes = fill.fixes;
        let written: Vec<String> = match &fixes {
            Some(hplugin::error::FailureFixes::Written { paths, .. }) => paths.clone(),
            _ => Vec::new(),
        };
        if source.is_none() && out.is_none() && written.is_empty() {
            return FailureSlot {
                fixes,
                material: fill.material,
                ..FailureSlot::default()
            };
        }
        let key = addr_key(fill.addr);
        let excl = self.slot_lock(&key).lock_owned().await;
        let manifest = SlotManifest {
            version: MANIFEST_VERSION,
            addr: fill.addr.format(),
            hashin: (!fill.hashin.is_empty()).then(|| fill.hashin.to_owned()),
            failed_at: crate::engine::event::now_unix_ms(),
            fill_id: Some(new_fill_id()),
            packages: Vec::new(),
        };
        let (root, lines, material, roots, scrub) = (
            self.root.clone(),
            fill.lines,
            fill.material,
            fill.roots,
            fill.scrub,
        );
        let addr = fill.addr.clone();
        let filled = hcore::blocking::run(move || {
            let source = source.and_then(|source| match source {
                LogSource::File(path) => match resolve_in_roots(&path, &roots) {
                    Ok(p) => p.map(LogSource::File),
                    Err(e) => {
                        tracing::warn!(addr = %addr, error = %e, "keeping the failure log");
                        None
                    }
                },
                other => Some(other),
            });
            let out = out.and_then(|tar| staged_out_tar(&addr, tar, &roots));
            let tail_of = |source: &LogSource| match memory_tail(source, lines) {
                Ok(mut t) => {
                    // Scrubbed whole, then cut (H-M1).
                    t.text = scrub.apply(&t.text);
                    let (tail, cut) = capped_tail(t);
                    Some(FailureLog::in_memory(tail, cut))
                }
                Err(e) => {
                    tracing::warn!(addr = %addr, error = %e, "reading the failure log's tail");
                    None
                }
            };
            // Material: the log never reaches the disk; `out/` still does.
            let (slot_log, memory) = match source {
                Some(s) if material => (None, tail_of(&s)),
                s => (s, None),
            };
            if slot_log.is_none() && out.is_none() && written.is_empty() {
                return Filled {
                    log: memory,
                    ..Filled::default()
                };
            }
            let built = fill_slot(
                &root,
                &key,
                &manifest,
                slot_log.as_ref(),
                out.as_ref(),
                &written,
                None,
                ops,
            );
            // The tar was packed for this fill alone: freed once read, whatever
            // became of the slot.
            if let Some(tar) = &out {
                free_owned_tar(&addr, tar);
            }
            match built {
                Ok(built) => {
                    if let Some(e) = &built.out_error {
                        tracing::warn!(addr = %addr, error = %format!("{e:#}"), "keeping failure_out");
                    }
                    Filled {
                        key: Some(key),
                        fill_id: manifest.fill_id,
                        log_lines: built.log.as_ref().map_or(0, |l| l.1),
                        log: built
                            .log
                            .map(|(path, _)| FailureLog::on_disk(path, lines))
                            .or(memory),
                        kept: built
                            .out
                            .as_deref()
                            .map(|out| kept_groups(out, addr.package.as_ref()))
                            .unwrap_or_default(),
                        out: built.out,
                        out_error: built.out_error.map(|e| format!("{e:#}")),
                        fixes: built.fixes,
                    }
                }
                Err(e) => {
                    tracing::warn!(addr = %addr, error = %format!("{e:#}"), "keeping the failure log");
                    Filled {
                        // The slot is lost, the tail need not be.
                        log: memory.or_else(|| slot_log.as_ref().and_then(tail_of)),
                        out_error: out.is_some().then(|| format!("{e:#}")),
                        ..Filled::default()
                    }
                }
            }
        })
        .await;
        if let Some(key) = filled.key {
            #[cfg(any(test, feature = "test-support"))]
            self.fills
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.known()
                .await
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key);
        }
        drop(excl);
        if let (Some(hplugin::error::FailureFixes::Written { list, .. }), Some(path)) =
            (fixes.as_mut(), filled.fixes)
        {
            *list = Some(path);
        }
        FailureSlot {
            log: filled.log,
            out: filled.out,
            kept: filled.kept.0,
            kept_more: filled.kept.1,
            out_error: filled.out_error,
            fixes,
            sandbox: None,
            streamed: false,
            after_run: false,
            fill_id: filled.fill_id,
            log_lines: filled.log_lines,
            material: fill.material,
        }
    }
}

/// What a fresh claim writes into the culprit's slot (P28): the claim's
/// record as `diagnostic.json`, beside the log the run kept, or in a slot of
/// its own for a failure that kept nothing (one before the run, a source
/// file that did not evaluate).
pub(crate) struct ClaimWrite {
    pub culprit: hplugin::error::Culprit,
    /// The claim's record, compacted and scrubbed.
    pub record: Arc<hcore::failure::FailureRecord>,
    /// The id of the fill that kept the run's log or `out/`: the slot the
    /// diagnostic joins. `None` builds a slot of its own.
    pub fill_id: Option<String>,
    /// For a file culprit: the packages its targets were reached through.
    pub packages: Vec<String>,
}

impl ClaimWrite {
    fn key(&self) -> String {
        match &self.culprit {
            hplugin::error::Culprit::Target(a) => addr_key(a),
            hplugin::error::Culprit::File(rel) => file_key(rel),
        }
    }
}

/// Counts a claim write on its counters until dropped, however its task
/// ends.
struct Registered([PendingCounter; 2]);

impl Registered {
    fn new(counters: [PendingCounter; 2]) -> Self {
        for c in &counters {
            c.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        Self(counters)
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        for c in &self.0 {
            c.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
}

/// Write `claim` into the failure slots of `engine`, in the background.
///
/// Registered here, synchronously, before the claim's winner returns: on
/// `bg_pending` (the process does not exit before it lands) and on the
/// slots' own count, with the slot's exclusion taken now when it is free. A
/// clear or a fill of the slot that comes later waits for it. With no
/// runtime it is written inline.
pub(crate) fn write_claim(
    engine: Arc<crate::engine::Engine>,
    bg_pending: PendingCounter,
    claim: ClaimWrite,
) {
    let key = claim.key();
    let file_packages =
        matches!(claim.culprit, hplugin::error::Culprit::File(_)).then(|| claim.packages.clone());
    let slots = &engine.failure_slots;
    let lock = slots.slot_lock(&key);
    let held = Arc::clone(&lock).try_lock_owned().ok();
    let registered = Registered::new([bg_pending, Arc::clone(&slots.writes)]);
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                let _registered = registered;
                let _excl = match held {
                    Some(g) => g,
                    None => lock.lock_owned().await,
                };
                let root = engine.failure_slots.root.clone();
                let (new_slot, key) =
                    hcore::blocking::run(move || (write_claim_logged(&root, &key, &claim), key))
                        .await;
                if new_slot {
                    let slots = &engine.failure_slots;
                    let known = slots.known().await;
                    if let Some(pkgs) = file_packages {
                        slots.lock_file_packages().insert(key.clone(), pkgs);
                    }
                    known
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(key);
                }
            });
        }
        Err(_) => {
            let _registered = registered;
            let _excl = held.unwrap_or_else(|| lock.blocking_lock_owned());
            if write_claim_logged(&slots.root, &key, &claim)
                && let Some(known) = slots.known.get()
            {
                if let Some(pkgs) = file_packages {
                    slots.lock_file_packages().insert(key.clone(), pkgs);
                }
                known
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(key);
            }
        }
    }
}

/// [`write_claim_files`], under the slot's exclusion. Never fails: it logs.
fn write_claim_logged(root: &Path, key: &str, claim: &ClaimWrite) -> bool {
    match write_claim_files(root, key, claim) {
        Ok(new_slot) => new_slot,
        Err(e) => {
            tracing::warn!(culprit = %claim.culprit, error = %format!("{e:#}"), "keeping the failure's diagnostic");
            false
        }
    }
}

/// [`write_claim`]'s files: into the slot the run filled, or a slot of its
/// own. True when it built a slot of its own.
fn write_claim_files(root: &Path, key: &str, claim: &ClaimWrite) -> anyhow::Result<bool> {
    use anyhow::Context as _;
    match &claim.fill_id {
        Some(id) => {
            let dir = root.join(key);
            // The run's slot, unless it was replaced or cleared since.
            match hcore::failure_log::peek_fill_id(&dir) {
                Ok(Some(found)) if found == *id => {}
                found => {
                    tracing::debug!(culprit = %claim.culprit, ?found, "the run's slot is gone or replaced; no diagnostic");
                    return Ok(false);
                }
            }
            let bytes = hcore::failure_log::encode_diagnostic(&claim.record, Some(id))
                .context("encoding the failure's diagnostic")?;
            let tmp = dir.join(format!(
                "{TEMP_PREFIX}diagnostic-{}",
                uuid::Uuid::new_v4().as_simple()
            ));
            let written = std::fs::write(&tmp, bytes)
                .with_context(|| format!("writing {}", tmp.display()))
                .and_then(|()| {
                    let to = dir.join(DIAGNOSTIC);
                    std::fs::rename(&tmp, &to)
                        .with_context(|| format!("renaming {} to {}", tmp.display(), to.display()))
                });
            match written {
                Ok(()) => Ok(false),
                Err(e) => {
                    drop(std::fs::remove_file(&tmp));
                    if e.downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
                    {
                        tracing::debug!(culprit = %claim.culprit, "the run's slot went while its diagnostic was written");
                        return Ok(false);
                    }
                    Err(e)
                }
            }
        }
        None => {
            let fill_id = new_fill_id();
            let bytes = hcore::failure_log::encode_diagnostic(&claim.record, Some(&fill_id))
                .context("encoding the failure's diagnostic")?;
            let manifest = SlotManifest {
                version: MANIFEST_VERSION,
                addr: match &claim.culprit {
                    hplugin::error::Culprit::Target(a) => a.format(),
                    hplugin::error::Culprit::File(rel) => format!("{FILE_ADDR_PREFIX}{rel}"),
                },
                hashin: None,
                failed_at: crate::engine::event::now_unix_ms(),
                fill_id: Some(fill_id),
                packages: claim.packages.clone(),
            };
            fill_slot(
                root,
                key,
                &manifest,
                None,
                None,
                &[],
                Some(&bytes),
                SlotIo::REAL,
            )?;
            Ok(true)
        }
    }
}

/// A new fill's id (P12).
fn new_fill_id() -> String {
    uuid::Uuid::new_v4().as_simple().to_string()
}

/// What a fill kept.
#[derive(Default)]
struct Filled {
    /// The slot's name, when one was renamed into place.
    key: Option<String>,
    /// The id of the fill that was renamed into place.
    fill_id: Option<String>,
    /// The lines of the slot's `log.txt`.
    log_lines: u64,
    log: Option<FailureLog>,
    out: Option<PathBuf>,
    kept: (Vec<hcore::events::KeptGroup>, u32),
    out_error: Option<String>,
    /// The slot's `fixes.txt`, when one was written.
    fixes: Option<PathBuf>,
}

/// The files under `out` (a slot's `out/`) at `package`, grouped by first
/// path component: a directory (`report/`) with how many files it holds, or
/// a top-level file alone. Sorted by name; the first
/// [`KEPT_GROUPS_CAP`](hcore::events::KEPT_GROUPS_CAP), and how many more.
/// `failure_out` is package-relative and the slot mirrors the workspace, so
/// `//web:lint`'s groups read `report/`, not `web/`. Symlinks are counted,
/// never followed; an unreadable directory reads as empty.
pub(crate) fn kept_groups(out: &Path, package: &str) -> (Vec<hcore::events::KeptGroup>, u32) {
    use hcore::events::{KEPT_GROUPS_CAP, KeptGroup};
    let Ok(entries) = std::fs::read_dir(out.join(package)) else {
        return (Vec::new(), 0);
    };
    let count_files = |dir: PathBuf| {
        let mut n = 0u64;
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                match entry.file_type() {
                    Ok(t) if t.is_dir() => stack.push(entry.path()),
                    Ok(_) => n += 1,
                    Err(_) => {}
                }
            }
        }
        n
    };
    let mut groups: Vec<KeptGroup> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                let files = count_files(entry.path());
                (files > 0).then(|| KeptGroup {
                    name: format!("{name}/"),
                    files,
                })
            } else {
                Some(KeptGroup { name, files: 1 })
            }
        })
        .collect();
    groups.sort_by(|a, b| a.name.cmp(&b.name));
    let more = groups.len().saturating_sub(KEPT_GROUPS_CAP);
    groups.truncate(KEPT_GROUPS_CAP);
    (groups, u32::try_from(more).unwrap_or(u32::MAX))
}

/// The file operations a fill performs, injectable for tests.
#[derive(Clone, Copy)]
struct SlotIo {
    /// Copy one file: `std::fs::copy`, which reflinks where it can.
    copy: fn(&Path, &Path) -> io::Result<()>,
    /// Unpack the `failure_out` tar into the new directory `to`.
    unpack: fn(&OutputArtifact, &Path) -> anyhow::Result<()>,
}

impl SlotIo {
    const REAL: Self = Self {
        copy: |from, to| std::fs::copy(from, to).map(drop),
        unpack: |tar, to| {
            use anyhow::Context as _;
            std::fs::create_dir(to).with_context(|| format!("creating {}", to.display()))?;
            // A member is a workspace-relative path, and a file: anything
            // else could land outside `out/` (`../x`, or a link `l -> ..`
            // followed by `l/x`). The managed layer packs a link as what it
            // resolves to, so a link member was never its.
            hcore::hartifactcontent::unpack::unpack_files_only(
                tar,
                to,
                Some(&|p: &Path| hdriver_support::failure_out::is_plain_relative(p)),
            )
        },
    };
}

/// The `failure_out` tar `tar` names, read from its resolved path, if that is
/// a regular file inside one of `roots`. Anything else is refused with a
/// warning: the tar is the managed layer's, packed in the sandbox, and heph
/// neither reads nor removes a file it does not own.
fn staged_out_tar(
    addr: &Addr,
    mut tar: OutputArtifact,
    roots: &[PathBuf],
) -> Option<OutputArtifact> {
    let Content::TarPath(content) = &mut tar.content else {
        return None;
    };
    let why = match resolve_in_roots(Path::new(&content.path), roots) {
        Ok(Some(real)) => match real.into_os_string().into_string() {
            Ok(real) => {
                content.path = real;
                return Some(tar);
            }
            Err(_) => "the tar's resolved path is not UTF-8".to_owned(),
        },
        Ok(None) => "the tar is gone, not a regular file, or outside the sandbox".to_owned(),
        Err(e) => e.to_string(),
    };
    tracing::warn!(addr = %addr, tar = %content.path, why, "failure_out not kept");
    None
}

/// Remove the `failure_out` tar once the fill has read it, when the driver
/// handed it over (`owned`); one it keeps is left alone.
fn free_owned_tar(addr: &Addr, tar: &OutputArtifact) {
    if let Content::TarPath(t) = &tar.content
        && t.owned
        && let Err(e) = std::fs::remove_file(&t.path)
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::debug!(addr = %addr, tar = %t.path, error = %e, "removing the failure_out tar");
    }
}

/// The `Log` artifact to keep, if the failure carries one this module can
/// read. Only `Log` is honored; the first one found is the outermost
/// attachment ([`hplugin::error::with_failure_artifacts`] puts newer ones
/// first), and any other is ignored.
fn select_log(addr: &Addr, err: &anyhow::Error) -> Option<LogSource> {
    let artifacts = hplugin::error::failure_artifacts(err)?;
    let mut chosen: Option<&OutputArtifact> = None;
    for a in &artifacts.0 {
        if a.r#type == ArtifactType::Output && a.name == hplugin::error::FAILURE_OUT_NAME {
            // The `failure_out` tar: `fill` unpacks it as `out/`.
        } else if a.r#type != ArtifactType::Log {
            tracing::debug!(addr = %addr, r#type = ?a.r#type, name = %a.name, "ignoring a failure artifact that is not a log");
        } else if chosen.is_some() {
            tracing::debug!(addr = %addr, name = %a.name, "ignoring a second log artifact");
        } else {
            chosen = Some(a);
        }
    }
    match &chosen?.content {
        Content::File(f) => Some(LogSource::File(PathBuf::from(&f.source_path))),
        Content::Raw(r) => Some(LogSource::Bytes(r.data.clone())),
        _ => {
            tracing::debug!(addr = %addr, "ignoring a log artifact that is neither a file nor inline bytes");
            None
        }
    }
}

/// `path` with every symlink resolved, if it is a regular file inside one of
/// `roots`. `None` for a missing file — not a second error — or one outside.
fn resolve_in_roots(path: &Path, roots: &[PathBuf]) -> io::Result<Option<PathBuf>> {
    let real = match crate::engine::run_log::inside_roots(path, roots) {
        Ok(Some(real)) => real,
        Ok(None) => {
            tracing::debug!(path = %path.display(), "the failure file is outside the sandbox; not read");
            return Ok(None);
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            tracing::debug!(path = %path.display(), "the failure file is gone");
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    if !std::fs::metadata(&real)?.is_file() {
        return Ok(None);
    }
    Ok(Some(real))
}

/// `tail` within [`RECORD_CAP`](hcore::failure::RECORD_CAP) (E2): a log
/// kept only in memory is held on the error chain as long as the failure
/// is, so it gets the record's cap, not [`TAIL_MAX_BYTES`]. The caller has
/// already scrubbed it ([`Fill::scrub`]). Whole lines are dropped from the
/// front, so no line is cut unless the last one alone is over the cap; then
/// its end is kept, at a char boundary. True when it cut.
///
/// [`TAIL_MAX_BYTES`]: hcore::failure_log::TAIL_MAX_BYTES
fn capped_tail(mut tail: LogTail) -> (LogTail, bool) {
    let cap = hcore::failure::RECORD_CAP;
    if tail.text.len() <= cap {
        return (tail, false);
    }
    let len = tail.text.len();
    let over = len - cap;
    // The first line that starts at or past `over`, when one starts before
    // the end; else the cut inside the last line.
    let from = over.saturating_sub(1);
    let line_start = tail
        .text
        .as_bytes()
        .get(from..)
        .and_then(|rest| rest.iter().position(|&b| b == b'\n'))
        .map(|i| from + i + 1)
        .filter(|&start| start < len);
    let start = line_start.unwrap_or_else(|| {
        let mut start = over;
        while !tail.text.is_char_boundary(start) {
            start += 1;
        }
        start
    });
    let dropped = tail
        .text
        .get(..start)
        .map_or(0, |head| head.matches('\n').count());
    tail.text.drain(..start);
    tail.start_line += dropped;
    (tail, true)
}

/// The tail of `source`, read into memory.
fn memory_tail(source: &LogSource, lines: usize) -> io::Result<LogTail> {
    match source {
        LogSource::File(path) => {
            use std::os::unix::fs::FileExt as _;
            let file = std::fs::File::open(path)?;
            let len = file.metadata()?.len();
            hplugin::error::read_tail_at(len, |buf, off| file.read_at(buf, off), lines)
        }
        LogSource::Fd(file) => {
            use std::os::unix::fs::FileExt as _;
            let len = file.metadata()?.len();
            hplugin::error::read_tail_at(len, |buf, off| file.read_at(buf, off), lines)
        }
        LogSource::Bytes(bytes) => hplugin::error::read_tail_at(
            bytes.len() as u64,
            |buf, off| {
                let start = usize::try_from(off).unwrap_or(usize::MAX);
                let src = bytes.get(start..).unwrap_or_default();
                let k = src.len().min(buf.len());
                buf.get_mut(..k)
                    .unwrap_or_default()
                    .copy_from_slice(src.get(..k).unwrap_or_default());
                Ok(k)
            },
            lines,
        ),
    }
}

/// A directory that is removed on drop unless [`disarm`](Self::disarm)ed:
/// a slot being built, or an old one being discarded.
struct TempDir(Option<PathBuf>);

impl TempDir {
    fn path(&self) -> &Path {
        self.0.as_deref().unwrap_or(Path::new(""))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            drop(std::fs::remove_dir_all(p));
        }
    }
}

fn temp_name(key: &str) -> String {
    format!("{TEMP_PREFIX}{key}-{}", uuid::Uuid::new_v4().as_simple())
}

/// What [`fill_slot`] put in place.
struct Built {
    /// The log's final path and its line count.
    log: Option<(PathBuf, u64)>,
    /// The slot's `out/`, when the `failure_out` tar was unpacked in.
    out: Option<PathBuf>,
    /// Why the `failure_out` tar was not unpacked in, when the slot was kept
    /// without it.
    out_error: Option<anyhow::Error>,
    /// The slot's `fixes.txt`, when there were fixes to list.
    fixes: Option<PathBuf>,
}

/// Build the slot `key` under `root` from `log` and the `failure_out` tar
/// `out`, and rename it into place, replacing any slot already there.
///
/// `out` is unpacked in as `out/`: every file a new inode, whatever
/// filesystem the sandbox is on. A failure there drops `out/` and keeps the
/// rest of the slot, unless there is neither a log nor a fixes list to keep.
/// Anything else that fails before the rename leaves the old slot as it was
/// and the temporary directory removed.
///
/// `diagnostic` is a claim's `diagnostic.json`, for a slot that keeps only
/// that ([`write_claim`]).
#[expect(
    clippy::too_many_arguments,
    reason = "each a part of the slot, passed through from one of two fills"
)]
fn fill_slot(
    root: &Path,
    key: &str,
    manifest: &SlotManifest,
    log: Option<&LogSource>,
    out: Option<&OutputArtifact>,
    fixes: &[String],
    diagnostic: Option<&[u8]>,
    ops: SlotIo,
) -> anyhow::Result<Built> {
    use anyhow::Context as _;
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let mut temp = TempDir(Some(root.join(temp_name(key))));
    std::fs::create_dir(temp.path())
        .with_context(|| format!("creating {}", temp.path().display()))?;
    let log = match log {
        Some(source) => {
            let path = temp.path().join(LOG);
            match source {
                LogSource::File(from) => (ops.copy)(from, &path)
                    .with_context(|| format!("copying {} to {}", from.display(), path.display()))?,
                LogSource::Bytes(bytes) => std::fs::write(&path, bytes)
                    .with_context(|| format!("writing {}", path.display()))?,
                LogSource::Fd(from) => copy_fd(from, &path)
                    .with_context(|| format!("copying the live log to {}", path.display()))?,
            }
            // Opened only to count; nothing keeps the file (E2).
            let file = std::fs::File::open(&path)
                .with_context(|| format!("opening {}", path.display()))?;
            let lines = hcore::failure_log::count_lines(&file)
                .with_context(|| format!("counting the lines of {}", path.display()))?;
            Some(lines)
        }
        None => None,
    };
    let (moved, out_error) = match out {
        Some(tar) => {
            let to = temp.path().join(OUT);
            match (ops.unpack)(tar, &to)
                .with_context(|| format!("unpacking failure_out into {}", to.display()))
            {
                Ok(()) => (true, None),
                Err(e) if log.is_some() || !fixes.is_empty() => {
                    // Half an unpack is not an `out/`: the slot is kept without
                    // it, for its log or its `fixes.txt` (a material target
                    // has no log on disk, and its fixes are still worth a
                    // list).
                    match std::fs::remove_dir_all(&to) {
                        Err(e) if e.kind() != io::ErrorKind::NotFound => {
                            return Err(e)
                                .with_context(|| format!("removing a partial {}", to.display()));
                        }
                        _ => {}
                    }
                    (false, Some(e))
                }
                Err(e) => return Err(e),
            }
        }
        None => (false, None),
    };
    let wrote_fixes = !fixes.is_empty();
    if wrote_fixes {
        let path = temp.path().join(FIXES);
        let mut list = fixes.join("\n");
        list.push('\n');
        std::fs::write(&path, list).with_context(|| format!("writing {}", path.display()))?;
    }
    if let Some(bytes) = diagnostic {
        let path = temp.path().join(DIAGNOSTIC);
        std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    }
    // Last: a slot is complete exactly when its manifest exists.
    let json = serde_json::to_vec(manifest).context("encoding the failure manifest")?;
    let manifest_path = temp.path().join(MANIFEST);
    std::fs::write(&manifest_path, json)
        .with_context(|| format!("writing {}", manifest_path.display()))?;

    let dest = root.join(key);
    // Move the old slot aside first: a directory cannot be renamed over a
    // non-empty one. A reader in between sees no slot, never a mixed one.
    let old = TempDir(Some(root.join(temp_name(key))));
    match std::fs::rename(&dest, old.path()) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("moving the old slot {} aside", dest.display()));
        }
    }
    std::fs::rename(temp.path(), &dest)
        .with_context(|| format!("renaming the slot into {}", dest.display()))?;
    temp.disarm();
    drop(old);
    Ok(Built {
        log: log.map(|lines| (dest.join(LOG), lines)),
        out: moved.then(|| dest.join(OUT)),
        out_error,
        fixes: wrote_fixes.then(|| dest.join(FIXES)),
    })
}

/// Copy all of `from`, a live log's descriptor, into the new file `to`.
/// `std::io::copy` between two files uses `copy_file_range` on Linux, which
/// reflinks where the filesystem can.
fn copy_fd(from: &std::fs::File, to: &Path) -> io::Result<()> {
    use std::io::Seek as _;
    let mut from = from;
    from.seek(io::SeekFrom::Start(0))?;
    let mut to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    io::copy(&mut from, &mut to).map(drop)
}

/// The manifest of the slot at `dir`, if it is complete, of a version this
/// heph reads, and for `addr`. Anything else reads as no slot.
pub fn read_manifest(dir: &Path, addr: &Addr) -> Option<SlotManifest> {
    read_any_manifest(dir).filter(|m| m.addr == addr.format())
}

/// The manifest of the slot at `dir`, if it is complete and of a version this
/// heph reads, whichever address it names. What `heph tool gc` reads a slot
/// it found by listing with. A complete slot holds a `log.txt`, an `out/`, a
/// `fixes.txt`, a `diagnostic.json` (a failure before the run keeps only
/// that), or any of them together. A `diagnostic.json` of a version this heph
/// does not read still makes the slot complete; it reads as no diagnostic
/// ([`hcore::failure_log::read_diagnostic`]).
pub(crate) fn read_any_manifest(dir: &Path) -> Option<SlotManifest> {
    let bytes = std::fs::read(dir.join(MANIFEST)).ok()?;
    let m: SlotManifest = serde_json::from_slice(&bytes).ok()?;
    (m.version == MANIFEST_VERSION
        && (dir.join(LOG).is_file()
            || dir.join(OUT).is_dir()
            || dir.join(FIXES).is_file()
            || dir.join(DIAGNOSTIC).is_file()))
    .then_some(m)
}

/// Write a complete slot for `addr` under `root` by hand, as a fill would
/// leave it. For tests outside this module.
#[cfg(test)]
pub(crate) fn write_test_slot(root: &Path, addr: &Addr, failed_at: u64, log: &[u8]) -> PathBuf {
    let dir = root.join(addr_key(addr));
    std::fs::create_dir_all(&dir).expect("mkdir slot");
    std::fs::write(dir.join(LOG), log).expect("write log");
    let m = SlotManifest {
        version: MANIFEST_VERSION,
        addr: addr.format(),
        hashin: Some("h".into()),
        failed_at,
        fill_id: Some(new_fill_id()),
        packages: Vec::new(),
    };
    std::fs::write(dir.join(MANIFEST), serde_json::to_vec(&m).expect("json")).expect("manifest");
    dir
}

/// Write a complete slot for the source file at `rel` under `root` by hand,
/// as a failure before the run leaves it: a manifest and `diagnostic.json`.
#[cfg(test)]
pub(crate) fn write_test_file_slot(root: &Path, rel: &str, failed_at: u64) -> PathBuf {
    let dir = root.join(file_key(rel));
    std::fs::create_dir_all(&dir).expect("mkdir slot");
    let fill_id = new_fill_id();
    let rec = hcore::failure::FailureRecord::new(hcore::diagnostic::Diagnostic::new(
        hcore::diagnostic::Stage::Eval,
        "boom",
    ));
    let diag = hcore::failure_log::encode_diagnostic(&rec, Some(&fill_id)).expect("encode");
    std::fs::write(dir.join(DIAGNOSTIC), diag).expect("write diagnostic");
    let m = SlotManifest {
        version: MANIFEST_VERSION,
        addr: format!("{FILE_ADDR_PREFIX}{rel}"),
        hashin: None,
        failed_at,
        fill_id: Some(fill_id),
        packages: Vec::new(),
    };
    std::fs::write(dir.join(MANIFEST), serde_json::to_vec(&m).expect("json")).expect("manifest");
    dir
}

/// The slot names under `root`; temporaries are not slots. With them, each
/// source file slot's name and the packages its manifest lists: a file key's
/// readable part starts `file_`, so only those manifests are read.
fn list_slots(root: &Path) -> (HashSet<String>, HashMap<String, Vec<String>>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Default::default();
    };
    let slots: HashSet<String> = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    let files = slots
        .iter()
        .filter(|n| n.starts_with("file_"))
        .filter_map(|n| {
            let m = read_any_manifest(&root.join(n))?;
            (m.addr.starts_with(FILE_ADDR_PREFIX) && !m.packages.is_empty())
                .then(|| (n.clone(), m.packages))
        })
        .collect();
    (slots, files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hplugin::error::{log_file_artifact, with_failure_artifacts};

    fn addr(s: &str) -> Addr {
        hmodel::htaddr::parse_addr(s).expect("addr")
    }

    /// The kept files are grouped at the target's package, a directory by
    /// its file count and a file alone, sorted and capped; an empty
    /// directory is no group.
    #[test]
    fn kept_groups_at_package_sorted_and_capped() {
        let dir = hcore::fsutil::tempdir().expect("tempdir");
        let pkg = dir.path().join("web");
        for f in [
            "report/a",
            "report/deep/b",
            "report/deep/c",
            "z.txt",
            "a.txt",
        ] {
            let p = pkg.join(f);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(&p, "x").expect("write");
        }
        std::fs::create_dir_all(pkg.join("empty")).expect("mkdir");
        let (groups, more) = kept_groups(dir.path(), "web");
        let named: Vec<_> = groups.iter().map(|g| (g.name.as_str(), g.files)).collect();
        assert_eq!(named, [("a.txt", 1), ("report/", 3), ("z.txt", 1)]);
        assert_eq!(more, 0);

        for i in 0..7 {
            std::fs::write(pkg.join(format!("m{i}")), "x").expect("write");
        }
        let (groups, more) = kept_groups(dir.path(), "web");
        assert_eq!(groups.len(), hcore::events::KEPT_GROUPS_CAP);
        assert_eq!(more, 5);
        assert_eq!(kept_groups(dir.path(), "gone"), (Vec::new(), 0));
    }

    fn manifest_for(a: &Addr) -> SlotManifest {
        SlotManifest {
            version: MANIFEST_VERSION,
            addr: a.format(),
            hashin: Some("h".into()),
            failed_at: 1,
            fill_id: Some("f".into()),
            packages: Vec::new(),
        }
    }

    fn oracle(bytes: &[u8], n: usize) -> LogTail {
        let s = String::from_utf8_lossy(bytes);
        let (text, start_line) = hplugin::error::last_n_lines_with_start(&s, n);
        LogTail { text, start_line }
    }

    fn tail_of(bytes: &[u8], n: usize) -> LogTail {
        memory_tail(&LogSource::Bytes(bytes.to_vec()), n).expect("tail")
    }

    #[test]
    fn addr_key_is_injective_and_fs_safe() {
        let long = format!("//p:{}", "x".repeat(300));
        let pairs = [
            ("//p:a", "//p:ab"),
            ("//a/b:c", "//a_b:c"),
            ("//a:bc", "//ab:c"),
            ("//p:a@k=v", "//p:a"),
            ("//p:A", "//p:a"),
            ("//P:a", "//p:a"),
            ("//:a", "//_:a"),
            ("//p:é", "//p:e"),
            (long.as_str(), "//p:x"),
        ];
        for (l, r) in pairs {
            let (Ok(la), Ok(ra)) = (hmodel::htaddr::parse_addr(l), hmodel::htaddr::parse_addr(r))
            else {
                continue;
            };
            assert_ne!(addr_key(&la), addr_key(&ra), "{l} vs {r}");
        }
        for s in [
            "//p:a",
            "//:a",
            "//a/b/c:d",
            "//p:a@k=v",
            "//p:.hidden",
            "//p:..",
            "//p:é",
            long.as_str(),
        ] {
            let Ok(a) = hmodel::htaddr::parse_addr(s) else {
                continue;
            };
            let k = addr_key(&a);
            assert!(k.len() < 255, "{s}: {}", k.len());
            assert!(!k.starts_with('.'), "{s}: {k}");
            assert!(k != "." && k != "..");
            assert!(
                k.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'_'
                    || b == b'-'),
                "{s}: {k}"
            );
            assert_eq!(k, addr_key(&a), "stable");
        }
    }

    /// FQ-11: a memory tail whose one line is over the cap keeps its end
    /// from a char boundary. `€` is 3 bytes and the cut point falls inside
    /// one: the cut moves to the next char, never splits it.
    #[test]
    fn memory_tail_cap_at_char_boundary() {
        let cap = hcore::failure::RECORD_CAP;
        let line = "€".repeat(cap / 3 + 100);
        assert_ne!((line.len() - cap) % 3, 0, "the cut falls inside a char");
        let (tail, cut) = capped_tail(LogTail {
            text: line.clone(),
            start_line: 1,
        });
        assert!(cut);
        assert!(tail.text.len() <= cap && tail.text.len() > cap - 3);
        assert!(tail.text.chars().all(|c| c == '€'));
        assert!(line.ends_with(&tail.text));
        assert_eq!(tail.start_line, 1, "no line was dropped");
        // Whole lines go from the front first.
        let lines = format!("{}\n{}", "x".repeat(100), "€".repeat(cap / 3 - 10));
        let (tail, cut) = capped_tail(LogTail {
            text: lines,
            start_line: 1,
        });
        assert!(cut);
        assert!(tail.text.starts_with('€') && tail.text.len() <= cap);
        assert_eq!(tail.start_line, 2);
    }

    #[test]
    fn failure_tail_cases() {
        let cases: [(&str, &[u8]); 9] = [
            ("non-utf8", b"a\n\xff\xfe\nb\n"),
            ("empty", b""),
            ("no trailing newline", b"a\nb\nc"),
            ("fewer than n", b"one\ntwo\n"),
            ("cr only", b"a\rb\rc\r"),
            ("crlf", b"a\r\nb\r\n"),
            ("ansi", b"\x1b[31mred\x1b[0m\nplain\n"),
            ("only newlines", b"\n\n\n\n"),
            ("many", b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n"),
        ];
        for (name, bytes) in cases {
            for n in [0, 1, 3, 10] {
                assert_eq!(tail_of(bytes, n), oracle(bytes, n), "{name}, n={n}");
            }
        }
    }

    #[test]
    fn tail_reads_only_end_of_large_log() {
        // 200k lines: the backward pass must hold the tail, not the log.
        let big: Vec<u8> = (1..=200_000)
            .flat_map(|i| format!("line {i}\n").into_bytes())
            .collect();
        let mut furthest_back = u64::MAX;
        let (cut, window) = hplugin::error::tail_window(
            big.len() as u64,
            &mut |buf: &mut [u8], off: u64| {
                furthest_back = furthest_back.min(off);
                let start = usize::try_from(off).expect("off");
                let k = buf.len().min(big.len() - start);
                buf[..k].copy_from_slice(&big[start..start + k]);
                Ok(k)
            },
            10,
        )
        .expect("window");
        assert!(window.len() < 200, "held {} bytes", window.len());
        assert!(
            big.len() as u64 - furthest_back <= 8 * 1024,
            "read back {furthest_back}"
        );
        assert_eq!(cut + window.len() as u64, big.len() as u64);
        let tail = tail_of(&big, 10);
        assert_eq!(tail, oracle(&big, 10));
        assert_eq!(tail.start_line, 199_991);
    }

    #[test]
    fn slot_without_manifest_is_absent() {
        let home = tempfile::tempdir().expect("tempdir");
        let slots = FailureSlots::new(home.path());
        let a = addr("//p:a");
        let dir = slots.slot_dir(&a);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(LOG), "x").expect("log");
        assert_eq!(slots.read(&a), None, "no manifest");
        let mut m = manifest_for(&a);
        m.version = 99;
        std::fs::write(dir.join(MANIFEST), serde_json::to_vec(&m).expect("json")).expect("m");
        assert_eq!(slots.read(&a), None, "unknown version");
        m = manifest_for(&addr("//p:other"));
        std::fs::write(dir.join(MANIFEST), serde_json::to_vec(&m).expect("json")).expect("m");
        assert_eq!(slots.read(&a), None, "another address");
        std::fs::write(
            dir.join(MANIFEST),
            serde_json::to_vec(&manifest_for(&a)).expect("j"),
        )
        .expect("m");
        assert_eq!(slots.read(&a), Some(manifest_for(&a)));
    }

    /// A file's key is readable, filesystem-safe, its own per path, and never
    /// an address's, even one spelled like the path.
    #[test]
    fn file_key_is_its_own() {
        let k = file_key("d/BUILD");
        assert!(k.starts_with("file_d_build-"), "{k}");
        assert!(
            k.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'),
            "{k}"
        );
        assert_ne!(k, file_key("d/build"), "case is not folded away");
        assert_ne!(k, file_key("d_BUILD"));
        assert_ne!(k, addr_key(&addr("//d:BUILD")));
        assert_ne!(k, addr_key(&addr("//file:d")));
    }

    /// A manifest written before `hashin` was optional, and one written for a
    /// failure before the run (no `hashin`, no `fill_id`), both read.
    #[test]
    fn slot_manifest_without_hashin_reads() {
        let home = tempfile::tempdir().expect("tempdir");
        let slots = FailureSlots::new(home.path());
        let a = addr("//p:a");
        let dir = slots.slot_dir(&a);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(LOG), "x").expect("log");
        let old = format!(
            r#"{{"version":1,"addr":"{}","hashin":"h","failed_at":7}}"#,
            a.format()
        );
        std::fs::write(dir.join(MANIFEST), old).expect("m");
        let m = slots.read(&a).expect("an older heph's slot reads");
        assert_eq!(
            (m.hashin.as_deref(), m.fill_id, m.failed_at),
            (Some("h"), None, 7)
        );
        let pre_run = format!(r#"{{"version":1,"addr":"{}","failed_at":8}}"#, a.format());
        std::fs::write(dir.join(MANIFEST), pre_run).expect("m");
        let m = slots.read(&a).expect("a slot with no revision reads");
        assert_eq!((m.hashin, m.failed_at), (None, 8));
    }

    /// A slot filled before the run holds a manifest and `diagnostic.json`
    /// only, and is complete: not read as absent.
    #[test]
    fn slot_with_diagnostic_and_no_log_is_complete() {
        let home = tempfile::tempdir().expect("tempdir");
        let slots = FailureSlots::new(home.path());
        let a = addr("//p:a");
        let dir = slots.slot_dir(&a);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let mut m = manifest_for(&a);
        m.hashin = None;
        std::fs::write(dir.join(MANIFEST), serde_json::to_vec(&m).expect("json")).expect("m");
        assert_eq!(slots.read(&a), None, "a manifest alone is no slot");
        let rec = hcore::failure::FailureRecord::new(hcore::diagnostic::Diagnostic::new(
            hcore::diagnostic::Stage::Spec,
            "no such dep",
        ));
        let bytes =
            hcore::failure_log::encode_diagnostic(&rec, m.fill_id.as_deref()).expect("encode");
        std::fs::write(dir.join(DIAGNOSTIC), bytes).expect("diagnostic");
        assert_eq!(slots.read(&a), Some(m));
        let read = hcore::failure_log::read_diagnostic(&dir).expect("the diagnostic reads");
        assert_eq!(read.diagnostic.message, "no such dep");
    }

    /// A `diagnostic.json` of a version this heph does not know still makes
    /// the slot complete, and reads as no diagnostic.
    #[test]
    fn diagnostic_json_unknown_version_ignored() {
        let home = tempfile::tempdir().expect("tempdir");
        let slots = FailureSlots::new(home.path());
        let a = addr("//p:a");
        let dir = slots.slot_dir(&a);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let m = manifest_for(&a);
        std::fs::write(dir.join(MANIFEST), serde_json::to_vec(&m).expect("json")).expect("m");
        let future = format!(
            r#"{{"version":99,"fill_id":"{}","stage":"def","message":"from the future"}}"#,
            m.fill_id.as_deref().unwrap_or_default()
        );
        std::fs::write(dir.join(DIAGNOSTIC), future).expect("diagnostic");
        assert_eq!(slots.read(&a), Some(m), "the slot is complete");
        assert!(
            hcore::failure_log::read_diagnostic(&dir).is_none(),
            "an unknown version is no diagnostic"
        );
    }

    #[test]
    fn slot_fill_is_atomic() {
        let home = tempfile::tempdir().expect("tempdir");
        let root = home.path().join(FAILURES_DIR);
        let a = addr("//p:a");
        let key = addr_key(&a);
        fill_slot(
            &root,
            &key,
            &manifest_for(&a),
            Some(&LogSource::Bytes(b"old\n".to_vec())),
            None,
            &[],
            None,
            SlotIo::REAL,
        )
        .expect("first fill");
        let src = home.path().join("src.txt");
        std::fs::write(&src, "new\n").expect("src");
        let err = fill_slot(
            &root,
            &key,
            &manifest_for(&a),
            Some(&LogSource::File(src)),
            None,
            &[],
            None,
            SlotIo {
                copy: |from, to| {
                    // Half a copy, then the disk fills.
                    std::fs::write(to, &std::fs::read(from)?[..2])?;
                    Err(io::Error::other("ENOSPC"))
                },
                ..SlotIo::REAL
            },
        );
        assert!(err.is_err());
        assert_eq!(
            std::fs::read(root.join(&key).join(LOG)).expect("old log"),
            b"old\n"
        );
        let names: Vec<String> = std::fs::read_dir(&root)
            .expect("ls")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [key], "no temporary left behind");
    }

    #[test]
    fn failure_slot_copies_never_moves() {
        let home = tempfile::tempdir().expect("tempdir");
        let src = home.path().join("log.txt");
        std::fs::write(&src, "kept\n").expect("src");
        let a = addr("//p:a");
        let (path, lines) = fill_slot(
            &home.path().join(FAILURES_DIR),
            &addr_key(&a),
            &manifest_for(&a),
            Some(&LogSource::File(src.clone())),
            None,
            &[],
            None,
            SlotIo::REAL,
        )
        .expect("fill")
        .log
        .expect("a log");
        assert_eq!(std::fs::read(&src).expect("source still there"), b"kept\n");
        assert_eq!(std::fs::read(&path).expect("copy"), b"kept\n");
        assert_eq!(lines, 1, "the copy's lines are counted");
    }

    fn raw_log(data: &[u8]) -> OutputArtifact {
        OutputArtifact {
            group: String::new(),
            name: LOG.into(),
            r#type: ArtifactType::Log,
            content: Content::Raw(crate::engine::driver::outputartifact::ContentRaw {
                data: data.to_vec(),
                path: LOG.into(),
                x: false,
            }),
            hashout: String::new(),
        }
    }

    async fn fill_with(
        slots: &FailureSlots,
        sandbox: &Path,
        artifacts: Vec<OutputArtifact>,
    ) -> Option<String> {
        let a = addr("//p:a");
        let err = with_failure_artifacts(anyhow::anyhow!("boom"), artifacts);
        let slot = slots
            .fill(Fill {
                addr: &a,
                hashin: "h",
                roots: vec![sandbox.to_path_buf()],
                err: &err,
                lines: 10,
                material: false,
                fixes: None,
                keep_log: true,
                scrub: Default::default(),
            })
            .await;
        slot.log.map(|l| l.tail().expect("tail").text)
    }

    #[tokio::test]
    async fn failure_artifact_kinds() {
        let home = tempfile::tempdir().expect("home");
        let sandbox = tempfile::tempdir().expect("sandbox");
        let outside = tempfile::tempdir().expect("outside");
        let slots = FailureSlots::new(home.path());
        let in_log = sandbox.path().join("log.txt");
        std::fs::write(&in_log, "in sandbox\n").expect("log");
        let empty = sandbox.path().join("empty.txt");
        std::fs::write(&empty, "").expect("empty");
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "outside\n").expect("secret");
        let link = sandbox.path().join("link.txt");
        std::os::unix::fs::symlink(&secret, &link).expect("symlink");
        let as_type = |t| {
            let mut a = log_file_artifact(&in_log).expect("artifact");
            a.r#type = t;
            a
        };
        let file = |p: &Path| log_file_artifact(p).expect("artifact");

        let cases: Vec<(&str, Vec<OutputArtifact>, Option<&str>)> = vec![
            ("output", vec![as_type(ArtifactType::Output)], None),
            ("support", vec![as_type(ArtifactType::SupportFile)], None),
            ("empty", vec![], None),
            ("log file", vec![file(&in_log)], Some("in sandbox")),
            ("zero-byte", vec![file(&empty)], Some("")),
            (
                "missing file",
                vec![file(&sandbox.path().join("gone.txt"))],
                None,
            ),
            ("symlink out of sandbox", vec![file(&link)], None),
            ("raw", vec![raw_log(b"inline\n")], Some("inline")),
            (
                "duplicate log",
                vec![raw_log(b"outer\n"), file(&in_log)],
                Some("outer"),
            ),
        ];
        for (name, artifacts, want) in cases {
            assert_eq!(
                fill_with(&slots, sandbox.path(), artifacts)
                    .await
                    .as_deref(),
                want,
                "{name}"
            );
        }
        assert_eq!(std::fs::read(&secret).expect("untouched"), b"outside\n");
    }

    /// Every fill mints its own id, written to the manifest and handed back
    /// on the slot with the copy's line count; an invocation's fill (no
    /// revision) writes no `hashin`.
    #[tokio::test]
    async fn each_fill_mints_its_fill_id() {
        let home = tempfile::tempdir().expect("home");
        let slots = FailureSlots::new(home.path());
        let a = addr("//p:a");
        let mut ids = Vec::new();
        for hashin in ["h", ""] {
            let err = with_failure_artifacts(anyhow::anyhow!("boom"), vec![raw_log(b"a\nb\nc")]);
            let slot = slots
                .fill(Fill {
                    addr: &a,
                    hashin,
                    roots: Vec::new(),
                    err: &err,
                    lines: 10,
                    material: false,
                    fixes: None,
                    keep_log: true,
                    scrub: Default::default(),
                })
                .await;
            let m = slots.read(&a).expect("a slot");
            assert_eq!(
                m.fill_id, slot.fill_id,
                "the slot names the manifest's fill"
            );
            assert_eq!(m.hashin.as_deref(), (!hashin.is_empty()).then_some(hashin));
            assert_eq!(slot.log_lines, 3);
            ids.push(slot.fill_id.expect("a fill id"));
        }
        assert_ne!(ids.first(), ids.get(1), "two fills, two ids");
    }

    /// Credential material never gets a slot: the tail is read into memory.
    #[tokio::test]
    async fn material_target_keeps_tail_in_memory() {
        let home = tempfile::tempdir().expect("home");
        let sandbox = tempfile::tempdir().expect("sandbox");
        let log = sandbox.path().join("log.txt");
        std::fs::write(&log, "producer said\n").expect("log");
        let slots = FailureSlots::new(home.path());
        let a = addr("//auth:fetch");
        let err = with_failure_artifacts(
            anyhow::anyhow!("boom"),
            vec![log_file_artifact(&log).expect("artifact")],
        );
        let slot = slots
            .fill(Fill {
                addr: &a,
                hashin: "h",
                roots: vec![sandbox.path().to_path_buf()],
                err: &err,
                lines: 10,
                material: true,
                fixes: None,
                keep_log: true,
                scrub: Default::default(),
            })
            .await;
        let log = slot.log.expect("a tail");
        assert!(log.path().is_none());
        assert_eq!(log.tail().expect("tail").text, "producer said");
        assert!(!slots.root().exists(), "no slot, not even the directory");
    }

    /// FQ-1: thousands of claim writes at distinct keys walk at most twice
    /// as many exclusion-map entries as there were lookups (a walk per
    /// lookup was N²/2); once they are done, the next lookup due prunes the
    /// map back down.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slot_locks_prune_amortized_over_many_claims() {
        const N: usize = 4_000;
        let (engine, _dir) = crate::engine::cache_test_support::test_engine();
        let started = std::time::Instant::now();
        for i in 0..N {
            let mut rec = hcore::failure::FailureRecord::new(hplugin::diagnostic::Diagnostic::new(
                hplugin::diagnostic::Stage::Run,
                "boom",
            ));
            rec.addr = Some(format!("//p:t{i}"));
            write_claim(
                Arc::clone(&engine),
                PendingCounter::default(),
                ClaimWrite {
                    culprit: hplugin::error::Culprit::Target(addr(&format!("//p:t{i}"))),
                    record: Arc::new(rec),
                    fill_id: None,
                    packages: Vec::new(),
                },
            );
        }
        engine.failure_slots.writes_drained().await;
        let elapsed = started.elapsed();
        let walked = engine
            .failure_slots
            .slot_locks
            .lock()
            .expect("locks")
            .walked;
        assert!(walked <= 2 * N, "{walked} entries walked for {N} lookups");
        for i in [0, N - 1] {
            let a = addr(&format!("//p:t{i}"));
            assert!(engine.failure_slots.read(&a).is_some(), "{a} landed");
        }
        // Nothing is held now: grow the map to its next prune, which
        // leaves only what that lookup holds.
        let mut held = Vec::new();
        let mut i = 0;
        while held.len() < 4 * N {
            held.push(engine.failure_slots.slot_lock(&format!("again-{i}")));
            i += 1;
            let len = engine
                .failure_slots
                .slot_locks
                .lock()
                .expect("locks")
                .map
                .len();
            if len == held.len() {
                break;
            }
        }
        let len = engine
            .failure_slots
            .slot_locks
            .lock()
            .expect("locks")
            .map
            .len();
        assert_eq!(len, held.len(), "the released exclusions were pruned");
        // Generous: the quadratic walk took seconds at this size.
        assert!(elapsed < std::time::Duration::from_secs(60), "{elapsed:?}");
    }

    /// H-M1: a material run's memory tail is scrubbed before it is cut to
    /// the record's cap. One line over the cap, cut inside a secret: cut
    /// first, the secret's suffix would head the tail, where no scrub knows
    /// it.
    #[tokio::test]
    async fn memory_tail_scrubbed_before_the_cut() {
        let secret = "MEMTAIL-SECRET-0123456789-abcdef";
        let suffix = secret.get(secret.len() / 2..).expect("suffix");
        let line = format!(
            "{}{secret}{}",
            "a".repeat(100),
            "b".repeat(hcore::failure::RECORD_CAP - suffix.len())
        );
        let home = tempfile::tempdir().expect("home");
        let sandbox = tempfile::tempdir().expect("sandbox");
        let slots = FailureSlots::new(home.path());
        let a = addr("//auth:fetch");
        let err = with_failure_artifacts(
            anyhow::anyhow!("boom"),
            vec![raw_log(format!("{line}\n").as_bytes())],
        );
        let fill = |scrub| Fill {
            addr: &a,
            hashin: "h",
            roots: vec![sandbox.path().to_path_buf()],
            err: &err,
            lines: 10,
            material: true,
            fixes: None,
            keep_log: true,
            scrub,
        };
        // The fixture has teeth: unscrubbed, the cut leaves the suffix.
        let raw = slots.fill(fill(Default::default())).await;
        let raw = raw.log.expect("a tail").tail().expect("tail").text.clone();
        assert!(raw.starts_with(suffix), "{:?}", raw.get(..40));

        let scrub = crate::engine::failure_record::Scrub::new([secret.to_owned()]);
        let slot = slots.fill(fill(scrub)).await;
        let log = slot.log.expect("a tail");
        assert!(log.memory_cut(), "the tail was cut");
        let text = &log.tail().expect("tail").text;
        assert!(text.len() <= hcore::failure::RECORD_CAP);
        assert!(!text.contains(suffix), "{:?}", text.get(..40));
    }

    #[tokio::test]
    async fn slot_fill_failure_keeps_target_error() {
        let home = tempfile::tempdir().expect("home");
        // `failures` is a file: no slot can be made under it.
        std::fs::write(home.path().join(FAILURES_DIR), "").expect("block");
        let sandbox = tempfile::tempdir().expect("sandbox");
        let slots = FailureSlots::new(home.path());
        let a = addr("//p:a");
        let err = with_failure_artifacts(
            anyhow::anyhow!("the target's own error"),
            vec![raw_log(b"why\n")],
        );
        let slot = slots
            .fill(Fill {
                addr: &a,
                hashin: "h",
                roots: vec![sandbox.path().to_path_buf()],
                err: &err,
                lines: 10,
                material: false,
                fixes: None,
                keep_log: true,
                scrub: Default::default(),
            })
            .await;
        let log = slot.log.clone().expect("the tail, in memory");
        assert!(log.path().is_none());
        assert_eq!(log.tail().expect("tail").text, "why");
        let e = err.context(slot);
        assert_eq!(
            hcore::hmemoizer::chain_message(&e),
            "the target's own error"
        );
    }

    /// A sandbox with a packed `failure_out` tar holding
    /// `pkg/report/index.html` and `pkg/report/data/trace.zip`, where the
    /// managed layer packs it; returns the tar.
    fn stage(sandbox: &Path) -> PathBuf {
        stage_with(sandbox, |_| {})
    }

    /// [`stage`], with more members added by `more`.
    fn stage_with(
        sandbox: &Path,
        more: impl FnOnce(&mut hcore::hartifactcontent::tar::TarPacker),
    ) -> PathBuf {
        let dir = sandbox.join(hdriver_support::driver_managed::COLLECT_ARTIFACTS_DIR);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let tar_path = dir.join(format!(
            "{}h-out.tar",
            hdriver_support::failure_out::FAILURE_OUT_TAR_PREFIX
        ));
        let mut tar = hcore::hartifactcontent::tar::TarPacker::new();
        tar.create_raw(b"<html>".to_vec(), "pkg/report/index.html", false);
        tar.create_raw(b"zip".to_vec(), "pkg/report/data/trace.zip", false);
        more(&mut tar);
        tar.pack(std::fs::File::create(&tar_path).expect("tar"))
            .expect("pack");
        tar_path
    }

    fn out_artifact(tar: &Path) -> OutputArtifact {
        hplugin::error::failure_out_artifact(tar.to_str().expect("utf-8").to_owned(), String::new())
    }

    /// Unpack ops that fail before writing anything.
    const FAIL_UP_FRONT: SlotIo = SlotIo {
        unpack: |_, _| Err(anyhow::anyhow!("EIO")),
        ..SlotIo::REAL
    };

    async fn fill_ops(
        slots: &FailureSlots,
        a: &Addr,
        sandbox: &Path,
        err: &anyhow::Error,
        material: bool,
        ops: SlotIo,
    ) -> FailureSlot {
        slots
            .fill_with(
                Fill {
                    addr: a,
                    hashin: "h",
                    roots: vec![sandbox.to_path_buf()],
                    err,
                    lines: 10,
                    material,
                    fixes: None,
                    keep_log: true,
                    scrub: Default::default(),
                },
                None,
                ops,
            )
            .await
    }

    /// D21: the paths a failed `in_place` run wrote back land in `fixes.txt`,
    /// one per line, and a slot holding only that list is still a slot. A
    /// reason with no paths writes no file and fills no slot.
    #[tokio::test]
    async fn fills_fixes_txt() {
        use hplugin::error::{FailureFixes, FixesNotWritten};
        let home = tempfile::tempdir().expect("home");
        let slots = FailureSlots::new(home.path());
        let a = addr("//p:a");
        let err = anyhow::anyhow!("boom");
        let paths = vec!["p/a.go".to_owned(), "p/sub/b.go".to_owned()];
        let root = std::path::PathBuf::from("/ws");
        let slot = slots
            .fill(Fill {
                addr: &a,
                hashin: "h",
                roots: vec![],
                err: &err,
                lines: 10,
                material: false,
                keep_log: true,
                scrub: Default::default(),
                fixes: Some(FailureFixes::Written {
                    root: root.clone(),
                    paths: paths.clone(),
                    list: None,
                }),
            })
            .await;
        let dir = slots.slot_dir(&a);
        assert_eq!(
            slot.fixes,
            Some(FailureFixes::Written {
                root,
                paths,
                list: Some(dir.join(FIXES)),
            })
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(FIXES)).expect("fixes.txt"),
            "p/a.go\np/sub/b.go\n"
        );
        assert_eq!(slots.read(&a).map(|m| m.addr), Some(a.format()));

        let b = addr("//p:b");
        let slot = slots
            .fill(Fill {
                addr: &b,
                hashin: "h",
                roots: vec![],
                err: &err,
                lines: 10,
                material: false,
                keep_log: true,
                scrub: Default::default(),
                fixes: Some(FailureFixes::NotWritten(FixesNotWritten::Frozen)),
            })
            .await;
        assert_eq!(
            slot.fixes,
            Some(FailureFixes::NotWritten(FixesNotWritten::Frozen))
        );
        assert!(!slots.slot_dir(&b).exists());
    }

    /// A source file's slot lists the packages reached through it; a target of
    /// one of them resolving clears it (the file evaluated), a target of
    /// another package does not, and a slot listing no package stays.
    #[tokio::test]
    async fn clear_takes_the_file_slots_of_the_targets_package() {
        let home = tempfile::tempdir().expect("home");
        let root = home.path().join(FAILURES_DIR);
        let with_packages = |rel: &str, pkgs: &[&str]| {
            let dir = write_test_file_slot(&root, rel, 1);
            let mut m = read_any_manifest(&dir).expect("manifest");
            m.packages = pkgs.iter().map(|p| (*p).to_owned()).collect();
            std::fs::write(dir.join(MANIFEST), serde_json::to_vec(&m).expect("json"))
                .expect("manifest");
            dir
        };
        let d = with_packages("d/BUILD", &["d"]);
        let defs = with_packages("lib/defs.bzl", &["e", "d"]);
        let none = with_packages("x/BUILD", &[]);
        let slots = FailureSlots::new(home.path());

        slots.clear(&addr("//other:t")).await;
        assert!(d.exists() && defs.exists());
        slots.clear(&addr("//d:t")).await;
        assert!(!d.exists(), "d/BUILD evaluated again");
        assert!(!defs.exists(), "d loads lib/defs.bzl, which evaluated");
        assert!(none.exists(), "a slot naming no package is gc's");
        assert_eq!(slots.read_file("d/BUILD"), None);
    }

    /// D10: a slot with an `out/` and no log is a slot; I3: `clear` takes
    /// `out/` with it. D22: a material target's log stays in memory while its
    /// `out/` is kept.
    #[tokio::test]
    async fn fills_and_reads_a_slot_without_a_log() {
        let home = tempfile::tempdir().expect("home");
        let sandbox = tempfile::tempdir().expect("sandbox");
        let slots = FailureSlots::new(home.path());
        let a = addr("//p:a");
        let staged_tar = stage(sandbox.path());
        let err = with_failure_artifacts(anyhow::anyhow!("boom"), vec![out_artifact(&staged_tar)]);
        let slot = fill_ops(&slots, &a, sandbox.path(), &err, false, SlotIo::REAL).await;
        assert!(slot.log.is_none());
        assert!(slot.out_error.is_none(), "{:?}", slot.out_error);
        let dir = slots.slot_dir(&a);
        assert_eq!(slot.out.as_deref(), Some(dir.join(OUT).as_path()));
        assert_eq!(
            std::fs::read(dir.join("out/pkg/report/data/trace.zip")).expect("kept"),
            b"zip"
        );
        assert!(!dir.join(LOG).exists());
        assert!(!staged_tar.exists(), "the owned tar is freed once unpacked");
        assert_eq!(slots.read(&a).map(|m| m.addr), Some(a.format()));
        slots.clear(&a).await;
        assert!(!dir.exists(), "cleared with its out/");
        assert_eq!(slots.read(&a), None);

        let m = addr("//auth:fetch");
        let log = sandbox.path().join("log.txt");
        std::fs::write(&log, "secret-ish\n").expect("log");
        let staged_tar = stage(sandbox.path());
        let err = with_failure_artifacts(
            anyhow::anyhow!("boom"),
            vec![
                out_artifact(&staged_tar),
                log_file_artifact(&log).expect("artifact"),
            ],
        );
        let slot = fill_ops(&slots, &m, sandbox.path(), &err, true, SlotIo::REAL).await;
        let tail = slot.log.expect("a tail");
        assert!(tail.path().is_none(), "material: in memory");
        assert_eq!(tail.tail().expect("tail").text, "secret-ish");
        let dir = slots.slot_dir(&m);
        assert!(dir.join("out/pkg/report/index.html").is_file());
        assert!(!dir.join(LOG).exists(), "a material log never reaches disk");
    }

    /// The tar is read only when it is a regular file inside the sandbox:
    /// anything else is left where it is, and no slot is made.
    #[tokio::test]
    async fn refuses_a_tar_outside_the_sandbox() {
        let home = tempfile::tempdir().expect("home");
        let sandbox = tempfile::tempdir().expect("sandbox");
        let outside = tempfile::tempdir().expect("outside");
        let slots = FailureSlots::new(home.path());
        let a = addr("//p:a");
        let elsewhere = stage(outside.path());
        let link = sandbox.path().join("link");
        std::os::unix::fs::symlink(&elsewhere, &link).expect("symlink");
        for (name, dir) in [
            ("outside", elsewhere.clone()),
            ("the sandbox itself", sandbox.path().to_path_buf()),
            ("symlink out", link),
            ("missing", sandbox.path().join("gone")),
        ] {
            let err = with_failure_artifacts(anyhow::anyhow!("boom"), vec![out_artifact(&dir)]);
            let slot = fill_ops(&slots, &a, sandbox.path(), &err, false, SlotIo::REAL).await;
            assert!(slot.out.is_none() && slot.log.is_none(), "{name}");
            assert_eq!(slots.read(&a), None, "{name}");
        }
        assert!(elsewhere.is_file(), "a refused tar is never removed");
        assert!(sandbox.path().is_dir());
    }

    /// I7: a failed unpack of `out/` — or no slot at all — never hides the
    /// target's error, and the log slot is kept where it can be. The owned
    /// tar is freed either way.
    #[tokio::test]
    async fn failed_output_copy_keeps_the_failure() {
        // The unpack dies half way, `out/` partly written.
        let fail_half_way = SlotIo {
            unpack: |tar, to| {
                (SlotIo::REAL.unpack)(tar, to)?;
                assert!(to.join("pkg/report/index.html").is_file());
                Err(anyhow::anyhow!("ENOSPC"))
            },
            ..SlotIo::REAL
        };
        for (name, ops) in [("up front", FAIL_UP_FRONT), ("half way", fail_half_way)] {
            let home = tempfile::tempdir().expect("home");
            let sandbox = tempfile::tempdir().expect("sandbox");
            let slots = FailureSlots::new(home.path());
            let a = addr("//p:a");
            let staged_tar = stage(sandbox.path());
            let err = with_failure_artifacts(
                anyhow::anyhow!("the target's own error"),
                vec![out_artifact(&staged_tar), raw_log(b"why\n")],
            );
            let slot = fill_ops(&slots, &a, sandbox.path(), &err, false, ops).await;
            assert!(slot.out.is_none(), "{name}");
            assert!(slot.out_error.is_some(), "{name}");
            let log = slot.log.clone().expect("the log");
            assert!(log.path().is_some(), "{name}: the log slot is kept");
            assert_eq!(log.tail().expect("tail").text, "why", "{name}");
            let dir = slots.slot_dir(&a);
            assert!(!dir.join(OUT).exists(), "{name}: no partial out/");
            assert!(!staged_tar.exists(), "{name}: the owned tar is freed");
            assert!(slots.read(&a).is_some(), "{name}");
            let e = err.context(slot);
            assert_eq!(
                hcore::hmemoizer::chain_message(&e),
                "the target's own error",
                "{name}"
            );

            // Nothing but `out/` to keep: no slot, and still the error.
            let b = addr("//p:b");
            let staged_tar = stage(sandbox.path());
            let err = with_failure_artifacts(
                anyhow::anyhow!("the target's own error"),
                vec![out_artifact(&staged_tar)],
            );
            let slot = fill_ops(&slots, &b, sandbox.path(), &err, false, ops).await;
            assert!(slot.out.is_none() && slot.log.is_none(), "{name}");
            assert!(slot.out_error.is_some(), "{name}");
            assert_eq!(slots.read(&b), None, "{name}");
            let names: Vec<String> = std::fs::read_dir(slots.root())
                .expect("ls")
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, [addr_key(&a)], "{name}: no temporary left behind");
        }
    }

    /// A run with no log on disk — a material target — and fixes written
    /// keeps its `fixes.txt` when unpacking `out/` fails: the slot is kept
    /// without `out/`, not dropped.
    #[tokio::test]
    async fn failed_unpack_keeps_a_no_log_slots_fixes() {
        use hplugin::error::FailureFixes;
        let home = tempfile::tempdir().expect("home");
        let sandbox = tempfile::tempdir().expect("sandbox");
        let slots = FailureSlots::new(home.path());
        let a = addr("//auth:fetch");
        let staged_tar = stage(sandbox.path());
        let log = sandbox.path().join("log.txt");
        std::fs::write(&log, "secret-ish\n").expect("log");
        let err = with_failure_artifacts(
            anyhow::anyhow!("boom"),
            vec![
                out_artifact(&staged_tar),
                log_file_artifact(&log).expect("artifact"),
            ],
        );
        let paths = vec!["auth/a.go".to_owned()];
        let slot = slots
            .fill_with(
                Fill {
                    addr: &a,
                    hashin: "h",
                    roots: vec![sandbox.path().to_path_buf()],
                    err: &err,
                    lines: 10,
                    material: true,
                    keep_log: true,
                    scrub: Default::default(),
                    fixes: Some(FailureFixes::Written {
                        root: sandbox.path().to_path_buf(),
                        paths: paths.clone(),
                        list: None,
                    }),
                },
                None,
                FAIL_UP_FRONT,
            )
            .await;
        let dir = slots.slot_dir(&a);
        assert!(slot.out.is_none());
        assert!(slot.out_error.is_some());
        assert!(
            slot.log.expect("a tail").path().is_none(),
            "material: in memory"
        );
        assert_eq!(
            slot.fixes,
            Some(FailureFixes::Written {
                root: sandbox.path().to_path_buf(),
                paths,
                list: Some(dir.join(FIXES)),
            })
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(FIXES)).expect("fixes.txt"),
            "auth/a.go\n"
        );
        assert!(!dir.join(OUT).exists(), "no partial out/");
        assert!(!dir.join(LOG).exists(), "a material log never reaches disk");
        assert_eq!(slots.read(&a).map(|m| m.addr), Some(a.format()));
    }

    /// The tar's files are unpacked, a link member is not, and the owned tar
    /// is freed; a borrowed one is left alone.
    #[tokio::test]
    async fn failure_out_unpacks_and_frees_the_owned_tar() {
        let home = tempfile::tempdir().expect("home");
        let sandbox = tempfile::tempdir().expect("sandbox");
        let links = tempfile::tempdir().expect("links");
        let slots = FailureSlots::new(home.path());
        let a = addr("//p:a");
        let link = links.path().join("latest.html");
        std::os::unix::fs::symlink("index.html", &link).expect("symlink");
        let staged_tar = stage_with(sandbox.path(), |tar| {
            tar.create_file(link.to_str().expect("utf-8"), "pkg/report/latest.html");
        });
        let err = with_failure_artifacts(anyhow::anyhow!("boom"), vec![out_artifact(&staged_tar)]);
        let slot = fill_ops(&slots, &a, sandbox.path(), &err, false, SlotIo::REAL).await;
        let out = slot.out.expect("unpacked");
        assert!(slot.out_error.is_none(), "{:?}", slot.out_error);
        assert_eq!(
            std::fs::read(out.join("pkg/report/index.html")).expect("index"),
            b"<html>"
        );
        assert_eq!(
            std::fs::read(out.join("pkg/report/data/trace.zip")).expect("trace"),
            b"zip"
        );
        assert!(
            std::fs::symlink_metadata(out.join("pkg/report/latest.html")).is_err(),
            "a link member is never unpacked"
        );
        assert!(!staged_tar.exists(), "the owned tar is freed");

        let staged_tar = stage(sandbox.path());
        let mut borrowed = out_artifact(&staged_tar);
        if let Content::TarPath(t) = &mut borrowed.content {
            t.owned = false;
        }
        let err = with_failure_artifacts(anyhow::anyhow!("boom"), vec![borrowed]);
        let slot = fill_ops(&slots, &a, sandbox.path(), &err, false, SlotIo::REAL).await;
        assert!(slot.out.is_some());
        assert!(
            staged_tar.is_file(),
            "a borrowed tar is the driver's to remove"
        );
    }

    /// A raw tar header for `name`, written byte for byte: the `tar` crate's
    /// builder refuses a `..` component, which is what this test feeds.
    fn raw_header(name: &str, kind: tar::EntryType, size: u64, link: &str) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        let gnu = h.as_gnu_mut().expect("gnu header");
        for (slot, b) in gnu.name.iter_mut().zip(name.bytes()) {
            *slot = b;
        }
        for (slot, b) in gnu.linkname.iter_mut().zip(link.bytes()) {
            *slot = b;
        }
        h.set_entry_type(kind);
        h.set_size(size);
        h.set_mode(0o644);
        h.set_cksum();
        h
    }

    /// Every path under `dir`, links not followed, relative to `dir`, with
    /// whether it is a link.
    fn tree(dir: &Path) -> Vec<(PathBuf, bool)> {
        let mut found = Vec::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(d) = todo.pop() {
            for e in std::fs::read_dir(&d).expect("ls") {
                let p = e.expect("entry").path();
                let ft = std::fs::symlink_metadata(&p).expect("lstat").file_type();
                if ft.is_dir() {
                    todo.push(p.clone());
                }
                let rel = p.strip_prefix(dir).expect("under dir").to_path_buf();
                found.push((rel, ft.is_symlink()));
            }
        }
        found.sort();
        found
    }

    /// A `failure_out` tar the managed layer never packs — a `../` member, a
    /// link to `../../..` and a file written through that link — unpacked
    /// through the real fill: nothing lands outside `out/`, and no link is
    /// created.
    #[tokio::test]
    async fn failure_out_never_writes_outside_out() {
        let root = tempfile::tempdir().expect("root");
        let home = root.path().join("home");
        let sandbox = root.path().join("sandbox");
        std::fs::create_dir_all(&sandbox).expect("sandbox");
        let slots = FailureSlots::new(&home);
        let a = addr("//p:a");
        let tar_path = stage_with(&sandbox, |_| {});
        let mut b = tar::Builder::new(std::fs::File::create(&tar_path).expect("tar"));
        for (name, kind, data, link) in [
            ("ok.txt", tar::EntryType::Regular, &b"ok"[..], ""),
            (
                "../escape.txt",
                tar::EntryType::Regular,
                &b"escaped"[..],
                "",
            ),
            ("l", tar::EntryType::Symlink, &b""[..], "../../.."),
            ("l/x.txt", tar::EntryType::Regular, &b"through"[..], ""),
        ] {
            let h = raw_header(name, kind, data.len() as u64, link);
            b.append(&h, data).expect("append");
        }
        b.finish().expect("finish");
        drop(b);

        let err = with_failure_artifacts(anyhow::anyhow!("boom"), vec![out_artifact(&tar_path)]);
        let slot = fill_ops(&slots, &a, &sandbox, &err, false, SlotIo::REAL).await;
        let out = slot.out.expect("unpacked");
        assert!(slot.out_error.is_none(), "{:?}", slot.out_error);
        assert_eq!(
            tree(&out),
            [
                (PathBuf::from("l"), false),
                (PathBuf::from("l/x.txt"), false),
                (PathBuf::from("ok.txt"), false),
            ],
            "the files, `l` a plain directory, nothing else"
        );
        let out_rel = out.strip_prefix(root.path()).expect("out under root");
        let strays: Vec<_> = tree(root.path())
            .into_iter()
            .filter(|(p, link)| {
                *link
                    || (!p.starts_with(out_rel)
                        && matches!(
                            p.file_name().and_then(|n| n.to_str()),
                            Some("escape.txt" | "x.txt")
                        ))
            })
            .collect();
        assert!(strays.is_empty(), "written outside out/: {strays:?}");
    }

    /// The `.tmp-` directories under the slots root.
    fn temp_dirs(root: &Path) -> Vec<String> {
        std::fs::read_dir(root)
            .expect("ls")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(TEMP_PREFIX))
            .collect()
    }

    /// D17: a fill dropped mid-way — the request cancelled while the blocking
    /// job unpacks `out/` into the slot — leaves no `.tmp-` directory behind.
    /// The job is detached, not killed: it runs to the end on its own and
    /// renames the slot into place.
    #[tokio::test]
    async fn cancel_mid_fill_leaves_no_tmp_dir() {
        use std::sync::{Condvar, Mutex, PoisonError};
        // `SlotIo` holds plain fns, so the gate is a static; only this test
        // uses it.
        static ENTERED: tokio::sync::Notify = tokio::sync::Notify::const_new();
        static RELEASE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
        let gated = SlotIo {
            unpack: |tar, to| {
                ENTERED.notify_one();
                let (lock, cvar) = &RELEASE;
                let mut released = lock.lock().unwrap_or_else(PoisonError::into_inner);
                while !*released {
                    released = cvar.wait(released).unwrap_or_else(PoisonError::into_inner);
                }
                drop(released);
                (SlotIo::REAL.unpack)(tar, to)
            },
            ..SlotIo::REAL
        };
        let home = tempfile::tempdir().expect("home");
        let sandbox = tempfile::tempdir().expect("sandbox");
        let slots = FailureSlots::new(home.path());
        let a = addr("//p:a");
        let staged_tar = stage(sandbox.path());
        let err = with_failure_artifacts(
            anyhow::anyhow!("boom"),
            vec![out_artifact(&staged_tar), raw_log(b"why\n")],
        );
        tokio::select! {
            _ = fill_ops(&slots, &a, sandbox.path(), &err, false, gated) => {
                panic!("the fill finished past a closed gate");
            }
            () = ENTERED.notified() => {}
        }
        // The fill's future is gone; its job is parked in `out/`'s unpack.
        assert_eq!(
            temp_dirs(slots.root()).len(),
            1,
            "the job is mid-fill, its temporary in place"
        );

        {
            let (lock, cvar) = &RELEASE;
            *lock.lock().unwrap_or_else(PoisonError::into_inner) = true;
            cvar.notify_all();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while slots.read(&a).is_none() || !temp_dirs(slots.root()).is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the detached fill never finished: {:?}",
                temp_dirs(slots.root())
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let dir = slots.slot_dir(&a);
        assert!(dir.join("out/pkg/report/index.html").is_file());
        assert_eq!(std::fs::read(dir.join(LOG)).expect("log"), b"why\n");
        assert!(temp_dirs(slots.root()).is_empty(), "no `.tmp-` left behind");
    }
}
