//! Session records the engine produces, beside the [`events`](crate::events)
//! types so the engine names them without depending on the TUI crate that
//! records sessions.
//!
//! [`RunLogRecord`] says where a running target's live log is: the engine
//! announces it through a [`RunLogAnnouncer`] when the run's first byte
//! creates the file, and the session recorder writes it to `events.ndjson`
//! as a session record (never a build event, so no plugin hook sees it).
//!
//! **Diagnostics only.** A record names a file under the workspace's home;
//! nothing about it reaches a cache key, a def hash, a sandbox, or a target's
//! env, argv or files. It carries no session id.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// The directory under the heph home that holds every engine's live run logs
/// (`<home>/runs/<engine-id>/<n>.log`).
pub const RUNS_DIR: &str = "runs";

/// The capability a session's `meta.json` lists when its heph records
/// [`RunLogRecord`]s, so a reader tells "that heph records no logs" from
/// "this target has no log".
pub const CAPABILITY_RUN_LOGS: &str = "run_logs";

/// A run's live log came into being: `addr` is running at `hashin`, and its
/// output is being appended to `file`.
///
/// `file` is relative to the heph home that the session's `meta.json` names
/// (`runs/<engine-id>/<n>.log`), never absolute. A reader trusts it only
/// through [`relative_file`](Self::relative_file).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLogRecord {
    #[serde(default)]
    pub addr: String,
    /// The run's input hash: what replaces the live file once the run ends
    /// (the cache entry's `Log`, the failure slot) is found by it.
    #[serde(default)]
    pub hashin: String,
    #[serde(default)]
    pub file: String,
}

impl RunLogRecord {
    /// `file` as a path relative to the home, when it has the one shape the
    /// engine writes: `runs/<dir>/<name>`, three plain components. Anything
    /// else (absolute, `..`, `.`, an empty or extra component, a NUL) is
    /// `None`, and the record reads as absent.
    ///
    /// Lexical only: a reader still refuses a symlink on the way
    /// (`runs/`, `runs/<dir>/`, the file itself).
    pub fn relative_file(&self) -> Option<&Path> {
        let mut parts = self.file.split('/');
        let plain = |p: &str| !p.is_empty() && p != "." && p != ".." && !p.contains('\0');
        let ok = parts.next() == Some(RUNS_DIR)
            && parts.next().is_some_and(plain)
            && parts.next().is_some_and(plain)
            && parts.next().is_none();
        ok.then(|| Path::new(&self.file))
    }
}

/// Where the engine announces each run log it creates. The session slot
/// implements it; an engine without one announces nothing.
///
/// Called on the writer's path, once per run that writes anything, with the
/// live log's own lock held: an implementation must not block, must not fail
/// the run, and must not call back into the engine. One channel send is the
/// budget.
pub trait RunLogAnnouncer: Send + Sync + std::fmt::Debug {
    fn run_log_created(&self, record: RunLogRecord);
}

/// What the live sessions of this workspace say about an address's newest
/// run log: what a [`LiveRunLogLocator`] answers.
#[derive(Debug)]
pub enum LocatedRunLog {
    /// The run is going on: its live file, opened (regular, not followed
    /// through a symlink). Read it to EOF and keep following until the run
    /// ends.
    Live {
        record: RunLogRecord,
        /// When the record was written, in milliseconds since the epoch.
        at_unix_ms: u64,
        /// The session that recorded it, for a human to read.
        session: String,
        file: std::fs::File,
    },
    /// The run ended (its end was recorded, or its live file is gone): what
    /// replaced the file is found by `record.hashin`.
    Ended {
        record: RunLogRecord,
        at_unix_ms: u64,
        session: String,
    },
    /// No live session recorded a log of the addr, but `session`, a live
    /// session of this workspace, runs a heph that records no live logs: it
    /// may be running the addr unseen. Answered only when nothing better was
    /// found; the UI says "records no live logs".
    Unsupported { session: String },
}

impl LocatedRunLog {
    /// The record found; `None` for [`Self::Unsupported`].
    pub fn record(&self) -> Option<&RunLogRecord> {
        match self {
            Self::Live { record, .. } | Self::Ended { record, .. } => Some(record),
            Self::Unsupported { .. } => None,
        }
    }
}

/// Finds a running target's live log through the sessions that recorded it,
/// in this process or another heph on the same workspace. The session
/// reader implements it; the engine's log API takes one so it never depends
/// on the crate that records sessions.
///
/// Blocking: an implementation reads session files. It applies the trust
/// rules itself (only live sessions of `home`, `file` kept under
/// `<home>/runs/`), so `None` is "no trusted record of `addr`".
pub trait LiveRunLogLocator: Send + Sync + std::fmt::Debug {
    fn locate(&self, home: &Path, addr: &str) -> anyhow::Result<Option<LocatedRunLog>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_file_accepts_only_runs_dir_name() {
        let rec = |file: &str| RunLogRecord {
            file: file.to_owned(),
            ..RunLogRecord::default()
        };
        assert_eq!(
            rec("runs/02kf5/17.log").relative_file(),
            Some(Path::new("runs/02kf5/17.log"))
        );
        for bad in [
            "",
            "runs",
            "runs/x",
            "/runs/x/1.log",
            "/etc/passwd",
            "runs/../1.log",
            "runs/x/..",
            "runs/../../etc/passwd",
            "runs/./1.log",
            "runs/x/./1.log",
            "runs//1.log",
            "runs/x/1.log/",
            "runs/x/y/1.log",
            "cache/x/1.log",
            "runs/x/1\0.log",
        ] {
            assert_eq!(rec(bad).relative_file(), None, "{bad:?}");
        }
    }
}
