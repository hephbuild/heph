//! The remote scratch lineage: publishing a cache and picking one up.
//!
//! This is what makes scratch worth anything in CI, where every runner starts
//! cold. A slot's contents are published as immutable snapshots under
//! `scratch/v1/<slot>/<scope>/<gen>.<hash>.tar.gz`, and a cold runner picks the
//! newest one for its branch — falling back to the branch it forked from.
//!
//! # Why "latest" is not a pointer
//!
//! The tempting design is a mutable `HEAD` object naming the newest snapshot. It
//! is wrong here twice over, and the first reason is the one that matters:
//!
//! **One cache serves many branches at once.** A remote holds a live lineage for
//! `master`, one for each open PR, one for each long-running branch — all
//! advancing concurrently and all legitimately different. There is no single
//! "latest" for a pointer to name. A pointer *per* branch models that and creates
//! two new problems: an unbounded set of mutable objects, and no way to relate the
//! heads that a cross-branch restore has to compare.
//!
//! **And the store could not maintain one safely anyway.** [`RemoteCacheBackend`]
//! has `open_read`, `open_write`, `exists` and `list_names` — no compare-and-swap
//! and no delete. So even for a single branch, two jobs finishing together race,
//! and the *loser* can be the one that finishes last: a job that started from an
//! older cache would overwrite the pointer with older content. "Latest" would mean
//! "written most recently", which is not the same as "descended from the most
//! work".
//!
//! So entries are **immutable and ordering is carried in the key**. Resolution is
//! a prefix list and a max: no mutation, no coordination, correct under concurrent
//! writers, and it extends to many branches by listing more than one prefix.
//!
//! # Generations, not timestamps
//!
//! A snapshot records the generation it descends from, and a publish is
//! `parent + 1`. That is deliberately not a clock. A slow runner that picked up
//! generation 5 an hour ago and finishes now publishes 6, which correctly *loses*
//! to a chain that has since reached 12 — a timestamp would have that backwards,
//! and clock skew across runners makes it worse. Generations need no clock and no
//! coordinator, only the parent each writer already picked up.
//!
//! Generations advance at **publish** time, not build time, which makes the model
//! exactly git's: the local directory is a working tree, its meta records the
//! parent it was last seeded from, and `heph tool scratch push` is the commit.

use crate::engine::Engine;
use crate::engine::remote_cache::{RemoteCacheBackend, SNAPSHOT_STREAM_BUF, ScratchSource};
use crate::engine::scratch_store::head_meta_path;
use anyhow::Context as _;
use borsh::{BorshDeserialize, BorshSerialize};
use futures::StreamExt as _;
use std::io::Seek as _;
use std::path::Path;
use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};

/// Key-layout version. Bumping it makes every older entry invisible rather than
/// misread, which is the right failure for a cache.
pub(crate) const KEY_PREFIX: &str = "scratch/v1";

/// Payload-format version, independent of the key layout above: one covers where
/// entries live, the other what is inside them.
pub(crate) const SNAPSHOT_FORMAT: u32 = 1;

/// What a published snapshot is, beside its bytes.
///
/// Small (a few hundred bytes) and fetched on its own, so resolving a head costs
/// one list plus one tiny GET and never pulls a tarball to find out it is the
/// wrong one.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SnapshotMeta {
    pub format: u32,
    /// Lineage this entry belongs to.
    pub scope: String,
    /// How far along that lineage.
    pub generation: u64,
    /// Lineage the entry descended from — the same scope in the ordinary case, a
    /// different one on the first publish after a branch fork.
    pub parent_scope: String,
    /// Generation within `parent_scope` this descended from.
    pub parent_generation: u64,
    /// Uncompressed size, for reporting.
    pub bytes: u64,
    /// Producing heph version, for diagnosis.
    pub heph_version: String,
    /// Free-form producer id (`--producer`, typically a CI run id).
    pub producer: String,
    /// Hash of the packed bytes. Recorded locally so a re-publish of unchanged
    /// contents can be skipped — the archive is deterministic (entries are
    /// sorted), so identical contents hash identically.
    pub content_hash: String,
    /// Absolute path the snapshot was produced at.
    ///
    /// Recorded because a cache whose entries embed absolute paths — Go's action
    /// IDs are the standing example — restores fine at a different path and is
    /// then *inert*: present, and useless. That is the worst failure mode
    /// available here, because it looks like a hit. Naming both paths in a log
    /// line is what makes it diagnosable instead of mysterious.
    pub produced_at: String,
}

/// A candidate head: its key stem, its meta, and which cache it came from.
#[derive(Debug, Clone)]
pub struct RemoteHead {
    pub cache: String,
    /// Key stem, i.e. everything before `.tar.gz` / `.meta`.
    pub stem: String,
    pub meta: SnapshotMeta,
}

fn scope_prefix(slot: &str, scope: &str) -> String {
    format!(
        "{KEY_PREFIX}/{slot}/{}/",
        crate::engine::config::sanitize_scope(scope)
    )
}

/// Order two entries in one lineage, best first.
///
/// `(generation, bytes, stem)` descending. Generation is the real order; `bytes`
/// breaks a same-generation fork — two runners both publishing `parent + 1` — by
/// preferring the fuller cache, which among two equally-derived ones has more
/// warm entries and is the better inheritance. `stem` last makes the order total,
/// so every reader converges on the same head instead of oscillating.
fn better(a: &RemoteHead, b: &RemoteHead) -> std::cmp::Ordering {
    b.meta
        .generation
        .cmp(&a.meta.generation)
        .then(b.meta.bytes.cmp(&a.meta.bytes))
        .then(a.stem.cmp(&b.stem))
}

