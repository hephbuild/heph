//! Build-progress event stream types emitted by the engine core and consumed by
//! the TUI + telemetry. Shared here (the lowest crate) so neither consumer needs
//! to depend on the engine.
//!
//! Events are serde-serializable. They carry the target address as a `String`
//! (`//pkg:name`), never the internal `Arc`-backed `Addr`. The server (engine)
//! stamps every event with a wall-clock timestamp at emit time (`at_unix_ms`).
//! The `emit_scope` helper that pairs Start/End events lives in the engine (it
//! needs the engine's `RequestState`).
//!
//! # This is a live wire format
//!
//! Not a "future" one. `StableRemoteHook` serde-JSON-encodes **every** event
//! across the plugin seam to out-of-process hooks
//! (`crates/plugin-stabby/src/load_stable.rs`), and a plugin cdylib such as
//! `plugin-gha-cdylib` ships as its own release artifact, pinned by manifest URL
//! independently of which `heph` binary the user runs. **Host/plugin version
//! skew is therefore a normal, reachable state**, and a skewed pair still loads
//! successfully at `dlopen` — the ABI check covers the `StreamItem` envelope,
//! not this payload.
//!
//! The decode path has **no slack**: a frame that fails to deserialize is
//! treated as end-of-stream by the plugin SDK's pull loop, which then calls
//! `on_close()`. A single undecodable event silently truncates that hook's whole
//! stream while the hook believes the build finished normally.
//!
//! So, when changing anything here:
//!
//! - **Never change an existing field's type or remove one.** Add new fields
//!   alongside, each `#[serde(default)]` — a bare `Option<T>` is *not*
//!   absent-tolerant under `serde_derive`; a missing key is an error without it.
//!   Unknown fields are already ignored on the way in (no
//!   `deny_unknown_fields`), so new-host → old-plugin is safe.
//! - **New variants are safe** only because of [`BuildEventKind::Unknown`]'s
//!   `#[serde(other)]`. Do not remove it.
//! - **Renaming a variant is a break**, not a refactor: the name *is* the `type`
//!   tag on the wire. An old consumer decodes the renamed variant as `Unknown`
//!   and silently loses that event — it degrades rather than truncating the
//!   stream, which is survivable but is still a loss. `RequestConfig` was renamed
//!   from `MaxWorkers` deliberately, pre-1.0, with that cost accepted.
//!
//! `scripts/abi-check.sh` watches this file and will flag changes for review.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildEvent {
    /// Server-stamped at emit time (`SystemTime::now()` since epoch, milliseconds).
    pub at_unix_ms: u64,
    pub kind: BuildEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BuildEventKind {
    /// Declares how this request is configured. Emitted once at the start of a
    /// `result` batch.
    ///
    /// This is the one-shot "here is how the build is set up" announcement, and
    /// the home for anything a consumer would otherwise have to *discover* about
    /// the run. It was `MaxWorkers { count }` until it grew a second field and
    /// the name started lying; further request-level configuration (which caches
    /// are wired up, whether they are read-only) belongs here rather than in new
    /// variants.
    RequestConfig {
        /// The maximum number of targets that can execute concurrently (the
        /// `result` semaphore size). Lets a client render a fixed worker-slot
        /// indicator.
        max_workers: usize,
        /// Whether this request stops at the first target failure.
        ///
        /// On the stream because a consumer must not have to guess it. The GHA
        /// hook previously scanned `std::env::args()` for `--fail-fast`, which is
        /// wrong twice over: a plugin is handed what it needs rather than reading
        /// the ambient environment — it may not even share the host's process —
        /// and a naive argv scan false-positives on a flag *value* that happens
        /// to look like the flag (`--define X=--ff`).
        ///
        /// It matters to a report because a one-failure summary under fail-fast
        /// reads as "one thing is broken" when the truth is "we stopped looking".
        fail_fast: bool,
        /// Whether this run was asked to ignore carried-over scratch state
        /// (`--no-scratch`). Scratch caches are still resolved, locked, created
        /// and announced through their environment variables — only the stored
        /// contents are withheld, so every target that uses one runs cold.
        ///
        /// On the stream because it explains *two* anomalies a consumer would
        /// otherwise have to guess at: a build that is inexplicably slow, and a
        /// report showing no cache reuse. A summary saying "0 cached" with no
        /// cause reads like a cache outage rather than a deliberate audit.
        ///
        /// Phrased negatively on purpose. `#[serde(default)]` fills a missing
        /// key with `false`, and for a frame from a host older than this field
        /// the true answer *is* "not disabled" — scratch was on. The positive
        /// spelling (`scratch_enabled`) would default those same frames to
        /// "scratch was off", which is both wrong and alarming.
        #[serde(default)]
        scratch_disabled: bool,
    },
    /// Incremental notice of matched top-level targets. `addrs` are newly
    /// matched addresses to add to the set; `complete` is false while the
    /// matcher is still resolving (client renders a provisional `X / ~N`) and
    /// true on the final event once the full set is known (drops the `~`).
    Matched {
        addrs: Vec<String>,
        complete: bool,
    },
    ResultStart {
        addr: String,
    },
    ResultEnd {
        addr: String,
        /// The failure, flattened by `format!("{e:#}")` — the whole cause chain
        /// on one line. Unchanged in type and meaning; the fields below add
        /// structure alongside it rather than replacing it, because retyping
        /// this field would break a version-skewed plugin (see the module docs).
        error: Option<String>,
        /// Set when this target failed *only* because a dependency did, naming
        /// the root. Lets a consumer separate the one thing a human has to fix
        /// from the collateral cone — which at 20k targets can be thousands of
        /// targets reporting the same underlying failure.
        #[serde(default)]
        upstream_of: Option<String>,
        /// The process's exit status as the OS reported it (e.g. `exit status:
        /// 1`), when the failure was a subprocess.
        ///
        /// A string, not an `i32`, because a string is what the engine actually
        /// has: `ProcessFailed::status` is already formatted. Parsing an integer
        /// back out of it in a consumer would be the same guesswork this field
        /// exists to remove.
        #[serde(default)]
        exit_status: Option<String>,
        /// The last lines of the target's process log. Only ever attached to a
        /// failing result, so a green build pays nothing for it.
        #[serde(default)]
        log_tail: Option<LogTailData>,
    },
    ExecuteStart {
        addr: String,
        driver: String,
        cache: bool,
    },
    ExecuteEnd {
        addr: String,
        error: Option<String>,
    },
    /// A credential was minted for a target.
    ///
    /// The audit trail. "This token leaked, what now" needs an answer, and the
    /// event stream is the right place for it: which descriptor, which target,
    /// which route, and how long the value lives.
    ///
    /// **Never the value, and never the subject.** A build log is not a place
    /// to put either, and everything an incident actually needs — what was
    /// minted, for whom, when, and by which route — is here without them.
    SecretGranted {
        /// The target that holds it.
        addr: String,
        /// The descriptor target that declared it.
        secret: String,
        /// The consumer's name for it, as `$SECRET_<NAME>`.
        name: String,
        /// Which provider obtained it: `static_env`, `exec`, `oidc`.
        provider: String,
        /// Which `acquire` entry ran, and what selected it. The route taken is
        /// otherwise invisible in a build's output.
        acquire_index: usize,
        selected_by: Option<String>,
        /// Seconds of usable life at the moment it was granted, and where that
        /// number came from — a declared `ttl` and a protocol-reported expiry
        /// are very different levels of confidence.
        ttl_secs: u64,
        expiry_source: String,
    },
    LocalCacheHit {
        addr: String,
    },
    LocalCacheMiss {
        addr: String,
    },
    /// Start of writing a target's artifacts to the local cache. Paired with
    /// `LocalCacheWriteEnd` via `emit_scope` (fires on completion, `?`, or
    /// cancellation). The client groups this span under the target alongside
    /// `Execute*` as one entry in the per-target operation timeline.
    LocalCacheWriteStart {
        addr: String,
    },
    LocalCacheWriteEnd {
        addr: String,
        error: Option<String>,
    },
    /// Acquiring the per-addr result lock has been blocked past the notice
    /// threshold. `holder_pid` is a process that stamped the lock and still held
    /// it when it was probed — a snapshot, and `None` whenever the holder cannot
    /// be named (including the common case of waiting on readers to drain, which
    /// nothing stamps). Paired one-to-one with
    /// `ResultLockWaitEnd` (which fires on acquire **or** cancellation), so a
    /// consumer can show the notice for exactly the duration of the wait.
    ResultLockWaitStart {
        addr: String,
        holder_pid: Option<u32>,
    },
    /// The execute-lock wait ended (lock acquired or the wait was cancelled).
    ResultLockWaitEnd {
        addr: String,
    },
    /// Acquiring a scratch slot has been blocked past the notice threshold.
    ///
    /// Deliberately *not* folded into `ResultLockWait*`, because the two name
    /// different problems with different fixes. A blocked result lock means
    /// another process is building this exact target — wait for it, or kill it.
    /// A blocked scratch slot means targets are serialized on a shared cache,
    /// which is a declaration-level choice (`access`) the user can change.
    ///
    /// Emitted per *consumer*, so nothing is lost for a machine reader. A
    /// renderer must collapse them by `scratch` before display: one `exclusive`
    /// cache with hundreds of consumers produces that many simultaneous
    /// waiters, and that many identical rows is not a diagnostic.
    ///
    /// Paired one-to-one with `ScratchLockWaitEnd` (which fires on acquire
    /// **or** cancellation).
    ScratchLockWaitStart {
        /// The consumer waiting for the slot.
        addr: String,
        /// The scratch *declaration* being waited on (`//build:gocache`). The
        /// subject of the diagnostic — the slot hash stays off the wire, being
        /// internal and already resolvable via `heph tool scratch path`.
        scratch: String,
        /// `"exclusive"` or `"shared"`, echoing the word used in the BUILD file
        /// rather than a bool the user would have to translate back.
        access: String,
        /// A process holding the slot when it was probed, when one can be named.
        ///
        /// Only an *exclusive* holder in **another** process stamps a nameable
        /// pid, so this is `None` whenever the wait is on shared readers
        /// draining, or on this build serializing against itself — the same
        /// limitation, for the same reason, as `ResultLockWaitStart::holder_pid`.
        holder_pid: Option<u32>,
    },
    /// The scratch-slot wait ended (slot acquired or the wait was cancelled).
    ScratchLockWaitEnd {
        addr: String,
        scratch: String,
    },
    /// Start of preparing a scratch directory for a consumer: everything done
    /// under the slot guard — resolving the lineage, seeding a new scope from a
    /// fallback, creating the directory, and pulling from the remote when the
    /// lineage is cold.
    ///
    /// One span rather than one per step: they share a guard, run in fixed
    /// order, and a user cannot act on them separately. What they *can* act on
    /// is the outcome, which is why the detail rides on the `End`.
    ///
    /// Only ever emitted on an execute (i.e. after a cache miss), so a fully
    /// cached run emits none of these.
    ScratchPrepareStart {
        addr: String,
        scratch: String,
    },
    ScratchPrepareEnd {
        addr: String,
        scratch: String,
        /// What preparing the directory actually did — `"warm"`, `"seeded"`,
        /// `"pulled"`, `"cold"`, `"audit"`, or `"interrupted"`.
        ///
        /// A string, not an enum, and for a wire reason: an enum variant a
        /// skewed plugin has never heard of fails to deserialize, and the SDK
        /// treats a decode failure as end-of-stream. An unrecognised string just
        /// prints. Same call, for the same reason, as `ResultEnd::exit_status`.
        #[serde(default)]
        outcome: String,
        /// Bytes pulled from the remote, when the outcome was `"pulled"`.
        #[serde(default)]
        bytes: u64,
        /// The snapshot was restored at a different path than it was produced
        /// at.
        ///
        /// Worth its own field rather than a log line: a cache whose entries
        /// embed absolute paths restores perfectly and is then *inert* — the
        /// pull reports success, the bytes are spent, and every future build
        /// still runs cold. It looks exactly like a hit, so nothing else in the
        /// stream can distinguish it.
        #[serde(default)]
        path_mismatch: bool,
        error: Option<String>,
    },
    RemoteCacheHit {
        addr: String,
    },
    RemoteCacheMiss {
        addr: String,
    },
    /// Start of pulling a target's revision from the remote cache(s) into the
    /// local cache (one span per target, covering all of its blobs from the one
    /// cache that had the manifest). Surfaced as one `↓` op in the per-target
    /// timeline, marked slow if it runs long. Paired with `RemoteCacheReadEnd`.
    RemoteCacheReadStart {
        addr: String,
    },
    RemoteCacheReadEnd {
        addr: String,
        error: Option<String>,
    },
    /// Start of pushing a target's artifacts to the remote cache(s). Runs on a
    /// background task after the build's critical path, so it appears in the
    /// per-target op timeline (and is surfaced as "slow" if it runs long).
    /// Paired with `RemoteCacheWriteEnd`.
    RemoteCacheWriteStart {
        addr: String,
    },
    RemoteCacheWriteEnd {
        addr: String,
        error: Option<String>,
    },
    /// One target finished a cache sweep: how many cache revisions it dropped
    /// and how many bytes those revisions freed (summed from manifest artifact
    /// sizes). Emitted once per target visited by `heph tool gc` or by
    /// `heph tool clean` — including targets that dropped nothing — so a consumer can
    /// show both the count of targets explored and the total data reclaimed.
    GcTargetSwept {
        revisions_removed: usize,
        bytes_removed: u64,
    },
    /// An event kind this build of the consumer does not know.
    ///
    /// **Load-bearing for cross-version safety, not a placeholder.** Events ride
    /// to out-of-process hooks as serde-JSON, and a plugin cdylib ships as its
    /// own release artifact pinned independently of the `heph` binary — so a
    /// consumer meeting an event kind newer than itself is a reachable state.
    ///
    /// Without this catch-all, that event fails to deserialize, and the plugin
    /// SDK's pull loop treats a decode failure as end-of-stream
    /// (`crates/plugin-sdk/src/serve.rs`: `decode_event_frame(..)` → `None` →
    /// `break`) and then calls `on_close()`. One unknown event would silently
    /// truncate that hook's **entire** stream while the hook believed the build
    /// had ended normally — no error surfaced anywhere.
    ///
    /// `#[serde(other)]` makes the unknown kind decode to this variant instead,
    /// so consumers skip it and keep receiving everything after it.
    #[serde(other)]
    Unknown,
}

