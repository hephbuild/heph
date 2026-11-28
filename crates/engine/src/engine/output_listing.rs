//! What a finished target wrote, file by file: the read behind a viewer's
//! Outputs tab.
//!
//! The `ResultOutputs` event names a target's groups with their sizes and
//! hashes; the files inside a group are not on any event, because an artifact
//! is the group's packed tree and listing it reads the cache entry. A viewer
//! asks for them here, off its render task, for the one target it shows.
//!
//! - [`Engine::output_listing`]: a local cache entry's output groups, each
//!   file's path, kind and size, read from the tar headers alone: no file's
//!   bytes are read. A group carries its `hashout` from the manifest. A group
//!   whose blob is not local (a remote hit that did not pull it) is listed
//!   with why, never fetched.
//! - [`Engine::failure_outputs`]: what a failed run kept, from its failure
//!   slot: the `failure_out` matches under `out/` and the fixes an `in_place`
//!   run wrote back.
//!
//! **Reading never builds**: no resolve, no hash of any input or file, no
//! lock of any target, no run.
//!
//! **Blocking.** Both read files; call them through `hcore::blocking`.

use crate::engine::Engine;
use crate::engine::cache_codec;
use crate::engine::failure_slot;
use crate::engine::local_cache::{
    LocalCache, Manifest, ManifestArtifact, ManifestArtifactContentType, ManifestArtifactType,
    NotFoundError,
};
use anyhow::Context as _;
use hcore::hartifactcontent::ReadSeek;
use hcore::hartifactcontent::tar_index::{IndexEntryKind, TarIndex};
use hmodel::htaddr::Addr;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Largest stored blob read into memory when its backend has no seekable
/// reader (a blob under the spill threshold, held in sqlite).
const IN_MEMORY_MAX: u64 = 64 << 20;

/// A cache entry's outputs, by group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputListing {
    /// The input hash of the entry listed.
    pub hashin: String,
    /// When the entry was written, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// The `Output` groups, in name order; log and support files are not
    /// outputs.
    pub groups: Vec<ListedGroup>,
}

/// One output group of a cache entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedGroup {
    /// Empty for the target's unnamed group.
    pub group: String,
    /// Bytes as stored, summed over the group's artifacts.
    pub size: u64,
    /// The group's `hashout`, when it is one artifact.
    pub hash: Option<String>,
    /// Its files in path order. A directory with nothing under it is one
    /// row; a directory that holds files is not a row of its own.
    pub files: Vec<ListedFile>,
    /// Set when the group's files could not be read: its blob is not in the
    /// local cache, or does not decode. `files` is then what was read before.
    pub unavailable: Option<String>,
}

/// One row of a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedFile {
    /// Workspace-relative.
    pub path: PathBuf,
    pub kind: ListedKind,
    /// The file's bytes; 0 for a symlink or a directory.
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListedKind {
    File,
    Symlink(PathBuf),
    Dir,
}

/// What a failed run kept, from its failure slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureOutputs {
    /// The input hash of the run that failed; `None` for a failure before
    /// the run.
    pub hashin: Option<String>,
    /// When it failed, in milliseconds since the Unix epoch.
    pub failed_at: u64,
    /// Its `failure_out` matches, workspace-relative, in path order.
    pub kept: Vec<ListedFile>,
    /// The workspace-relative paths a failed `in_place` run wrote back over
    /// the tree, as `fixes.txt` lists them.
    pub fixes: Vec<String>,
}

impl Engine {
    /// The outputs of `addr`'s local cache entry at `hashin`, or of its
    /// newest one when `hashin` is `None`. `None` when there is no such
    /// entry. Blocking.
    pub fn output_listing(
        &self,
        addr: &Addr,
        hashin: Option<&str>,
    ) -> anyhow::Result<Option<OutputListing>> {
        let Some(manifest) = self.listed_manifest(addr, hashin)? else {
            return Ok(None);
        };
        let mut groups: Vec<ListedGroup> = Vec::new();
        let mut outputs: Vec<&ManifestArtifact> = manifest
            .artifacts
            .iter()
            .filter(|a| a.r#type == ManifestArtifactType::Output)
            .collect();
        outputs.sort_by(|a, b| a.group.cmp(&b.group));
        for artifact in outputs {
            let group = match groups.last_mut() {
                Some(g) if g.group == artifact.group => {
                    g.size = g.size.saturating_add(artifact.size);
                    g.hash = None;
                    g
                }
                _ => {
                    groups.push(ListedGroup {
                        group: artifact.group.clone(),
                        size: artifact.size,
                        hash: Some(artifact.hashout.clone()),
                        files: Vec::new(),
                        unavailable: None,
                    });
                    groups.last_mut().context("a group was just pushed")?
                }
            };
            match list_artifact(self.local_cache.as_ref(), addr, &manifest.hashin, artifact) {
                Ok(files) => group.files.extend(files),
                Err(Unavailable(reason)) => {
                    group.unavailable.get_or_insert(reason);
                }
            }
        }
        for g in &mut groups {
            g.files.sort_by(|a, b| a.path.cmp(&b.path));
        }
        Ok(Some(OutputListing {
            hashin: manifest.hashin,
            created_at_ms: u64::try_from(manifest.created_at_nanos / 1_000_000).unwrap_or(0),
            groups,
        }))
    }

    /// What `addr`'s last failed run kept: its `failure_out` matches and the
    /// fixes it wrote back. `None` when no failure is recorded for it.
    /// Blocking.
    pub fn failure_outputs(&self, addr: &Addr) -> anyhow::Result<Option<FailureOutputs>> {
        let Some(manifest) = self.failure_slots.read(addr) else {
            return Ok(None);
        };
        let slot = self.failure_slots.slot_dir(addr);
        let mut kept = Vec::new();
        walk_kept(&slot.join(failure_slot::OUT), Path::new(""), &mut kept)
            .with_context(|| format!("listing what the failed run of {addr} kept"))?;
        kept.sort_by(|a, b| a.path.cmp(&b.path));
        let fixes = match std::fs::read_to_string(slot.join(failure_slot::FIXES)) {
            Ok(text) => text
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("reading the fixes the failed run of {addr} wrote"));
            }
        };
        Ok(Some(FailureOutputs {
            hashin: manifest.hashin,
            failed_at: manifest.failed_at,
            kept,
            fixes,
        }))
    }

    /// The manifest of `addr`'s entry at `hashin`, or its newest one.
    fn listed_manifest(
        &self,
        addr: &Addr,
        hashin: Option<&str>,
    ) -> anyhow::Result<Option<Manifest>> {
        if let Some(h) = hashin {
            return self.read_manifest(addr, h);
        }
        let hashins = self
            .local_cache
            .list_target_entries(addr)
            .with_context(|| format!("listing the cache entries of {addr}"))?;
        let mut newest: Option<Manifest> = None;
        for h in hashins {
            let Some(manifest) = self.read_manifest(addr, &h)? else {
                continue;
            };
            if newest
                .as_ref()
                .is_none_or(|n| manifest.created_at_nanos > n.created_at_nanos)
            {
                newest = Some(manifest);
            }
        }
        Ok(newest)
    }
}