/// A writer that hashes and counts everything written through it.
///
/// What lets [`pack`] stream the archive out while still knowing its content
/// hash: the digest of the streamed bytes is the digest of the whole archive.
struct Digesting<W> {
    inner: W,
    hasher: xxhash_rust::xxh3::Xxh3,
    len: u64,
}

impl<W> Digesting<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: xxhash_rust::xxh3::Xxh3::new(),
            len: 0,
        }
    }

    /// The content hash, as the snapshot key and meta spell it.
    fn hash(&self) -> String {
        format!("{:016x}", self.hasher.digest())
    }
}

impl<W: std::io::Write> std::io::Write for Digesting<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        let written = buf
            .get(..n)
            .ok_or_else(|| std::io::Error::other("writer reported more bytes than it was given"))?;
        self.hasher.update(written);
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Pack a directory tree into a gzipped tar written to `out`: the digesting
/// writer it was written through, and the uncompressed size.
///
/// Symlinks are archived as symlinks rather than followed, so a slot that has
/// acquired a link out of the tree does not publish whatever it points at to
/// every machine that picks the snapshot up.
fn pack<W: std::io::Write>(dir: &Path, out: W) -> anyhow::Result<(Digesting<W>, u64)> {
    let mut bytes = 0u64;
    let enc = flate2::write::GzEncoder::new(Digesting::new(out), flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.follow_symlinks(false);

    append_dir(&mut tar, dir, Path::new(""), &mut bytes)?;
    let enc = tar.into_inner().context("finish scratch tar")?;
    Ok((enc.finish().context("finish scratch gzip")?, bytes))
}

/// A snapshot packed into an anonymous temp file, rewound to its start.
struct Packed {
    file: std::fs::File,
    /// Content hash of the archive, as the key and meta spell it.
    hash: String,
    /// Compressed size: what the upload moves.
    len: u64,
    /// Uncompressed size, for the meta.
    bytes: u64,
}

/// Pack `dir` into a temp file beside it, hashing the archive on the way out.
fn pack_to_temp(dir: &Path) -> anyhow::Result<Packed> {
    let out = std::io::BufWriter::with_capacity(SNAPSHOT_STREAM_BUF, temp_file_beside(dir)?);
    let (digest, bytes) = pack(dir, out)?;
    let hash = digest.hash();
    let mut file = digest
        .inner
        .into_inner()
        .map_err(std::io::IntoInnerError::into_error)
        .context("flush scratch snapshot temp file")?;
    file.seek(std::io::SeekFrom::Start(0))
        .context("rewind scratch snapshot temp file")?;
    Ok(Packed {
        file,
        hash,
        len: digest.len,
        bytes,
    })
}

/// Download `key` from `source` and unpack it into `dir`: the bytes
/// downloaded.
///
/// The archive goes to a temp file beside `dir` first and is unpacked only once
/// the download completed, so a transfer that dies part way leaves `dir`
/// exactly as it was.
async fn pull_snapshot(source: &ScratchSource<'_>, key: &str, dir: &Path) -> anyhow::Result<u64> {
    let mut dest = tokio::io::BufWriter::with_capacity(
        SNAPSHOT_STREAM_BUF,
        tokio::fs::File::from_std(temp_file_beside(dir)?),
    );
    let n = source
        .read_snapshot_into(key, &mut dest)
        .await
        .with_context(|| format!("read scratch snapshot {key}"))?
        .ok_or_else(|| anyhow::anyhow!("scratch snapshot {key} vanished"))?;
    dest.flush()
        .await
        .with_context(|| format!("write scratch snapshot {key} locally"))?;
    let mut file = dest.into_inner().into_std().await;

    let dir = dir.to_path_buf();
    hcore::blocking::run(move || {
        file.seek(std::io::SeekFrom::Start(0))
            .context("rewind scratch snapshot temp file")
            .and_then(|_| unpack(file, &dir))
            .map_err(std::io::Error::other)
    })
    .await
    .context("unpack scratch snapshot")?;
    Ok(n)
}

/// An anonymous temp file next to `dir`, never inside it.
///
/// Beside the directory rather than in memory, because a snapshot is a whole
/// compressed cache; and beside it rather than in it, so neither a pack nor a
/// failed download ever touches the tree itself. Anonymous, so a process that
/// dies mid-transfer leaves nothing behind.
fn temp_file_beside(dir: &Path) -> anyhow::Result<std::fs::File> {
    match dir.parent() {
        Some(parent) => {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create scratch lineage dir {parent:?}"))?;
            tempfile::tempfile_in(parent)
                .with_context(|| format!("create scratch snapshot temp file in {parent:?}"))
        }
        None => tempfile::tempfile().context("create scratch snapshot temp file"),
    }
}

/// Recursively append `dir`'s contents to `tar` under `rel`.
///
/// Hand-rolled rather than pulling in a directory walker: this needs symlinks
/// archived *as symlinks* and everything that is not a file, dir or symlink
/// skipped, which is a per-entry decision either way.
fn append_dir<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    dir: &Path,
    rel: &Path,
    bytes: &mut u64,
) -> anyhow::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("read scratch dir {dir:?}"))?
        .collect::<Result<_, _>>()?;
    // Sorted, so the same tree always produces the same archive — and therefore
    // the same content hash, which is what makes a re-publish of unchanged
    // contents recognizable rather than a fresh blob every time.
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let path = entry.path();
        let rel = rel.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            // Checked first: a symlink to a directory reports as a directory
            // under `metadata`, and following it is exactly what must not happen.
            let target = std::fs::read_link(&path)?;
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            tar.append_link(&mut header, &rel, target)?;
        } else if ft.is_dir() {
            tar.append_dir(&rel, &path)?;
            append_dir(tar, &path, &rel, bytes)?;
        } else if ft.is_file() {
            let mut f = std::fs::File::open(&path)?;
            *bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            tar.append_file(&rel, &mut f)?;
        }
        // Sockets, fifos and devices are skipped: they are not cache contents,
        // and a tar member for one is meaningless where it is unpacked.
    }
    Ok(())
}

