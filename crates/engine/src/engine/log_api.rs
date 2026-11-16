//! The log API: a target's log, by address.
//!
//! A log is reached by address, never by a path. The API reads whichever of
//! these is newest (spec "Where a log comes from"):
//!
//! - **a run going on**: its live file, `<home>/runs/<engine-id>/<n>.log`,
//!   found in this engine's own [`LiveRuns`] or, for another heph on the same
//!   workspace, through a [`LiveRunLogLocator`] (the session records, which
//!   the engine cannot read itself); a running run is always the newest.
//!   One that has written nothing has no file, and is [`NoOutputYet`]:
//!   this engine knows its own from the moment its log is made, with or
//!   without a session;
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
    LocalCache, Manifest, ManifestArtifact, ManifestArtifactEncoding, ManifestArtifactType,
    NotFoundError,
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
/// How long a follow waiting on a run that does not stream its log looks
/// for its stored log once the run is over, before ending with none. The
/// session says the run is over once its result is (`ResultEnd`), after the
/// store; the store's sqlite write can still be queued then, unseen by
/// another heph, for a moment.
const NOT_STREAMED_SETTLE: Duration = Duration::from_secs(2);
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
    /// shows a log asks for it; a list's last-line column never does. Only
    /// such a reader waits, on a miss, for queued cache writes to commit.
    pub fetch_remote: bool,
    /// Only the log of the run at this input hash (`heph log --current`):
    /// every source at another hash is passed over, however new.
    pub hashin: Option<String>,
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

/// The address is running and has written nothing yet: there is no log to
/// open, and following it waits for its first byte. The UI says
/// "running · no output yet".
#[derive(Debug, thiserror::Error)]
#[error("no output yet; the run {} has written nothing", running_where(.session.as_deref()))]
pub struct NoOutputYet {
    /// The session it runs in; `None` for a run of this engine, known
    /// in-process whether or not a session records it.
    pub session: Option<String>,
    /// When the run started, in milliseconds since the epoch.
    pub at_unix_ms: u64,
}

fn running_where(session: Option<&str>) -> String {
    match session {
        Some(session) => format!("in session {session}"),
        None => "in this heph".to_owned(),
    }
}

/// The address is running on a driver that returns its log when the run
/// ends instead of streaming it, no byte of it has arrived, and nothing
/// stored is newer. The UI says "this target's driver does not stream its
/// log".
/// Outranks [`NoOutputYet`]: such a run is started too. For another
/// session's run, only when no hashin was asked for, since the run's start
/// does not say which hashin it runs. Only [`Engine::open_log`] returns it: a
/// follow waits for the run instead.
#[derive(Debug, thiserror::Error)]
#[error(
    "no live log; the run {} is on a driver that does not stream its log",
    running_where(.session.as_deref())
)]
pub struct LogNotStreamed {
    /// The session it runs in; `None` for a run of this engine.
    pub session: Option<String>,
}

/// A finished log the API could read, before it is opened.
enum Stored {
    /// A finished live file (a `cache = false` success's), opened when it
    /// was found: the file of a run whose entry is being written is
    /// unlinked a moment later, and the open file still reads.
    Retained(LiveRun, File),
    Slot(failure_slot::SlotManifest),
    Cache {
        manifest: Manifest,
        log: ManifestArtifact,
    },
}

impl Stored {
    fn at_unix_ms(&self) -> u64 {
        match self {
            Self::Retained(run, _) => run.at_unix_ms,
            Self::Slot(m) => m.failed_at,
            Self::Cache { manifest, .. } => created_ms(manifest),
        }
    }
}

