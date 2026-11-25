//! The snapshot export: build a new generation of `<dir>` from a job's
//! working set and point the marker at it (the CI cache snapshots spec, "A
//! job, end to end" step 3, and "Layout, whole revisions, integrity").
//!
//! # Order
//!
//! 1. An empty working set leaves `<dir>` untouched: no sweep, no write.
//! 2. Sweep `<dir>`: every `gen-*` entry and marker temp file the marker does
//!    not name goes. An unreadable or newer marker names nothing, so every
//!    generation goes with it — a clean break, never a refusal.
//! 3. Index the previous generation: every `manifest-v2.borsh` under it whose
//!    target and `hashin` give back its own directory. Any other manifest
//!    name is one this heph does not export.
//! 4. Build `gen-<nonce>` revision by revision, each under its per-addr read
//!    lock, with bounded concurrency on the blocking pool.
//! 5. Publish the job's scratch slots into it under `scratch/v1/`, with
//!    `scratch_push`'s rules ([`Engine::scratch_publish`]); on an incomplete
//!    run, carry over the previous generation's entries of slots it did not
//!    export.
//! 6. Point the marker at it (written last, by rename), then sweep again.
//!
//! A crash anywhere before the marker moves leaves the previous generation
//! current; the next export's sweep removes the half-built one.
//!
//! # Whole revisions
//!
//! A revision goes in with every non-log blob, or not at all. Its manifest is
//! the local one, re-encoded through [`RemoteManifest::from_local`], the
//! upload's own builder. Each blob is hard-linked from the previous
//! generation when that generation's entry for it is equal on type, content
//! type, group, hashout, size and encoding and is a regular file, else copied from the
//! local cache — never linked out of `<home>/cache/blobs/`. Every placed blob
//! is checked: a raw one's length against `size`, a zstd one decoded through
//! its frame checksums. Anything that fails leaves the whole revision out,
//! counted by [`LeftOut`] reason.

use super::marker::{
    GENERATION_PREFIX, GenerationStats, MARKER_TMP_PREFIX, NoTier, read_marker, write_marker_with,
};
use super::{RecordedRevision, WorkingSet, working_set_dir};
use crate::engine::Engine;
use crate::engine::cache_codec;
use crate::engine::local_cache::{
    LocalCache, MANIFEST_V1, MANIFEST_V2, Manifest, ManifestArtifactEncoding, ManifestArtifactType,
    NotFoundError,
};
use crate::engine::remote_cache::{RemoteCacheSet, RemoteManifest, RemoteManifestArtifact};
use anyhow::Context as _;
use futures::StreamExt as _;
use hcore::hasync::Cancellable;
use hmodel::htaddr::{Addr, parse_addr};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The snapshot lock's file name, under the working-set dir.
pub const SNAPSHOT_LOCK_FILE: &str = "snapshot.lock";

/// How many revisions are exported at once.
const REVISION_CONCURRENCY: usize = 16;

/// How many revisions the report lists by size.
const TOP_REVISIONS: usize = 20;

/// The exclusive `<home>/cache/working-set/snapshot.lock`, held for a whole
/// `snapshot` command so two never overlap. Released on drop. Holding one is
/// what [`Engine::export_snapshot`] takes as proof that no other export runs.
#[derive(Debug)]
pub struct SnapshotLock {
    _file: std::fs::File,
}

fn open_lock_file(home: &Path) -> anyhow::Result<std::fs::File> {
    let dir = working_set_dir(home);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(SNAPSHOT_LOCK_FILE);
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))
}

/// Take the snapshot lock, waiting for a running export to finish.
pub fn lock_snapshot(home: &Path) -> anyhow::Result<SnapshotLock> {
    let file = open_lock_file(home)?;
    file.lock().context("locking the cache snapshot")?;
    Ok(SnapshotLock { _file: file })
}

/// Take the snapshot lock if no export holds it; `None` when one does.
pub fn try_lock_snapshot(home: &Path) -> anyhow::Result<Option<SnapshotLock>> {
    let file = open_lock_file(home)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(SnapshotLock { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).context("locking the cache snapshot"),
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// What an export did to `<dir>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExportOutcome {
    /// A new generation is current.
    Written { generation: String },
    /// The working set was empty: `<dir>` is as it was.
    Unchanged,
}

/// A count of revisions and their bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Tally {
    pub count: u64,
    pub bytes: u64,
}

impl Tally {
    fn add(&mut self, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
    }
}

/// Why a revision was left out of the new generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeftOut {
    /// The local cache no longer holds the revision: gc trimmed it after it
    /// was recorded.
    Trimmed,
    /// A non-log blob is in neither the local cache nor, with an equal
    /// entry, the previous generation.
    MissingBlob,
    /// A blob failed its integrity check: a raw blob's length is not its
    /// `size`, or a zstd blob does not decode to it.
    Corrupt,
    /// The manifest's target or `hashin` is not its key's.
    KeyMismatch,
    /// Two blob names are equal ignoring case, which alias on a
    /// case-insensitive filesystem.
    CaseCollision,
    /// A blob is stored in an encoding the snapshot does not carry (gzip, or
    /// a v1 blob that is not raw).
    Unsupported,
    /// A previous-generation entry under a manifest name this heph does not
    /// export, or one whose manifest does not decode.
    UnknownManifest,
}

impl LeftOut {
    pub fn as_str(self) -> &'static str {
        match self {
            LeftOut::Trimmed => "trimmed",
            LeftOut::MissingBlob => "missing_blob",
            LeftOut::Corrupt => "corrupt",
            LeftOut::KeyMismatch => "key_mismatch",
            LeftOut::CaseCollision => "case_collision",
            LeftOut::Unsupported => "unsupported",
            LeftOut::UnknownManifest => "unknown_manifest",
        }
    }
}

impl std::fmt::Display for LeftOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One revision of the new generation, by size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RevisionBytes {
    pub addr: String,
    pub hashin: String,
    pub bytes: u64,
}

/// A journal that made the run incomplete, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IncompleteJournal {
    pub journal: PathBuf,
    pub reason: String,
}

/// What [`Engine::export_snapshot`] did. Bytes are those stored in the
/// generation (a zstd blob counts compressed), except for left-out
/// revisions, which count the decoded sizes their manifest records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportReport {
    pub outcome: ExportOutcome,
    /// Recorded revisions the previous generation also held.
    pub kept: Tally,
    /// Recorded revisions the previous generation did not hold.
    pub new: Tally,
    /// On an incomplete run, previous-generation revisions the job did not
    /// touch, carried into the new generation.
    pub carried_over: Tally,
    /// Previous-generation revisions the new generation does not hold
    /// because the job did not use them.
    pub dropped: Tally,
    /// Revisions that could not be completed, by reason.
    pub left_out: BTreeMap<LeftOut, Tally>,
    /// The journals the working set was read from.
    pub journals: Vec<PathBuf>,
    /// Why the run was incomplete; empty for a complete run.
    pub incomplete: Vec<IncompleteJournal>,
    /// The largest revisions written, by bytes, ties by addr.
    pub top: Vec<RevisionBytes>,
    /// The targets of the dropped revisions, sorted.
    pub dropped_addrs: Vec<String>,
    /// The scratch slots, counted by entry (a slot in one scope).
    pub scratch: ScratchReport,
}

