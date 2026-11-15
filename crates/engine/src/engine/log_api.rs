//! The log API: a target's log, by address.
//!
//! A log is reached by address, never by a path. The API reads whichever of
//! these is newest (spec "Where a log comes from"):
//!
//! - **a run going on**: its live file, `<home>/runs/<engine-id>/<n>.log`,
//!   found in this engine's own [`LiveRuns`] or, for another heph on the same
//!   workspace, through a [`LiveRunLogLocator`] (the session records, which
//!   the engine cannot read itself); a running run is always the newest;
//! - **a `cache = false` success**: its live file, kept until its heph exits;
//! - **a failure**: the failure slot's `log.txt`;
//! - **a success or a local hit**: the cache entry's `Log` artifact, a tar of
//!   one file in seekable zstd, read through the seek table, so a tail
//!   decodes about one frame;
//! - **a remote hit**: the same artifact, fetched on demand, once, into the
//!   local entry ([`Engine::open_log`] with `fetch_remote`), under the shared
//!   read lock, only after the remote manifest is checked against the local
//!   mirrored one.
//!
//! Among the finished ones, the newest by time wins.
//!
//! **Reading never builds** (I10): no resolve, no hash, no exclusive lock, no
//! run. The only lock taken is a remote fetch's shared read lock on the
//! revision. Nothing here reaches a hash (I6).
//!
//! **Blocking.** [`OpenLog`]'s reads and [`LogFollower::poll`] do file IO;
//! call them through `hcore::blocking`, off the render task.

use crate::engine::Engine;
use crate::engine::cache_codec;
use crate::engine::failure_slot;
use crate::engine::local_cache::{
    LocalCache, Manifest, ManifestArtifact, ManifestArtifactType, NotFoundError,
};
use crate::engine::run_log::{LiveRun, RunLog};
use anyhow::Context as _;
use hcore::hartifactcontent::ReadSeek;
use hcore::hartifactcontent::tar_index::{IndexEntryKind, TarIndex};
use hcore::session_record::{LiveRunLogLocator, LocatedRunLog};
use hmodel::htaddr::Addr;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// The most bytes a tail returns, whatever was asked.
pub const TAIL_MAX: usize = 256 * 1024;
/// The most bytes one [`LogFollower::poll`] returns.
const CHUNK: usize = 64 * 1024;
/// How often a follower asks the locator, or looks at the stored sources,
/// while it waits: each costs reading files.
const RECHECK: Duration = Duration::from_millis(250);
/// Largest stored blob read into memory when its backend has no seekable
/// reader (a blob under the spill threshold, held in sqlite).
const IN_MEMORY_MAX: u64 = 64 << 20;

/// Where a log was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogSource {
    /// A run's live file. `running` while the run goes on; a finished one is
    /// a `cache = false` success's kept file. `session` names the other
    /// heph's session it was found through, `None` for this engine's own.
    Live {
        running: bool,
        session: Option<String>,
    },
    /// The failure slot.
    FailureSlot,
    /// The cache entry's `Log` artifact. `remote` when the entry was mirrored
    /// from a remote cache and its log fetched from there.
    Cache { remote: bool },
}

/// What a log is, for the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogInfo {
    pub source: LogSource,
    /// The input hash of the run that wrote it.
    pub hashin: String,
    /// When: the run's start for a live file, the failure for a slot, the
    /// entry's creation for a cache entry. Milliseconds since the epoch.
    pub at_unix_ms: u64,
}

/// A log exists but cannot be read: a remote entry whose manifest differs
/// from the local one, a fetch that failed, an encoding no reader decodes.
/// The UI says "log unavailable".
#[derive(Debug, thiserror::Error)]
#[error("log unavailable: {reason}")]
pub struct LogUnavailable {
    pub info: LogInfo,
    pub reason: String,
}

/// How to look a log up.
#[derive(Debug, Clone, Default)]
pub struct LogOptions {
    /// Finds runs going on in other hephs on this workspace.
    pub locator: Option<Arc<dyn LiveRunLogLocator>>,
    /// Fetch a remote hit's log into the local entry. Only a reader that
    /// shows a log asks for it; a list's last-line column never does.
    pub fetch_remote: bool,
}

