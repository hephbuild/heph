//! The on-disk shapes of a session directory, shared by the recorder (this
//! crate) and every viewer (`heph session …`, a future daemon's replies).
//!
//! Three independent version axes (design amendment A6), each bumped only on an
//! incompatible change:
//!
//! - [`META_FORMAT`] versions the *directory contract*: the file names, the
//!   `lock` semantics, the id scheme, the cleanup rules. A viewer refuses a
//!   newer one by name ([`MetaError::TooNew`]).
//! - [`SNAPSHOT_VERSION`] versions [`SessionSnapshot`] (`status.json`, a
//!   viewer's `--json`).
//! - `events.ndjson` has no version: it only ever grows additively, under the
//!   same skew rules as the plugin wire (`hcore::events` module docs).
//!
//! Everywhere: a new field is `#[serde(default)]`, an unknown field is ignored,
//! an unknown enum variant decodes to `Unknown`. No untagged enums — their
//! failure mode is a silent misparse rather than an error or an `Unknown`.

use std::fmt;

use hcore::events::{BuildEvent, BuildEventKind};
use serde::{Deserialize, Serialize};

use super::id::SessionId;

/// The directory contract's version. See the module docs.
pub const META_FORMAT: u32 = 1;

/// [`SessionSnapshot`]'s version. See the module docs.
pub const SNAPSHOT_VERSION: u32 = 1;

/// The capability a session advertises when its process answers an in-flight
/// request (`inflight.request` → `inflight.txt`). A viewer refuses to ask a
/// session that does not list it rather than wait out a timeout.
pub const CAPABILITY_INFLIGHT: &str = "inflight";

/// File names inside a session directory. Frozen for every format: cleanup and
/// `list` of a newer format's directory depend on `lock` meaning the same thing.
pub mod files {
    /// Written once at start, atomically.
    pub const META: &str = "meta.json";
    /// `flock(LOCK_EX)` held by the owner for its whole life. Held = alive.
    pub const LOCK: &str = "lock";
    /// The append-only record log, one JSON record per line.
    pub const EVENTS: &str = "events.ndjson";
    /// The latest [`super::SessionSnapshot`], rewritten atomically.
    pub const STATUS: &str = "status.json";
    /// Created by a viewer to ask for an in-flight report; the only file a
    /// viewer ever writes.
    pub const INFLIGHT_REQUEST: &str = "inflight.request";
    /// The in-flight report the owner wrote in answer.
    pub const INFLIGHT_REPORT: &str = "inflight.txt";
}

/// Which header a viewer renders the session with: the build counts, or `gc`'s
/// sweep totals. Comes from the view the command ran with
/// ([`crate::tui::TUIAppView::session_header`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeaderKind {
    #[default]
    Build,
    Gc,
    /// A header a newer heph knows about; a viewer falls back to `Build`.
    #[serde(other)]
    Unknown,
}

/// What a command's view says about how a viewer should title the session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionHeader {
    pub kind: HeaderKind,
    /// The view's label (the TUI's bottom border, the line backend's first line).
    pub label: String,
}

/// `meta.json`: who the session is. Written once, before the lock is taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// [`META_FORMAT`] of the heph that wrote it.
    pub format: u32,
    pub id: SessionId,
    /// The process that owns the session. A detail: a session is named by its
    /// id, and under a daemon one pid holds many sessions.
    pub owner_pid: u32,
    pub started_unix_ms: u64,
    pub heph_version: String,
    /// The command line, lossily decoded. Nothing the terminal would not have
    /// shown (I6).
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub workspace_root: Option<String>,
    /// The workspace's resolved home (`.heph/` by default).
    #[serde(default)]
    pub home_dir: Option<String>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub header: HeaderKind,
    /// Whether the run drew the interactive TUI (else the line backend).
    #[serde(default)]
    pub interactive: bool,
    /// What the owner answers besides writing its records, e.g.
    /// [`CAPABILITY_INFLIGHT`].
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// Why a `meta.json` could not be used.
#[derive(Debug)]
pub enum MetaError {
    /// Written by a heph whose directory contract this one does not know.
    TooNew { found: u32, supported: u32 },
    /// Not a `meta.json` this heph can read.
    Invalid(serde_json::Error),
}