/// Unpack a gzipped tar into `dir`, which is created if absent.
fn unpack(archive: impl std::io::Read, dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let dec = flate2::read::GzDecoder::new(archive);
    let mut archive = tar::Archive::new(dec);
    archive.set_overwrite(true);
    archive
        .unpack(dir)
        .with_context(|| format!("unpack scratch snapshot into {dir:?}"))
}

impl Engine {
    /// The best head available for `slot`, trying `scope` then each fallback.
    ///
    /// First lineage with anything wins, then the best entry within it — mirroring
    /// GitHub Actions' `key`-then-`restore-keys` precedence, so the behaviour is
    /// one a CI author already knows. Generations are compared **only within a
    /// scope**: `feat` at generation 40 is not "ahead of" `master` at 12, and
    /// asking would be a category error.
    pub async fn scratch_remote_head(
        &self,
        slot: &str,
        scope: &str,
        fallbacks: &[String],
    ) -> Option<RemoteHead> {
        let sources = self.remote_caches().scratch_sources().await;
        if sources.is_empty() {
            return None;
        }
        for scope in std::iter::once(scope).chain(fallbacks.iter().map(String::as_str)) {
            if let Some(best) = best_in_scope(&sources, slot, scope).await {
                return Some(best);
            }
        }
        None
    }

    /// Every candidate scope that would be consulted, with the head found in it.
    ///
    /// The resolution in [`scratch_remote_head`](Self::scratch_remote_head) stops
    /// at the first scope with anything, which is right for resolving and useless
    /// for explaining. This returns the whole walk so a person can see *why* a
    /// build started cold: which lineages were looked at, in what order, and what
    /// each held.
    pub async fn scratch_remote_trace(
        &self,
        slot: &str,
        scope: &str,
        fallbacks: &[String],
    ) -> Vec<(String, Option<RemoteHead>)> {
        let sources = self.remote_caches().scratch_sources().await;
        let mut scopes = vec![scope.to_string()];
        scopes.extend(fallbacks.iter().cloned());

        let mut out = Vec::with_capacity(scopes.len());
        for scope in scopes {
            let best = best_in_scope(&sources, slot, &scope).await;
            out.push((scope, best));
        }
        out
    }

    /// Fetch a head's snapshot and unpack it into `dir`.
    pub async fn scratch_pull(&self, head: &RemoteHead, dir: &Path) -> anyhow::Result<u64> {
        let sources = self.remote_caches().scratch_sources().await;
        let source = sources
            .iter()
            .find(|s| s.name() == head.cache)
            .ok_or_else(|| anyhow::anyhow!("remote cache `{}` is gone or paused", head.cache))?;

        pull_snapshot(source, &format!("{}.tar.gz", head.stem), dir).await
    }

    /// Publish `dir` as the next generation of `scope`'s lineage, to every
    /// writable remote cache: `(generation, compressed bytes)`, the bytes 0
    /// when the lineage already held these contents.
    ///
    /// [`Self::scratch_publish`] to [`ScratchDestination::Remote`].
    pub async fn scratch_push(
        &self,
        slot: &str,
        scope: &str,
        dir: &Path,
        parent: Option<&SnapshotMeta>,
        producer: &str,
    ) -> anyhow::Result<(u64, u64)> {
        let p = self
            .scratch_publish(
                slot,
                scope,
                dir,
                parent,
                producer,
                ScratchDestination::Remote,
            )
            .await?;
        Ok((p.generation, p.bytes))
    }

