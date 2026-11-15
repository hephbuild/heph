//! The target screen's background reader (I4, I7): one thread per open
//! target screen that opens its log, follows it, and reads its outputs, so
//! no frame does IO. The screen sends it what it wants and picks up what it
//! found on a later frame. Closing the screen drops the [`TargetFeed`],
//! which ends the thread and closes the log it was following.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::time::Duration;

/// How long the thread waits before polling a followed log again when it
/// had nothing new.
const IDLE: Duration = Duration::from_millis(100);

/// Messages in flight to the screen at most. A follower reading a log
/// faster than frames take it blocks here rather than queueing the log.
const IN_FLIGHT: usize = 16;

/// Where a target screen reads a target's log and outputs. Every call
/// blocks (file IO, maybe a download) and runs on [`TargetFeed`]'s thread,
/// never on the render task. Texts it returns never name a path into
/// heph's home (I9).
pub trait TargetReader: Send + Sync {
    /// `addr`'s log for the Log tab: the run going on, followed from its
    /// first byte, else the newest log held, a remote hit's fetched on
    /// demand.
    fn open_log(&self, addr: &str) -> LogOpened;

    /// What `addr` produced: the cache entry's listing, or with `failed`,
    /// what its failure slot kept.
    fn outputs(&self, addr: &str, failed: bool) -> OutputsFound;
}

/// A log the Log tab reads.
pub enum LogOpened {
    /// A finished log: its last bytes (at most the pane's 256 KB) and its
    /// whole length. `about` says where it came from (`from the cache`).
    Tail {
        bytes: Vec<u8>,
        len: u64,
        about: String,
    },
    /// A run going on: its bytes from the first, as they are written.
    Follow {
        feed: Box<dyn LogFeed>,
        about: String,
    },
    /// No log: `why` says so in words (`no log yet: the target has not run`).
    Nothing(String),
    /// A log that exists and cannot be read: "log unavailable: …".
    Unavailable(String),
}

impl std::fmt::Debug for LogOpened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tail { len, about, .. } => write!(f, "Tail({len}, {about})"),
            Self::Follow { about, .. } => write!(f, "Follow({about})"),
            Self::Nothing(w) => write!(f, "Nothing({w})"),
            Self::Unavailable(w) => write!(f, "Unavailable({w})"),
        }
    }
}

/// A followed log: [`LogFeed::poll`] blocks on IO and never waits.
pub trait LogFeed: Send {
    fn poll(&mut self) -> FeedStep;
}

/// One step of a followed log.
#[derive(Debug, PartialEq, Eq)]
pub enum FeedStep {
    Bytes(Vec<u8>),
    /// Nothing new yet.
    Idle,
    /// The run ended and every byte was returned.
    End,
    /// Following failed: the words to show.
    Failed(String),
}

/// What a target produced, as the Outputs tab lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputsFound {
    /// The cache entry's output groups, by name.
    Listing(Vec<OutputGroup>),
    /// What a failed run's slot kept (`failure_out`) and the fixes an
    /// `in_place` run wrote back.
    Failure {
        kept: Vec<OutputFile>,
        fixes: Vec<String>,
    },
    /// Nothing held.
    Nothing,
    /// Reading failed: the words to show.
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputGroup {
    /// `""` is the unnamed group.
    pub name: String,
    /// Stored bytes.
    pub size: u64,
    pub hash: Option<String>,
    pub files: Vec<OutputFile>,
    /// Why the files are not listed (`not in the local cache`).
    pub unavailable: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFile {
    /// Relative to the workspace.
    pub path: String,
    pub kind: FileKind,
    pub size: u64,
    pub hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileKind {
    File,
    Symlink(String),
    Dir,
}

/// What the thread hands the screen.
#[derive(Debug)]
pub enum FeedMsg {
    /// The log was opened and is read through: a tail, nothing, or
    /// unavailable (never [`LogOpened::Follow`]).
    Log(LogOpened),
    /// A run's log is followed: its bytes come next, from the first.
    Following(String),
    Bytes(Vec<u8>),
    End,
    Failed(String),
    Outputs(OutputsFound),
}

enum Cmd {
    Log,
    Outputs { failed: bool },
}

/// The screen's end of the reader thread.
#[derive(Debug)]
pub struct TargetFeed {
    cmds: Sender<Cmd>,
    msgs: Receiver<FeedMsg>,
}

impl TargetFeed {
    /// Start the thread for `addr`. `None` when it cannot be spawned: the
    /// screen then shows only what the events said.
    pub fn spawn(reader: Arc<dyn TargetReader>, addr: String) -> Option<Self> {
        let (cmds, todo) = mpsc::channel::<Cmd>();
        let (found, msgs) = mpsc::sync_channel(IN_FLIGHT);
        std::thread::Builder::new()
            .name("heph-target-feed".into())
            .spawn(move || run(&*reader, &addr, &todo, &found))
            .map_err(|e| tracing::debug!(error = %e, "target reader unavailable"))
            .ok()?;
        Some(Self { cmds, msgs })
    }

    /// Open (or reopen) the log.
    pub fn want_log(&self) {
        drop(self.cmds.send(Cmd::Log));
    }

    /// Read the outputs, or what a failed run kept.
    pub fn want_outputs(&self, failed: bool) {
        drop(self.cmds.send(Cmd::Outputs { failed }));
    }

    /// What the thread found since the last call.
    pub fn drain(&self) -> Vec<FeedMsg> {
        let mut out = Vec::new();
        loop {
            match self.msgs.try_recv() {
                Ok(m) => out.push(m),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return out,
            }
        }
    }
}

/// The thread: serve commands, and poll the followed log between them.
fn run(reader: &dyn TargetReader, addr: &str, todo: &Receiver<Cmd>, found: &SyncSender<FeedMsg>) {
    let mut follow: Option<Box<dyn LogFeed>> = None;
    loop {
        let cmd = if follow.is_some() {
            match todo.recv_timeout(IDLE) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match todo.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            }
        };
        let sent = match cmd {
            Some(Cmd::Log) => match reader.open_log(addr) {
                LogOpened::Follow { feed, about } => {
                    follow = Some(feed);
                    found.send(FeedMsg::Following(about))
                }
                other => {
                    follow = None;
                    found.send(FeedMsg::Log(other))
                }
            },
            Some(Cmd::Outputs { failed }) => {
                found.send(FeedMsg::Outputs(reader.outputs(addr, failed)))
            }
            None => Ok(()),
        };
        if sent.is_err() {
            return;
        }
        // Read what is there, a bounded number of chunks a round so a new
        // command is not kept waiting behind a long log.
        if let Some(f) = follow.as_mut() {
            for _ in 0..IN_FLIGHT {
                let msg = match f.poll() {
                    FeedStep::Bytes(b) => FeedMsg::Bytes(b),
                    FeedStep::Idle => break,
                    FeedStep::End => FeedMsg::End,
                    FeedStep::Failed(why) => FeedMsg::Failed(why),
                };
                let last = !matches!(msg, FeedMsg::Bytes(_));
                if found.send(msg).is_err() {
                    return;
                }
                if last {
                    follow = None;
                    break;
                }
            }
        }
    }
}
