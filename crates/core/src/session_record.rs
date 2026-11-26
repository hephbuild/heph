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

/// `HEPH_SESSION_EXPORT_DIR=<absolute dir>`: at seal, a session also writes
/// `<dir>/<id>.hephsession`, so a CI job keeps every heph run it contains.
///
/// It is heph's, never a target's input (spec I1, E4). The exec driver strips
/// it from the `pass_env = ["*"]` and `runtime_pass_env = ["*"]` wildcards
/// and refuses it in a named `pass_env` (directly or through a dependency's
/// hashed `transitive` env), where it would reach the def hash; the session
/// runner removes it from its launch and its targets' base environment (all
/// three through [`HEPH_OWNED_ENV`]). Only a name in a `runtime_pass_env`,
/// which is not hashed, passes it to a target: the target's own, a
/// dependency's unhashed `transitive` env, or a wrap runner's.
pub const SESSION_EXPORT_DIR_ENV: &str = "HEPH_SESSION_EXPORT_DIR";

/// `HEPH_TOOLS_DIR`: the directory the exec driver stages a target's tools
/// under. It names the tools of the heph target a process runs inside, never
/// anything of another target's.
pub const TOOLS_DIR_ENV: &str = "HEPH_TOOLS_DIR";

/// The environment variables that are heph's own and never come to a target
/// from an environment it inherits wholesale: the exec driver's `"*"`
/// passthroughs, a session runner's launch (even when its `launch_env` names
/// one), and the base environment a session runner's agent gives every
/// target. A target still sets or passes one by name where the driver allows
/// it; that goes through its own, explicit environment.
pub const HEPH_OWNED_ENV: [&str; 2] = [TOOLS_DIR_ENV, SESSION_EXPORT_DIR_ENV];

/// Whether `name` is one of [`HEPH_OWNED_ENV`]: an exact, case-sensitive
/// match.
pub fn is_heph_owned_env(name: impl AsRef<std::ffi::OsStr>) -> bool {
    let name = name.as_ref();
    HEPH_OWNED_ENV.iter().any(|owned| name == *owned)
}

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
    /// The addr is running in `session`, which records run logs, and has
    /// written nothing yet: no live file, no record. Its first byte makes it
    /// [`Self::Live`].
    Started {
        /// When its `ExecuteStart` was recorded, in milliseconds since the
        /// epoch.
        at_unix_ms: u64,
        session: String,
    },
    /// `session`, a live session of this workspace, is running the addr
    /// (started at `at_unix_ms`) on a driver that returns its log when the
    /// run ends instead of streaming it, and no byte of its log has arrived.
    /// The UI says "this target's driver does not stream its log".
    /// Beats [`Self::Started`]: such a run is started too, but "no output
    /// yet" would promise bytes that come only at its end.
    NotStreamed { session: String, at_unix_ms: u64 },
}

impl LocatedRunLog {
    /// The record found; `None` for [`Self::Started`] and
    /// [`Self::NotStreamed`].
    pub fn record(&self) -> Option<&RunLogRecord> {
        match self {
            Self::Live { record, .. } | Self::Ended { record, .. } => Some(record),
            Self::Started { .. } | Self::NotStreamed { .. } => None,
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

    /// Exactly heph's names: a near miss, another case or a prefix is a
    /// target's own variable.
    #[test]
    fn heph_owned_env_is_an_exact_match() {
        for owned in [SESSION_EXPORT_DIR_ENV, TOOLS_DIR_ENV] {
            assert!(is_heph_owned_env(owned), "{owned}");
            assert!(
                is_heph_owned_env(std::ffi::OsString::from(owned)),
                "{owned}"
            );
        }
        for other in [
            "HEPH_SESSION_EXPORT_DIRS",
            "heph_tools_dir",
            "HEPH_TOOLS",
            "HEPH_TOOLS_DIR ",
            "",
            "PATH",
        ] {
            assert!(!is_heph_owned_env(other), "{other:?}");
        }
    }
}