/// One log, opened: a snapshot of its length at open.
pub struct OpenLog {
    info: LogInfo,
    src: Box<dyn ReadSeek + Send>,
    /// Where the log's bytes start in `src`.
    base: u64,
    len: u64,
}

impl std::fmt::Debug for OpenLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenLog")
            .field("info", &self.info)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl OpenLog {
    fn plain(info: LogInfo, file: File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self {
            info,
            src: Box::new(file),
            base: 0,
            len,
        })
    }

    pub fn info(&self) -> &LogInfo {
        &self.info
    }

    /// The log's length when it was opened.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The log's last `max` bytes, never more than [`TAIL_MAX`]. Reads only
    /// those bytes (for a cached log, the frames that hold them).
    pub fn tail(&mut self, max: usize) -> io::Result<Vec<u8>> {
        let n = self.len.min(max.min(TAIL_MAX) as u64);
        self.read_range(self.len - n, n)
    }

    /// The log from byte `from` to its end, as one reader.
    pub fn reader_from(mut self, from: u64) -> io::Result<impl Read + Send + 'static> {
        let from = from.min(self.len);
        self.src.seek(SeekFrom::Start(self.base + from))?;
        Ok(self.src.take(self.len - from))
    }

    /// The whole log, as one reader.
    pub fn reader(self) -> io::Result<impl Read + Send + 'static> {
        self.reader_from(0)
    }

    fn read_range(&mut self, from: u64, n: u64) -> io::Result<Vec<u8>> {
        self.src.seek(SeekFrom::Start(self.base + from))?;
        let mut out = Vec::with_capacity(usize::try_from(n).map_err(io::Error::other)?);
        (&mut self.src).take(n).read_to_end(&mut out)?;
        Ok(out)
    }
}

/// A finished log the API could read, before it is opened.
enum Stored {
    /// A finished live file (a `cache = false` success's).
    Retained(LiveRun),
    Slot(failure_slot::SlotManifest),
    Cache {
        manifest: Manifest,
        log: ManifestArtifact,
    },
}

impl Stored {
    fn at_unix_ms(&self) -> u64 {
        match self {
            Self::Retained(run) => run.at_unix_ms,
            Self::Slot(m) => m.failed_at,
            Self::Cache { manifest, .. } => created_ms(manifest),
        }
    }
}

/// When a cache entry was written, in milliseconds since the epoch.
fn created_ms(manifest: &Manifest) -> u64 {
    u64::try_from(manifest.created_at_nanos / 1_000_000).unwrap_or(0)
}

/// A run going on, found in-process or through the locator.
struct Running {
    file: File,
    hashin: String,
    at_unix_ms: u64,
    end: LiveEnd,
}

/// How a follower learns that the run it reads has ended.
enum LiveEnd {
    /// This engine's run: its `ended` flag, set after its last write.
    InProcess(Weak<RunLog>),
    /// Another heph's: its file unlinked, or its session saying so.
    Located { file: String, session: String },
}

/// What [`Engine::latest_log`] found.
enum Latest {
    Running(Running),
    Stored(Stored),
    None,
}

impl Engine {
    /// `addr`'s newest log, opened. `Ok(None)` when heph holds none for it;
    /// [`LogUnavailable`] when one exists but cannot be read.
    ///
    /// Never builds. A remote hit's log is fetched only when `fetch_remote`
    /// is set, and then once: the next open reads the local copy.
    pub async fn open_log(
        self: &Arc<Self>,
        addr: &Addr,
        opts: &LogOptions,
    ) -> anyhow::Result<Option<OpenLog>> {
        let engine = Arc::clone(self);
        let (a, locator) = (addr.clone(), opts.locator.clone());
        let latest =
            hcore::blocking::run(move || engine.latest_log(&a, locator.as_deref())).await?;
        match latest {
            Latest::None => Ok(None),
            Latest::Running(run) => Ok(Some(OpenLog::plain(
                LogInfo {
                    source: LogSource::Live {
                        running: true,
                        session: match &run.end {
                            LiveEnd::InProcess(_) => None,
                            LiveEnd::Located { session, .. } => Some(session.clone()),
                        },
                    },
                    hashin: run.hashin,
                    at_unix_ms: run.at_unix_ms,
                },
                run.file,
            )?)),
            Latest::Stored(stored) => self
                .open_stored(addr, stored, opts.fetch_remote)
                .await
                .map(Some),
        }
    }

