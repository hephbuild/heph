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
pub use hcore::session_record::RunLogRecord;
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
/// The recording itself is [`super::store::FILE`].
pub mod files {
    /// `flock(LOCK_EX)` held by the owner for its whole life. Held = alive.
    pub const LOCK: &str = "lock";
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

/// One record: the `body` of a row of `session.db`'s `records` table.
///
/// Both keys optional: a build event has `kind` (a [`BuildEventKind`] encoded
/// exactly as on the plugin wire, so a `BuildEvent`'s own JSON *is* a valid
/// record), a session record has `session`. A body with neither is skipped by
/// a reader; one that does not decode is skipped and counted
/// ([`super::store::RecordCursor`]).
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
    /// A line the terminal showed, at the terminal's filter (so `debug!` too
    /// under `-v`), lossy UTF-8 and stored verbatim: a viewer escapes control
    /// bytes on output. `level` is empty for a diagnostic the process wrote
    /// to the sink itself.
    Log {
        #[serde(default)]
        level: String,
        #[serde(default)]
        msg: String,
        /// The line's number in this process's log sink, from 1: what a
        /// replay after a full screen selects by.
        #[serde(default)]
        seq: u64,
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
    /// The recording reached a size cap (D15). The first marks
    /// [`FULL_CAP`](super::recorder::FULL_CAP): only the
    /// [`is_critical`](super::recorder::is_critical) kinds follow it (no
    /// `Log`, `Output` or progress-only event). A second marks the
    /// [`CEILING`](super::recorder::CEILING): derivation stops at it, and only
    /// `End` follows. The `status` row keeps updating.
    Truncated,
    /// The owner answered an in-flight request: `inflight.txt` is fresh.
    InflightWritten,
    /// The TUI's label changed mid-run (`heph run` learning it invokes an
    /// entrypoint): `meta.json`'s `label` is only the first one.
    Label {
        #[serde(default)]
        label: String,
    },
    /// The approval prompt the TUI shows, or `None` once none is pending, and
    /// the standing Y/N decision in force. The prompt's keys act on the run,
    /// so a viewer shows it without them; a notice's content stays out (a
    /// viewer reads it from the store by the sources recorded instead).
    /// Written past the cap, so the notice sources and the standing decision
    /// are there to read however long the build ran.
    Approval {
        #[serde(default)]
        prompt: Option<Box<ApprovalPrompt>>,
        /// Absent while none stands. A value this heph does not know reads as `None`
        /// rather than dropping the whole record (and with it the prompt).
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient_standing"
        )]
        standing: Option<ApprovalStanding>,
    },
    /// The TUI was torn down at `at_unix_ms`, the instant its final summary
    /// measured the elapsed time against. What follows (`Log`, `Output`) the
    /// terminal printed below that summary.
    Finished {
        /// The user stopped the run, so its summary said `stopped`. Absent
        /// (and so `false`) when not stopped.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stopped: bool,
    },
    /// Text the process wrote straight to its terminal rather than through
    /// the log: the rendered failure report. Stored verbatim, like `Log`, up
    /// to its first 1 MiB; past that, cut with a marker line.
    Output {
        #[serde(default)]
        text: String,
    },
    /// A run's live log was created: where its output is going while it
    /// runs (`file`, relative to `meta.json`'s home). Written past the cap,
    /// so a follower finds a run that starts after it. Trusted only while
    /// the session's lock is held and no `ExecuteEnd` for the addr follows
    /// it; read through [`super::run_logs`], never by joining `file` itself.
    RunLog(Box<RunLogRecord>),
    /// The run named the session behind a lock holder's pid when it first
    /// waited on it (D6): what a viewer names the holder by, so no viewer
    /// scans for pids, and a recording read elsewhere still names it.
    LockHolder {
        pid: u32,
        /// `None`: no live session on this machine had that pid.
        #[serde(default)]
        session: Option<String>,
    },
    /// A record a newer heph writes.
    #[serde(other)]
    Unknown,
}

/// A pending approval prompt as [`SessionRecordKind::Approval`] records it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPrompt {
    /// Every pending request, oldest first: the front one is what the prompt
    /// row names, and the whole queue is what an attached viewer's Approvals
    /// tab and approval view show. Never empty in a written record: no
    /// request pending is an absent prompt.
    #[serde(default)]
    pub queue: Vec<ApprovalQueued>,
}

/// One pending request of [`ApprovalPrompt::queue`]: its number, target and
/// notices by where their text is kept, never the text itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalQueued {
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub addr: String,
    /// When the request was queued: the viewer measures its wait from it.
    #[serde(default)]
    pub queued_at_unix_ms: u64,
    #[serde(default)]
    pub notices: Vec<ApprovalPromptNotice>,
}