/// What an export did with scratch slots. Bytes are the archives'.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ScratchReport {
    /// Slots the job used, published as a new generation of its scope.
    pub new: Tally,
    /// Slots the job used whose contents its lineage already held, written
    /// under that entry: no new generation.
    pub kept: Tally,
    /// On an incomplete run, previous-generation entries of slots the job
    /// exported nothing of.
    pub carried_over: Tally,
    /// Previous-generation entries the new generation does not hold.
    pub dropped: Tally,
    /// Slots the job used that were not exported: opted out of the remote
    /// cache, no longer on disk, or unreadable.
    pub skipped: u64,
}

impl ExportReport {
    fn new(ws: &WorkingSet) -> Self {
        ExportReport {
            outcome: ExportOutcome::Unchanged,
            kept: Tally::default(),
            new: Tally::default(),
            carried_over: Tally::default(),
            dropped: Tally::default(),
            left_out: BTreeMap::new(),
            journals: ws.journals.clone(),
            incomplete: ws
                .incomplete
                .iter()
                .map(|(journal, why)| IncompleteJournal {
                    journal: journal.clone(),
                    reason: why.to_string(),
                })
                .collect(),
            top: Vec::new(),
            dropped_addrs: Vec::new(),
            scratch: ScratchReport::default(),
        }
    }

    fn leave_out(&mut self, why: LeftOut, bytes: u64) {
        self.left_out.entry(why).or_default().add(bytes);
    }
}

// ---------------------------------------------------------------------------
// Sweep
// ---------------------------------------------------------------------------

/// Remove every `gen-*` entry and marker temp file of `dir` the marker does
/// not name, and return the generation it does name, if any. An unreadable
/// or newer marker names none, so every generation goes.
fn sweep(dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    let current = match read_marker(dir) {
        Ok(generation) => Some(generation),
        Err(NoTier::NoDir | NoTier::NoMarker) => None,
        Err(reason) => {
            tracing::warn!(
                dir = %dir.display(),
                format = ?reason.format(),
                reason = %reason,
                "cache snapshot: replacing a snapshot this heph cannot read",
            );
            None
        }
    };
    let keep = current
        .as_ref()
        .map(|g| std::ffi::OsString::from(&g.marker.current));
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
        let name = entry.file_name();
        if keep.as_ref() == Some(&name) {
            continue;
        }
        let Some(s) = name.to_str() else { continue };
        if !(s.starts_with(GENERATION_PREFIX) || s.starts_with(MARKER_TMP_PREFIX)) {
            continue;
        }
        let path = entry.path();
        let is_dir = entry
            .file_type()
            .with_context(|| format!("reading the type of {}", path.display()))?
            .is_dir();
        let removed = if is_dir {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match removed {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
    Ok(current.map(|g| g.path))
}

// ---------------------------------------------------------------------------
// The previous generation
// ---------------------------------------------------------------------------

/// One revision directory of the previous generation.
#[derive(Debug)]
struct PrevEntry {
    /// `Some` when the manifest is a `manifest-v2.borsh` that decodes and
    /// whose target and `hashin` give back this directory.
    revision: Option<(Addr, RemoteManifest)>,
    /// Why `revision` is `None`.
    unusable: Option<LeftOut>,
    /// The manifest's target, when it decoded.
    target: Option<String>,
    /// Every regular file directly in the directory.
    bytes: u64,
}

/// The previous generation's revisions, keyed by their path under it
/// (`revision_prefix`, `/`-separated).
#[derive(Debug, Default)]
struct PrevIndex {
    entries: FxHashMap<String, PrevEntry>,
}

fn index_generation(root: &Path) -> anyhow::Result<PrevIndex> {
    let mut index = PrevIndex::default();
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("listing {}", dir.display()))?;
        let mut manifests: Vec<String> = Vec::new();
        let mut bytes = 0u64;
        for entry in entries {
            let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let ty = entry
                .file_type()
                .with_context(|| format!("reading the type of {}", entry.path().display()))?;
            if ty.is_dir() {
                let child = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                stack.push((entry.path(), child));
            } else if ty.is_file() {
                if name.starts_with("manifest-") {
                    manifests.push(name);
                } else if let Ok(m) = entry.metadata() {
                    bytes += m.len();
                }
            }
        }
        if manifests.is_empty() {
            continue;
        }
        let entry = if manifests.iter().any(|m| m == MANIFEST_V2) {
            read_prev_manifest(&dir.join(MANIFEST_V2), &rel, bytes)
        } else {
            PrevEntry {
                revision: None,
                unusable: Some(LeftOut::UnknownManifest),
                target: None,
                bytes,
            }
        };
        index.entries.insert(rel, entry);
    }
    Ok(index)
}

fn read_prev_manifest(path: &Path, rel: &str, bytes: u64) -> PrevEntry {
    let unusable = |why, target| PrevEntry {
        revision: None,
        unusable: Some(why),
        target,
        bytes,
    };
    let Ok(raw) = std::fs::read(path) else {
        return unusable(LeftOut::UnknownManifest, None);
    };
    let Ok(manifest) = borsh::from_slice::<RemoteManifest>(&raw) else {
        return unusable(LeftOut::UnknownManifest, None);
    };
    let target = Some(manifest.target.clone());
    let Ok(addr) = parse_addr(&manifest.target) else {
        return unusable(LeftOut::KeyMismatch, target);
    };
    if RemoteCacheSet::revision_prefix(&addr, &manifest.hashin) != rel {
        return unusable(LeftOut::KeyMismatch, target);
    }
    PrevEntry {
        revision: Some((addr, manifest)),
        unusable: None,
        target,
        bytes,
    }
}

// ---------------------------------------------------------------------------
// One revision
// ---------------------------------------------------------------------------

/// Why one revision is in the new generation, which the report counts it as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Kept,
    New,
    CarriedOver,
}

/// One revision to export.
struct Job {
    addr: Addr,
    hashin: String,
    kind: Kind,
    /// The previous generation's manifest for it, to fill from.
    prev: Option<Arc<RemoteManifest>>,
}

/// What became of one [`Job`].
enum Placed {
    /// Written, with its bytes in the generation.
    Written(u64),
    /// Left out, with the decoded bytes its manifest records.
    LeftOut(LeftOut, u64),
}

/// Paths one revision is exported between.
struct Roots {
    generation: PathBuf,
    prev: Option<PathBuf>,
}

/// The manifest a revision is exported under, and for each artifact the
/// name the local cache stores its blob under.
struct Reference {
    manifest: RemoteManifest,
    local_names: Vec<Option<String>>,
}