    /// Follow `addr`'s log: the run going on, from its first byte, to its
    /// end. When none is going on, `wait_for_start` waits for the next one
    /// (or for a newer finished log to appear); otherwise the newest
    /// finished log is read through once. `Ok(None)` when there is nothing
    /// to read and nothing to wait for.
    pub async fn follow_log(
        self: &Arc<Self>,
        addr: &Addr,
        opts: &LogOptions,
        wait_for_start: bool,
    ) -> anyhow::Result<Option<LogFollower>> {
        let engine = Arc::clone(self);
        let (a, locator) = (addr.clone(), opts.locator.clone());
        let latest =
            hcore::blocking::run(move || engine.latest_log(&a, locator.as_deref())).await?;
        let state = match latest {
            Latest::Running(run) => Follow::Live(run),
            Latest::Stored(stored) if wait_for_start => Follow::Waiting {
                baseline: Some(stored.at_unix_ms()),
            },
            Latest::None if wait_for_start => Follow::Waiting { baseline: None },
            Latest::None => return Ok(None),
            Latest::Stored(stored) => {
                Follow::Stored(self.open_stored(addr, stored, opts.fetch_remote).await?)
            }
        };
        Ok(Some(LogFollower {
            engine: Arc::clone(self),
            addr: addr.clone(),
            locator: opts.locator.clone(),
            state,
            offset: 0,
            checked: None,
        }))
    }

    /// The newest of `addr`'s logs. Blocking: reads the slot's manifest and
    /// the cache entries' manifests, and asks the locator.
    fn latest_log(
        &self,
        addr: &Addr,
        locator: Option<&dyn LiveRunLogLocator>,
    ) -> anyhow::Result<Latest> {
        let own = self.run_logs.live().get(addr);
        let mut retained = None;
        if let Some(run) = own {
            if run.log.upgrade().is_some_and(|log| !log.ended()) {
                // `None`: unlinked meanwhile, so the run ended, and what
                // replaced it is among the stored sources below.
                if let Some(file) = open_plain(&run.path)? {
                    return Ok(Latest::Running(Running {
                        file,
                        hashin: run.hashin.clone(),
                        at_unix_ms: run.at_unix_ms,
                        end: LiveEnd::InProcess(run.log),
                    }));
                }
            } else if run.path.exists() {
                retained = Some(run);
            }
        }
        if let Some(locator) = locator {
            match locator
                .locate(&self.home, &addr.format())
                .context("finding a live run log in this workspace's sessions")?
            {
                Some(LocatedRunLog::Live {
                    record,
                    at_unix_ms,
                    session,
                    file,
                }) if !retained
                    .as_ref()
                    .is_some_and(|r| r.path == self.home.join(&record.file)) =>
                {
                    return Ok(Latest::Running(Running {
                        file,
                        hashin: record.hashin,
                        at_unix_ms,
                        end: LiveEnd::Located {
                            file: record.file,
                            session,
                        },
                    }));
                }
                // Ended: what replaced it is among the stored sources.
                _ => {}
            }
        }
        let mut newest = retained.map(Stored::Retained);
        let mut consider = |s: Stored| {
            if newest
                .as_ref()
                .is_none_or(|n| s.at_unix_ms() > n.at_unix_ms())
            {
                newest = Some(s);
            }
        };
        if let Some(slot) = self.failure_slots.read(addr) {
            consider(Stored::Slot(slot));
        }
        if let Some(entry) = self.newest_cached_log(addr, None)? {
            consider(entry);
        }
        Ok(newest.map_or(Latest::None, Latest::Stored))
    }