impl fmt::Display for MetaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooNew { found, supported } => write!(
                f,
                "session format {found} needs a newer heph (this heph reads format {supported})"
            ),
            Self::Invalid(e) => write!(f, "unreadable session meta.json: {e}"),
        }
    }
}

impl std::error::Error for MetaError {}

impl SessionMeta {
    /// Decode a `meta.json`, refusing a newer [`META_FORMAT`] by name *before*
    /// decoding the rest — a newer format may have changed any other field.
    pub fn decode(bytes: &[u8]) -> Result<Self, MetaError> {
        #[derive(Deserialize)]
        struct Probe {
            format: u32,
        }
        let probe: Probe = serde_json::from_slice(bytes).map_err(MetaError::Invalid)?;
        if probe.format > META_FORMAT {
            return Err(MetaError::TooNew {
                found: probe.format,
                supported: META_FORMAT,
            });
        }
        serde_json::from_slice(bytes).map_err(MetaError::Invalid)
    }

    /// Whether the owner answers in-flight requests.
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }
}

/// One line of `events.ndjson`.
///
/// Both keys optional: a build event has `kind` (a [`BuildEventKind`] encoded
/// exactly as on the plugin wire, so a `BuildEvent`'s own JSON *is* a valid
/// record), a session record has `session`. A line with neither is skipped by
/// a reader; one that does not decode is skipped and counted
/// ([`super::reader::RecordReader`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<BuildEventKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionRecordKind>,
}

impl SessionRecord {
    /// The record's build event, if it carries one.
    pub fn build_event(&self) -> Option<BuildEvent> {
        self.kind.as_ref().map(|kind| BuildEvent {
            at_unix_ms: self.at_unix_ms,
            kind: kind.clone(),
        })
    }

    /// A session-only record stamped `at_unix_ms`.
    pub fn session(at_unix_ms: u64, kind: SessionRecordKind) -> Self {
        Self {
            at_unix_ms,
            kind: None,
            session: Some(kind),
        }
    }
}

/// The records only a session has — never [`BuildEventKind`] variants, because a
/// plugin hook must not receive them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionRecordKind {
    /// A log line the terminal showed (`info!` and above), lossy UTF-8 and
    /// stored verbatim: a viewer escapes control bytes on output.
    Log {
        #[serde(default)]
        level: String,
        #[serde(default)]
        msg: String,
    },
    /// The process is exiting. Written before the lock is released, so "lock
    /// free and no `End`" means exactly "crashed".
    End {
        /// `None`: killed by a signal, or a panic before an exit code existed.
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        panicked: bool,
    },
    /// `events.ndjson` reached its size cap; no build event or log line follows
    /// (`End` and `InflightWritten` still do). `status.json` keeps updating.
    Truncated,
    /// The owner answered an in-flight request: `inflight.txt` is fresh.
    InflightWritten,
    /// A record a newer heph writes.
    #[serde(other)]
    Unknown,
}

/// `status.json`, a viewer's `--json`, and later a daemon's reply: what the
/// session is doing now. Full lists — a human renderer caps them, `--json` does
/// not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// [`SNAPSHOT_VERSION`] of the heph that wrote it.
    pub version: u32,
    /// The first and the latest build event's server timestamp.
    #[serde(default)]
    pub first_event_unix_ms: Option<u64>,
    #[serde(default)]
    pub last_event_unix_ms: Option<u64>,
    #[serde(default)]
    pub counts: SnapshotCounts,
    /// Targets with an operation open (executing, reading or writing a cache),
    /// oldest first.
    #[serde(default)]
    pub running: Vec<RunningTarget>,
    /// Targets blocked on a result lock or on admission, sorted.
    #[serde(default)]
    pub waiting: Vec<String>,
    /// Failed targets, in failure order.
    #[serde(default)]
    pub failed: Vec<String>,
    #[serde(default)]
    pub lock_waits: Vec<LockWaitEntry>,
    #[serde(default)]
    pub admission_waits: Vec<AdmissionWaitEntry>,
    /// `events.ndjson` hit its cap; a fold of it is short of this snapshot.
    #[serde(default)]
    pub truncated: bool,
    /// Set once the process wrote its `End`.
    #[serde(default)]
    pub end: Option<SessionEnd>,
}