    /// Publish `dir` as the next generation of `scope`'s lineage, to `dest`.
    ///
    /// Writes into `scope` and **never** into a fallback, even the one the
    /// directory was seeded from. That is the isolation: a PR job cannot advance
    /// the lineage its base builds from. One set of rules for every
    /// destination: `parent + 1` within `scope` (0 when the parent is another
    /// lineage's), and no new generation for contents the lineage already holds.
    pub async fn scratch_publish(
        &self,
        slot: &str,
        scope: &str,
        dir: &Path,
        parent: Option<&SnapshotMeta>,
        producer: &str,
        dest: ScratchDestination<'_>,
    ) -> anyhow::Result<Published> {
        let backends = match dest {
            ScratchDestination::Remote => {
                let backends = self.remote_caches().writable_backends();
                if backends.is_empty() {
                    anyhow::bail!("no writable remote cache is configured");
                }
                backends
            }
            ScratchDestination::Dir { .. } => Vec::new(),
        };

        let d = dir.to_path_buf();
        let Packed {
            file,
            hash,
            len: blob_len,
            bytes,
        } = hcore::blocking::run(move || pack_to_temp(&d).map_err(std::io::Error::other)).await?;

        // `parent + 1` within this scope. A first publish — or one whose parent
        // came from another lineage — starts this scope at 0, because a
        // generation only means anything relative to the lineage it is in.
        let generation = match parent {
            Some(p) if p.scope == scope => p.generation + 1,
            _ => 0,
        };
        let meta = SnapshotMeta {
            format: SNAPSHOT_FORMAT,
            scope: scope.to_string(),
            generation,
            parent_scope: parent.map(|p| p.scope.clone()).unwrap_or_default(),
            parent_generation: parent.map(|p| p.generation).unwrap_or(0),
            bytes,
            heph_version: hcore::version::current().to_string(),
            producer: producer.to_string(),
            content_hash: String::new(),
            produced_at: dir.to_string_lossy().into_owned(),
        };

        // Nothing to publish: this lineage already holds exactly these bytes.
        // Skipping keeps a no-op CI run from adding a generation that says
        // nothing, which would otherwise make the chain grow with every build.
        let unchanged = parent.filter(|p| p.content_hash == hash && p.scope == scope);
        let (generation, meta) = match (unchanged, dest) {
            (Some(p), ScratchDestination::Remote) => {
                self.record_published_scratch(slot, p);
                return Ok(Published {
                    generation: p.generation,
                    bytes: 0,
                    unchanged: true,
                });
            }
            // A directory starts empty: the parent's own entry goes in it,
            // under its key, as the lineage already holds it.
            (Some(p), ScratchDestination::Dir { .. }) => (p.generation, p.clone()),
            (None, _) => (
                generation,
                SnapshotMeta {
                    content_hash: hash.clone(),
                    ..meta
                },
            ),
        };
        let stem = format!("{}{:016x}.{hash}", scope_prefix(slot, scope), generation);
        let meta_bytes = borsh::to_vec(&meta).context("encode scratch snapshot meta")?;

        if let ScratchDestination::Dir { root, prev } = dest {
            let (root, prev) = (root.to_path_buf(), prev.map(Path::to_path_buf));
            let bytes = hcore::blocking::run(move || {
                write_entry_into(&root, prev.as_deref(), &stem, file, blob_len, &meta_bytes)
            })
            .await?;
            return Ok(Published {
                generation,
                bytes,
                unchanged: unchanged.is_some(),
            });
        }

        let mut blob = tokio::io::BufReader::with_capacity(
            SNAPSHOT_STREAM_BUF,
            tokio::fs::File::from_std(file),
        );
        for (name, backend) in &backends {
            // Payload first, meta last. A reader only ever discovers an entry via
            // its meta, so a half-finished publish is invisible rather than a
            // head pointing at bytes that are not there yet.
            blob.seek(std::io::SeekFrom::Start(0))
                .await
                .context("rewind scratch snapshot temp file")?;
            write_all(backend.as_ref(), &format!("{stem}.tar.gz"), &mut blob)
                .await
                .with_context(|| format!("upload scratch snapshot to `{name}`"))?;
            write_all(
                backend.as_ref(),
                &format!("{stem}.meta"),
                &mut meta_bytes.as_slice(),
            )
            .await
            .with_context(|| format!("upload scratch meta to `{name}`"))?;
        }
        // Recorded locally so the next publish knows its parent, and so an
        // unchanged re-publish is recognized above.
        write_local_meta(&self.home, slot, scope, &meta);
        self.record_published_scratch(slot, &meta);
        Ok(Published {
            generation,
            bytes: blob_len,
            unchanged: false,
        })
    }

    /// Put a slot published to the remote caches in this engine's working
    /// set, so the job's snapshot carries it. A push runs outside any
    /// request, so it goes to the journal directly.
    fn record_published_scratch(&self, slot: &str, meta: &SnapshotMeta) {
        if let Some(journal) = self.working_set_journal() {
            journal.record_scratch_now(crate::engine::cache_snapshot::RecordedScratch {
                slot: slot.to_owned(),
                scope: meta.scope.clone(),
                generation: meta.generation,
                content_hash: meta.content_hash.clone(),
            });
        }
    }
}

/// Where [`Engine::scratch_publish`] writes a slot.
#[derive(Debug, Clone, Copy)]
pub enum ScratchDestination<'a> {
    /// Every writable remote cache (`heph tool scratch push`). The local
    /// lineage meta then records what was published, and the slot joins the
    /// engine's working set. Unchanged contents write nothing: the lineage
    /// already holds them.
    Remote,
    /// `root`, laid out as a remote cache is: a cache snapshot's generation
    /// being built. It starts empty, so unchanged contents are written under
    /// the parent's own key rather than skipped, hard-linked from `prev` (the
    /// previous generation) when it holds that entry. Local state is not
    /// touched.
    Dir {
        root: &'a Path,
        prev: Option<&'a Path>,
    },
}

/// What [`Engine::scratch_publish`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Published {
    /// The generation in the scope that holds the contents.
    pub generation: u64,
    /// Compressed bytes written to each destination; 0 when nothing was.
    pub bytes: u64,
    /// The lineage already held these contents, as `generation`.
    pub unchanged: bool,
}

/// Write one entry, `<stem>.tar.gz` then `<stem>.meta`, under `root`: the
/// archive's bytes.
///
/// Payload first, meta last, as on a bucket. The archive is hard-linked from
/// `prev` when that holds a regular file of this key and length (the key
/// carries the content hash) under real directories, else streamed from
/// `packed`: a symlinked slot or scope dir would lead out of `<dir>`.
fn write_entry_into(
    root: &Path,
    prev: Option<&Path>,
    stem: &str,
    mut packed: std::fs::File,
    len: u64,
    meta: &[u8],
) -> anyhow::Result<u64> {
    let tar_rel = format!("{stem}.tar.gz");
    let dst = root.join(&tar_rel);
    let parent = dst.parent().context("scratch entry has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let linked = prev.is_some_and(|prev| {
        let src = prev.join(&tar_rel);
        // Every directory between `prev` and the archive is a real one;
        // `symlink_metadata` follows all but the last component.
        let real_dirs = src
            .ancestors()
            .skip(1)
            .take_while(|a| *a != prev)
            .all(|a| std::fs::symlink_metadata(a).is_ok_and(|m| m.is_dir()));
        real_dirs
            && std::fs::symlink_metadata(&src).is_ok_and(|m| m.is_file() && m.len() == len)
            && std::fs::hard_link(&src, &dst).is_ok()
    });
    if !linked {
        let mut out =
            std::fs::File::create(&dst).with_context(|| format!("creating {}", dst.display()))?;
        std::io::copy(&mut packed, &mut out)
            .with_context(|| format!("writing {}", dst.display()))?;
    }
    let meta_path = root.join(format!("{stem}.meta"));
    std::fs::write(&meta_path, meta).with_context(|| format!("writing {}", meta_path.display()))?;
    Ok(len)
}