/// Why a group's files could not be read.
struct Unavailable(String);

/// The rows of one stored artifact, from its tar's headers.
fn list_artifact(
    cache: &dyn LocalCache,
    addr: &Addr,
    hashin: &str,
    artifact: &ManifestArtifact,
) -> Result<Vec<ListedFile>, Unavailable> {
    if artifact.content_type != ManifestArtifactContentType::Tar {
        return Err(Unavailable("a cpio artifact cannot be listed".into()));
    }
    let not_local = || Unavailable("not in the local cache".into());
    let stored: Box<dyn ReadSeek + Send> = match cache.seekable_reader(addr, hashin, &artifact.name)
    {
        Ok(Some(r)) => r,
        Ok(None) => {
            // Held in rows: small, read whole.
            let sized = match cache.reader(addr, hashin, &artifact.name) {
                Ok(s) => s,
                Err(e) if e.is::<NotFoundError>() => return Err(not_local()),
                Err(e) => return Err(Unavailable(format!("reading the blob: {e:#}"))),
            };
            if sized.size > IN_MEMORY_MAX {
                return Err(Unavailable(format!(
                    "a {}-byte blob with no random access",
                    sized.size
                )));
            }
            let bytes: Arc<[u8]> = match sized.bytes {
                Some(b) => b,
                None => {
                    let mut buf = Vec::new();
                    sized
                        .reader
                        .take(sized.size)
                        .read_to_end(&mut buf)
                        .map_err(|e| Unavailable(format!("reading the blob: {e}")))?;
                    buf.into()
                }
            };
            Box::new(io::Cursor::new(bytes))
        }
        Err(e) if e.is::<NotFoundError>() => return Err(not_local()),
        Err(e) => return Err(Unavailable(format!("opening the blob: {e:#}"))),
    };
    let mut decoded = match cache_codec::decode_seekable(stored, &artifact.encoding) {
        Ok(Some(d)) => d,
        Ok(None) => {
            return Err(Unavailable(
                "its encoding has no random-access reader".into(),
            ));
        }
        Err(e) => return Err(Unavailable(format!("decoding the blob: {e:#}"))),
    };
    let index = TarIndex::build(&mut decoded)
        .map_err(|e| Unavailable(format!("reading the tar: {e:#}")))?;
    Ok(rows(&index))
}

/// The rows of a tar index, in path order: every file and symlink, and each
/// directory nothing is under.
fn rows(index: &TarIndex) -> Vec<ListedFile> {
    let mut out = Vec::new();
    let mut entries = index.entries.iter().peekable();
    while let Some((path, entry)) = entries.next() {
        let (kind, size) = match &entry.kind {
            IndexEntryKind::File => (ListedKind::File, entry.size),
            IndexEntryKind::Symlink(target) => (ListedKind::Symlink(target.clone()), 0),
            // Paths sort by component, so whatever is under a directory
            // follows it at once.
            IndexEntryKind::Dir => {
                if entries
                    .peek()
                    .is_some_and(|(next, _)| next.starts_with(path))
                {
                    continue;
                }
                (ListedKind::Dir, 0)
            }
        };
        out.push(ListedFile {
            path: path.clone(),
            kind,
            size,
        });
    }
    out
}

/// The files under a slot's `out/` (only regular files are unpacked there),
/// as workspace-relative rows. A missing `out/` is no rows.
fn walk_kept(dir: &Path, rel: &Path, out: &mut Vec<ListedFile>) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = rel.join(entry.file_name());
        let ty = entry.file_type()?;
        if ty.is_dir() {
            walk_kept(&entry.path(), &path, out)?;
        } else if ty.is_file() {
            out.push(ListedFile {
                path,
                kind: ListedKind::File,
                size: entry.metadata()?.len(),
            });
        }
    }
    Ok(())
}