impl Default for SessionSnapshot {
    fn default() -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            first_event_unix_ms: None,
            last_event_unix_ms: None,
            counts: SnapshotCounts::default(),
            running: Vec::new(),
            waiting: Vec::new(),
            failed: Vec::new(),
            lock_waits: Vec::new(),
            admission_waits: Vec::new(),
            truncated: false,
            end: None,
        }
    }
}

/// A [`SessionSnapshot`] written by a heph this one cannot read.
#[derive(Debug)]
pub enum SnapshotError {
    TooNew { found: u32, supported: u32 },
    Invalid(serde_json::Error),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooNew { found, supported } => write!(
                f,
                "session snapshot version {found} needs a newer heph (this heph reads version {supported})"
            ),
            Self::Invalid(e) => write!(f, "unreadable session snapshot: {e}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl SessionSnapshot {
    /// Decode a snapshot, refusing a newer [`SNAPSHOT_VERSION`] by name.
    pub fn decode(bytes: &[u8]) -> Result<Self, SnapshotError> {
        #[derive(Deserialize)]
        struct Probe {
            version: u32,
        }
        let probe: Probe = serde_json::from_slice(bytes).map_err(SnapshotError::Invalid)?;
        if probe.version > SNAPSHOT_VERSION {
            return Err(SnapshotError::TooNew {
                found: probe.version,
                supported: SNAPSHOT_VERSION,
            });
        }
        serde_json::from_slice(bytes).map_err(SnapshotError::Invalid)
    }
}

/// The header's numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCounts {
    /// Matched top-level targets so far; `None` before the matcher reported.
    #[serde(default)]
    pub matched: Option<u64>,
    /// Whether `matched` is final.
    #[serde(default)]
    pub matched_complete: bool,
    /// Matched targets that finished.
    #[serde(default)]
    pub matched_done: u64,
    /// Matched targets that were cache hits.
    #[serde(default)]
    pub matched_cached: u64,
    /// Every target that finished, matched or not.
    #[serde(default)]
    pub finished: u64,
    /// Targets between their result's start and end.
    #[serde(default)]
    pub in_flight: u64,
    /// Targets whose driver ran to success.
    #[serde(default)]
    pub executed: u64,
    #[serde(default)]
    pub failed: u64,
}

/// A target with an operation open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningTarget {
    pub addr: String,
    /// `execute`, `local_cache_write`, `remote_cache_read`, …; a viewer shows an
    /// unknown name as is.
    pub op: String,
    /// When the open operation started.
    pub since_unix_ms: u64,
}

/// A target blocked on its result lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockWaitEntry {
    pub addr: String,
    /// The process holding the lock, when it could be named. A viewer maps it to
    /// that process's session.
    #[serde(default)]
    pub holder_pid: Option<u32>,
    /// With no pid: the wait is on readers of the cached revision.
    #[serde(default)]
    pub in_use_by_readers: bool,
    pub since_unix_ms: u64,
}

/// A target held at admission past the notice threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionWaitEntry {
    pub addr: String,
    pub pool: String,
    pub need: u64,
    pub free: u64,
    pub ahead: u64,
    pub since_unix_ms: u64,
}