/// Stream `src` to `key` on `backend`, whole.
async fn write_all(
    backend: &dyn RemoteCacheBackend,
    key: &str,
    src: &mut (impl tokio::io::AsyncBufRead + Unpin + ?Sized),
) -> anyhow::Result<()> {
    let mut w = backend.open_write(key).await?;
    tokio::io::copy_buf(src, &mut w).await?;
    tokio::io::AsyncWriteExt::shutdown(&mut w).await?;
    Ok(())
}

/// The best head in one lineage across every source, by [`better`].
///
/// Shared by resolution and its explanation, so the trace can never name a
/// different head than the one a build would restore. Of equally good heads the
/// first found wins: the earlier source, then the earlier listing entry.
async fn best_in_scope(
    sources: &[ScratchSource<'_>],
    slot: &str,
    scope: &str,
) -> Option<RemoteHead> {
    let mut heads = Vec::new();
    for source in sources {
        heads.extend(heads_in(source, slot, scope).await);
    }
    heads.into_iter().min_by(better)
}

/// `.meta` reads in flight at once while walking one lineage.
///
/// A lineage is never pruned, so its entry count grows with every publish; one
/// read at a time made a cold resolve cost a round trip per generation.
const META_READ_CONCURRENCY: usize = 8;

/// Every readable head in one lineage of one cache.
///
/// A meta that will not decode, or one from a format this build does not know, is
/// skipped rather than failing the listing: it is a cache, and one bad entry must
/// not make the rest unreachable.
async fn heads_in(source: &ScratchSource<'_>, slot: &str, scope: &str) -> Vec<RemoteHead> {
    let prefix = scope_prefix(slot, scope);
    let Ok(names) = source.list_names(&prefix).await else {
        return Vec::new();
    };
    let stems = names.iter().filter_map(|name| {
        let stem = name.strip_suffix(".meta")?;
        // `list_names` may return a bare name or a full key depending on backend;
        // normalize to a full key so `open_read` finds it either way.
        Some(if stem.starts_with(KEY_PREFIX) {
            stem.to_string()
        } else {
            format!("{prefix}{stem}")
        })
    });
    // Collected before streaming: a closure in the stream's type makes the
    // future not `Send` to the engine's `async_recursion` callers.
    let reads: Vec<_> = stems.map(|stem| read_head(source, stem)).collect();
    // `buffered`, so the heads come out in listing order, as they did when the
    // metas were read one at a time.
    let heads: Vec<Option<RemoteHead>> = futures::stream::iter(reads)
        .buffered(META_READ_CONCURRENCY)
        .collect()
        .await;
    heads.into_iter().flatten().collect()
}

/// The head whose meta is `<stem>.meta`, or `None` when it is absent, will not
/// decode, or is from a format this build does not know.
async fn read_head(source: &ScratchSource<'_>, stem: String) -> Option<RemoteHead> {
    let buf = source.read_small(&format!("{stem}.meta")).await.ok()??;
    let meta = SnapshotMeta::try_from_slice(&buf).ok()?;
    (meta.format == SNAPSHOT_FORMAT).then(|| RemoteHead {
        cache: source.name().to_string(),
        stem,
        meta,
    })
}

/// Read the snapshot a local lineage was last seeded from or published as.
pub fn read_local_meta(home: &Path, slot: &str, scope: &str) -> Option<SnapshotMeta> {
    let bytes = std::fs::read(head_meta_path(home, slot, scope)).ok()?;
    let meta = SnapshotMeta::try_from_slice(&bytes).ok()?;
    (meta.format == SNAPSHOT_FORMAT).then_some(meta)
}

/// Record what a local lineage descends from. Best-effort: losing it costs a
/// generation reset, never a wrong build.
///
/// Write-then-rename through a uniquely named temp file, so a concurrent reader
/// never sees half a meta and two writers never share a temp file.
pub fn write_local_meta(home: &Path, slot: &str, scope: &str, meta: &SnapshotMeta) {
    let path = head_meta_path(home, slot, scope);
    let write = || -> anyhow::Result<()> {
        let parent = path.parent().context("scratch head meta has no parent")?;
        std::fs::create_dir_all(parent).with_context(|| format!("create {parent:?}"))?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("create a temp file in {parent:?}"))?;
        let bytes = borsh::to_vec(meta).context("encode the meta")?;
        std::io::Write::write_all(&mut tmp, &bytes)
            .with_context(|| format!("write {:?}", tmp.path()))?;
        tmp.persist(&path)
            .with_context(|| format!("rename into {path:?}"))?;
        Ok(())
    };
    if let Err(err) = write() {
        tracing::debug!(slot, scope, error = %format!("{err:#}"), "recording scratch lineage meta");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(scope: &str, generation: u64, bytes: u64) -> SnapshotMeta {
        SnapshotMeta {
            format: SNAPSHOT_FORMAT,
            scope: scope.to_string(),
            generation,
            parent_scope: scope.to_string(),
            parent_generation: generation.saturating_sub(1),
            bytes,
            heph_version: "test".to_string(),
            producer: String::new(),
            content_hash: format!("h{generation}"),
            produced_at: "/tmp".to_string(),
        }
    }

    fn head(stem: &str, m: SnapshotMeta) -> RemoteHead {
        RemoteHead {
            cache: "c".to_string(),
            stem: stem.to_string(),
            meta: m,
        }
    }

    #[test]
    fn a_higher_generation_always_wins() {
        let a = head("a", meta("m", 12, 1));
        let b = head("b", meta("m", 11, 999_999));
        assert!(better(&a, &b).is_lt(), "generation beats size");
    }

    /// A same-generation fork — two runners both publishing `parent + 1` — is
    /// expected, not an error. The tie-break only has to be deterministic so every
    /// reader converges; preferring the fuller cache makes it a useful choice too.
    #[test]
    fn a_same_generation_fork_prefers_the_fuller_cache() {
        let a = head("a", meta("m", 12, 900));
        let b = head("b", meta("m", 12, 100));
        assert!(better(&a, &b).is_lt());
    }

    #[test]
    fn the_order_is_total_so_readers_converge() {
        let a = head("aaa", meta("m", 12, 100));
        let b = head("bbb", meta("m", 12, 100));
        assert!(better(&a, &b).is_lt());
        assert!(better(&b, &a).is_gt());
    }

    /// Zero-padded generation in the key means lexicographic order *is* generation
    /// order, so a listing can be sorted without fetching anything.
    #[test]
    fn keys_sort_by_generation_across_the_padding_width() {
        let p = scope_prefix("slot", "master");
        let k = |g: u64| format!("{p}{g:016x}.hash");
        assert!(k(9) < k(10));
        assert!(k(255) < k(256));
        assert!(k(u32::MAX as u64) < k(u32::MAX as u64 + 1));
    }

    /// A branch name is one key component, or two branches collide the moment one
    /// is a path prefix of another.
    #[test]
    fn a_branch_name_stays_one_key_component() {
        let p = scope_prefix("slot", "feature/x");
        assert_eq!(p, "scratch/v1/slot/feature_x/");
        // The point: `feature/x` must not become two components, or it would
        // collide with a branch literally called `x` under `feature`, and a
        // prefix list for one would sweep up the other.
        assert!(!p.contains("feature/x"), "{p}");
        assert_eq!(
            scope_prefix("slot", "feature/x").matches('/').count(),
            scope_prefix("slot", "featurex").matches('/').count(),
            "a slash in a branch name must not add a level"
        );
    }

    #[test]
    fn pack_and_unpack_round_trip_including_symlinks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (src, dst) = (tmp.path().join("s"), tmp.path().join("d"));
        std::fs::create_dir_all(src.join("sub")).expect("mkdir");
        std::fs::write(src.join("sub").join("f"), b"contents").expect("write");
        std::os::unix::fs::symlink("elsewhere", src.join("link")).expect("symlink");

        let (blob, bytes) = pack(&src, Vec::new()).expect("pack");
        assert_eq!(bytes, 8);
        unpack(blob.inner.as_slice(), &dst).expect("unpack");

        assert_eq!(
            std::fs::read(dst.join("sub").join("f")).expect("read"),
            b"contents"
        );
        // A link must arrive as a link — following it on the way out would publish
        // whatever it points at to every machine that picks the snapshot up.
        let md = std::fs::symlink_metadata(dst.join("link")).expect("stat");
        assert!(md.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(dst.join("link")).expect("readlink"),
            Path::new("elsewhere")
        );
    }

    /// The hash of the streamed archive is the hash of the whole archive, so a
    /// snapshot's key and meta are what hashing it in memory gave.
    #[test]
    fn the_streamed_digest_is_the_digest_of_the_whole_archive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("lineage").join("head");
        std::fs::create_dir_all(src.join("sub")).expect("mkdir");
        for i in 0..64 {
            // Enough incompressible-ish content to span many writes.
            let body: Vec<u8> = (0..4096u32).map(|b| (b * 31 + i) as u8).collect();
            std::fs::write(src.join("sub").join(format!("f{i}")), body).expect("write");
        }

        let (digest, _) = pack(&src, Vec::new()).expect("pack");
        assert_eq!(
            digest.hash(),
            format!("{:016x}", xxhash_rust::xxh3::xxh3_64(&digest.inner))
        );
        assert_eq!(digest.len, digest.inner.len() as u64);

        let mut packed = pack_to_temp(&src).expect("pack to temp");
        assert_eq!(packed.hash, digest.hash(), "same tree, same archive");
        assert_eq!(packed.len, digest.len);
        let mut on_disk = Vec::new();
        std::io::Read::read_to_end(&mut packed.file, &mut on_disk).expect("read temp");
        assert_eq!(on_disk, digest.inner, "the temp file holds the archive");
        assert_eq!(
            std::fs::read_dir(tmp.path().join("lineage"))
                .expect("ls lineage")
                .count(),
            1,
            "the temp file is anonymous: nothing but `head` in the lineage",
        );
    }

    /// Backend serving one object, optionally dying after `fail_after` bytes of
    /// its body.
    struct OneObject {
        body: Vec<u8>,
        fail_after: Option<usize>,
    }

    /// A body that errors on its first read.
    struct Broken;

    impl tokio::io::AsyncRead for Broken {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("connection reset")))
        }
    }

    #[async_trait::async_trait]
    impl RemoteCacheBackend for OneObject {
        async fn open_read(
            &self,
            _key: &str,
        ) -> anyhow::Result<Option<std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>>> {
            let r: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>> = match self.fail_after {
                None => Box::pin(std::io::Cursor::new(self.body.clone())),
                Some(n) => Box::pin(tokio::io::AsyncReadExt::chain(
                    std::io::Cursor::new(self.body.get(..n).expect("prefix").to_vec()),
                    Broken,
                )),
            };
            Ok(Some(r))
        }
        async fn open_write(
            &self,
            _key: &str,
        ) -> anyhow::Result<std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>> {
            anyhow::bail!("read-only")
        }
        async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn list_names(&self, _prefix: &str) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    /// Backend holding a flat set of objects under one lineage, recording how
    /// many reads were in flight at once.
    #[derive(Default)]
    struct Lineage {
        objects: std::collections::BTreeMap<String, Vec<u8>>,
        in_flight: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl RemoteCacheBackend for Lineage {
        async fn open_read(
            &self,
            key: &str,
        ) -> anyhow::Result<Option<std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>>> {
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.in_flight.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            self.in_flight.fetch_sub(1, SeqCst);
            let name = key.rsplit('/').next().unwrap_or(key);
            Ok(self.objects.get(name).map(|b| {
                Box::pin(std::io::Cursor::new(b.clone()))
                    as std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>
            }))
        }
        async fn open_write(
            &self,
            _key: &str,
        ) -> anyhow::Result<std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>> {
            anyhow::bail!("read-only")
        }
        async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn list_names(&self, _prefix: &str) -> anyhow::Result<Vec<String>> {
            Ok(self.objects.keys().cloned().collect())
        }
    }

    /// The metas are read concurrently, the heads still come out in listing
    /// order, and an entry that will not decode or is from another format is
    /// skipped without hiding the rest.
    #[tokio::test]
    async fn heads_in_reads_metas_concurrently_and_skips_bad_ones() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut objects = std::collections::BTreeMap::new();
        let enc = |m: &SnapshotMeta| borsh::to_vec(m).expect("encode");
        for g in 0..16u64 {
            objects.insert(format!("{g:016x}.h.meta"), enc(&meta("master", g, 1)));
            objects.insert(format!("{g:016x}.h.tar.gz"), b"payload".to_vec());
        }
        objects.insert("garbage.meta".to_string(), b"not borsh".to_vec());
        let mut foreign = meta("master", 99, 1);
        foreign.format = SNAPSHOT_FORMAT + 1;
        objects.insert(format!("{:016x}.h.meta", 99), enc(&foreign));

        let backend = std::sync::Arc::new(Lineage {
            objects,
            ..Lineage::default()
        });
        let set = crate::engine::remote_cache::RemoteCacheSet::with_backend(
            backend.clone(),
            tmp.path().to_path_buf(),
        );
        let sources = set.scratch_sources().await;

        let heads = heads_in(&sources[0], "slot", "master").await;
        let generations: Vec<_> = heads.iter().map(|h| h.meta.generation).collect();
        assert_eq!(generations, (0..16).collect::<Vec<_>>());
        assert_eq!(
            heads[3].stem,
            format!("{}{:016x}.h", scope_prefix("slot", "master"), 3),
            "a bare listed name is normalized to a full key",
        );
        let peak = backend.peak.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            (2..=META_READ_CONCURRENCY).contains(&peak),
            "metas are read concurrently, within the bound: peak {peak}",
        );
    }

    /// The newest generation wins, a same-generation fork goes to the fuller
    /// cache, and an empty lineage has no head.
    #[tokio::test]
    async fn best_in_scope_picks_what_better_ranks_first() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let enc = |m: &SnapshotMeta| borsh::to_vec(m).expect("encode");
        let objects = [
            ("0000000000000004.a.meta", enc(&meta("master", 4, 900))),
            ("0000000000000005.b.meta", enc(&meta("master", 5, 100))),
            ("0000000000000005.c.meta", enc(&meta("master", 5, 300))),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let set = crate::engine::remote_cache::RemoteCacheSet::with_backend(
            std::sync::Arc::new(Lineage {
                objects,
                ..Lineage::default()
            }),
            tmp.path().to_path_buf(),
        );
        let sources = set.scratch_sources().await;

        let best = best_in_scope(&sources, "slot", "master")
            .await
            .expect("a head");
        assert!(best.stem.ends_with("0000000000000005.c"), "{}", best.stem);
        assert!(best_in_scope(&[], "slot", "master").await.is_none());
    }

    fn packed_tree(root: &Path) -> Vec<u8> {
        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("mkdir");
        std::fs::write(src.join("new"), b"from the snapshot").expect("write");
        pack(&src, Vec::new()).expect("pack").0.inner
    }

    #[tokio::test]
    async fn a_pull_unpacks_what_it_downloaded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let body = packed_tree(tmp.path());
        let len = body.len() as u64;
        let set = crate::engine::remote_cache::RemoteCacheSet::with_backend(
            std::sync::Arc::new(OneObject {
                body,
                fail_after: None,
            }),
            tmp.path().to_path_buf(),
        );
        let sources = set.scratch_sources().await;
        let dir = tmp.path().join("lineage").join("head");

        let n = pull_snapshot(&sources[0], "k.tar.gz", &dir)
            .await
            .expect("pull");
        assert_eq!(n, len, "reports the bytes downloaded");
        assert_eq!(
            std::fs::read(dir.join("new")).expect("read"),
            b"from the snapshot"
        );
        assert_eq!(
            std::fs::read_dir(tmp.path().join("lineage"))
                .expect("ls lineage")
                .count(),
            1,
            "no temp file is left beside `head`",
        );
    }

    /// A transfer that dies part way must leave the directory exactly as it was:
    /// the archive is unpacked only once it is whole.
    #[tokio::test]
    async fn a_download_that_dies_midway_leaves_the_dir_untouched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let body = packed_tree(tmp.path());
        let half = body.len() / 2;
        let set = crate::engine::remote_cache::RemoteCacheSet::with_backend(
            std::sync::Arc::new(OneObject {
                body,
                fail_after: Some(half),
            }),
            tmp.path().to_path_buf(),
        );
        let sources = set.scratch_sources().await;
        let lineage = tmp.path().join("lineage");
        let dir = lineage.join("head");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("old"), b"was here").expect("write");

        let err = pull_snapshot(&sources[0], "k.tar.gz", &dir)
            .await
            .expect_err("the body fails half way");
        assert!(format!("{err:#}").contains("connection reset"), "{err:#}");

        let names: Vec<_> = std::fs::read_dir(&dir)
            .expect("ls head")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["old"], "nothing was unpacked");
        assert_eq!(std::fs::read(dir.join("old")).expect("read"), b"was here");
        assert_eq!(
            std::fs::read_dir(&lineage).expect("ls lineage").count(),
            1,
            "no temp file is left beside `head`",
        );
    }

    #[test]
    fn local_meta_round_trips_and_rejects_a_foreign_format() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_local_meta(tmp.path(), "s", "master", &meta("master", 3, 10));
        assert_eq!(
            read_local_meta(tmp.path(), "s", "master"),
            Some(meta("master", 3, 10))
        );
        // Overwritten in place, through a temp file that does not outlive it.
        write_local_meta(tmp.path(), "s", "master", &meta("master", 5, 10));
        assert_eq!(
            read_local_meta(tmp.path(), "s", "master").map(|m| m.generation),
            Some(5)
        );
        let lineage = crate::engine::scratch_store::lineage_dir(tmp.path(), "s", "master");
        let names: Vec<_> = std::fs::read_dir(&lineage)
            .expect("ls lineage")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["head.meta"]);

        let mut future = meta("master", 4, 10);
        future.format = SNAPSHOT_FORMAT + 1;
        let p = head_meta_path(tmp.path(), "s", "master");
        std::fs::write(&p, borsh::to_vec(&future).expect("ser")).expect("write");
        assert_eq!(read_local_meta(tmp.path(), "s", "master"), None);
    }

    #[test]
    fn an_absent_local_meta_is_none_rather_than_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_local_meta(tmp.path(), "nope", "master"), None);
    }

    /// Every regular file under `root`, as `(relative path, bytes)`, sorted.
    fn files_under(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("ls") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let rel = path.strip_prefix(root).expect("under root");
                    out.push((
                        rel.to_string_lossy().into_owned(),
                        std::fs::read(&path).expect("read"),
                    ));
                }
            }
        }
        out.sort();
        out
    }

    /// `scratch_push` now goes through `scratch_publish` with a destination;
    /// a bucket must get exactly what it got before: the archive `pack`
    /// produces and the borsh meta, under `<prefix><gen:016x>.<hash>`, the
    /// lineage meta recorded locally, and nothing at all for unchanged
    /// contents. A snapshot generation (`Dir`) gets the same entry, byte for
    /// byte.
    #[tokio::test]
    async fn push_to_a_bucket_is_unchanged_by_the_destination_refactor() {
        let bucket = tempfile::tempdir().expect("bucket");
        let ws = tempfile::tempdir().expect("ws");
        let engine = Engine::new(crate::engine::Config {
            remote_caches: vec![crate::engine::RemoteCacheDef {
                name: "shared".to_string(),
                uri: format!("file://{}", bucket.path().display()),
                read: true,
                write: true,
                concurrency: 10,
                endpoint: None,
                region: None,
            }],
            ..crate::engine::Config::for_tests(ws.path())
        })
        .expect("engine");

        let src = ws.path().join("slot").join("head");
        std::fs::create_dir_all(src.join("sub")).expect("mkdir");
        std::fs::write(src.join("a"), b"alpha").expect("write a");
        std::fs::write(src.join("sub").join("b"), vec![7u8; 4096]).expect("write b");
        let (digest, bytes) = pack(&src, Vec::new()).expect("pack");
        let hash = digest.hash();
        let archive = digest.inner;

        let (generation, len) = engine
            .scratch_push("s1", "main", &src, None, "run-1")
            .await
            .expect("push");
        assert_eq!((generation, len), (0, archive.len() as u64));

        let stem = format!("scratch/v1/s1/main/{:016x}.{hash}", 0);
        let meta = SnapshotMeta {
            format: SNAPSHOT_FORMAT,
            scope: "main".to_string(),
            generation: 0,
            parent_scope: String::new(),
            parent_generation: 0,
            bytes,
            heph_version: hcore::version::current().to_string(),
            producer: "run-1".to_string(),
            content_hash: hash.clone(),
            produced_at: src.to_string_lossy().into_owned(),
        };
        let want = vec![
            (
                format!("{stem}.meta"),
                borsh::to_vec(&meta).expect("encode"),
            ),
            (format!("{stem}.tar.gz"), archive),
        ];
        assert_eq!(files_under(bucket.path()), want);
        let parent = read_local_meta(&engine.home, "s1", "main");
        assert_eq!(parent.as_ref(), Some(&meta), "the lineage meta is recorded");

        // Unchanged contents: no new generation, nothing written.
        let again = engine
            .scratch_push("s1", "main", &src, parent.as_ref(), "run-2")
            .await
            .expect("push again");
        assert_eq!(again, (0, 0));
        assert_eq!(files_under(bucket.path()), want);

        // The same publish into a directory writes the same entry.
        let generation_dir = tempfile::tempdir().expect("generation");
        let published = engine
            .scratch_publish(
                "s1",
                "main",
                &src,
                parent.as_ref(),
                "run-2",
                ScratchDestination::Dir {
                    root: generation_dir.path(),
                    prev: None,
                },
            )
            .await
            .expect("publish to a dir");
        assert!(published.unchanged, "{published:?}");
        assert_eq!(published.generation, 0);
        assert_eq!(files_under(generation_dir.path()), want);

        // Changed contents: `parent + 1`, in both.
        std::fs::write(src.join("a"), b"beta").expect("rewrite a");
        let (g1, _) = engine
            .scratch_push("s1", "main", &src, parent.as_ref(), "run-3")
            .await
            .expect("push changed");
        assert_eq!(g1, 1);
        let next = engine
            .scratch_publish(
                "s1",
                "main",
                &src,
                parent.as_ref(),
                "run-3",
                ScratchDestination::Dir {
                    root: generation_dir.path(),
                    prev: None,
                },
            )
            .await
            .expect("publish changed to a dir");
        assert_eq!((next.generation, next.unchanged), (1, false));
        assert_eq!(
            files_under(bucket.path()),
            files_under(generation_dir.path())
        );
    }
}