    /// The newest local cache entry of `addr` that keeps a `Log`, or the one
    /// at `hashin`. Blocking.
    fn newest_cached_log(
        &self,
        addr: &Addr,
        hashin: Option<&str>,
    ) -> anyhow::Result<Option<Stored>> {
        let hashins = match hashin {
            Some(h) => vec![h.to_owned()],
            None => self
                .local_cache
                .list_target_entries(addr)
                .with_context(|| format!("listing the cache entries of {addr}"))?,
        };
        let mut newest: Option<(Manifest, ManifestArtifact)> = None;
        for h in hashins {
            let Some(manifest) = self.read_manifest(addr, &h)? else {
                continue;
            };
            let Some(log) = manifest
                .artifacts
                .iter()
                .find(|a| a.r#type == ManifestArtifactType::Log)
                .cloned()
            else {
                continue;
            };
            if newest
                .as_ref()
                .is_none_or(|(n, _)| manifest.created_at_nanos > n.created_at_nanos)
            {
                newest = Some((manifest, log));
            }
        }
        Ok(newest.map(|(manifest, log)| Stored::Cache { manifest, log }))
    }

    /// Open a finished log, fetching a remote hit's when asked to.
    async fn open_stored(
        self: &Arc<Self>,
        addr: &Addr,
        stored: Stored,
        fetch_remote: bool,
    ) -> anyhow::Result<OpenLog> {
        let (manifest, log) = match stored {
            Stored::Retained(run) => {
                let info = LogInfo {
                    source: LogSource::Live {
                        running: false,
                        session: None,
                    },
                    hashin: run.hashin.clone(),
                    at_unix_ms: run.at_unix_ms,
                };
                return hcore::blocking::run(move || match open_plain(&run.path)? {
                    Some(file) => Ok(OpenLog::plain(info, file)?),
                    None => Err(LogUnavailable {
                        info,
                        reason: "the run's log was removed".into(),
                    }
                    .into()),
                })
                .await;
            }
            Stored::Slot(slot) => {
                let path = self.failure_slots.slot_dir(addr).join(failure_slot::LOG);
                let info = LogInfo {
                    source: LogSource::FailureSlot,
                    hashin: slot.hashin,
                    at_unix_ms: slot.failed_at,
                };
                return hcore::blocking::run(move || match open_plain(&path)? {
                    Some(file) => Ok(OpenLog::plain(info, file)?),
                    None => Err(LogUnavailable {
                        info,
                        reason: "the failure slot keeps no log".into(),
                    }
                    .into()),
                })
                .await;
            }
            Stored::Cache { manifest, log } => (manifest, log),
        };
        let at = created_ms(&manifest);
        let hashin = manifest.hashin.clone();
        let resident = self
            .exists_local(addr, &hashin, &log.name)
            .await
            .with_context(|| format!("probing the log of {addr}"))?;
        let mut info = LogInfo {
            source: LogSource::Cache { remote: !resident },
            hashin: hashin.clone(),
            at_unix_ms: at,
        };
        if !resident {
            if !fetch_remote {
                return Err(LogUnavailable {
                    info,
                    reason: "the log is in the remote cache and was not fetched".into(),
                }
                .into());
            }
            if let Err(reason) = self.fetch_remote_log(addr, &manifest, &log.name).await? {
                return Err(LogUnavailable { info, reason }.into());
            }
            info.source = LogSource::Cache { remote: true };
        }
        let cache = Arc::clone(&self.local_cache);
        let a = addr.clone();
        hcore::blocking::run(move || open_cached(cache.as_ref(), &a, &log, info)).await
    }

    /// Fetch the `Log` blob `name` of `addr`'s remote-mirrored entry into the
    /// local one, once, under the revision's shared read lock (which keeps
    /// GC off it), and only when the remote manifest is the one the local
    /// entry mirrored: a log must never describe another run than the
    /// outputs held. `Ok(Err(reason))` when it cannot be served; nothing is
    /// written then.
    async fn fetch_remote_log(
        self: &Arc<Self>,
        addr: &Addr,
        local: &Manifest,
        name: &str,
    ) -> anyhow::Result<Result<(), String>> {
        let hashin = local.hashin.as_str();
        let ctoken = hcore::hasync::StdCancellationToken::new();
        let _read = self
            .result_lock
            .read(addr, hashin, &ctoken)
            .await
            .with_context(|| format!("taking the read lock of {addr} {hashin}"))?;
        // Another reader fetched it while this one waited.
        if self.exists_local(addr, hashin, name).await? {
            return Ok(Ok(()));
        }
        let Some(rev) = self
            .remote_caches()
            .fetch_manifest(&ctoken, addr, hashin)
            .await
            .with_context(|| format!("fetching the remote manifest of {addr} {hashin}"))?
        else {
            return Ok(Err("no remote cache holds the entry any more".into()));
        };
        let remote = crate::engine::remote_cache::local_manifest_from_remote(&rev.manifest, hashin);
        if !same_revision(local, &remote)? {
            return Ok(Err(
                "the remote entry is not the one the local entry mirrored".into(),
            ));
        }
        if !self
            .pull_remote_blobs(&ctoken, addr, hashin, &rev, &[name.to_owned()])
            .await
            .with_context(|| format!("fetching the log of {addr} {hashin}"))?
        {
            return Ok(Err("the remote cache could not serve the log".into()));
        }
        Ok(Ok(()))
    }
}

/// Whether two manifests name the same revision: target, input hash and
/// every artifact, whenever each was written.
fn same_revision(a: &Manifest, b: &Manifest) -> anyhow::Result<bool> {
    let key = |m: &Manifest| borsh::to_vec(&(&m.target, &m.hashin, &m.artifacts));
    Ok(key(a).context("encoding a manifest")? == key(b).context("encoding a manifest")?)
}

/// Open a plain log file for reading, refusing a symlink. `None` when it is
/// gone.
fn open_plain(path: &Path) -> io::Result<Option<File>> {
    use std::os::unix::fs::OpenOptionsExt as _;
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) if f.metadata()?.is_file() => Ok(Some(f)),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a log that is not a regular file",
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Open the local `Log` blob `log` of `addr`'s entry: decode it with random
/// access and find its one file in the tar, reading only headers.
fn open_cached(
    cache: &dyn LocalCache,
    addr: &Addr,
    log: &ManifestArtifact,
    info: LogInfo,
) -> anyhow::Result<OpenLog> {
    let unavailable =
        |info: LogInfo, reason: String| anyhow::Error::new(LogUnavailable { info, reason });
    let hashin = info.hashin.clone();
    let stored = match cache.seekable_reader(addr, &hashin, &log.name) {
        Ok(Some(r)) => r,
        Ok(None) => {
            // Held in rows: small, read whole.
            let sized = match cache.reader(addr, &hashin, &log.name) {
                Ok(s) => s,
                Err(e) if e.is::<NotFoundError>() => {
                    return Err(unavailable(
                        info,
                        "the cache entry's log was removed".into(),
                    ));
                }
                Err(e) => return Err(e).with_context(|| format!("reading the log of {addr}")),
            };
            if sized.size > IN_MEMORY_MAX {
                return Err(unavailable(
                    info,
                    format!("a {}-byte log blob with no random access", sized.size),
                ));
            }
            let bytes: Arc<[u8]> = match sized.bytes {
                Some(b) => b,
                None => {
                    let mut buf = Vec::with_capacity(usize::try_from(sized.size)?);
                    sized
                        .reader
                        .take(sized.size)
                        .read_to_end(&mut buf)
                        .with_context(|| format!("reading the log of {addr}"))?;
                    buf.into()
                }
            };
            Box::new(io::Cursor::new(bytes))
        }
        Err(e) if e.is::<NotFoundError>() => {
            return Err(unavailable(
                info,
                "the cache entry's log was removed".into(),
            ));
        }
        Err(e) => return Err(e).with_context(|| format!("opening the log of {addr}")),
    };
    let Some(mut decoded) = cache_codec::decode_seekable(stored, &log.encoding)
        .with_context(|| format!("decoding the log of {addr}"))?
    else {
        return Err(unavailable(info, "the log's encoding has no reader".into()));
    };
    let index =
        TarIndex::build(&mut decoded).with_context(|| format!("indexing the log of {addr}"))?;
    let member = index
        .entries
        .values()
        .filter(|e| matches!(e.kind, IndexEntryKind::File))
        .min_by_key(|e| e.data_offset);
    let (base, len) = member.map_or((0, 0), |e| (e.data_offset, e.size));
    Ok(OpenLog {
        info,
        src: decoded,
        base,
        len,
    })
}

/// One step of a follow.
#[derive(Debug, PartialEq, Eq)]
pub enum FollowStep {
    /// The next bytes, in order.
    Bytes(Vec<u8>),
    /// Nothing new yet: poll again later.
    Idle,
    /// The log ended; every byte was returned.
    End,
}

enum Follow {
    /// No run going on when following began, and none since. `baseline` is
    /// the newest finished log then: a newer one means a run came and went
    /// between two polls.
    Waiting {
        baseline: Option<u64>,
    },
    Live(Running),
    Stored(OpenLog),
    Done,
}

/// A log being followed: [`Engine::follow_log`]. Each [`poll`](Self::poll)
/// returns what is new; a run's bytes end where the run ends, and once its
/// live file is gone the follower goes on in what replaced it (the cache
/// entry's `Log`, the failure slot), found by the run's input hash, from the
/// byte it had reached: no gap, no repeated byte.
pub struct LogFollower {
    engine: Arc<Engine>,
    addr: Addr,
    locator: Option<Arc<dyn LiveRunLogLocator>>,
    state: Follow,
    /// Bytes returned so far from the current run's log.
    offset: u64,
    /// When the locator or the stored sources were last looked at.
    checked: Option<Instant>,
}

impl std::fmt::Debug for LogFollower {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogFollower")
            .field("addr", &self.addr)
            .field("offset", &self.offset)
            .finish_non_exhaustive()
    }
}