/// How the session ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEnd {
    pub at_unix_ms: u64,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub panicked: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> SessionMeta {
        SessionMeta {
            format: META_FORMAT,
            id: SessionId::new(1_763_000_000_000, 3),
            owner_pid: 42,
            started_unix_ms: 1_763_000_000_000,
            heph_version: "v0.0.0-dev".into(),
            argv: vec!["heph".into(), "run".into()],
            cwd: Some("/w".into()),
            workspace_root: Some("/w".into()),
            home_dir: Some("/w/.heph".into()),
            label: "run".into(),
            header: HeaderKind::Build,
            interactive: false,
            capabilities: vec![CAPABILITY_INFLIGHT.into()],
        }
    }

    /// I5/A6: a viewer refuses a newer directory format and says which two
    /// versions disagree — before it tries to read fields the newer format may
    /// have changed (here, an id it cannot parse).
    #[test]
    fn newer_format_is_refused_by_name() {
        let json = br#"{"format": 9, "id": "not-an-id-in-format-9"}"#;
        let err = SessionMeta::decode(json).expect_err("format 9 is refused");
        assert!(
            matches!(
                err,
                MetaError::TooNew {
                    found: 9,
                    supported: META_FORMAT
                }
            ),
            "{err:?}"
        );
        let text = err.to_string();
        assert!(
            text.contains("format 9") && text.contains(&format!("format {META_FORMAT}")),
            "{text}"
        );

        let m = meta();
        let bytes = serde_json::to_vec(&m).expect("encode");
        assert_eq!(SessionMeta::decode(&bytes).expect("own format decodes"), m);
    }

    /// A6: new fields default, unknown fields are ignored, an unknown header
    /// kind degrades to `Unknown`.
    #[test]
    fn meta_tolerates_missing_and_unknown_fields() {
        let json = br#"{"format":1,"id":"0000000001","owner_pid":7,"started_unix_ms":0,
            "heph_version":"v1","header":"cache_report","from_the_future":true}"#;
        let m = SessionMeta::decode(json).expect("decodes");
        assert_eq!(m.header, HeaderKind::Unknown);
        assert!(m.argv.is_empty() && m.capabilities.is_empty());
        assert!(!m.has_capability(CAPABILITY_INFLIGHT));
    }

    /// I5: an event record is the plugin wire's `BuildEvent` byte for byte, and
    /// an unknown kind of either family decodes as `Unknown`.
    #[test]
    fn unknown_record_decodes_as_unknown() {
        let ev = BuildEvent {
            at_unix_ms: 5,
            kind: BuildEventKind::ResultStart {
                addr: "//a:b".into(),
            },
        };
        let wire = serde_json::to_string(&ev).expect("encode event");
        let rec = SessionRecord {
            at_unix_ms: 5,
            kind: Some(ev.kind.clone()),
            session: None,
        };
        assert_eq!(serde_json::to_string(&rec).expect("encode record"), wire);

        let future_event: SessionRecord =
            serde_json::from_str(r#"{"at_unix_ms":1,"kind":{"type":"FromTheFuture","x":1}}"#)
                .expect("an unknown event kind decodes");
        assert!(matches!(future_event.kind, Some(BuildEventKind::Unknown)));

        let future_session: SessionRecord =
            serde_json::from_str(r#"{"at_unix_ms":1,"session":{"type":"Paused","why":"x"}}"#)
                .expect("an unknown session kind decodes");
        assert_eq!(future_session.session, Some(SessionRecordKind::Unknown));

        let end: SessionRecord =
            serde_json::from_str(r#"{"at_unix_ms":1,"session":{"type":"End"}}"#)
                .expect("an End with no fields decodes");
        assert_eq!(
            end.session,
            Some(SessionRecordKind::End {
                exit_code: None,
                panicked: false
            })
        );
    }

    /// A6/A17: the snapshot round-trips, and a newer version is refused naming
    /// both versions.
    #[test]
    fn snapshot_json_is_versioned() {
        let snap = SessionSnapshot {
            last_event_unix_ms: Some(9),
            failed: vec!["//a:b".into()],
            lock_waits: vec![LockWaitEntry {
                addr: "//a:c".into(),
                holder_pid: Some(77),
                in_use_by_readers: false,
                since_unix_ms: 3,
            }],
            end: Some(SessionEnd {
                at_unix_ms: 10,
                exit_code: Some(1),
                panicked: false,
            }),
            ..SessionSnapshot::default()
        };
        let bytes = serde_json::to_vec(&snap).expect("encode");
        assert_eq!(SessionSnapshot::decode(&bytes).expect("decode"), snap);

        let newer = format!(r#"{{"version":{}}}"#, SNAPSHOT_VERSION + 1);
        let err = SessionSnapshot::decode(newer.as_bytes()).expect_err("newer refused");
        let text = err.to_string();
        assert!(
            text.contains(&format!("version {}", SNAPSHOT_VERSION + 1))
                && text.contains(&format!("version {SNAPSHOT_VERSION}")),
            "{text}"
        );
        let minimal: SessionSnapshot =
            SessionSnapshot::decode(br#"{"version":1}"#).expect("every field defaults");
        assert_eq!(minimal, SessionSnapshot::default());
    }
}