/// What a finished live file is, for the status line.
fn retained_info(run: LiveRun) -> LogInfo {
    LogInfo {
        source: LogSource::Live {
            running: false,
            session: None,
        },
        hashin: run.hashin,
        at_unix_ms: run.at_unix_ms,
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
    /// Running, in this engine or another session, nothing written yet.
    Started(NoOutputYet),
    Stored(Stored),
    /// This engine (`session: None`) or the locator's session is running the
    /// addr on a driver that returns its log when the run ends instead of
    /// streaming it, and nothing stored is newer than that run's start.
    NotStreamed {
        session: Option<String>,
        at_unix_ms: u64,
    },
    None,
}

impl Engine {
    /// `addr`'s newest log, opened. `Ok(None)` when heph holds none for it;
    /// [`LogUnavailable`] when one exists but cannot be read.
    ///
    /// Never builds. A remote hit's log is fetched only when `fetch_remote`
    /// is set, and then once: the next open reads the local copy.
    /// Whether `heph log <addr>` would find a log for `addr`: the log API
    /// over local sources, never fetching, with a remote hit's log (which
    /// `heph log` fetches) counted. What a failure's `log:` row is decided on.
    pub async fn log_readable(self: &Arc<Self>, addr: &Addr) -> bool {
        match self.open_log(addr, &LogOptions::default()).await {
            Ok(found) => found.is_some(),
            Err(e) => e
                .downcast_ref::<LogUnavailable>()
                .is_some_and(|u| u.info.source == LogSource::Cache { remote: true }),
        }
    }

    pub async fn open_log(
        self: &Arc<Self>,
        addr: &Addr,
        opts: &LogOptions,
    ) -> anyhow::Result<Option<OpenLog>> {
        let mut latest = self.latest_log_blocking(addr, opts).await?;
        if matches!(latest, Latest::None) && opts.fetch_remote {
            // A remote hit's mirrored manifest is a queued write until the
            // sqlite writer commits it, and the cache listing does not wait
            // on that queue: an open right after the hit found nothing. On
            // the miss path only, let what is queued commit and look again.
            // Only for a reader that shows a log (`fetch_remote`): the
            // last-line column opens every visible row with no log, and
            // waiting there on every queued write in a busy build stalls it;
            // it looks again on its next pass anyway.
            self.sqlite_writes.drain_queued_writes().await;
            latest = self.latest_log_blocking(addr, opts).await?;
        }
        match latest {
            Latest::None => Ok(None),
            Latest::Started(started) => Err(started.into()),
            Latest::NotStreamed { session, .. } => Err(LogNotStreamed { session }.into()),
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
    /// to read and nothing to wait for. A run on a driver that does not
    /// stream its log is waited on, as a live run is followed, until its log
    /// is stored ([`LogFollower::not_streamed`] says so meanwhile), or until
    /// it is over with none stored (cancelled, `cache = false`).
    pub async fn follow_log(
        self: &Arc<Self>,
        addr: &Addr,
        opts: &LogOptions,
        wait_for_start: bool,
    ) -> anyhow::Result<Option<LogFollower>> {
        let latest = self.latest_log_blocking(addr, opts).await?;
        let state = match latest {
            Latest::Running(run) => Follow::Live(run),
            // A run going on that has written nothing: wait for its first
            // byte, and end with it if it ends without one. Anything stored
            // newer than its start is its own.
            Latest::Started(started) => Follow::Waiting {
                baseline: Some(started.at_unix_ms),
                started: Some(started),
                not_streamed: None,
                over_since: None,
            },
            // A run is going on, as for `Running`, so the follow waits for it
            // whatever `wait_for_start` says: its log is the first stored one
            // newer than its start.
            Latest::NotStreamed {
                session,
                at_unix_ms,
            } => Follow::Waiting {
                baseline: Some(at_unix_ms),
                started: None,
                not_streamed: Some(LogNotStreamed { session }),
                over_since: None,
            },
            Latest::Stored(stored) if wait_for_start => Follow::Waiting {
                baseline: Some(stored.at_unix_ms()),
                started: None,
                not_streamed: None,
                over_since: None,
            },
            Latest::None if wait_for_start => Follow::Waiting {
                baseline: None,
                started: None,
                not_streamed: None,
                over_since: None,
            },
            Latest::None => return Ok(None),
            Latest::Stored(stored) => {
                Follow::Stored(self.open_stored(addr, stored, opts.fetch_remote).await?)
            }
        };
        Ok(Some(LogFollower {
            engine: Arc::clone(self),
            addr: addr.clone(),
            locator: opts.locator.clone(),
            hashin: opts.hashin.clone(),
            fetch_remote: opts.fetch_remote,
            state,
            offset: 0,
            checked: None,
            replaced_by: None,
        }))
    }

    /// [`latest_log`](Self::latest_log), off the runtime's workers.
    async fn latest_log_blocking(
        self: &Arc<Self>,
        addr: &Addr,
        opts: &LogOptions,
    ) -> anyhow::Result<Latest> {
        let engine = Arc::clone(self);
        let (a, locator, hashin) = (addr.clone(), opts.locator.clone(), opts.hashin.clone());
        hcore::blocking::run(move || engine.latest_log(&a, locator.as_deref(), hashin.as_deref()))
            .await
    }

    /// The newest of `addr`'s logs, or of its logs at `hashin` when given.
    /// Blocking: reads the slot's manifest and the cache entries' manifests,
    /// and asks the locator.
    fn latest_log(
        &self,
        addr: &Addr,
        locator: Option<&dyn LiveRunLogLocator>,
        hashin: Option<&str>,
    ) -> anyhow::Result<Latest> {
        let wanted = |h: &str| hashin.is_none_or(|w| w == h);
        // This engine's own run going on, before its first byte or after:
        // known in-process, so no session is needed to tell it from a
        // target that has not run.
        let started = self
            .run_logs
            .live()
            .started(addr)
            .filter(|run| wanted(&run.hashin));
        let own = self
            .run_logs
            .live()
            .get(addr)
            .filter(|run| wanted(&run.hashin));
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
            } else if let Some(file) = open_plain(&run.path)? {
                retained = Some((run, file));
            }
        }
        // Going on, its file not created yet (or created a moment ago, and
        // the next look reads it): newer than anything retained or stored.
        // A driver that does not stream its log outranks "no output yet":
        // its bytes come only as the run ends.
        if let Some(run) = started {
            if run.not_streamed {
                return Ok(Latest::NotStreamed {
                    session: None,
                    at_unix_ms: run.at_unix_ms,
                });
            }
            return Ok(Latest::Started(NoOutputYet {
                session: None,
                at_unix_ms: run.at_unix_ms,
            }));
        }
        let mut not_streamed = None;
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
                }) if wanted(&record.hashin)
                    && !retained
                        .as_ref()
                        .is_some_and(|(r, _)| r.path == self.home.join(&record.file)) =>
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
                // Going on, nothing written: newer than anything stored. Its
                // input hash is not known until its first byte, so a wait
                // for one hash does not take it.
                Some(LocatedRunLog::Started {
                    at_unix_ms,
                    session,
                }) if hashin.is_none() => {
                    return Ok(Latest::Started(NoOutputYet {
                        session: Some(session),
                        at_unix_ms,
                    }));
                }
                // The start names no hashin: only an unfiltered ask matches.
                Some(LocatedRunLog::NotStreamed {
                    session,
                    at_unix_ms,
                }) if hashin.is_none() => not_streamed = Some((session, at_unix_ms)),
                // Ended: what replaced it is among the stored sources.
                _ => {}
            }
        }
        let mut newest = retained.map(|(run, file)| Stored::Retained(run, file));
        let mut consider = |s: Stored| {
            if newest
                .as_ref()
                .is_none_or(|n| s.at_unix_ms() > n.at_unix_ms())
            {
                newest = Some(s);
            }
        };
        if let Some(slot) = self.failure_slots.read(addr).filter(|m| wanted(&m.hashin)) {
            consider(Stored::Slot(slot));
        }
        if let Some(entry) = self.newest_cached_log(addr, hashin)? {
            consider(entry);
        }
        if let Some((session, at)) = not_streamed
            && start_outranks(newest.as_ref().map(Stored::at_unix_ms), at)
        {
            return Ok(Latest::NotStreamed {
                session: Some(session),
                at_unix_ms: at,
            });
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
            Stored::Retained(run, file) => {
                return Ok(OpenLog::plain(retained_info(run), file)?);
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
        // One fetch per entry in this engine: a concurrent open waits here,
        // then finds the blob below instead of pulling it again.
        let _fetch = self
            .log_fetches
            .lock((addr.clone(), hashin.to_owned()), &ctoken)
            .await
            .with_context(|| format!("waiting for another fetch of the log of {addr} {hashin}"))?;
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
            return Ok(Err(
                "the log was never fetched for this entry, and no remote cache holds it".into(),
            ));
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
/// Whether a run that started at `start_ms` on a driver that does not stream
/// its log is the answer over the newest stored log, stamped `stored_ms`. A
/// stored log from the same millisecond is an older run's (this run stores its
/// log only when it ends, after its start), so the start wins the tie; only a
/// strictly newer stored log outranks it.
fn start_outranks(stored_ms: Option<u64>, start_ms: u64) -> bool {
    stored_ms.is_none_or(|stored| stored <= start_ms)
}

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
    let hashin = info.hashin.as_str();
    let stored = match cache.seekable_reader(addr, hashin, &log.name) {
        Ok(Some(r)) => r,
        Ok(None) => {
            // Held in rows: small, read whole.
            let sized = match cache.reader(addr, hashin, &log.name) {
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
    open_encoded(stored, &log.encoding, info)
}

/// Open a stored `Log` blob, `encoding`-encoded: decode it with random
/// access and find its one file in the tar, reading only headers. A blob
/// that does not decode is [`LogUnavailable`].
fn open_encoded(
    stored: Box<dyn ReadSeek + Send>,
    encoding: &ManifestArtifactEncoding,
    info: LogInfo,
) -> anyhow::Result<OpenLog> {
    let unavailable =
        |info: LogInfo, reason: String| anyhow::Error::new(LogUnavailable { info, reason });
    let mut decoded = match cache_codec::decode_seekable(stored, encoding) {
        Ok(Some(decoded)) => decoded,
        Ok(None) => return Err(unavailable(info, "the log's encoding has no reader".into())),
        Err(e) => return Err(unavailable(info, format!("decoding the log: {e:#}"))),
    };
    let index = match TarIndex::build(&mut decoded) {
        Ok(index) => index,
        Err(e) => return Err(unavailable(info, format!("reading the log's tar: {e:#}"))),
    };
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
        /// Waiting on a run going on, in this engine or another session,
        /// that has written nothing yet: when it ends without a byte, so
        /// does the follow.
        started: Option<NoOutputYet>,
        /// Waiting on a run, in this engine or another session, on a driver
        /// that does not stream its log (`baseline` is then its start).
        not_streamed: Option<LogNotStreamed>,
        /// When that not-streamed run was first seen over with no newer log
        /// stored.
        over_since: Option<Instant>,
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
    /// [`LogOptions::hashin`]: the only run a wait accepts.
    hashin: Option<String>,
    /// [`LogOptions::fetch_remote`]: a wait that ends in a remote hit's
    /// entry fetches its log, or fails, rather than wait on it forever.
    fetch_remote: bool,
    state: Follow,
    /// Bytes returned so far from the current run's log.
    offset: u64,
    /// When the locator or the stored sources were last looked at.
    checked: Option<Instant>,
    /// What replaced the followed run's live file once it ended (the cache
    /// entry's `Log`, the failure slot), even when it held no byte more.
    replaced_by: Option<LogSource>,
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
                Follow::Waiting {
                    baseline,
                    started,
                    not_streamed,
                    over_since,
                } => {
                    let (baseline, on_a_run, waits_on_run, over_since) = (
                        *baseline,
                        started.is_some(),
                        not_streamed.is_some(),
                        *over_since,
                    );
                    if !self.due() {
                        return Ok(FollowStep::Idle);
                    }
                    let latest = self.engine.latest_log(
                        &self.addr,
                        self.locator.as_deref(),
                        self.hashin.as_deref(),
                    )?;
                    self.state = match latest {
                        Latest::Running(run) => Follow::Live(run),
                        Latest::Stored(s) if baseline.is_none_or(|b| s.at_unix_ms() > b) => {
                            // A cache entry whose log is not on disk is a
                            // remote hit's: it never becomes local by
                            // itself, so it is fetched or refused, never
                            // waited on.
                            let entry = match &s {
                                Stored::Cache { manifest, log } => {
                                    Some((manifest.clone(), log.clone()))
                                }
                                Stored::Retained(..) | Stored::Slot(_) => None,
                            };
                            match (self.open_local(s), entry) {
                                (Some(log), _) => Follow::Stored(log),
                                (None, Some((manifest, log))) => {
                                    Follow::Stored(self.open_entry(manifest, log)?)
                                }
                                (None, None) => return Ok(FollowStep::Idle),
                            }
                        }
                        Latest::Started(_) => return Ok(FollowStep::Idle),
                        Latest::NotStreamed { .. } => {
                            self.set_over_since(None);
                            return Ok(FollowStep::Idle);
                        }
                        // The run waited on is over (its result ended, or its
                        // session did) and no newer log is stored: once the
                        // store's write has had time to land, none will be (a
                        // cancelled run, a `cache = false` success). The
                        // follow ends, as a streamed run's does.
                        _ if waits_on_run => {
                            let since = over_since.unwrap_or_else(Instant::now);
                            if since.elapsed() < NOT_STREAMED_SETTLE {
                                self.set_over_since(Some(since));
                                return Ok(FollowStep::Idle);
                            }
                            Follow::Done
                        }
                        // The run waited on ended without writing a byte.
                        _ if on_a_run => Follow::Done,
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
                    // This engine's run whose returned log was renamed over
                    // the streamed one: the file read is one whole log,
                    // either of them, and continuing in the entry from its
                    // offset would splice the two. A run gone from memory
                    // is done too: past `ended`, the file read here is the
                    // one its entry or slot copied.
                    let whole = matches!(&run.end,
                        LiveEnd::InProcess(log) if log.upgrade().is_none_or(|log| log.replaced()));
                    let hashin = run.hashin.clone();
                    self.state = if whole {
                        Follow::Done
                    } else {
                        self.replacement(&hashin)
                    };
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

    /// The run being waited on, while it is going on and has written
    /// nothing yet; `None` otherwise.
    pub fn started(&self) -> Option<&NoOutputYet> {
        match &self.state {
            Follow::Waiting {
                started: Some(started),
                ..
            } => Some(started),
            _ => None,
        }
    }

    /// The session of the run being waited on, while it is going on in
    /// another session and has written nothing yet; `None` otherwise (and
    /// for a run of this engine: see [`Self::started`]).
    pub fn started_in(&self) -> Option<&str> {
        self.started().and_then(|s| s.session.as_deref())
    }

    /// The run on a driver that does not stream its log, while the follow
    /// waits for that run to end and its log to be stored
    /// ([`LogNotStreamed`], followed); `None` otherwise.
    pub fn not_streamed(&self) -> Option<&LogNotStreamed> {
        match &self.state {
            Follow::Waiting {
                not_streamed: Some(not_streamed),
                ..
            } => Some(not_streamed),
            _ => None,
        }
    }

    /// Record when the not-streamed run waited on was first seen over.
    fn set_over_since(&mut self, at: Option<Instant>) {
        if let Follow::Waiting { over_since, .. } = &mut self.state {
            *over_since = at;
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
                // Unlinked once what replaces it is in place. Across
                // processes, a driver's returned log renamed over the
                // streamed one drops this inode's nlink to 0 as well: the
                // follower then goes on in the cache entry from its offset,
                // which can splice the streamed head onto the imported rest.
                // Accepted: it takes a driver that both streams and returns
                // a log, followed from another heph, and the entry
                // holds the whole imported log.
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
    fn replacement(&mut self, hashin: &str) -> Follow {
        let stored = match self.engine.newest_cached_log(&self.addr, Some(hashin)) {
            Ok(Some(s)) => Some(s),
            Ok(None) | Err(_) => self
                .engine
                .failure_slots
                .read(&self.addr)
                .filter(|m| m.hashin == hashin)
                .map(Stored::Slot),
        };
        let opened = stored.and_then(|s| self.open_local(s));
        self.replaced_by = opened.as_ref().map(|log| log.info().source.clone());
        match opened {
            Some(log) if log.len() > self.offset => Follow::Stored(log),
            _ => Follow::Done,
        }
    }

    /// Once a followed run ended: where its log went, and so how it ended
    /// (`FailureSlot`: it failed; `Cache`: it succeeded and was cached).
    /// `None` while it runs, when the follow never saw a run going on, or
    /// when nothing replaced its live file (an uncached success, a run whose
    /// heph keeps it until it exits).
    pub fn replaced_by(&self) -> Option<&LogSource> {
        self.replaced_by.as_ref()
    }

    /// Open a finished log with what is on disk, never fetching.
    fn open_local(&self, stored: Stored) -> Option<OpenLog> {
        let engine = &self.engine;
        let opened = match stored {
            Stored::Retained(run, file) => Some(OpenLog::plain(retained_info(run), file)),
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

    /// Open a cache entry's log that is not on disk as [`Engine::open_log`]
    /// does: fetched when the follow asked for it, else [`LogUnavailable`].
    /// `poll` is sync and may be called from inside the runtime, so the
    /// fetch is driven by the engine's runtime on a scoped thread.
    fn open_entry(&self, manifest: Manifest, log: ManifestArtifact) -> anyhow::Result<OpenLog> {
        let (engine, addr, fetch) = (&self.engine, &self.addr, self.fetch_remote);
        let stored = Stored::Cache { manifest, log };
        std::thread::scope(|s| {
            s.spawn(move || {
                engine
                    .runtime
                    .block_on(engine.open_stored(addr, stored, fetch))
            })
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::local_cache_fs::LocalCacheFS;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A not-streamed start against the newest stored log: no stored log, an
    /// older one, and one from the same millisecond all read as NotStreamed;
    /// a strictly newer stored log wins.
    #[test]
    fn a_not_streamed_start_wins_a_same_millisecond_tie() {
        assert!(start_outranks(None, 100));
        assert!(start_outranks(Some(99), 100));
        assert!(start_outranks(Some(100), 100));
        assert!(!start_outranks(Some(101), 100));
    }

    fn info(source: LogSource) -> LogInfo {
        LogInfo {
            source,
            hashin: "h".into(),
            at_unix_ms: 1,
        }
    }

    /// A stored blob that counts the bytes read from it.
    struct Counting {
        inner: File,
        read: Arc<AtomicU64>,
    }

    impl Read for Counting {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read.fetch_add(n as u64, Ordering::Relaxed);
            Ok(n)
        }
    }

    impl Seek for Counting {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    /// `n` bytes zstd cannot shrink, ending in a known line.
    fn incompressible(n: usize) -> Vec<u8> {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut out: Vec<u8> = (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x.to_le_bytes()[0]
            })
            .collect();
        out.extend_from_slice(b"\nlast line\n");
        out
    }

    /// spec `log_tail_is_bounded`: whatever is asked, a tail returns at most
    /// [`TAIL_MAX`] bytes, reading only those: a 1 GB (sparse) slot log, and
    /// a cached `Log` blob of many seekable-zstd frames, of which a tail
    /// decodes about the frames that hold it, not the log. A blob that does
    /// not decode is [`LogUnavailable`]. (The second-tail cache is the
    /// list's, in the UI: I4.)
    #[test]
    fn log_tail_is_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");

        // A 1 GB log, as a failure slot's `log.txt` (sparse: no disk).
        let path = dir.path().join("log.txt");
        let f = File::create(&path).expect("create");
        f.set_len(1 << 30).expect("set_len");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|mut f| {
                f.seek(SeekFrom::End(-5))?;
                std::io::Write::write_all(&mut f, b"tail\n")
            })
            .expect("write the end");
        let mut log = OpenLog::plain(
            info(LogSource::FailureSlot),
            open_plain(&path).expect("open").expect("present"),
        )
        .expect("open log");
        assert_eq!(log.len(), 1 << 30);
        let tail = log.tail(usize::MAX).expect("tail");
        assert_eq!(tail.len(), TAIL_MAX);
        assert!(tail.ends_with(b"tail\n"));
        assert_eq!(log.tail(3).expect("tail"), b"il\n");

        // A cached log: one tar member in seekable zstd, 8 frames.
        let cache = LocalCacheFS::new(dir.path().join("cache")).expect("cache");
        let addr = Addr::new(
            hmodel::htpkg::PkgBuf::from("p"),
            "t".into(),
            std::collections::BTreeMap::new(),
        );
        let content = incompressible(8 * cache_codec::SEEKABLE_FRAME_SIZE);
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "log.txt", content.as_slice())
            .expect("tar");
        let tar = tar.into_inner().expect("tar");
        let mut w = cache_codec::open_writer(
            &cache,
            &addr,
            "h",
            "log.zst",
            &ManifestArtifactEncoding::Zstd,
        )
        .expect("writer");
        std::io::Write::write_all(&mut w, &tar).expect("write");
        w.commit().expect("commit");
        let stored_path = dir.path().join("stored");
        {
            let mut r = cache.reader(&addr, "h", "log.zst").expect("reader").reader;
            let mut out = File::create(&stored_path).expect("stored copy");
            io::copy(&mut r, &mut out).expect("copy");
        }
        let stored_len = std::fs::metadata(&stored_path).expect("stat").len();
        let read = Arc::new(AtomicU64::new(0));
        let counting = Counting {
            inner: File::open(&stored_path).expect("open stored"),
            read: Arc::clone(&read),
        };
        let mut log = open_encoded(
            Box::new(counting),
            &ManifestArtifactEncoding::Zstd,
            info(LogSource::Cache { remote: false }),
        )
        .expect("open cached");
        assert_eq!(log.len(), content.len() as u64);
        let tail = log.tail(usize::MAX).expect("tail");
        assert_eq!(tail.len(), TAIL_MAX);
        assert!(tail.ends_with(b"\nlast line\n"));
        let (_, expected) = content.split_at(content.len() - TAIL_MAX);
        assert_eq!(tail, expected);
        // The tar's first header, its end and the tail's frame: three frames
        // at most of the eight, plus the seek table.
        let frame = cache_codec::SEEKABLE_FRAME_SIZE as u64;
        let read = read.load(Ordering::Relaxed);
        assert!(
            read <= 3 * frame + frame / 2,
            "read {read} of {stored_len} stored bytes for a tail"
        );

        // Unreadable: a corrupt blob, and an encoding with no random access.
        for (bytes, encoding) in [
            (
                b"not zstd at all".as_slice(),
                ManifestArtifactEncoding::Zstd,
            ),
            (b"gzip".as_slice(), ManifestArtifactEncoding::Gzip),
        ] {
            let err = open_encoded(
                Box::new(io::Cursor::new(bytes.to_vec())),
                &encoding,
                info(LogSource::Cache { remote: false }),
            )
            .expect_err("unreadable");
            assert!(err.is::<LogUnavailable>(), "{err:#}");
            assert!(format!("{err}").starts_with("log unavailable"), "{err}");
        }
    }
}