impl LogFollower {
    /// What is new since the last poll. Blocking (file IO), and never waits:
    /// on [`FollowStep::Idle`] the caller polls again after a while.
    pub fn poll(&mut self) -> anyhow::Result<FollowStep> {
        loop {
            match &mut self.state {
                Follow::Done => return Ok(FollowStep::End),
                Follow::Stored(log) => {
                    let n = log.len().saturating_sub(self.offset).min(CHUNK as u64);
                    if n == 0 {
                        self.state = Follow::Done;
                        continue;
                    }
                    let bytes = log
                        .read_range(self.offset, n)
                        .with_context(|| format!("reading the log of {}", self.addr))?;
                    if bytes.is_empty() {
                        self.state = Follow::Done;
                        continue;
                    }
                    self.offset += bytes.len() as u64;
                    return Ok(FollowStep::Bytes(bytes));
                }
                Follow::Waiting { baseline } => {
                    let baseline = *baseline;
                    if !self.due() {
                        return Ok(FollowStep::Idle);
                    }
                    let latest = self
                        .engine
                        .latest_log(&self.addr, self.locator.as_deref())?;
                    self.state = match latest {
                        Latest::Running(run) => Follow::Live(run),
                        Latest::Stored(s) if baseline.is_none_or(|b| s.at_unix_ms() > b) => {
                            match self.open_local(s) {
                                Some(log) => Follow::Stored(log),
                                None => return Ok(FollowStep::Idle),
                            }
                        }
                        _ => return Ok(FollowStep::Idle),
                    };
                    self.offset = 0;
                }
                Follow::Live(run) => {
                    let mut buf = vec![0; CHUNK];
                    let n = read_some(&mut run.file, &mut buf)
                        .with_context(|| format!("reading the live log of {}", self.addr))?;
                    if n > 0 {
                        buf.truncate(n);
                        self.offset += n as u64;
                        return Ok(FollowStep::Bytes(buf));
                    }
                    if !self.live_ended()? {
                        return Ok(FollowStep::Idle);
                    }
                    // Ended, seen before this final read: what it returns is
                    // the rest of the file.
                    let Follow::Live(run) = &mut self.state else {
                        continue;
                    };
                    let mut rest = Vec::new();
                    run.file
                        .read_to_end(&mut rest)
                        .with_context(|| format!("reading the live log of {}", self.addr))?;
                    let hashin = run.hashin.clone();
                    self.state = self.replacement(&hashin);
                    if !rest.is_empty() {
                        self.offset += rest.len() as u64;
                        return Ok(FollowStep::Bytes(rest));
                    }
                }
            }
        }
    }