/// Pick the manifest `job` is exported under: the local one (a v1 converted
/// to v2 names), or for a carry-over the local cache no longer holds, the
/// previous generation's. A local manifest that does not read or decode
/// leaves the revision out as [`LeftOut::Corrupt`], like a corrupt blob.
fn reference(cache: &Arc<dyn LocalCache>, job: &Job) -> Result<Reference, (LeftOut, u64)> {
    let local = match Engine::read_newest_manifest(cache, &job.addr, &job.hashin) {
        Ok(local) => local,
        Err(e) => {
            tracing::warn!(
                addr = %job.addr, hashin = %job.hashin, error = %format!("{e:#}"),
                "cache snapshot: the local manifest does not read; leaving the revision out",
            );
            return Err((LeftOut::Corrupt, 0));
        }
    };
    let Some((name, local)) = local else {
        return match (&job.prev, job.kind) {
            (Some(prev), Kind::CarriedOver) => Ok(Reference {
                manifest: (**prev).clone(),
                local_names: vec![None; prev.artifacts.len()],
            }),
            _ => Err((LeftOut::Trimmed, 0)),
        };
    };
    let decoded: u64 = local.artifacts.iter().map(|a| a.size).sum();
    if local.target != job.addr.format() || local.hashin != job.hashin {
        return Err((LeftOut::KeyMismatch, decoded));
    }
    match name {
        MANIFEST_V1 => {
            // A v1 manifest names bare blobs, every one raw: each goes under
            // its v2 name for that encoding, as `duplicate_cache_entry`
            // converts it.
            let mut converted: Manifest = local.clone();
            let mut local_names = Vec::with_capacity(local.artifacts.len());
            for a in &mut converted.artifacts {
                if a.encoding != ManifestArtifactEncoding::None {
                    return Err((LeftOut::Unsupported, decoded));
                }
                let v2 = cache_codec::entry_name(&a.name, &ManifestArtifactEncoding::None);
                local_names.push(Some(std::mem::replace(&mut a.name, v2)));
            }
            Ok(Reference {
                manifest: RemoteManifest::from_local(&converted, &job.hashin),
                local_names,
            })
        }
        MANIFEST_V2 => Ok(Reference {
            manifest: RemoteManifest::from_local(&local, &job.hashin),
            local_names: local
                .artifacts
                .iter()
                .map(|a| Some(a.name.clone()))
                .collect(),
        }),
        // A manifest name a later heph adds: not one this export converts.
        _ => Err((LeftOut::Unsupported, decoded)),
    }
}

/// Whether a previous-generation entry may stand in for `want`.
fn same_entry(prev: &RemoteManifestArtifact, want: &RemoteManifestArtifact) -> bool {
    prev.name == want.name
        && prev.r#type == want.r#type
        && prev.content_type == want.content_type
        && prev.group == want.group
        && prev.hashout == want.hashout
        && prev.size == want.size
        && prev.encoding == want.encoding
}

/// Whether the blob at `path` holds what `artifact` records.
fn verify(path: &Path, artifact: &RemoteManifestArtifact) -> bool {
    match artifact.encoding {
        ManifestArtifactEncoding::None => {
            std::fs::symlink_metadata(path).is_ok_and(|m| m.len() == artifact.size)
        }
        ManifestArtifactEncoding::Zstd => {
            let decoded = std::fs::File::open(path)
                .map_err(anyhow::Error::from)
                .and_then(|f| {
                    cache_codec::decode_reader(
                        Box::new(io::BufReader::new(f)),
                        &ManifestArtifactEncoding::Zstd,
                    )
                })
                .and_then(|mut r| Ok(io::copy(&mut r, &mut io::sink())?));
            decoded.is_ok_and(|n| n == artifact.size)
        }
        ManifestArtifactEncoding::Gzip => false,
    }
}

/// Place one blob at `dest`: linked or copied from the previous generation
/// when its entry there is the same, else copied from the local cache.
/// `Ok(false)` when neither has it.
fn place_blob(
    cache: &Arc<dyn LocalCache>,
    job: &Job,
    roots: &Roots,
    artifact: &RemoteManifestArtifact,
    local_name: Option<&str>,
    key: &str,
    dest: &Path,
) -> anyhow::Result<bool> {
    if let (Some(prev), Some(prev_root)) = (&job.prev, &roots.prev)
        && prev.artifacts.iter().any(|p| same_entry(p, artifact))
    {
        let src = prev_root.join(key);
        // Only a regular file is linked: a symlink would point out of `<dir>`.
        if std::fs::symlink_metadata(&src).is_ok_and(|m| m.is_file())
            && (std::fs::hard_link(&src, dest).is_ok() || std::fs::copy(&src, dest).is_ok())
        {
            return Ok(true);
        }
        // A failed copy may leave a partial file: the local cache's copy
        // replaces it.
        drop(std::fs::remove_file(dest));
    }
    let Some(local_name) = local_name else {
        return Ok(false);
    };
    let sized = match cache.reader(&job.addr, &job.hashin, local_name) {
        Ok(sized) => sized,
        Err(e) if e.is::<NotFoundError>() => return Ok(false),
        Err(e) => {
            tracing::debug!(addr = %job.addr, blob = local_name, error = ?e, "cache snapshot: local blob unreadable");
            return Ok(false);
        }
    };
    // Never a link into `<home>/cache/blobs/`: the local cache owns those
    // files and may rewrite them. A copy through its reader.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut reader = sized.reader.take(sized.size);
    match io::copy(&mut reader, &mut file) {
        Ok(_) => Ok(true),
        // A source that fails mid-read is a missing blob; a destination that
        // fails (ENOSPC) fails the export.
        Err(e) if e.kind() == io::ErrorKind::StorageFull => {
            Err(e).with_context(|| format!("writing {}", dest.display()))
        }
        Err(e) => {
            tracing::debug!(addr = %job.addr, blob = local_name, error = ?e, "cache snapshot: copying a local blob failed");
            Ok(false)
        }
    }
}