/// One notice of an [`ApprovalPrompt`]: where its text is kept, not what it
/// says. No notice is written anywhere; a reader reads it from the store
/// (heph's local cache, or a source file in the workspace) by these sources.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPromptNotice {
    #[serde(default)]
    pub name: String,
    /// The artifacts the notice's text is kept in, in order.
    #[serde(default)]
    pub sources: Vec<hcore::approval_notice::NoticeSource>,
}

impl ApprovalPromptNotice {
    /// The notice's text, read by `read` (the engine's
    /// `read_notice_from_store`, capped) from [`Self::sources`] under
    /// `roots` — the session's home and its `meta.workspace_root` — or
    /// [`hcore::approval_notice::NOTICE_UNAVAILABLE`] when a source is not
    /// where it was recorded or the record names none. Blocking: call it off
    /// the render task.
    pub fn read(
        &self,
        roots: &hcore::approval_notice::NoticeRoots,
        read: hcore::approval_notice::ReadNotice,
    ) -> Result<String, NoticeUnavailable> {
        if self.sources.is_empty() {
            return Err(NoticeUnavailable);
        }
        read(roots, &self.sources).ok_or(NoticeUnavailable)
    }
}

/// A recorded notice could not be read back: show
/// [`hcore::approval_notice::NOTICE_UNAVAILABLE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoticeUnavailable;

impl std::fmt::Display for NoticeUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(hcore::approval_notice::NOTICE_UNAVAILABLE)
    }
}

/// A standing approval decision: after Y (or N), every request of the build
/// is approved (or rejected) without a prompt, until the build ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, output_derive::Shape)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStanding {
    /// Y: shown as `auto-approving (Y)`.
    Approve,
    /// N: shown as `auto-rejecting (N)`.
    Reject,
}

/// `Approval.standing` as this heph knows it: a value a newer writer added
/// decodes as `None`.
fn lenient_standing<'de, D>(d: D) -> Result<Option<ApprovalStanding>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Lenient {
        Known(ApprovalStanding),
        Unknown(serde::de::IgnoredAny),
    }
    Ok(match Option::<Lenient>::deserialize(d)? {
        Some(Lenient::Known(s)) => Some(s),
        Some(Lenient::Unknown(_)) | None => None,
    })
}

impl ApprovalStanding {
    /// The indicator the line, the title row and `heph session show` print.
    pub fn indicator(self) -> &'static str {
        match self {
            Self::Approve => "auto-approving (Y)",
            Self::Reject => "auto-rejecting (N)",
        }
    }
}

/// The most entries each list of a `status.json` snapshot keeps (U3, which
/// reverses design amendment A17). `totals` carries each list's full length;
/// a viewer that needs every entry folds `events.ndjson` instead.
pub const STATUS_LIST_CAP: usize = 100;

/// `status.json`, a viewer's `--json`, and later a daemon's reply: what the
/// session is doing now.
///
/// In `status.json` each list holds at most its first [`STATUS_LIST_CAP`]
/// entries, so the file stays small whatever the build's size; `totals` says
/// how long each list really is and whether it was cut. A fold of the event
/// log (`show`, `attach`) has every entry, and says `truncated: false`.
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
    /// Each list's full length, and whether the list above was cut to
    /// [`STATUS_LIST_CAP`].
    #[serde(default)]
    pub totals: ListTotals,
    /// `events.ndjson` hit its cap; a fold of it is short of this snapshot.
    #[serde(default)]
    pub truncated: bool,
    /// Set once the process wrote its `End`.
    #[serde(default)]
    pub end: Option<SessionEnd>,
}

/// One list's full length, and whether the snapshot's copy was cut.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, output_derive::Shape,
)]
pub struct ListTotal {
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub truncated: bool,
}

impl ListTotal {
    /// The full length of a list of `len` entries, of which the snapshot keeps
    /// at most `cap`.
    pub fn of(len: usize, cap: usize) -> Self {
        Self {
            total: u64::try_from(len).unwrap_or(u64::MAX),
            truncated: len > cap,
        }
    }
}

/// [`SessionSnapshot::totals`]: one [`ListTotal`] per list.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, output_derive::Shape,
)]
pub struct ListTotals {
    #[serde(default)]
    pub running: ListTotal,
    #[serde(default)]
    pub waiting: ListTotal,
    #[serde(default)]
    pub failed: ListTotal,
    #[serde(default)]
    pub lock_waits: ListTotal,
    #[serde(default)]
    pub admission_waits: ListTotal,
}