    /// What is being read now, `None` while waiting or done.
    pub fn info(&self) -> Option<LogInfo> {
        match &self.state {
            Follow::Live(run) => Some(LogInfo {
                source: LogSource::Live {
                    running: true,
                    session: match &run.end {
                        LiveEnd::InProcess(_) => None,
                        LiveEnd::Located { session, .. } => Some(session.clone()),
                    },
                },
                hashin: run.hashin.clone(),
                at_unix_ms: run.at_unix_ms,
            }),
            Follow::Stored(log) => Some(log.info().clone()),
            Follow::Waiting { .. } | Follow::Done => None,
        }
    }

    /// Whether a rate-limited check is due, marking it done.
    fn due(&mut self) -> bool {
        let now = Instant::now();
        if self
            .checked
            .is_some_and(|t| now.duration_since(t) < RECHECK)
        {
            return false;
        }
        self.checked = Some(now);
        true
    }

    /// Whether the live run being read has ended.
    fn live_ended(&mut self) -> anyhow::Result<bool> {
        let Follow::Live(run) = &self.state else {
            return Ok(true);
        };
        match &run.end {
            LiveEnd::InProcess(log) => Ok(log.upgrade().is_none_or(|log| log.ended())),
            LiveEnd::Located { file, .. } => {
                use std::os::unix::fs::MetadataExt as _;
                // Unlinked once what replaces it is in place.
                if run.file.metadata()?.nlink() == 0 {
                    return Ok(true);
                }
                let file = file.clone();
                if !self.due() {
                    return Ok(false);
                }
                let Some(locator) = &self.locator else {
                    return Ok(false);
                };
                // Its end recorded, a newer run of the addr, or its session
                // gone: this run is over.
                Ok(!matches!(
                    locator.locate(&self.engine.home, &self.addr.format())?,
                    Some(LocatedRunLog::Live { record, .. }) if record.file == file
                ))
            }
        }
    }