/// A bounded slice of a target's process log, carried on the event stream.
///
/// A mirror of `hplugin::error::LogTail` rather than that type itself: it has no
/// serde derives, and `crates/plugin` depends on this crate, not the reverse.
/// `hcore` is deliberately the lowest crate so the TUI and telemetry need no
/// engine or plugin dependency to read events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogTailData {
    pub text: String,
    /// 1-based line number of `text`'s first line within the full log, so a
    /// renderer can show true file positions rather than renumbering from 1.
    pub start_line: usize,
}

pub type EventSender = tokio::sync::mpsc::UnboundedSender<BuildEvent>;
pub type EventReceiver = tokio::sync::mpsc::UnboundedReceiver<BuildEvent>;

pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Wall-clock milliseconds since the Unix epoch. Stamped once at emit time.
#[cfg(test)]
mod tests {
    use super::*;

    /// A frame from a host *older* than this build: the sibling fields are absent
    /// from the JSON entirely.
    ///
    /// This must decode. Without `#[serde(default)]` on each new field, a missing
    /// key is a hard error — a bare `Option<T>` is not absent-tolerant under
    /// `serde_derive` — and the plugin SDK treats a decode error as end-of-stream,
    /// silently truncating everything after it.
    #[test]
    fn old_host_frame_decodes_against_new_fields() {
        let old = r#"{"at_unix_ms":1,"kind":{"type":"ResultEnd","addr":"//a:x","error":"boom"}}"#;
        let ev: BuildEvent = serde_json::from_str(old).expect("old frame must decode");
        match ev.kind {
            BuildEventKind::ResultEnd {
                addr,
                error,
                upstream_of,
                exit_status,
                log_tail,
            } => {
                assert_eq!(addr, "//a:x");
                assert_eq!(error.as_deref(), Some("boom"));
                assert_eq!(upstream_of, None);
                assert_eq!(exit_status, None);
                assert_eq!(log_tail, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// `RequestConfig` from a host predating `scratch_disabled`.
    ///
    /// The default must read as "scratch was on", which is the truth for every
    /// frame emitted before the field existed. The positive spelling would
    /// default these to "scratch was off" and have consumers report a
    /// deliberate-looking audit on runs that never asked for one.
    #[test]
    fn an_old_request_config_defaults_to_scratch_enabled() {
        let old = r#"{"at_unix_ms":1,"kind":{"type":"RequestConfig","max_workers":8,
            "fail_fast":false}}"#;
        let ev: BuildEvent = serde_json::from_str(old).expect("old frame must decode");
        match ev.kind {
            BuildEventKind::RequestConfig {
                scratch_disabled, ..
            } => assert!(
                !scratch_disabled,
                "a host with no such field had scratch enabled",
            ),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// The scratch spans decode with their detail fields absent — the shape an
    /// older host, or one that emits only the `Start` half, produces.
    #[test]
    fn scratch_prepare_end_decodes_without_its_detail_fields() {
        let frame = r#"{"at_unix_ms":1,"kind":{"type":"ScratchPrepareEnd",
            "addr":"//a:x","scratch":"//b:cache","error":null}}"#;
        let ev: BuildEvent = serde_json::from_str(frame).expect("must decode");
        match ev.kind {
            BuildEventKind::ScratchPrepareEnd {
                outcome,
                bytes,
                path_mismatch,
                ..
            } => {
                assert_eq!(outcome, "");
                assert_eq!(bytes, 0);
                assert!(!path_mismatch);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// A frame from a host *newer* than this build: it carries fields this build
    /// has never heard of. Serde ignores unknown fields by default, and nothing
    /// here sets `deny_unknown_fields` — adding it would break this.
    #[test]
    fn newer_host_frame_with_unknown_fields_decodes() {
        let newer = r#"{"at_unix_ms":1,"kind":{"type":"ResultEnd","addr":"//a:x",
            "error":null,"upstream_of":null,"exit_status":null,"log_tail":null,
            "some_field_from_the_future":{"nested":true}}}"#;
        let ev: BuildEvent = serde_json::from_str(newer).expect("unknown fields are ignored");
        assert!(matches!(ev.kind, BuildEventKind::ResultEnd { .. }));
    }

    /// An event *kind* from a newer host. Without `Unknown`'s `#[serde(other)]`
    /// this fails to deserialize, and one such event silently ends the whole
    /// stream for an out-of-process hook while it believes the build finished.
    #[test]
    fn unknown_event_kind_decodes_instead_of_ending_the_stream() {
        let future = r#"{"at_unix_ms":1,"kind":{"type":"SomethingInventedLater","x":1}}"#;
        let ev: BuildEvent = serde_json::from_str(future).expect("unknown kind must not be fatal");
        assert!(matches!(ev.kind, BuildEventKind::Unknown));
    }

    /// A whole stream survives an unknown kind in the middle — the property that
    /// actually matters, since the failure mode is truncation, not one lost event.
    #[test]
    fn a_stream_continues_past_an_unknown_kind() {
        let frames = [
            r#"{"at_unix_ms":1,"kind":{"type":"RequestConfig","max_workers":8,"fail_fast":false}}"#,
            r#"{"at_unix_ms":2,"kind":{"type":"FutureKind","whatever":true}}"#,
            r#"{"at_unix_ms":3,"kind":{"type":"ResultEnd","addr":"//a:x","error":null}}"#,
        ];
        let decoded: Vec<BuildEvent> = frames
            .iter()
            .map(|f| serde_json::from_str(f).expect("every frame decodes"))
            .collect();
        assert_eq!(decoded.len(), 3);
        assert!(matches!(decoded[1].kind, BuildEventKind::Unknown));
        assert!(matches!(decoded[2].kind, BuildEventKind::ResultEnd { .. }));
    }

    /// New fields round-trip when both sides know them.
    #[test]
    fn structured_failure_detail_round_trips() {
        let ev = BuildEvent {
            at_unix_ms: 7,
            kind: BuildEventKind::ResultEnd {
                addr: "//a:x".into(),
                error: Some("target failed".into()),
                upstream_of: Some("//base:proto".into()),
                exit_status: Some("exit status: 1".into()),
                log_tail: Some(LogTailData {
                    text: "FAIL".into(),
                    start_line: 88,
                }),
            },
        };
        let json = serde_json::to_string(&ev).expect("encode");
        let back: BuildEvent = serde_json::from_str(&json).expect("decode");
        match back.kind {
            BuildEventKind::ResultEnd {
                upstream_of,
                exit_status,
                log_tail,
                ..
            } => {
                assert_eq!(upstream_of.as_deref(), Some("//base:proto"));
                assert_eq!(exit_status.as_deref(), Some("exit status: 1"));
                assert_eq!(log_tail.map(|l| l.start_line), Some(88));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