/// Export one revision into the new generation: whole, or not at all.
fn export_revision(
    cache: &Arc<dyn LocalCache>,
    job: &Job,
    roots: &Roots,
) -> anyhow::Result<Placed> {
    let reference = match reference(cache, job) {
        Ok(r) => r,
        Err((why, bytes)) => return Ok(Placed::LeftOut(why, bytes)),
    };
    let manifest = &reference.manifest;
    let decoded: u64 = manifest.artifacts.iter().map(|a| a.size).sum();
    // Every non-log blob, with the name the local cache stores it under and
    // its key in the generation. Logs stay in the manifest, uncarried: a
    // reader that wants one falls through to another tier.
    let carried: Vec<(&RemoteManifestArtifact, Option<&str>, String)> = manifest
        .artifacts
        .iter()
        .zip(&reference.local_names)
        .filter(|(a, _)| a.r#type != ManifestArtifactType::Log)
        .map(|(a, local)| {
            let key = RemoteCacheSet::key(&job.addr, &job.hashin, &a.name);
            (a, local.as_deref(), key)
        })
        .collect();
    if carried
        .iter()
        .any(|(a, _, _)| a.encoding == ManifestArtifactEncoding::Gzip)
    {
        return Ok(Placed::LeftOut(LeftOut::Unsupported, decoded));
    }
    let manifest_key = RemoteCacheSet::key(&job.addr, &job.hashin, MANIFEST_V2);
    let mut folded: FxHashSet<String> = FxHashSet::default();
    folded.insert(manifest_key.to_lowercase());
    if !carried
        .iter()
        .all(|(_, _, key)| folded.insert(key.to_lowercase()))
    {
        return Ok(Placed::LeftOut(LeftOut::CaseCollision, decoded));
    }

    let mut written: Vec<PathBuf> = Vec::with_capacity(carried.len() + 1);
    let undo = |written: &[PathBuf]| {
        for path in written {
            drop(std::fs::remove_file(path));
        }
    };
    let mut bytes = 0u64;
    for (artifact, local_name, key) in &carried {
        let (artifact, local_name) = (*artifact, *local_name);
        let dest = roots.generation.join(key);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let placed = match place_blob(cache, job, roots, artifact, local_name, key, &dest) {
            Ok(placed) => placed,
            Err(e) => {
                undo(&written);
                return Err(e);
            }
        };
        if !placed {
            drop(std::fs::remove_file(&dest));
            undo(&written);
            return Ok(Placed::LeftOut(LeftOut::MissingBlob, decoded));
        }
        written.push(dest.clone());
        if !verify(&dest, artifact) {
            undo(&written);
            return Ok(Placed::LeftOut(LeftOut::Corrupt, decoded));
        }
        bytes += std::fs::symlink_metadata(&dest).map_or(0, |m| m.len());
    }
    let encoded = manifest.encode()?;
    let dest = roots.generation.join(&manifest_key);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if let Err(e) = std::fs::write(&dest, &encoded) {
        undo(&written);
        return Err(e).with_context(|| format!("writing {}", dest.display()));
    }
    bytes += encoded.len() as u64;
    Ok(Placed::Written(bytes))
}

// ---------------------------------------------------------------------------
// The export
// ---------------------------------------------------------------------------

impl Engine {
    /// Build a new generation of the snapshot at `dir` from `ws` and point
    /// the marker at it; see the [module docs](self) for the order.
    ///
    /// `held` is the snapshot lock: no other export runs while this one
    /// does. The caller also holds the lock of every journal `ws` was read
    /// from, and deletes those journals once this returns `Ok`. An empty
    /// working set leaves `dir` untouched ([`ExportOutcome::Unchanged`]).
    pub async fn export_snapshot(
        &self,
        held: &SnapshotLock,
        dir: &Path,
        ws: &WorkingSet,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ExportReport> {
        self.export_with(held, dir, ws, ctoken, |_| Ok(())).await
    }

    /// [`Self::export_snapshot`], calling `before_pointer` with the complete
    /// new generation just before the marker moves: the fault hook for a
    /// crash at the last moment.
    async fn export_with(
        &self,
        _held: &SnapshotLock,
        dir: &Path,
        ws: &WorkingSet,
        ctoken: &(dyn Cancellable + Send + Sync),
        before_pointer: impl FnOnce(&Path) -> anyhow::Result<()>,
    ) -> anyhow::Result<ExportReport> {
        let mut report = ExportReport::new(ws);
        if ws.is_empty() {
            return Ok(report);
        }
        let incomplete = !ws.is_complete();

        let (prev, index) = {
            let dir = dir.to_path_buf();
            hcore::blocking::run(move || -> anyhow::Result<_> {
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                let prev = sweep(&dir)?;
                let index = match &prev {
                    Some(prev) => index_generation(prev)?,
                    None => PrevIndex::default(),
                };
                Ok((prev, index))
            })
            .await?
        };
        let PrevIndex { mut entries } = index;

        let generation_name = format!("{GENERATION_PREFIX}{}", uuid::Uuid::new_v4().simple());
        let generation = dir.join(&generation_name);
        std::fs::create_dir(&generation)
            .with_context(|| format!("creating {}", generation.display()))?;

        // The recorded revisions, each once, then on an incomplete run every
        // previous-generation revision the job did not touch.
        let mut jobs: Vec<Job> = Vec::with_capacity(ws.revisions.len());
        for RecordedRevision { addr, hashin } in &ws.revisions {
            let prefix = RemoteCacheSet::revision_prefix(addr, hashin);
            let prev = entries.remove(&prefix);
            let kind = if prev.is_some() {
                Kind::Kept
            } else {
                Kind::New
            };
            jobs.push(Job {
                addr: addr.clone(),
                hashin: hashin.clone(),
                kind,
                prev: prev.and_then(|e| e.revision).map(|(_, m)| Arc::new(m)),
            });
        }
        let mut rest: Vec<(String, PrevEntry)> = entries.into_iter().collect();
        rest.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, entry) in rest {
            if !incomplete {
                report.dropped.add(entry.bytes);
                if let Some(target) = entry.target {
                    report.dropped_addrs.push(target);
                }
                continue;
            }
            match entry.revision {
                Some((addr, manifest)) => jobs.push(Job {
                    hashin: manifest.hashin.clone(),
                    addr,
                    kind: Kind::CarriedOver,
                    prev: Some(Arc::new(manifest)),
                }),
                None => report.leave_out(
                    entry.unusable.unwrap_or(LeftOut::UnknownManifest),
                    entry.bytes,
                ),
            }
        }
        report.dropped_addrs.sort();

        let roots = Arc::new(Roots {
            generation: generation.clone(),
            prev,
        });
        let results: Vec<anyhow::Result<(Job, Placed)>> = futures::stream::iter(jobs)
            .map(|job| {
                let roots = roots.clone();
                let cache = self.local_cache.clone();
                async move {
                    let _guard = self
                        .result_lock()
                        .read(&job.addr, &job.hashin, ctoken)
                        .await
                        .with_context(|| format!("locking {} {}", job.addr, job.hashin))?;
                    hcore::blocking::run(move || {
                        let placed = export_revision(&cache, &job, &roots)?;
                        Ok((job, placed))
                    })
                    .await
                }
            })
            .buffer_unordered(REVISION_CONCURRENCY)
            .collect()
            .await;
        anyhow::ensure!(!ctoken.is_cancelled(), "cache snapshot export cancelled");

        let mut top: Vec<RevisionBytes> = Vec::new();
        for result in results {
            let (job, placed) = result?;
            match placed {
                Placed::Written(bytes) => {
                    match job.kind {
                        Kind::Kept => report.kept.add(bytes),
                        Kind::New => report.new.add(bytes),
                        Kind::CarriedOver => report.carried_over.add(bytes),
                    }
                    top.push(RevisionBytes {
                        addr: job.addr.format(),
                        hashin: job.hashin,
                        bytes,
                    });
                }
                Placed::LeftOut(why, bytes) => {
                    tracing::debug!(addr = %job.addr, hashin = %job.hashin, reason = %why, "cache snapshot: revision left out");
                    report.leave_out(why, bytes);
                }
            }
        }
        top.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then_with(|| a.addr.cmp(&b.addr))
                .then_with(|| a.hashin.cmp(&b.hashin))
        });
        top.truncate(TOP_REVISIONS);
        report.top = top;

        // Scratch slots, before the pointer moves, so they are part of the
        // generation it names.
        report.scratch = self
            .export_scratch(ws, &generation, roots.prev.as_deref(), incomplete, ctoken)
            .await?;

        before_pointer(&generation)?;
        {
            let (dir, name) = (dir.to_path_buf(), generation_name.clone());
            // What the generation holds, for the next job's start-of-run line.
            let revisions = [&report.kept, &report.new, &report.carried_over];
            let scratch = &report.scratch;
            let stats = GenerationStats {
                revisions: revisions
                    .iter()
                    .fold(0u64, |n, t| n.saturating_add(t.count)),
                bytes: revisions
                    .iter()
                    .chain([&scratch.new, &scratch.kept, &scratch.carried_over].iter())
                    .fold(0u64, |n, t| n.saturating_add(t.bytes)),
            };
            hcore::blocking::run(move || -> anyhow::Result<()> {
                write_marker_with(&dir, &name, incomplete, Some(stats))?;
                sweep(&dir)?;
                Ok(())
            })
            .await?;
        }
        report.outcome = ExportOutcome::Written {
            generation: generation_name,
        };
        Ok(report)
    }

    /// Write the job's scratch slots into `generation`, then carry or drop
    /// the previous generation's.
    ///
    /// Each slot the job used is published in this engine's scope with
    /// [`Engine::scratch_publish`]'s rules, from its local head and against
    /// the lineage meta it descends from: `parent + 1`, or the parent's own
    /// entry when the contents are unchanged. A slot that opted out of the
    /// remote cache (or whose slot meta is gone, so it cannot say) is not
    /// carried. One slot failing to pack is skipped and counted; a failed
    /// write into `<dir>` (ENOSPC) fails the export.
    async fn export_scratch(
        &self,
        ws: &WorkingSet,
        generation: &Path,
        prev: Option<&Path>,
        incomplete: bool,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ScratchReport> {
        use crate::engine::scratch_remote::{ScratchDestination, read_local_meta};
        use crate::engine::scratch_store::{head_dir, read_slot_meta};

        let mut report = ScratchReport::default();
        let scope = self.scratch_scope();
        let mut exported: FxHashSet<&str> = FxHashSet::default();
        let mut seen: FxHashSet<&str> = FxHashSet::default();
        for slot in ws.scratch.iter().map(|s| s.slot.as_str()) {
            if !seen.insert(slot) {
                continue;
            }
            anyhow::ensure!(!ctoken.is_cancelled(), "cache snapshot export cancelled");
            let dir = head_dir(&self.home, slot, scope);
            if !read_slot_meta(&self.home, slot).is_some_and(|m| m.remote) || !dir.is_dir() {
                tracing::debug!(slot, scope, "cache snapshot: scratch slot not carried");
                report.skipped += 1;
                continue;
            }
            let parent = read_local_meta(&self.home, slot, scope);
            let dest = ScratchDestination::Dir {
                root: generation,
                prev,
            };
            match self
                .scratch_publish(slot, scope, &dir, parent.as_ref(), "cache-snapshot", dest)
                .await
            {
                Ok(p) if p.unchanged => report.kept.add(p.bytes),
                Ok(p) => report.new.add(p.bytes),
                Err(e) if is_storage_full(&e) => {
                    return Err(e.context(format!("exporting scratch slot {slot}")));
                }
                Err(e) => {
                    tracing::warn!(slot, scope, error = %format!("{e:#}"), "cache snapshot: could not export a scratch slot");
                    report.skipped += 1;
                    continue;
                }
            }
            exported.insert(slot);
        }

        if let Some(prev) = prev {
            let exported: FxHashSet<String> = exported.into_iter().map(str::to_owned).collect();
            let (prev, generation) = (prev.to_path_buf(), generation.to_path_buf());
            report = hcore::blocking::run(move || {
                carry_scratch(&prev, &generation, &exported, incomplete, report)
                    .map_err(io::Error::other)
            })
            .await?;
        }
        Ok(report)
    }
}