    /// Where to go on once the run at `hashin` ended: what replaced its live
    /// file, from the byte reached, when it holds more; else the end.
    fn replacement(&self, hashin: &str) -> Follow {
        let stored = match self.engine.newest_cached_log(&self.addr, Some(hashin)) {
            Ok(Some(s)) => Some(s),
            Ok(None) | Err(_) => self
                .engine
                .failure_slots
                .read(&self.addr)
                .filter(|m| m.hashin == hashin)
                .map(Stored::Slot),
        };
        match stored.and_then(|s| self.open_local(s)) {
            Some(log) if log.len() > self.offset => Follow::Stored(log),
            _ => Follow::Done,
        }
    }

    /// Open a finished log with what is on disk, never fetching.
    fn open_local(&self, stored: Stored) -> Option<OpenLog> {
        let engine = &self.engine;
        let opened = match stored {
            Stored::Retained(run) => open_plain(&run.path).ok().flatten().map(|f| {
                OpenLog::plain(
                    LogInfo {
                        source: LogSource::Live {
                            running: false,
                            session: None,
                        },
                        hashin: run.hashin,
                        at_unix_ms: run.at_unix_ms,
                    },
                    f,
                )
            }),
            Stored::Slot(slot) => {
                let path = engine
                    .failure_slots
                    .slot_dir(&self.addr)
                    .join(failure_slot::LOG);
                open_plain(&path).ok().flatten().map(|f| {
                    OpenLog::plain(
                        LogInfo {
                            source: LogSource::FailureSlot,
                            hashin: slot.hashin,
                            at_unix_ms: slot.failed_at,
                        },
                        f,
                    )
                })
            }
            Stored::Cache { manifest, log } => {
                let info = LogInfo {
                    source: LogSource::Cache { remote: false },
                    at_unix_ms: created_ms(&manifest),
                    hashin: manifest.hashin,
                };
                return open_cached(engine.local_cache.as_ref(), &self.addr, &log, info).ok();
            }
        };
        opened.and_then(Result::ok)
    }
}

/// One read, retrying an interrupt; 0 at the current end.
fn read_some(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match file.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // `O_NONBLOCK` on a regular file never blocks; treat as nothing yet.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(0),
            other => return other,
        }
    }
}