impl ListTotals {
    /// Whether any list was cut.
    pub fn any_truncated(&self) -> bool {
        [
            self.running,
            self.waiting,
            self.failed,
            self.lock_waits,
            self.admission_waits,
        ]
        .iter()
        .any(|t| t.truncated)
    }
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
            totals: ListTotals::default(),
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, output_derive::Shape)]
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
    /// Matched targets that finished from the cache (a hit announced on a
    /// target still running counts once it ends).
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
    /// Targets that failed. A target a stop ended (Ctrl-C, or fail-fast
    /// after another target failed) is not one and is not in the snapshot's
    /// `failed` list either; it counts only in `finished`. A writer before
    /// the dashboard counted those here.
    #[serde(default)]
    pub failed: u64,
    /// Running targets that are in no lock or admission wait. Counted over the
    /// full lists, so a viewer can tell "every running target waits" from a
    /// capped `status.json`, whose `running` and `waiting` may each be cut.
    /// `None` when the writer predates the field: a viewer falls back to
    /// comparing the lists.
    #[serde(default)]
    pub running_unblocked: Option<u64>,
}

/// A target with an operation open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, output_derive::Shape)]
pub struct RunningTarget {
    pub addr: String,
    /// `execute`, `local_cache_write`, `remote_cache_read`, …; a viewer shows an
    /// unknown name as is.
    pub op: String,
    /// When the open operation started.
    pub since_unix_ms: u64,
}

/// A target blocked on its result lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, output_derive::Shape)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, output_derive::Shape)]
pub struct AdmissionWaitEntry {
    pub addr: String,
    pub pool: String,
    pub need: u64,
    pub free: u64,
    pub ahead: u64,
    pub since_unix_ms: u64,
}