fn is_storage_full(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<io::Error>())
        .any(|io| io.kind() == io::ErrorKind::StorageFull)
}

/// Walk the previous generation's scratch entries
/// (`scratch/v1/<slot>/<scope>/<stem>.{tar.gz,meta}`). One the new generation
/// already holds was kept. On an incomplete run, one of a slot the job
/// exported nothing of is carried over, linked or copied, when its meta
/// decodes and its archive is a regular file. Every other one is dropped.
fn carry_scratch(
    prev: &Path,
    generation: &Path,
    exported: &FxHashSet<String>,
    incomplete: bool,
    mut report: ScratchReport,
) -> anyhow::Result<ScratchReport> {
    use crate::engine::scratch_remote::KEY_PREFIX;

    let Ok(slots) = std::fs::read_dir(prev.join(KEY_PREFIX)) else {
        return Ok(report);
    };
    for slot in slots.flatten() {
        let slot_name = slot.file_name().to_string_lossy().into_owned();
        let carry = incomplete && !exported.contains(&slot_name);
        let Ok(scopes) = std::fs::read_dir(slot.path()) else {
            continue;
        };
        for scope in scopes.flatten() {
            let scope_name = scope.file_name().to_string_lossy().into_owned();
            let Ok(files) = std::fs::read_dir(scope.path()) else {
                continue;
            };
            for file in files.flatten() {
                let name = file.file_name().to_string_lossy().into_owned();
                let Some(stem) = name.strip_suffix(".meta") else {
                    continue;
                };
                let rel = format!("{KEY_PREFIX}/{slot_name}/{scope_name}/{stem}");
                if generation.join(format!("{rel}.meta")).exists() {
                    continue;
                }
                let tar = prev.join(format!("{rel}.tar.gz"));
                let bytes = std::fs::symlink_metadata(&tar)
                    .ok()
                    .filter(std::fs::Metadata::is_file)
                    .map_or(0, |m| m.len());
                if !carry {
                    report.dropped.add(bytes);
                    continue;
                }
                if carry_scratch_entry(prev, generation, &rel)? {
                    report.carried_over.add(bytes);
                } else {
                    report.dropped.add(bytes);
                }
            }
        }
    }
    Ok(report)
}

/// Carry one previous-generation scratch entry, archive then meta.
/// `Ok(false)` when it is not one this heph reads.
fn carry_scratch_entry(prev: &Path, generation: &Path, rel: &str) -> anyhow::Result<bool> {
    use crate::engine::scratch_remote::{SNAPSHOT_FORMAT, SnapshotMeta};
    use borsh::BorshDeserialize as _;

    let meta_src = prev.join(format!("{rel}.meta"));
    let tar_src = prev.join(format!("{rel}.tar.gz"));
    // Only a regular file is linked: a symlink would point out of `<dir>`.
    let regular = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file());
    if !regular(&meta_src) || !regular(&tar_src) {
        return Ok(false);
    }
    let readable = std::fs::read(&meta_src)
        .ok()
        .and_then(|b| SnapshotMeta::try_from_slice(&b).ok())
        .is_some_and(|m| m.format == SNAPSHOT_FORMAT);
    if !readable {
        return Ok(false);
    }
    let tar_dst = generation.join(format!("{rel}.tar.gz"));
    let parent = tar_dst.parent().context("scratch entry has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    for (src, dst) in [
        (tar_src, tar_dst),
        (meta_src, generation.join(format!("{rel}.meta"))),
    ] {
        if std::fs::hard_link(&src, &dst).is_err() {
            std::fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::cache_snapshot::{Incomplete, JournalContents, MARKER_FILE};
    use crate::engine::cache_test_support::{addr, test_engine};
    use crate::engine::local_cache::{
        MANIFEST_V2_VERSION, ManifestArtifact, ManifestArtifactContentType,
    };
    use crate::engine::remote_cache::Need;
    use hcore::hasync::StdCancellationToken;
    use std::io::Write as _;

    struct Blob {
        base: &'static str,
        encoding: ManifestArtifactEncoding,
        r#type: ManifestArtifactType,
        data: Vec<u8>,
    }

    fn raw(base: &'static str, data: &[u8]) -> Blob {
        Blob {
            base,
            encoding: ManifestArtifactEncoding::None,
            r#type: ManifestArtifactType::Output,
            data: data.to_vec(),
        }
    }

    fn zst(base: &'static str, data: &[u8]) -> Blob {
        Blob {
            encoding: ManifestArtifactEncoding::Zstd,
            ..raw(base, data)
        }
    }

    fn artifact(name: String, b: &Blob) -> ManifestArtifact {
        ManifestArtifact {
            hashout: format!("ho-{}", b.base),
            group: String::new(),
            name,
            size: b.data.len() as u64,
            r#type: b.r#type.clone(),
            content_type: ManifestArtifactContentType::Tar,
            encoding: b.encoding.clone(),
        }
    }

    fn manifest(addr: &Addr, hashin: &str, artifacts: Vec<ManifestArtifact>) -> Manifest {
        Manifest {
            version: MANIFEST_V2_VERSION.to_string(),
            target: addr.format(),
            created_at_nanos: 1,
            hashin: hashin.to_string(),
            artifacts,
        }
    }

    fn put_bytes(engine: &Engine, addr: &Addr, hashin: &str, name: &str, bytes: &[u8]) {
        let mut w = engine
            .local_cache
            .writer(addr, hashin, name)
            .expect("writer");
        w.write_all(bytes).expect("write");
        w.commit().expect("commit");
    }

    fn put_manifest(engine: &Engine, addr: &Addr, hashin: &str, name: &str, m: &Manifest) {
        put_bytes(
            engine,
            addr,
            hashin,
            name,
            &borsh::to_vec(m).expect("encode"),
        );
        assert!(
            engine
                .local_cache
                .exists(addr, hashin, name)
                .expect("exists")
        );
    }

    /// A v2 revision, each blob stored under its v2 name, encoded as it says.
    fn put(engine: &Engine, addr: &Addr, hashin: &str, blobs: &[Blob]) -> Manifest {
        let mut artifacts = Vec::new();
        for b in blobs {
            let name = cache_codec::entry_name(b.base, &b.encoding);
            let mut w =
                cache_codec::open_writer(&*engine.local_cache, addr, hashin, &name, &b.encoding)
                    .expect("writer");
            w.write_all(&b.data).expect("write");
            w.commit().expect("commit");
            artifacts.push(artifact(name, b));
        }
        let m = manifest(addr, hashin, artifacts);
        put_manifest(engine, addr, hashin, MANIFEST_V2, &m);
        m
    }

    fn ws(revs: &[(&Addr, &str)]) -> WorkingSet {
        WorkingSet {
            revisions: revs
                .iter()
                .map(|(addr, hashin)| RecordedRevision {
                    addr: (*addr).clone(),
                    hashin: (*hashin).to_string(),
                })
                .collect(),
            ..WorkingSet::default()
        }
    }

    fn incomplete(mut ws: WorkingSet) -> WorkingSet {
        ws.incomplete
            .push((PathBuf::from("1-a.log"), Incomplete::NoTrailer));
        ws
    }

    async fn export(engine: &Engine, dir: &Path, ws: &WorkingSet) -> ExportReport {
        let lock = lock_snapshot(engine.home.as_path()).expect("lock");
        engine
            .export_snapshot(&lock, dir, ws, &StdCancellationToken::new())
            .await
            .expect("export")
    }

    fn current(dir: &Path) -> PathBuf {
        read_marker(dir).expect("marker").path
    }

    fn has(dir: &Path, addr: &Addr, hashin: &str) -> bool {
        current(dir)
            .join(RemoteCacheSet::key(addr, hashin, MANIFEST_V2))
            .is_file()
    }

    fn left(report: &ExportReport, why: LeftOut) -> u64 {
        report.left_out.get(&why).map_or(0, |t| t.count)
    }

    fn generations(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(GENERATION_PREFIX))
            .collect();
        out.sort();
        out
    }

    /// Every file under `dir`, with its contents.
    fn tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).expect("read dir") {
                let p = e.expect("entry").path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let rel = p.strip_prefix(dir).expect("under dir").to_path_buf();
                    out.push((rel, std::fs::read(&p).expect("read")));
                }
            }
        }
        out.sort();
        out
    }

    fn snapshot_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[tokio::test]
    async fn a_fill_with_another_hashout_leaves_the_revision_out() {
        let (engine, _home) = test_engine();
        let dir = snapshot_dir();
        let (a, b) = (addr("a"), addr("b"));
        put(&engine, &a, "ha", &[raw("a", b"aaaa")]);
        let mb = put(&engine, &b, "hb", &[raw("b", b"bbbb")]);
        let first = export(&engine, dir.path(), &ws(&[(&a, "ha"), (&b, "hb")])).await;
        assert_eq!(first.new.count, 2);

        // A warm job: the local cache holds the manifests but never pulled
        // the blobs. `b`'s local manifest records another hashout than the
        // previous generation's entry, so that entry can't stand in for it.
        engine
            .local_cache
            .delete(&a, "ha", "a.raw")
            .expect("delete");
        engine
            .local_cache
            .delete(&b, "hb", "b.raw")
            .expect("delete");
        let mut other = mb.clone();
        other.artifacts[0].hashout = "another".to_string();
        put_manifest(&engine, &b, "hb", MANIFEST_V2, &other);

        let report = export(&engine, dir.path(), &ws(&[(&a, "ha"), (&b, "hb")])).await;
        assert_eq!(report.kept.count, 1, "{report:?}");
        assert_eq!(left(&report, LeftOut::MissingBlob), 1, "{report:?}");
        assert!(has(dir.path(), &a, "ha"));
        assert!(!has(dir.path(), &b, "hb"));
        let blob = current(dir.path()).join(RemoteCacheSet::key(&a, "ha", "a.raw"));
        assert_eq!(std::fs::read(blob).expect("filled blob"), b"aaaa");
    }

    #[tokio::test]
    async fn export_leaves_out_a_corrupt_revision() {
        let (engine, _home) = test_engine();
        let dir = snapshot_dir();
        let (good, short, bad_zstd, wrong_key) =
            (addr("good"), addr("short"), addr("zstd"), addr("key"));
        put(&engine, &good, "h", &[raw("o", b"fine")]);

        // A raw blob whose length is not its size.
        let b = raw("o", b"12345");
        put_bytes(&engine, &short, "h", "o.raw", b"1234");
        put_manifest(
            &engine,
            &short,
            "h",
            MANIFEST_V2,
            &manifest(&short, "h", vec![artifact("o.raw".into(), &b)]),
        );

        // A zstd blob with its frame checksum flipped: a single frame, so
        // the checksum is the last byte before the 25-byte seek table.
        let data = b"some compressible data, some compressible data".to_vec();
        put(&engine, &bad_zstd, "scratch", &[zst("o", &data)]);
        let mut stored = Vec::new();
        engine
            .local_cache
            .reader(&bad_zstd, "scratch", "o.zst")
            .expect("reader")
            .reader
            .read_to_end(&mut stored)
            .expect("read");
        let at = stored.len() - 25 - 1;
        stored[at] ^= 0xff;
        let decodes = cache_codec::decode_reader(
            Box::new(io::Cursor::new(stored.clone())),
            &ManifestArtifactEncoding::Zstd,
        )
        .and_then(|mut r| Ok(io::copy(&mut r, &mut io::sink())?));
        assert!(decodes.is_err(), "the flipped checksum must not decode");
        put_bytes(&engine, &bad_zstd, "h", "o.zst", &stored);
        let z = zst("o", &data);
        put_manifest(
            &engine,
            &bad_zstd,
            "h",
            MANIFEST_V2,
            &manifest(&bad_zstd, "h", vec![artifact("o.zst".into(), &z)]),
        );

        // A manifest naming another target than its key.
        put(&engine, &wrong_key, "h", &[raw("o", b"data")]);
        let mut m = manifest(
            &good,
            "h",
            vec![artifact("o.raw".into(), &raw("o", b"data"))],
        );
        m.hashin = "h".into();
        put_manifest(&engine, &wrong_key, "h", MANIFEST_V2, &m);

        let report = export(
            &engine,
            dir.path(),
            &ws(&[
                (&good, "h"),
                (&short, "h"),
                (&bad_zstd, "h"),
                (&wrong_key, "h"),
            ]),
        )
        .await;
        assert_eq!(left(&report, LeftOut::Corrupt), 2, "{report:?}");
        assert_eq!(left(&report, LeftOut::KeyMismatch), 1, "{report:?}");
        assert_eq!(report.new.count, 1, "{report:?}");
        assert!(has(dir.path(), &good, "h"));
        for a in [&short, &bad_zstd, &wrong_key] {
            assert!(!has(dir.path(), a, "h"), "{a}");
        }
        // Nothing of a left-out revision stays behind.
        let gen_dir = current(dir.path());
        assert!(
            !gen_dir
                .join(RemoteCacheSet::key(&short, "h", "o.raw"))
                .exists()
        );
        assert!(
            !gen_dir
                .join(RemoteCacheSet::key(&bad_zstd, "h", "o.zst"))
                .exists()
        );
    }

    #[tokio::test]
    async fn case_colliding_blob_names_are_left_out() {
        let (engine, _home) = test_engine();
        let dir = snapshot_dir();
        let (a, ok) = (addr("a"), addr("ok"));
        put(&engine, &a, "h", &[raw("Out", b"1"), raw("out", b"2")]);
        put(&engine, &ok, "h", &[raw("out", b"1")]);
        let report = export(&engine, dir.path(), &ws(&[(&a, "h"), (&ok, "h")])).await;
        assert_eq!(left(&report, LeftOut::CaseCollision), 1, "{report:?}");
        assert!(!has(dir.path(), &a, "h"));
        assert!(has(dir.path(), &ok, "h"));
    }

    #[tokio::test]
    async fn a_v1_only_revision_is_converted_or_left_out() {
        let (engine, _home) = test_engine();
        let dir = snapshot_dir();
        let (v1, gz) = (addr("v1"), addr("gz"));
        // A v1 revision names bare blobs, stored raw.
        put_bytes(&engine, &v1, "h", "o", b"data");
        let b = raw("o", b"data");
        put_manifest(
            &engine,
            &v1,
            "h",
            MANIFEST_V1,
            &manifest(&v1, "h", vec![artifact("o".into(), &b)]),
        );
        // One that says its blob is not raw is left out.
        put_bytes(&engine, &gz, "h", "o", b"data");
        let z = zst("o", b"data");
        put_manifest(
            &engine,
            &gz,
            "h",
            MANIFEST_V1,
            &manifest(&gz, "h", vec![artifact("o".into(), &z)]),
        );

        let report = export(&engine, dir.path(), &ws(&[(&v1, "h"), (&gz, "h")])).await;
        assert_eq!(report.new.count, 1, "{report:?}");
        assert_eq!(left(&report, LeftOut::Unsupported), 1, "{report:?}");
        let gen_dir = current(dir.path());
        // Never under the v1 name; the blob takes its v2 name.
        assert!(
            !gen_dir
                .join(RemoteCacheSet::key(&v1, "h", MANIFEST_V1))
                .exists()
        );
        let bytes = std::fs::read(gen_dir.join(RemoteCacheSet::key(&v1, "h", MANIFEST_V2)))
            .expect("v2 manifest");
        let m: RemoteManifest = borsh::from_slice(&bytes).expect("decode");
        assert_eq!(m.artifacts[0].name, "o.raw");
        assert_eq!(
            std::fs::read(gen_dir.join(RemoteCacheSet::key(&v1, "h", "o.raw"))).expect("blob"),
            b"data"
        );
    }

    #[tokio::test]
    async fn a_crash_before_the_pointer_keeps_the_old_generation() {
        let (engine, _home) = test_engine();
        let dir = snapshot_dir();
        let (a, b) = (addr("a"), addr("b"));
        put(&engine, &a, "h", &[raw("o", b"a")]);
        put(&engine, &b, "h", &[raw("o", b"b")]);
        export(&engine, dir.path(), &ws(&[(&a, "h")])).await;
        let before = read_marker(dir.path()).expect("marker").marker;

        let lock = lock_snapshot(engine.home.as_path()).expect("lock");
        let crashed = engine
            .export_with(
                &lock,
                dir.path(),
                &ws(&[(&b, "h")]),
                &StdCancellationToken::new(),
                |_| anyhow::bail!("crash"),
            )
            .await;
        drop(lock);
        assert!(crashed.is_err());
        assert_eq!(read_marker(dir.path()).expect("marker").marker, before);
        assert!(has(dir.path(), &a, "h"));
        assert_eq!(generations(dir.path()).len(), 2, "the half-built one stays");

        // The next export sweeps it.
        export(&engine, dir.path(), &ws(&[(&b, "h")])).await;
        assert_eq!(generations(dir.path()).len(), 1);
        assert!(has(dir.path(), &b, "h"));
        assert!(!has(dir.path(), &a, "h"));
    }

    #[tokio::test]
    async fn a_mount_point_dir_is_updated() {
        use std::os::unix::fs::MetadataExt as _;
        let (engine, _home) = test_engine();
        let dir = snapshot_dir();
        let a = addr("a");
        put(&engine, &a, "h", &[raw("o", b"a")]);
        std::fs::write(dir.path().join("not-ours"), b"x").expect("write");
        let ino = std::fs::metadata(dir.path()).expect("stat").ino();
        for _ in 0..2 {
            export(&engine, dir.path(), &ws(&[(&a, "h")])).await;
            assert_eq!(std::fs::metadata(dir.path()).expect("stat").ino(), ino);
        }
        assert_eq!(generations(dir.path()).len(), 1);
        assert!(dir.path().join("not-ours").is_file(), "only gen-* is swept");
    }

    #[tokio::test]
    async fn a_newer_snapshot_is_replaced_cleanly() {
        for marker in [r#"{"format": 99, "current": "gen-new"}"#, "not json"] {
            let (engine, _home) = test_engine();
            let dir = snapshot_dir();
            let a = addr("a");
            put(&engine, &a, "h", &[raw("o", b"a")]);
            std::fs::create_dir(dir.path().join("gen-new")).expect("mkdir");
            std::fs::create_dir(dir.path().join("gen-old")).expect("mkdir");
            std::fs::write(dir.path().join(MARKER_FILE), marker).expect("write");
            let report = export(&engine, dir.path(), &ws(&[(&a, "h")])).await;
            assert!(matches!(report.outcome, ExportOutcome::Written { .. }));
            let written = read_marker(dir.path()).expect("readable");
            assert_eq!(generations(dir.path()), vec![written.marker.current]);
            assert!(has(dir.path(), &a, "h"));
        }
    }

    #[tokio::test]
    async fn two_snapshots_do_not_overlap() {
        let home = tempfile::tempdir().expect("tempdir");
        let held = lock_snapshot(home.path()).expect("lock");
        assert!(try_lock_snapshot(home.path()).expect("try").is_none());
        drop(held);
        assert!(try_lock_snapshot(home.path()).expect("try").is_some());
    }

    #[tokio::test]
    async fn a_revision_in_two_journals_is_exported_once() {
        let (engine, _home) = test_engine();
        let dir = snapshot_dir();
        let a = addr("a");
        put(&engine, &a, "h", &[raw("o", b"a")]);
        let journal = |path: &str| JournalContents {
            path: PathBuf::from(path),
            version: 1,
            revisions: ws(&[(&a, "h")]).revisions,
            scratch: Vec::new(),
            incomplete: None,
        };
        let merged = WorkingSet::from_journals([journal("1.log"), journal("2.log")]);
        let report = export(&engine, dir.path(), &merged).await;
        assert_eq!(report.new.count, 1, "{report:?}");
        assert_eq!(report.journals.len(), 2);
    }

    #[tokio::test]
    async fn an_empty_journal_keeps_the_old_snapshot() {
        let (engine, _home) = test_engine();
        let dir = snapshot_dir();
        let a = addr("a");
        put(&engine, &a, "h", &[raw("o", b"a")]);
        export(&engine, dir.path(), &ws(&[(&a, "h")])).await;
        // A stray generation stays too: an empty run does not sweep.
        std::fs::create_dir(dir.path().join("gen-stray")).expect("mkdir");
        std::fs::write(dir.path().join("gen-stray").join("f"), b"x").expect("write");
        let before = tree(dir.path());
        let report = export(&engine, dir.path(), &WorkingSet::default()).await;
        assert_eq!(report.outcome, ExportOutcome::Unchanged);
        assert_eq!(tree(dir.path()), before);
    }

    #[tokio::test]
    async fn an_incomplete_run_carries_over_what_it_did_not_touch() {
        let (engine, _home) = test_engine();
        let dir = snapshot_dir();
        let (a, b, gone) = (addr("a"), addr("b"), addr("gone"));
        put(&engine, &a, "h", &[raw("o", b"a")]);
        put(&engine, &b, "h", &[zst("o", b"bbbbbbbb")]);
        export(&engine, dir.path(), &ws(&[(&a, "h"), (&b, "h")])).await;
        // This machine never had `b`: only the restored snapshot does.
        for name in [MANIFEST_V2, "o.zst"] {
            engine.local_cache.delete(&b, "h", name).expect("delete");
        }

        let report = export(
            &engine,
            dir.path(),
            &incomplete(ws(&[(&a, "h"), (&gone, "h")])),
        )
        .await;
        assert_eq!(report.kept.count, 1, "{report:?}");
        assert_eq!(report.carried_over.count, 1, "{report:?}");
        assert_eq!(left(&report, LeftOut::Trimmed), 1, "{report:?}");
        assert_eq!(report.incomplete.len(), 1);
        assert!(has(dir.path(), &b, "h"));
        assert!(read_marker(dir.path()).expect("marker").marker.incomplete);

        // A complete run drops what it did not use.
        let report = export(&engine, dir.path(), &ws(&[(&a, "h")])).await;
        assert_eq!(report.kept.count, 1, "{report:?}");
        assert_eq!(report.dropped.count, 1, "{report:?}");
        assert_eq!(report.dropped_addrs, vec![b.format()]);
        assert!(!has(dir.path(), &b, "h"));
        assert!(!read_marker(dir.path()).expect("marker").marker.incomplete);
        assert_eq!(report.top.len(), 1);
        assert_eq!(report.top[0].addr, a.format());
    }

    /// The export writes what the snapshot read tier serves: the manifest
    /// under the key the tier asks for, every non-log blob next to it.
    #[tokio::test]
    async fn an_export_reads_back_through_the_snapshot_tier() {
        let (engine, home) = test_engine();
        let dir = snapshot_dir();
        let a = addr("a");
        let log = Blob {
            r#type: ManifestArtifactType::Log,
            ..raw("log", b"log line")
        };
        let rawdata = b"raw bytes".to_vec();
        let zdata = b"zstd bytes zstd bytes zstd bytes".to_vec();
        put(
            &engine,
            &a,
            "h",
            &[raw("r", &rawdata), zst("z", &zdata), log],
        );
        export(&engine, dir.path(), &ws(&[(&a, "h")])).await;
        let gen_dir = current(dir.path());
        assert!(
            !gen_dir
                .join(RemoteCacheSet::key(&a, "h", "log.raw"))
                .exists(),
            "logs are not carried"
        );

        let set = RemoteCacheSet::with_snapshot(&[], Some(&gen_dir), home.path().to_path_buf())
            .expect("tier");
        let tok = StdCancellationToken::new();
        let rev = set
            .locate(&tok, &a, "h", Need::Groups(&[String::new()]))
            .await
            .expect("the tier serves the revision");
        let tmp = tempfile::tempdir().expect("tempdir");
        for (name, want) in [("r.raw", &rawdata), ("z.zst", &zdata)] {
            let blob = set
                .fetch_blob(&tok, &rev, &a, "h", name, tmp.path())
                .await
                .expect("fetch")
                .expect("present");
            let encoding = rev
                .manifest
                .artifacts
                .iter()
                .find(|x| x.name == name)
                .expect("named")
                .encoding
                .clone();
            let mut decoded = Vec::new();
            cache_codec::decode_reader(
                Box::new(std::fs::File::open(blob.path()).expect("open")),
                &encoding,
            )
            .expect("decoder")
            .read_to_end(&mut decoded)
            .expect("decode");
            assert_eq!(&decoded, want, "{name}");
        }
    }
}