/// How the session ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, output_derive::Shape)]
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

    /// Each view record round-trips, with its wire `type` named as written; a
    /// reader that predates one (simulated by an unknown `type`) skips it as
    /// `Unknown` rather than failing the stream.
    #[test]
    fn view_records_round_trip() {
        let prompt = ApprovalPrompt {
            queue: vec![ApprovalQueued {
                seq: 4,
                addr: "//a:gated".into(),
                queued_at_unix_ms: 1_700_000_000_000,
                notices: vec![ApprovalPromptNotice {
                    name: "terms".into(),
                    sources: vec![
                        hcore::approval_notice::NoticeSource {
                            addr: "//a:terms".into(),
                            origin: hcore::approval_notice::NoticeOrigin::Cache {
                                addr: "//a:terms".into(),
                                revision: "abc123_1700000000000000000".into(),
                                hashout: "def456".into(),
                            },
                        },
                        hcore::approval_notice::NoticeSource {
                            addr: "//@heph/fs:file@f=a/TERMS".into(),
                            origin: hcore::approval_notice::NoticeOrigin::View {
                                source: Box::new(hcore::approval_notice::NoticeOrigin::File {
                                    path: "a/TERMS".into(),
                                    out_path: "a/TERMS".into(),
                                    x: false,
                                    hashout: "0f0f".into(),
                                }),
                                paths: vec![("a/TERMS".into(), "TERMS".into())],
                            },
                        },
                    ],
                }],
            }],
        };
        for (kind, ty) in [
            (
                SessionRecordKind::Label {
                    label: "Invoking //a:b".into(),
                },
                "Label",
            ),
            (
                SessionRecordKind::Approval {
                    prompt: Some(Box::new(prompt)),
                    standing: None,
                },
                "Approval",
            ),
            (
                SessionRecordKind::Approval {
                    prompt: None,
                    standing: Some(ApprovalStanding::Reject),
                },
                "Approval",
            ),
            (SessionRecordKind::Finished { stopped: false }, "Finished"),
            (SessionRecordKind::Finished { stopped: true }, "Finished"),
            (
                SessionRecordKind::Output {
                    text: "× target failed\n\u{1b}[31mred\u{1b}[0m\n".into(),
                },
                "Output",
            ),
            (
                SessionRecordKind::RunLog(Box::new(RunLogRecord {
                    addr: "//a:b".into(),
                    hashin: "abc123".into(),
                    file: "runs/02kf5/17.log".into(),
                })),
                "RunLog",
            ),
        ] {
            let rec = SessionRecord::session(7, kind);
            let json = serde_json::to_string(&rec).expect("encode");
            assert!(json.contains(&format!(r#""type":"{ty}""#)), "{json}");
            let back: SessionRecord = serde_json::from_str(&json).expect("decode");
            assert_eq!(back.session, rec.session, "{json}");
            assert_eq!(back.at_unix_ms, 7);
            // What an older reader sees: the same record under a name it lacks.
            let older = json.replace(&format!(r#""type":"{ty}""#), r#""type":"NotYetKnown""#);
            let old: SessionRecord = serde_json::from_str(&older).expect("decode as unknown");
            assert_eq!(old.session, Some(SessionRecordKind::Unknown), "{older}");
        }
    }

    /// `Finished` writes `stopped` only when set: unstopped it is a bare
    /// `{"type":"Finished"}`, which reads back unstopped, and a stopped one
    /// round-trips.
    #[test]
    fn finished_writes_stopped_only_when_set() {
        let plain = SessionRecord::session(7, SessionRecordKind::Finished { stopped: false });
        let json = serde_json::to_string(&plain).expect("encode");
        assert!(
            json.ends_with(r#""session":{"type":"Finished"}}"#),
            "{json}"
        );
        let back: SessionRecord = serde_json::from_str(&json).expect("decode");
        assert_eq!(
            back.session,
            Some(SessionRecordKind::Finished { stopped: false })
        );

        let stopped = SessionRecord::session(7, SessionRecordKind::Finished { stopped: true });
        let json = serde_json::to_string(&stopped).expect("encode");
        assert!(json.contains(r#""stopped":true"#), "{json}");
        let back: SessionRecord = serde_json::from_str(&json).expect("decode");
        assert_eq!(back.session, stopped.session);
    }

    /// A notice whose record names no input — any record without `sources`
    /// decodes as one — reads as unavailable without asking the store, and
    /// one that names inputs is read from them.
    #[test]
    fn notice_reads_through_the_store_by_its_sources() {
        use hcore::approval_notice::{NoticeOrigin, NoticeRoots, NoticeSource};
        fn store(roots: &NoticeRoots, sources: &[NoticeSource]) -> Option<String> {
            assert!(
                !sources.is_empty(),
                "a notice with no sources reaches the store"
            );
            let NoticeOrigin::Cache { revision, .. } = &sources[0].origin else {
                panic!("not a cache origin: {sources:?}");
            };
            Some(format!(
                "{}@{revision} in {}",
                sources[0].addr,
                roots.home.display()
            ))
        }
        fn never(_: &NoticeRoots, _: &[NoticeSource]) -> Option<String> {
            None
        }
        let json = r#"{"at_unix_ms":3,"session":{"type":"Approval","prompt":{"queue":[{"seq":1,"addr":"//a:b","notices":[{"name":"plan"}]}]}}}"#;
        let rec: SessionRecord = serde_json::from_str(json).expect("decode");
        let Some(SessionRecordKind::Approval { prompt, .. }) = rec.session else {
            panic!("not an Approval: {rec:?}");
        };
        let mut notice = prompt.expect("prompt").queue[0].notices[0].clone();
        let roots = NoticeRoots {
            home: "/nonexistent-heph-home".into(),
            root: "/nonexistent-heph-root".into(),
        };
        assert_eq!(notice.read(&roots, store), Err(NoticeUnavailable));
        assert_eq!(NoticeUnavailable.to_string(), "notice unavailable");

        notice.sources = vec![NoticeSource {
            addr: "//a:plan".into(),
            origin: NoticeOrigin::Cache {
                addr: "//a:plan".into(),
                revision: "h1".into(),
                hashout: "o1".into(),
            },
        }];
        assert_eq!(
            notice.read(&roots, store).as_deref(),
            Ok("//a:plan@h1 in /nonexistent-heph-home")
        );
        assert_eq!(notice.read(&roots, never), Err(NoticeUnavailable));
    }

    /// An `Approval` from a newer writer whose standing decision this heph
    /// does not know keeps its prompt: only the standing reads as `None`.
    #[test]
    fn approval_record_with_an_unknown_standing_keeps_its_prompt() {
        for standing in [r#""approve_once""#, r#"{"later":1}"#, "7"] {
            let json = format!(
                r#"{{"at_unix_ms":3,"session":{{"type":"Approval","prompt":{{"queue":[{{"seq":1,"addr":"//a:b"}}]}},"standing":{standing}}}}}"#
            );
            let rec: SessionRecord = serde_json::from_str(&json).expect("decode");
            let Some(SessionRecordKind::Approval { prompt, standing }) = rec.session else {
                panic!("not an Approval: {rec:?}");
            };
            assert_eq!(standing, None, "{json}");
            assert_eq!(prompt.expect("the prompt survives").queue[0].addr, "//a:b");
        }
        // A known value still decodes.
        let json =
            r#"{"at_unix_ms":3,"session":{"type":"Approval","prompt":null,"standing":"reject"}}"#;
        let rec: SessionRecord = serde_json::from_str(json).expect("decode");
        let Some(SessionRecordKind::Approval { standing, .. }) = rec.session else {
            panic!("not an Approval: {rec:?}");
        };
        assert_eq!(standing, Some(ApprovalStanding::Reject));
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
