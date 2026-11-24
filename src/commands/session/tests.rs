use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use hcore::events::BuildEventKind;

use super::attach::{Outcome, outcome_result, trigger_on};
use super::*;
use crate::tui::session::format::{
    LockWaitEntry, RunningTarget, SessionEnd, SnapshotCounts, files,
};
use crate::tui::session::testing::{self, Recording};
use crate::tui::session::{
    HeaderKind, SessionItem, SessionMeta, SessionRecord, SessionRecordKind, SessionSnapshot,
};

const T0: u64 = 1_763_000_000_000;
const NOW: u64 = T0 + 120_000;

fn opts() -> ViewOpts {
    ViewOpts {
        now_ms: NOW,
        stall: Duration::from_secs(60),
        version: "v0.48.1",
        retention: crate::tui::session::cleanup::Retention::default(),
    }
}

fn meta(id: SessionId, pid: u32, argv: &[&str]) -> SessionMeta {
    SessionMeta {
        format: crate::tui::session::META_FORMAT,
        id,
        owner_pid: pid,
        started_unix_ms: id.started_unix_ms(),
        heph_version: "v0.48.1".to_owned(),
        argv: argv.iter().map(|s| (*s).to_owned()).collect(),
        cwd: Some("/w/pacu/sub".to_owned()),
        workspace_root: Some("/w/pacu".to_owned()),
        home_dir: None,
        label: "run".to_owned(),
        header: HeaderKind::Build,
        interactive: false,
        capabilities: vec![crate::tui::session::CAPABILITY_INFLIGHT.to_owned()],
    }
}

fn ended(exit_code: Option<i32>, at: u64) -> SessionSnapshot {
    SessionSnapshot {
        last_event_unix_ms: Some(at),
        end: Some(SessionEnd {
            at_unix_ms: at,
            exit_code,
            panicked: false,
        }),
        ..SessionSnapshot::default()
    }
}

fn entry(meta: SessionMeta, live: bool, status: Option<SessionSnapshot>) -> SessionEntry {
    SessionEntry {
        id: meta.id,
        meta: EntryMeta::Known(Box::new(meta)),
        live,
        status,
        status_unreadable: false,
    }
}

fn record(at: u64, kind: BuildEventKind) -> SessionRecord {
    SessionRecord {
        at_unix_ms: at,
        kind: Some(kind),
        session: None,
    }
}

fn build_records(addr: &str, at: u64, exit: Option<i32>) -> Vec<SessionRecord> {
    let mut v = vec![
        record(
            at,
            BuildEventKind::Matched {
                addrs: vec![addr.to_owned()],
                complete: true,
            },
        ),
        record(at + 10, BuildEventKind::ResultStart { addr: addr.into() }),
    ];
    if let Some(code) = exit {
        v.push(SessionRecord::session(
            at + 1_000,
            SessionRecordKind::End {
                exit_code: Some(code),
                panicked: false,
            },
        ));
    }
    v
}

/// A source held in memory: what a daemon, or a test, hands the commands.
#[derive(Debug, Default)]
struct FakeSource {
    sessions: Vec<(SessionEntry, Vec<SessionRecord>)>,
}

#[async_trait]
impl SessionSource for FakeSource {
    async fn list(&self) -> anyhow::Result<Vec<SessionEntry>> {
        Ok(self.sessions.iter().map(|(e, _)| e.clone()).collect())
    }

    async fn snapshot(&self, id: SessionId) -> anyhow::Result<Option<SessionEntry>> {
        Ok(self
            .sessions
            .iter()
            .find(|(e, _)| e.id == id)
            .map(|(e, _)| e.clone()))
    }

    async fn subscribe(
        &self,
        id: SessionId,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<SessionItem>>> {
        let (e, records) = self
            .sessions
            .iter()
            .find(|(e, _)| e.id == id)
            .ok_or_else(|| anyhow::anyhow!("no session {id}"))?;
        let exit = records.iter().find_map(|r| match r.session {
            Some(SessionRecordKind::End { exit_code, .. }) => Some(exit_code),
            _ => None,
        });
        let mut items: Vec<SessionItem> = records
            .iter()
            .cloned()
            .map(Box::new)
            .map(SessionItem::Record)
            .collect();
        items.push(SessionItem::CaughtUp { skipped: 0 });
        match (exit, e.live) {
            (Some(exit), _) => items.push(SessionItem::Ended { exit, lost: false }),
            (None, false) => items.push(SessionItem::Ended {
                exit: None,
                lost: true,
            }),
            (None, true) => {}
        }
        Ok(futures::stream::iter(items.into_iter().map(Ok)).boxed())
    }

    async fn inflight(&self, id: SessionId, _timeout: Duration) -> anyhow::Result<String> {
        Ok(format!("in-flight report for {id}\n"))
    }
}

/// The output flags as `--json` gives them, or as none does.
fn json_args<R: output::Record + 'static>(json: bool) -> output::JsonArgs<R> {
    if json {
        output::JsonArgs::try_parse_from(["--json"]).expect("--json parses")
    } else {
        output::JsonArgs::default()
    }
}

/// A buffer as piped stdout.
fn piped(w: &mut Vec<u8>) -> Out<'_> {
    Out {
        w,
        style: output::Style::Compact,
    }
}

async fn run_list(sources: &[&dyn SessionSource], ws: Workspace<'_>, json: bool) -> String {
    let mut out = Vec::new();
    list::list(sources, opts(), ws, &json_args(json), piped(&mut out))
        .await
        .expect("list");
    String::from_utf8(out).expect("utf-8")
}

async fn run_show(sources: &[&dyn SessionSource], id: &str, json: bool) -> anyhow::Result<String> {
    let mut out = Vec::new();
    show::show(
        sources,
        opts(),
        id,
        &json_args(json),
        piped(&mut out),
        &no_opener(),
    )
    .await?;
    Ok(String::from_utf8(out).expect("utf-8"))
}

/// A download that never happens: a test that reaches it named a URL.
struct NoDownload;

impl reference::Download for NoDownload {
    fn get_to_temp(&self, url: &str, _dir: &Path) -> anyhow::Result<tempfile::NamedTempFile> {
        anyhow::bail!("no download in this test: {url}")
    }
}

/// An opener for tests that name sessions by id: nothing is downloaded, and
/// a path `<ref>` puts its temp files in a directory that does not exist.
fn no_opener() -> reference::Opener {
    reference::Opener {
        tmp_dir: Path::new("/nonexistent/heph-session-tmp").into(),
        download: Arc::new(NoDownload),
    }
}

#[test]
fn state_is_derived() {
    let id = SessionId::new(T0, 1);
    let m = || meta(id, 10, &["heph", "run", "//a:b"]);
    let running = |addr: &str, since: u64| RunningTarget {
        addr: addr.to_owned(),
        op: "execute".to_owned(),
        since_unix_ms: since,
    };
    let live = |s: SessionSnapshot| entry(m(), true, Some(s));
    let fresh = |s: SessionSnapshot| SessionSnapshot {
        last_event_unix_ms: Some(NOW - 1_000),
        ..s
    };
    let cases: Vec<(&str, SessionEntry, SessionState)> = vec![
        (
            "live, nothing written yet",
            entry(m(), true, None),
            SessionState::Running,
        ),
        (
            "live, a target running",
            live(fresh(SessionSnapshot {
                running: vec![running("//a:b", NOW - 5_000)],
                ..SessionSnapshot::default()
            })),
            SessionState::Running,
        ),
        (
            "every running target waits",
            live(fresh(SessionSnapshot {
                running: vec![running("//a:b", NOW - 5_000)],
                waiting: vec!["//a:b".to_owned(), "//a:c".to_owned()],
                ..SessionSnapshot::default()
            })),
            SessionState::Waiting,
        ),
        (
            "one waits, another runs",
            live(fresh(SessionSnapshot {
                running: vec![running("//a:b", NOW - 5_000), running("//a:d", NOW)],
                waiting: vec!["//a:b".to_owned()],
                counts: SnapshotCounts {
                    running_unblocked: Some(1),
                    ..SnapshotCounts::default()
                },
                ..SessionSnapshot::default()
            })),
            SessionState::Running,
        ),
        (
            // `status.json` cut both lists; the counts cover them in full.
            "capped lists, every running target waits",
            live(fresh(SessionSnapshot {
                running: vec![running("//z:z", NOW - 5_000)],
                waiting: vec!["//a:a".to_owned()],
                counts: SnapshotCounts {
                    running_unblocked: Some(0),
                    ..SnapshotCounts::default()
                },
                totals: crate::tui::session::format::ListTotals {
                    waiting: crate::tui::session::format::ListTotal {
                        total: 500,
                        truncated: true,
                    },
                    ..Default::default()
                },
                ..SessionSnapshot::default()
            })),
            SessionState::Waiting,
        ),
        (
            "quiet past the stall threshold",
            live(SessionSnapshot {
                last_event_unix_ms: Some(NOW - 61_000),
                running: vec![running("//a:b", NOW - 90_000)],
                ..SessionSnapshot::default()
            }),
            SessionState::Quiet,
        ),
        (
            "quiet, but not for long",
            live(SessionSnapshot {
                last_event_unix_ms: Some(NOW - 59_000),
                ..SessionSnapshot::default()
            }),
            SessionState::Running,
        ),
        (
            "ended 0",
            entry(m(), false, Some(ended(Some(0), NOW))),
            SessionState::Ok,
        ),
        (
            "ended 1",
            entry(m(), false, Some(ended(Some(1), NOW))),
            SessionState::Failed,
        ),
        (
            "ended by a signal",
            entry(m(), false, Some(ended(None, NOW))),
            SessionState::Failed,
        ),
        (
            "panicked with 0",
            entry(
                m(),
                false,
                Some(SessionSnapshot {
                    end: Some(SessionEnd {
                        at_unix_ms: NOW,
                        exit_code: Some(0),
                        panicked: true,
                    }),
                    ..SessionSnapshot::default()
                }),
            ),
            SessionState::Failed,
        ),
        (
            "gone without an end",
            entry(m(), false, Some(SessionSnapshot::default())),
            SessionState::Lost,
        ),
        (
            "gone, no status",
            entry(m(), false, None),
            SessionState::Lost,
        ),
        (
            "gone, status too new to read",
            SessionEntry {
                status_unreadable: true,
                ..entry(m(), false, None)
            },
            SessionState::Unknown,
        ),
        (
            "newer format",
            SessionEntry {
                id,
                meta: EntryMeta::TooNew {
                    found: 9,
                    supported: 1,
                },
                live: true,
                status: None,
                status_unreadable: false,
            },
            SessionState::Unknown,
        ),
    ];
    for (name, e, want) in cases {
        assert_eq!(
            derive_state(&e, NOW, Duration::from_secs(60)),
            want,
            "{name}"
        );
    }
    assert_eq!(stall_threshold(Duration::ZERO), Duration::from_secs(60));
    assert_eq!(
        stall_threshold(Duration::from_secs(5)),
        Duration::from_secs(5)
    );
    // `quiet` is strictly past the threshold, under the default (no
    // `--stall-notice`) and under a custom one.
    let quiet_for = |ms: u64| {
        live(SessionSnapshot {
            last_event_unix_ms: Some(NOW - ms),
            ..SessionSnapshot::default()
        })
    };
    let thresholds: &[(&str, Duration, u64, SessionState)] = &[
        (
            "default, exactly 60s",
            Duration::ZERO,
            60_000,
            SessionState::Running,
        ),
        (
            "default, 60s + 1ms",
            Duration::ZERO,
            60_001,
            SessionState::Quiet,
        ),
        (
            "--stall-notice 5s, 4.999s",
            Duration::from_secs(5),
            4_999,
            SessionState::Running,
        ),
        (
            "--stall-notice 5s, exactly 5s",
            Duration::from_secs(5),
            5_000,
            SessionState::Running,
        ),
        (
            "--stall-notice 5s, 5.001s",
            Duration::from_secs(5),
            5_001,
            SessionState::Quiet,
        ),
        // A custom threshold above the default holds a session past 60s.
        (
            "--stall-notice 5m, 61s",
            Duration::from_secs(300),
            61_000,
            SessionState::Running,
        ),
    ];
    for &(name, notice, ms, want) in thresholds {
        assert_eq!(
            derive_state(&quiet_for(ms), NOW, stall_threshold(notice)),
            want,
            "{name}"
        );
    }
    // Unknown is what a newer heph's state decodes as.
    let s: SessionState = serde_json::from_str("\"hibernating\"").expect("decode");
    assert_eq!(s, SessionState::Unknown);
}

/// A `status.json` from a writer without `running_unblocked` (a dev build)
/// does not read as `waiting` by default: the lists are compared instead.
#[test]
fn status_without_running_unblocked_compares_the_lists() {
    let id = SessionId::new(T0, 1);
    let status = |running: &[&str], waiting: &[&str]| {
        let running: Vec<_> = running
            .iter()
            .map(|a| serde_json::json!({"addr": a, "op": "execute", "since_unix_ms": NOW - 5_000}))
            .collect();
        let json = serde_json::json!({
            "version": 1,
            "last_event_unix_ms": NOW - 1_000,
            "counts": {"in_flight": running.len()},
            "running": running,
            "waiting": waiting,
        });
        let s: SessionSnapshot = serde_json::from_value(json).expect("an old status.json");
        assert_eq!(s.counts.running_unblocked, None);
        entry(meta(id, 10, &["heph", "run"]), true, Some(s))
    };
    let state = |e: &SessionEntry| derive_state(e, NOW, Duration::from_secs(60));
    assert_eq!(
        state(&status(&["//a:b", "//a:c"], &["//a:b"])),
        SessionState::Running,
        "one target runs outside a wait"
    );
    assert_eq!(
        state(&status(&["//a:b"], &["//a:b"])),
        SessionState::Waiting,
        "every running target waits"
    );
}

#[tokio::test]
async fn commands_run_against_any_source() {
    let a = SessionId::new(T0, 1);
    let b = SessionId::new(T0 + 5_000, 2);
    let holder = SessionId::new(T0 + 9_000, 3);
    let root_holder = SessionId::new(T0 + 11_000, 4);

    let waiting_snapshot = SessionSnapshot {
        last_event_unix_ms: Some(NOW - 1_000),
        waiting: vec!["//go:sdk".to_owned()],
        lock_waits: vec![LockWaitEntry {
            addr: "//go:sdk".to_owned(),
            holder_pid: Some(300),
            in_use_by_readers: false,
            since_unix_ms: NOW - 39_000,
        }],
        ..SessionSnapshot::default()
    };
    let mut a_records = build_records("//a:b", T0, None);
    a_records.push(record(
        NOW - 39_000,
        BuildEventKind::ResultLockWaitStart {
            addr: "//go:sdk".into(),
            holder_pid: Some(300),
            in_use_by_readers: false,
        },
    ));
    let first = FakeSource {
        sessions: vec![
            (
                entry(
                    meta(a, 100, &["/usr/bin/heph", "run", "//go/..."]),
                    true,
                    Some(waiting_snapshot),
                ),
                a_records,
            ),
            // The holder waits in turn on the root holder.
            (
                entry(
                    meta(holder, 300, &["heph", "run", "//..."]),
                    true,
                    Some(SessionSnapshot {
                        last_event_unix_ms: Some(NOW),
                        lock_waits: vec![LockWaitEntry {
                            addr: "//go:base".to_owned(),
                            holder_pid: Some(400),
                            in_use_by_readers: false,
                            since_unix_ms: NOW - 50_000,
                        }],
                        ..SessionSnapshot::default()
                    }),
                ),
                Vec::new(),
            ),
        ],
    };
    let second = FakeSource {
        sessions: vec![
            (
                entry(
                    meta(b, 200, &["heph", "run", "//x:y"]),
                    false,
                    Some(SessionSnapshot {
                        counts: SnapshotCounts {
                            matched: Some(1),
                            matched_complete: true,
                            matched_done: 1,
                            ..SnapshotCounts::default()
                        },
                        ..ended(Some(0), T0 + 6_000)
                    }),
                ),
                {
                    // `show --json` folds the log: the target finishes in it.
                    let mut r = build_records("//x:y", T0 + 5_000, Some(0));
                    r.insert(
                        2,
                        record(
                            T0 + 5_500,
                            BuildEventKind::ResultEnd {
                                left_behind: None,
                                spans: Vec::new(),
                                cancelled: false,
                                addr: "//x:y".into(),
                                error: None,
                                upstream_of: None,
                                exit_status: None,
                                exit_code: None,
                                log_tail: None,
                                timed_out: None,
                            },
                        ),
                    );
                    r
                },
            ),
            (
                entry(
                    meta(root_holder, 400, &["heph", "run", "//base"]),
                    true,
                    None,
                ),
                Vec::new(),
            ),
            // Listed by both sources: the first one's wins.
            (
                entry(meta(a, 999, &["heph", "dup"]), false, None),
                Vec::new(),
            ),
        ],
    };
    let sources: [&dyn SessionSource; 2] = [&first, &second];

    let table = run_list(&sources, Workspace::All, false).await;
    let lines: Vec<&str> = table.lines().collect();
    assert!(lines[0].starts_with("ID"), "{table}");
    for col in ["WORKTREE", "COMMAND", "STATE", "EXIT", "PROGRESS", "NOW"] {
        assert!(lines[0].contains(col), "{table}");
    }
    assert_eq!(lines.len(), 5, "header + 4 sessions:\n{table}");
    // Live first; the ended one last.
    assert!(lines[4].starts_with(&b.encode()), "{table}");
    assert!(lines[4].contains("ok"), "{table}");
    assert!(lines[4].contains("1/1"), "{table}");
    let a_line = lines
        .iter()
        .find(|l| l.starts_with(&a.encode()))
        .expect("a listed");
    assert!(a_line.contains("pacu"), "{a_line}");
    assert!(
        a_line.contains("heph run //go/..."),
        "program by its name: {a_line}"
    );
    assert!(a_line.contains("waiting"), "{a_line}");
    assert!(
        a_line.contains(&format!("lock on //go:sdk held by {holder}")),
        "{a_line}"
    );
    assert!(
        !table.contains("dup"),
        "the first source wins an id:\n{table}"
    );

    // Another workspace's view hides them.
    assert_eq!(
        run_list(&sources, Workspace::Only("/elsewhere"), false)
            .await
            .trim(),
        "no sessions in this workspace (--all for every workspace)"
    );
    assert_eq!(
        run_list(&[], Workspace::All, false).await.trim(),
        "no sessions"
    );

    let json: serde_json::Value =
        serde_json::from_str(&run_list(&sources, Workspace::Only("/w/pacu"), true).await)
            .expect("json");
    let ids: Vec<&str> = json
        .as_array()
        .expect("a bare array of sessions")
        .iter()
        .map(|s| s["id"].as_str().expect("id"))
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "sorted by id");
    let a_json = &json[0];
    assert_eq!(a_json["state"], "waiting");
    assert_eq!(a_json["owner_pid"], 100);
    assert_eq!(a_json["started_unix_ms"], T0);
    assert_eq!(a_json["blocked_on"][0]["kind"], "lock");
    assert_eq!(a_json["blocked_on"][0]["holder_session"], holder.encode());
    // The table's NOW and PROGRESS, as data.
    assert_eq!(
        a_json["now"],
        format!("lock on //go:sdk held by {holder}"),
        "{a_json}"
    );
    assert_eq!(a_json["progress"]["total"], serde_json::Value::Null);
    assert_eq!(a_json["progress"]["running_ops"], 0);
    assert_eq!(a_json["panicked"], false);
    assert!(a_json.get("format").is_none(), "only on too-new rows");
    let b_json = &json[1];
    assert_eq!(b_json["exit_code"], 0);
    assert_eq!(b_json["state"], "ok");
    assert_eq!(b_json["now"], serde_json::Value::Null, "it ended");
    assert_eq!(
        b_json["progress"],
        serde_json::json!({"done": 1, "total": 1, "total_complete": true, "running_ops": 0, "failed": 0})
    );

    // show: header, folded body, the stuck-on chain, one next action.
    let out = run_show(&sources, &a.encode(), false)
        .await
        .expect("show a");
    let first_line = out.lines().next().expect("header");
    assert!(
        first_line.starts_with(&format!("heph run //go/... (session {a}, pid 100, pacu, /w/pacu/sub, started 2m00s ago, heph v0.48.1)")),
        "{out}"
    );
    assert!(out.contains("Stuck on"), "{out}");
    assert!(
        out.contains(&format!("//go:sdk   waiting 39s for its result lock, held by session {holder} (heph run //..., pid 300)")),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "which waits for //go:base, held by session {root_holder}"
        )),
        "the chain is followed to its root:\n{out}"
    );
    // The bare command on the `Next:` line, its reason on the next.
    assert!(
        out.trim_end().ends_with(&format!(
            "\nNext: heph session show {root_holder}\n  what the session holding the lock is doing"
        )),
        "{out}"
    );

    // show --json: list's `blocked_on`, the chain's root, a structured next.
    let json: serde_json::Value =
        serde_json::from_str(&run_show(&sources, &a.encode(), true).await.expect("json"))
            .expect("json");
    assert_eq!(json["state"], "waiting");
    assert_eq!(json["blocked_on"][0]["kind"], "lock");
    assert_eq!(json["blocked_on"][0]["addr"], "//go:sdk");
    assert_eq!(json["blocked_on"][0]["holder_pid"], 300);
    assert_eq!(json["blocked_on"][0]["holder_session"], holder.encode());
    assert_eq!(json["chain_root"], root_holder.encode());
    assert_eq!(json["next"]["kind"], "show_holder");
    assert_eq!(
        json["next"]["command"],
        format!("heph session show {root_holder}")
    );
    assert_eq!(
        json["next"]["reason"],
        "what the session holding the lock is doing"
    );

    // An ended session, from the second source.
    let out = run_show(&sources, &b.encode(), false)
        .await
        .expect("show b");
    assert!(out.contains(&format!("(session {b}, pid 200")), "{out}");
    assert!(!out.contains("Stuck on"), "{out}");
    assert!(
        out.trim_end().ends_with("\nNext: nothing\n  it succeeded"),
        "{out}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&run_show(&sources, &b.encode(), true).await.expect("json"))
            .expect("json");
    assert_eq!(json["state"], "ok");
    assert_eq!(json["id"], b.encode());
    assert_eq!(json["end"]["exit_code"], 0);
    assert_eq!(json["counts"]["matched_done"], 1);
    assert_eq!(json["blocked_on"], serde_json::json!([]));
    assert_eq!(json["chain_root"], serde_json::Value::Null);
    assert_eq!(json["next"]["kind"], "none");
    assert_eq!(json["next"]["command"], serde_json::Value::Null);
    // The snapshot's own `version` versions `status.json`, not this output.
    assert!(json.get("version").is_none(), "{json}");

    // inflight goes to the source that has the session.
    let mut out = Vec::new();
    inflight(
        &sources,
        opts(),
        &b.encode(),
        &json_args(false),
        piped(&mut out),
    )
    .await
    .expect("inflight");
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        format!("in-flight report for {b}\n")
    );
    let mut out = Vec::new();
    inflight(
        &sources,
        opts(),
        &b.encode(),
        &json_args(true),
        piped(&mut out),
    )
    .await
    .expect("inflight --json");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out).expect("json"),
        serde_json::json!({"id": b.encode(), "report": format!("in-flight report for {b}\n")})
    );
}

/// Item 12: outside a workspace `list` widens to every workspace on its own,
/// and says so on stderr; `--all`, or a workspace to narrow to, says nothing.
#[test]
fn list_notes_when_it_widens_outside_a_workspace() {
    assert_eq!(
        list::scope_note(false, None),
        Some("heph: not in a workspace; listing the sessions of every workspace\n")
    );
    assert_eq!(list::scope_note(true, None), None);
    assert_eq!(list::scope_note(false, Some("/w/pacu")), None);
}

/// Item 9: every state's next step, as `show --json`'s `next` and as the
/// human `Next:` line (the bare command, the reason on its own line).
#[tokio::test]
async fn next_is_structured_for_every_state() {
    let fail_records = |addr: &str| {
        vec![
            record(T0 + 1, BuildEventKind::ResultStart { addr: addr.into() }),
            record(
                T0 + 2,
                BuildEventKind::ResultEnd {
                    left_behind: None,
                    spans: Vec::new(),
                    cancelled: false,
                    addr: addr.into(),
                    error: Some("boom".to_owned()),
                    upstream_of: None,
                    exit_status: None,
                    exit_code: None,
                    log_tail: None,
                    timed_out: None,
                },
            ),
        ]
    };
    let quiet = SessionSnapshot {
        last_event_unix_ms: Some(NOW - 61_000),
        ..SessionSnapshot::default()
    };
    let busy = SessionSnapshot {
        last_event_unix_ms: Some(NOW - 1_000),
        ..SessionSnapshot::default()
    };
    let id = SessionId::new(T0, 1);
    let e = |live, status| entry(meta(id, 1, &["/bin/heph", "run", "//a:b"]), live, status);
    let (attach, inflight) = (
        format!("heph session attach {id}"),
        format!("heph session inflight {id}"),
    );
    // Waiting on a lock whose holder records no session: there is no chain
    // root to point at, so the next step falls back to following the run.
    let waits_on_a_stranger = |last: u64| SessionSnapshot {
        last_event_unix_ms: Some(last),
        waiting: vec!["//go:sdk".to_owned()],
        lock_waits: vec![LockWaitEntry {
            addr: "//go:sdk".to_owned(),
            holder_pid: Some(999),
            in_use_by_readers: false,
            since_unix_ms: NOW - 90_000,
        }],
        ..SessionSnapshot::default()
    };
    let stranger_records = vec![record(
        NOW - 90_000,
        BuildEventKind::ResultLockWaitStart {
            addr: "//go:sdk".into(),
            holder_pid: Some(999),
            in_use_by_readers: false,
        },
    )];
    /// (session, records, next kind, next command, state)
    type Case<'a> = (
        SessionEntry,
        Vec<SessionRecord>,
        &'a str,
        Option<&'a str>,
        &'a str,
    );
    let cases: Vec<Case<'_>> = vec![
        (
            e(true, Some(busy)),
            Vec::new(),
            "attach",
            Some(&attach),
            "running",
        ),
        (
            e(true, Some(quiet)),
            Vec::new(),
            "inflight",
            Some(&inflight),
            "quiet",
        ),
        (
            e(false, Some(ended(Some(1), NOW))),
            fail_records("//a:b"),
            "inspect_failure",
            Some("heph inspect failure //a:b"),
            "failed",
        ),
        (
            e(false, Some(ended(Some(1), NOW))),
            Vec::new(),
            "rerun",
            Some("heph run //a:b"),
            "failed",
        ),
        (
            e(false, Some(SessionSnapshot::default())),
            Vec::new(),
            "rerun",
            Some("heph run //a:b"),
            "lost",
        ),
        (
            SessionEntry {
                status_unreadable: true,
                ..e(false, None)
            },
            Vec::new(),
            "list",
            Some("heph session list"),
            "unknown",
        ),
        (
            e(false, Some(ended(Some(0), NOW))),
            Vec::new(),
            "none",
            None,
            "ok",
        ),
        // Waiting with no chain root: attach, not show_holder.
        (
            e(true, Some(waits_on_a_stranger(NOW - 1_000))),
            stranger_records.clone(),
            "attach",
            Some(&attach),
            "waiting",
        ),
        // Waiting and quiet both apply: waiting wins, a wait explains the quiet.
        (
            e(true, Some(waits_on_a_stranger(NOW - 61_000))),
            stranger_records,
            "attach",
            Some(&attach),
            "waiting",
        ),
        // Failed and quiet both apply: an ended session is never quiet.
        (
            e(false, Some(ended(Some(1), NOW - 61_000))),
            fail_records("//a:b"),
            "inspect_failure",
            Some("heph inspect failure //a:b"),
            "failed",
        ),
    ];
    for (n, (session, records, kind, command, state)) in cases.into_iter().enumerate() {
        let source = FakeSource {
            sessions: vec![(session, records)],
        };
        let sources: [&dyn SessionSource; 1] = [&source];
        let json: serde_json::Value =
            serde_json::from_str(&run_show(&sources, &id.encode(), true).await.expect("json"))
                .expect("json");
        assert_eq!(json["state"], state, "case {n}: {json}");
        assert_eq!(json["next"]["kind"], kind, "case {n}: {json}");
        assert_eq!(json["next"]["command"].as_str(), command, "case {n}");
        if state == "waiting" {
            assert_eq!(json["chain_root"], serde_json::Value::Null, "case {n}");
            assert_eq!(json["blocked_on"][0]["holder_pid"], 999, "case {n}: {json}");
        }
        // `argv` is what an agent runs: the recorded argv for a rerun (program
        // as recorded), `heph …` otherwise; `cwd` where it depends on one.
        let (want_argv, want_cwd): (serde_json::Value, serde_json::Value) = match kind {
            "rerun" => (
                serde_json::json!(["/bin/heph", "run", "//a:b"]),
                serde_json::json!("/w/pacu/sub"),
            ),
            "none" => (serde_json::Value::Null, serde_json::Value::Null),
            "inspect_failure" | "list" => (
                serde_json::json!(command.expect("command").split(' ').collect::<Vec<_>>()),
                serde_json::json!("/w/pacu/sub"),
            ),
            _ => (
                serde_json::json!(command.expect("command").split(' ').collect::<Vec<_>>()),
                serde_json::Value::Null,
            ),
        };
        assert_eq!(json["next"]["argv"], want_argv, "case {n}: {json}");
        assert_eq!(json["next"]["cwd"], want_cwd, "case {n}: {json}");
        let reason = json["next"]["reason"].as_str().expect("reason");
        let out = run_show(&sources, &id.encode(), false).await.expect("show");
        let want = format!("\nNext: {}\n  {reason}\n", command.unwrap_or("nothing"));
        assert!(out.ends_with(&want), "case {n}: {out}");
        // Item 11: a quiet session says how long, and what is still open.
        if kind == "inflight" {
            assert_eq!(json["state"], "quiet");
            assert!(
                out.contains("\nno events for 1m01s; 0 targets still running\n"),
                "{out}"
            );
        }
    }
}

/// A rerun of an argument with a space and a quote: `argv` is exact, and the
/// `Next:` line is shell-quoted so it pastes back into the same words.
#[tokio::test]
async fn next_rerun_argv_is_exact_and_its_line_is_quoted() {
    let id = SessionId::new(T0, 1);
    let argv = ["/opt/bin/heph", "run", "//go:test x", "it's"];
    let source = FakeSource {
        sessions: vec![(
            entry(meta(id, 1, &argv), false, Some(ended(Some(1), NOW))),
            Vec::new(),
        )],
    };
    let sources: [&dyn SessionSource; 1] = [&source];
    let json: serde_json::Value =
        serde_json::from_str(&run_show(&sources, &id.encode(), true).await.expect("json"))
            .expect("json");
    assert_eq!(json["next"]["kind"], "rerun", "{json}");
    assert_eq!(json["next"]["argv"], serde_json::json!(argv), "{json}");
    assert_eq!(json["next"]["cwd"], "/w/pacu/sub", "{json}");
    let command = json["next"]["command"].as_str().expect("command");
    assert_eq!(command, r#"heph run '//go:test x' "it's""#);
    // The line reads back into the recorded words (the program by its name).
    assert_eq!(
        shlex::split(command).expect("a shell reads it"),
        ["heph", "run", "//go:test x", "it's"]
    );
    let out = run_show(&sources, &id.encode(), false).await.expect("show");
    assert!(out.contains(&format!("\nNext: {command}\n")), "{out}");
}

/// A lock cycle: A waits on a lock B holds, and B on one A holds. Showing
/// either names the other as the chain root and points `next` at it; the
/// chain stops at the cycle after one hop instead of going round.
#[tokio::test]
async fn a_lock_cycle_keeps_the_chain_root_and_next_bounded() {
    let a = SessionId::new(T0, 1);
    let b = SessionId::new(T0 + 5_000, 2);
    let (a_pid, b_pid) = (100, 200);
    // `status.json` (what `list` and the chain walk read) and the event log
    // (what `show` folds for the session shown) say the same wait.
    let waits_on = |addr: &str, holder: u32| {
        let status = SessionSnapshot {
            last_event_unix_ms: Some(NOW - 1_000),
            waiting: vec![addr.to_owned()],
            lock_waits: vec![LockWaitEntry {
                addr: addr.to_owned(),
                holder_pid: Some(holder),
                in_use_by_readers: false,
                since_unix_ms: NOW - 30_000,
            }],
            ..SessionSnapshot::default()
        };
        let records = vec![record(
            NOW - 30_000,
            BuildEventKind::ResultLockWaitStart {
                addr: addr.into(),
                holder_pid: Some(holder),
                in_use_by_readers: false,
            },
        )];
        (status, records)
    };
    let (a_status, a_records) = waits_on("//go:sdk", b_pid);
    let (b_status, b_records) = waits_on("//go:base", a_pid);
    let source = FakeSource {
        sessions: vec![
            (
                entry(
                    meta(a, a_pid, &["heph", "run", "//a:x"]),
                    true,
                    Some(a_status),
                ),
                a_records,
            ),
            (
                entry(
                    meta(b, b_pid, &["heph", "run", "//b:x"]),
                    true,
                    Some(b_status),
                ),
                b_records,
            ),
        ],
    };
    let sources: [&dyn SessionSource; 1] = [&source];
    for (shown, other) in [(a, b), (b, a)] {
        let shown_id = shown.encode();
        let run = run_show(&sources, &shown_id, true);
        let json: serde_json::Value = serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(10), run)
                .await
                .expect("show on a cycle returns")
                .expect("json"),
        )
        .expect("json");
        assert_eq!(json["state"], "waiting", "{json}");
        assert_eq!(json["chain_root"], other.encode(), "{json}");
        assert_eq!(
            json["blocked_on"][0]["holder_session"],
            other.encode(),
            "{json}"
        );
        assert_eq!(json["next"]["kind"], "show_holder", "{json}");
        let argv = ["heph", "session", "show", &other.encode()].map(str::to_owned);
        assert_eq!(json["next"]["argv"], serde_json::json!(argv), "{json}");
        assert_eq!(json["next"]["command"], argv.join(" "), "{json}");

        let out = run_show(&sources, &shown.encode(), false)
            .await
            .expect("show");
        assert_eq!(
            out.matches("    which waits for ").count(),
            1,
            "one hop, then the cycle: {out}"
        );
        assert!(out.contains(" — a cycle\n"), "{out}");
        assert!(
            out.ends_with(&format!(
                "\nNext: heph session show {other}\n  what the session holding the lock is doing\n"
            )),
            "{out}"
        );
    }
}

#[tokio::test]
async fn ids_resolve_prefix_suffix_pid() {
    // Two seconds apart: they share their leading characters.
    let a = SessionId::new(T0, 7);
    let b = SessionId::new(T0 + 2_000, 200);
    let (ae, be) = (a.encode(), b.encode());
    assert_eq!(ae.get(..4), be.get(..4), "the ids share a prefix");
    let source = FakeSource {
        sessions: vec![
            (
                entry(meta(a, 4242, &["heph", "run"]), true, None),
                Vec::new(),
            ),
            (
                entry(
                    meta(b, 5151, &["heph", "run"]),
                    false,
                    Some(ended(Some(0), NOW)),
                ),
                Vec::new(),
            ),
        ],
    };
    let sources: [&dyn SessionSource; 1] = [&source];
    let resolve_id =
        |arg: String| async move { resolve(&sources, &arg, opts()).await.map(|(_, e)| e.id) };

    assert_eq!(resolve_id(ae.clone()).await.expect("exact"), a);
    assert_eq!(resolve_id(ae.to_uppercase()).await.expect("any case"), a);
    let common = ae.get(..4).expect("prefix").to_owned();
    let err = resolve_id(common.clone()).await.expect_err("ambiguous");
    let msg = format!("{err:#}");
    assert!(msg.contains("matches 2 sessions"), "{msg}");
    assert!(msg.contains(&ae) && msg.contains(&be), "lists both: {msg}");
    assert!(
        msg.contains("Give more of the id (or its last characters)."),
        "points at the suffix: {msg}"
    );
    // The first character after the shared prefix tells them apart.
    let split = ae
        .chars()
        .zip(be.chars())
        .position(|(x, y)| x != y)
        .expect("ids differ");
    let unique = ae.get(..=split).expect("prefix").to_owned();
    assert_eq!(resolve_id(unique).await.expect("unique prefix"), a);
    let suffix = be.get(be.len() - 3..).expect("suffix").to_owned();
    assert!(!ae.ends_with(&suffix), "the suffix is unique");
    assert_eq!(resolve_id(suffix).await.expect("unique suffix"), b);

    assert_eq!(resolve_id("4242".to_owned()).await.expect("live pid"), a);
    let err = resolve_id("5151".to_owned())
        .await
        .expect_err("an ended session's pid is not an id");
    assert!(format!("{err:#}").contains("no session 5151"), "{err:#}");
    // A well-formed id that is gone: retention is the likely reason, named.
    let gone = SessionId::new(T0 - 30 * 86_400_000, 9).encode();
    let err = resolve_id(gone.clone()).await.expect_err("gone");
    assert_eq!(
        format!("{err:#}"),
        format!(
            "no session {gone} (`heph session list --all` lists them): an ended recording is \
             removed once it is older than 7d00h (`session.keep`), or sooner when all of them \
             together pass 2.0 GiB (`session.keepBytes`); see docs/SESSIONS.md#retention"
        )
    );
    let err = resolve_id("zz-not!".to_owned())
        .await
        .expect_err("not an id");
    assert!(format!("{err:#}").contains("no session"), "{err:#}");
}

#[tokio::test]
async fn empty_events_say_no_events_recorded() {
    let id = SessionId::new(T0, 1);
    let source = FakeSource {
        sessions: vec![(
            entry(
                meta(id, 1, &["heph", "run", "//a:b"]),
                false,
                Some(ended(Some(0), NOW)),
            ),
            Vec::new(),
        )],
    };
    let sources: [&dyn SessionSource; 1] = [&source];
    let out = run_show(&sources, &id.encode(), false).await.expect("show");
    assert!(out.contains("\nno events recorded\n"), "{out}");
}

/// The `heph session show` clause of `standing_yes_covers_later_requests`:
/// a Y (or N) the run's terminal set is printed, and in `--json`, for as
/// long as it acts; a build with none, and a build that is over, print none.
#[tokio::test]
async fn show_prints_the_standing_decision() {
    use crate::tui::session::ApprovalStanding;
    let approval = |at: u64, standing| {
        SessionRecord::session(
            at,
            SessionRecordKind::Approval {
                prompt: None,
                standing,
            },
        )
    };
    let running = SessionSnapshot {
        last_event_unix_ms: Some(NOW - 1_000),
        ..SessionSnapshot::default()
    };
    for (standing, line, json) in [
        (
            Some(ApprovalStanding::Approve),
            Some("auto-approving (Y) · approval requests are approved without a prompt"),
            Some("\"approval_standing\":\"approve\""),
        ),
        (
            Some(ApprovalStanding::Reject),
            Some("auto-rejecting (N) · approval requests are rejected (their targets fail)"),
            Some("\"approval_standing\":\"reject\""),
        ),
        (None, None, None),
    ] {
        let id = SessionId::new(T0, 1);
        let mut records = build_records("//a:gated", T0, None);
        records.push(approval(T0 + 20, standing));
        let source = FakeSource {
            sessions: vec![(
                entry(
                    meta(id, 1, &["heph", "run", "//a:gated"]),
                    true,
                    Some(running.clone()),
                ),
                records.clone(),
            )],
        };
        let sources: [&dyn SessionSource; 1] = [&source];
        let out = run_show(&sources, &id.encode(), false).await.expect("show");
        let out_json = run_show(&sources, &id.encode(), true).await.expect("show");
        match (line, json) {
            (Some(line), Some(json)) => {
                assert!(out.contains(line), "{standing:?}: {out}");
                assert!(out_json.contains(json), "{standing:?}: {out_json}");
            }
            _ => {
                assert!(!out.contains("auto-"), "{out}");
                assert!(!out_json.contains("approval_standing"), "{out_json}");
            }
        }

        // The build ended: the decision acts on nothing any more.
        records.push(SessionRecord::session(
            T0 + 50,
            SessionRecordKind::Finished { stopped: false },
        ));
        records.extend(build_records("//a:gated", T0 + 60, Some(0)).pop());
        let source = FakeSource {
            sessions: vec![(
                entry(
                    meta(id, 1, &["heph", "run", "//a:gated"]),
                    false,
                    Some(ended(Some(0), T0 + 1_060)),
                ),
                records,
            )],
        };
        let sources: [&dyn SessionSource; 1] = [&source];
        let out = run_show(&sources, &id.encode(), false).await.expect("show");
        assert!(!out.contains("auto-"), "{standing:?}, ended: {out}");
    }
}

/// U3: `status.json` keeps the first entries of each list, with the full count;
/// `show` folds the event log and has them all.
#[tokio::test]
async fn status_lists_are_capped_and_show_has_them_all() {
    const N: usize = 50_000;
    let id = SessionId::new(T0, 1);
    let m = meta(id, 1, &["heph", "run", "//..."]);
    let mut records = Vec::with_capacity(3 * N);
    for i in 0..N {
        let addr = format!("//f:t{i:05}");
        records.push(record(
            T0 + 1,
            BuildEventKind::ResultStart { addr: addr.clone() },
        ));
        records.push(record(
            T0 + 2,
            BuildEventKind::ResultEnd {
                left_behind: None,
                spans: Vec::new(),
                cancelled: false,
                addr,
                error: Some("boom".to_owned()),
                upstream_of: None,
                exit_status: None,
                exit_code: None,
                log_tail: None,
                timed_out: None,
            },
        ));
    }
    for i in 0..N {
        records.push(record(
            T0 + 3,
            BuildEventKind::AdmissionWaitStart {
                addr: format!("//w:t{i:05}"),
                pool: "cpu".to_owned(),
                need: 1,
                free: 0,
                ahead: 0,
                removals_pending: None,
                yielding_to: None,
            },
        ));
    }
    use crate::tui::TUIAppView as _;
    // What the recorder writes: the same fold, capped.
    let mut view = show::new_view(&m);
    for r in &records {
        if let Some(ev) = r.build_event() {
            view.apply(&ev);
        }
    }
    let status = view
        .state()
        .snapshot_capped(crate::tui::session::STATUS_LIST_CAP);
    let bytes = serde_json::to_vec(&status).expect("encode");
    assert!(
        bytes.len() < 32 * 1024,
        "status.json stays small: {} bytes",
        bytes.len()
    );
    assert_eq!(status.waiting.len(), 100);
    assert_eq!(status.failed.len(), 100);
    assert_eq!(status.admission_waits.len(), 100);
    assert_eq!(status.totals.waiting.total, N as u64);
    assert!(status.totals.waiting.truncated);
    assert_eq!(status.totals.failed.total, N as u64);
    assert!(status.totals.failed.truncated);
    assert_eq!(status.totals.admission_waits.total, N as u64);
    assert!(!status.totals.lock_waits.truncated);
    // The first entries: failure order, and sorted waits.
    assert_eq!(status.failed[0], "//f:t00000");
    assert_eq!(status.waiting[99], "//w:t00099");
    // It reads back, the counts intact.
    let decoded = SessionSnapshot::decode(&bytes).expect("decode");
    assert_eq!(decoded.counts.failed, N as u64);

    let source = FakeSource {
        sessions: vec![(entry(m, true, Some(decoded)), records)],
    };
    let sources: [&dyn SessionSource; 1] = [&source];
    // `list` reads only status.json, and still sees every waiter.
    let listed: serde_json::Value =
        serde_json::from_str(&run_list(&sources, Workspace::All, true).await).expect("json");
    assert_eq!(listed[0]["state"], "waiting");

    let out = run_show(&sources, &id.encode(), false).await.expect("show");
    assert!(out.contains(&format!("{N} failed")), "{out}");
    assert!(
        out.contains(&format!("… and {} more", N - 10)),
        "every admission wait is counted:\n{out}"
    );
    let json: serde_json::Value =
        serde_json::from_str(&run_show(&sources, &id.encode(), true).await.expect("json"))
            .expect("json");
    assert_eq!(json["waiting"].as_array().expect("waiting").len(), N);
    assert_eq!(json["failed"].as_array().expect("failed").len(), N);
    assert_eq!(json["totals"]["failed"]["total"], N);
    assert_eq!(json["totals"]["failed"]["truncated"], false);
}

#[tokio::test]
async fn older_viewer_warns_on_newer_heph() {
    for (recorded, viewer, newer) in [
        ("v0.49.0", "v0.48.1", true),
        ("v1.0.0-build.5.1464+g36e65abc", "v0.48.1-build.2", true),
        ("v0.48.1-build.9", "v0.48.1-build.2", false),
        ("v0.48.1", "v0.48.1", false),
        ("v0.47.9", "v0.48.1", false),
        ("v0.0.0-dev", "v0.48.1", false),
        ("v9.0.0", "v0.0.0-dev", false),
        ("garbage", "v0.48.1", false),
        ("v1.2", "v0.48.1", false),
    ] {
        assert_eq!(
            recorded_by_newer(recorded, viewer),
            newer,
            "{recorded} vs {viewer}"
        );
    }

    let id = SessionId::new(T0, 1);
    let mut m = meta(id, 1, &["heph", "run"]);
    m.heph_version = "v0.99.0".to_owned();
    let source = FakeSource {
        sessions: vec![(entry(m, false, Some(ended(Some(0), NOW))), Vec::new())],
    };
    let sources: [&dyn SessionSource; 1] = [&source];
    let out = run_show(&sources, &id.encode(), false).await.expect("show");
    assert!(
        out.contains("recorded by a newer heph (v0.99.0), may be incomplete"),
        "{out}"
    );
}

#[test]
fn exit_code_mapping() {
    let own: anyhow::Error = SessionExit::own(anyhow::anyhow!("no session x")).into();
    assert_eq!(exit_code(&own), Some(125));
    assert_eq!(exit_code(&SessionExit::mirrored(3).into()), Some(3));
    assert_eq!(exit_code(&SessionExit::mirrored(300).into()), Some(125));
    assert_eq!(exit_code(&anyhow::anyhow!("not ours")), None);

    let wrapped = wrap_exit(Err(anyhow::anyhow!("ambiguous"))).expect_err("err");
    assert_eq!(exit_code(&wrapped), Some(125));
    let kept = wrap_exit(Err(SessionExit::mirrored(7).into())).expect_err("err");
    assert_eq!(exit_code(&kept), Some(7));
    assert!(wrap_exit(Ok(())).is_ok());

    let id = SessionId::new(T0, 1);
    assert!(outcome_result(id, Outcome::Detached).is_ok());
    assert!(outcome_result(id, Outcome::Ended(Some(0))).is_ok());
    let code = |o| outcome_result(id, o).map_err(|e| exit_code(&e));
    assert_eq!(code(Outcome::Ended(Some(2))), Err(Some(2)));
    assert_eq!(code(Outcome::Ended(None)), Err(Some(125)));
    assert_eq!(code(Outcome::Lost), Err(Some(125)));

    // U1: a 125 that is not this command's own failure says why on stderr.
    let note = |o| attach::exit_note(id, o);
    assert_eq!(
        note(Outcome::Ended(Some(125))),
        Some(format!("heph: session {id} exited 125\n"))
    );
    assert_eq!(code(Outcome::Ended(Some(125))), Err(Some(125)));
    for weird in [-1, 256, i32::MIN] {
        assert_eq!(
            note(Outcome::Ended(Some(weird))),
            Some(format!(
                "heph: session {id} exited {weird}, not an exit status (0-255); exiting 125\n"
            )),
        );
        assert_eq!(code(Outcome::Ended(Some(weird))), Err(Some(125)));
    }
    for quiet in [
        Outcome::Ended(Some(0)),
        Outcome::Ended(Some(2)),
        Outcome::Ended(Some(124)),
        Outcome::Ended(Some(255)),
        // These carry their reason in the error itself.
        Outcome::Ended(None),
        Outcome::Lost,
        Outcome::Detached,
    ] {
        assert_eq!(note(quiet), None, "{quiet:?}");
    }
}

/// `attach` on the line backend follows to the end and mirrors the exit code;
/// a lost session is 125.
#[tokio::test]
async fn attach_mirrors_the_session_exit() {
    let done = SessionId::new(T0, 1);
    let lost = SessionId::new(T0 + 1_000, 2);
    let mut records = build_records("//a:b", T0, Some(3));
    records.insert(
        0,
        SessionRecord::session(
            T0,
            SessionRecordKind::Log {
                level: "INFO".to_owned(),
                msg: "hello \u{1b}[2J".to_owned(),
                seq: 0,
            },
        ),
    );
    let source = FakeSource {
        sessions: vec![
            (entry(meta(done, 1, &["heph", "run"]), false, None), records),
            (
                entry(meta(lost, 2, &["heph", "run"]), false, None),
                build_records("//a:b", T0, None),
            ),
        ],
    };
    let sources: [&dyn SessionSource; 1] = [&source];
    let slot = std::sync::Arc::new(crate::tui::session::SessionSlot::new());
    let run = |id: SessionId| {
        let sources = &sources;
        let slot = &slot;
        async move {
            attach::attach(
                sources,
                opts(),
                &id.encode(),
                crate::tui::LogSink::new_direct_with(std::sync::Arc::clone(slot)),
                slot,
                &crate::commands::GlobalOptions {
                    no_tui: true,
                    ..Default::default()
                },
                &no_opener(),
            )
            .await
        }
    };
    let err = run(done).await.expect_err("exit 3");
    assert_eq!(exit_code(&err), Some(3));
    let err = run(lost).await.expect_err("lost");
    assert_eq!(exit_code(&err), Some(125));
}

/// The detach listener is armed when `trigger_on` returns, not when its task
/// is first polled: the signal is raised on this current-thread runtime
/// before it yields, so the task cannot have run yet. With the listener
/// created inside the task (what `ctrl_c()` amounts to), the signal keeps its
/// default action and kills the test binary. SIGALRM stands in for SIGINT
/// because nothing else in this binary listens for it, and a SIGINT would
/// detach every `attach` running in parallel in this process.
#[tokio::test]
async fn detach_listener_is_armed_before_its_task_runs() {
    let (trigger, mut rx) = hcore::shutdown::ShutdownTrigger::new();
    let task = trigger_on(tokio::signal::unix::SignalKind::alarm(), trigger)
        .expect("listening for SIGALRM");
    // SAFETY: `raise` only delivers a signal to this thread; the handler was
    // installed by `trigger_on` above.
    let rc = unsafe { libc::raise(libc::SIGALRM) };
    assert_eq!(rc, 0, "raise(SIGALRM)");
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("the signal raised before the task ran still pulls the trigger")
        .expect("trigger alive");
    task.abort();
}

/// Attach's own wiring of the approval view: a recorded session with a
/// pending request, followed from its directory into the viewer's feed, and
/// the viewer built as `attach` builds it. With the run's home recorded, the
/// view reads the notice from the run's store (here a workspace source file)
/// by the sources the session recorded, through the engine's reader; with
/// another home recorded, the readers are unavailable and the notice reads
/// "notice unavailable", never its text.
#[tokio::test(flavor = "multi_thread")]
async fn attach_reads_a_pending_notice_from_the_run_home() {
    use crate::tui::session::{ApprovalPrompt, ApprovalPromptNotice, ApprovalQueued, FileSource};
    use crate::tui::{App as _, TUIAppView as _};

    const NOTICE: &str = "drop the table";
    let ws = tempfile::tempdir().expect("tempdir");
    std::fs::write(ws.path().join(".hephconfig"), "").expect("write .hephconfig");
    let global = crate::commands::GlobalOptions::default();
    let (_engine, home) =
        crate::commands::bootstrap::new_reader_engine(&global, ws.path()).expect("reader engine");
    std::fs::write(ws.path().join("PLAN"), NOTICE).expect("write the notice source");
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(NOTICE.as_bytes());
    hasher.update(&[0]);
    let sources = vec![hcore::approval_notice::NoticeSource {
        addr: "//a:plan".to_owned(),
        origin: hcore::approval_notice::NoticeOrigin::File {
            path: "PLAN".to_owned(),
            out_path: "PLAN".to_owned(),
            x: false,
            hashout: format!("{:x}", hasher.digest()),
        },
    }];
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let sessions = tempfile::tempdir().expect("tempdir");

    for (pid, recorded_home, shows) in [
        (1_u32, home.as_path(), NOTICE),
        (2, elsewhere.path(), "notice unavailable"),
    ] {
        let id = SessionId::new(T0 + u64::from(pid), 7);
        let mut m = meta(id, pid, &["heph", "run"]);
        m.interactive = true;
        m.workspace_root = Some(ws.path().display().to_string());
        m.home_dir = Some(recorded_home.display().to_string());
        let prompt = ApprovalPrompt {
            queue: vec![ApprovalQueued {
                seq: 1,
                addr: "//a:gate".to_owned(),
                queued_at_unix_ms: T0,
                notices: vec![ApprovalPromptNotice {
                    name: "plan".to_owned(),
                    sources: sources.clone(),
                }],
            }],
        };
        let records = [SessionRecord::session(
            T0,
            SessionRecordKind::Approval {
                prompt: Some(Box::new(prompt)),
                standing: None,
            },
        )];
        let _live = write_live(sessions.path(), &m, &records, None);

        let source = FileSource::new(sessions.path());
        let (_detach_tx, detach_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = attach::AttachApp::new(
            &global,
            m,
            attach::Origin::Local { live: true },
            source.holder_names(id),
            futures::stream::empty().boxed(),
            detach_rx,
            true,
        );
        // What `run` does with each record, up to the end of what is recorded.
        let mut follow = source.subscribe(id).await.expect("subscribe");
        while let Some(item) = follow.next().await {
            match item.expect("item") {
                SessionItem::Record(r) => drop(app.feed().route(*r)),
                SessionItem::CaughtUp { .. } => break,
                SessionItem::Ended { .. } => panic!("the session is live"),
            }
        }

        let mut view = app.tui_view();
        assert!(view.approval_open(), "a pending request opens");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let frame: String = view
                .render_full(NOW, 120, 40)
                .iter()
                .map(|l| {
                    let mut row: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                    row.push('\n');
                    row
                })
                .collect();
            if frame.contains(shows) {
                assert!(
                    pid == 1 || !frame.contains(NOTICE),
                    "another home never shows the notice:\n{frame}"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{shows:?} never showed (home {}):\n{frame}",
                recorded_home.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[test]
fn control_bytes_escaped() {
    assert_eq!(escape("plain"), "plain");
    assert_eq!(
        escape("a\u{1b}[1;31mred\u{1b}[0m b\u{7}c"),
        "ared b\\u{7}c",
        "SGR is dropped, BEL escaped"
    );
    assert_eq!(
        escape("\u{1b}[2J\u{1b}]0;title\u{7}"),
        "\\u{1b}[2J\\u{1b}]0;title\\u{7}",
        "every other escape is escaped"
    );
    assert_eq!(escape("a\nb\tc\r"), "a\\nb\\tc\\r");
    assert_eq!(escape_text("a\nb\tc\r\u{9b}"), "a\nb\tc\\r\\u{9b}");
    assert_eq!(escape("\u{1b}[31"), "\\u{1b}[31", "an unterminated SGR");
    assert_eq!(escape("\u{1b}"), "\\u{1b}");
}

/// What `attach` replays from the run's terminal keeps its colours on a
/// viewer that takes colour, and nothing else that would move the viewer's
/// cursor, clear its screen or retitle it.
#[test]
fn replayed_text_keeps_only_sgr() {
    const ESC: &str = "\u{1b}";
    // (name, input, on a colour sink, on a no-colour sink)
    let table: &[(&str, &str, &str, &str)] = &[
        (
            "valid SGR",
            "a\u{1b}[31mred\u{1b}[0m\n",
            "a\u{1b}[31mred\u{1b}[0m\n",
            "ared\n",
        ),
        (
            "SGR with many params",
            "\u{1b}[1;4;38;2;255;128;0;48:5:17mx\u{1b}[m",
            "\u{1b}[1;4;38;2;255;128;0;48:5:17mx\u{1b}[m",
            "x",
        ),
        ("truncated ESC[", "a\u{1b}[", "a\\u{1b}[", "a\\u{1b}["),
        (
            "truncated SGR params",
            "a\u{1b}[31",
            "a\\u{1b}[31",
            "a\\u{1b}[31",
        ),
        ("clear screen", "\u{1b}[2J", "\\u{1b}[2J", "\\u{1b}[2J"),
        (
            "cursor move",
            "\u{1b}[10;5H",
            "\\u{1b}[10;5H",
            "\\u{1b}[10;5H",
        ),
        (
            "OSC title, BEL-terminated",
            "\u{1b}]0;pwned\u{7}",
            "\\u{1b}]0;pwned\\u{7}",
            "\\u{1b}]0;pwned\\u{7}",
        ),
        (
            "OSC title, ST-terminated",
            "\u{1b}]0;pwned\u{1b}\\",
            "\\u{1b}]0;pwned\\u{1b}\\",
            "\\u{1b}]0;pwned\\u{1b}\\",
        ),
        (
            "alternate screen",
            "\u{1b}[?1049h",
            "\\u{1b}[?1049h",
            "\\u{1b}[?1049h",
        ),
        ("lone ESC", "a\u{1b}", "a\\u{1b}", "a\\u{1b}"),
        (
            "C1 CSI, as an SGR would be",
            "\u{9b}31mx",
            "\\u{9b}31mx",
            "\\u{9b}31mx",
        ),
        (
            "non-ASCII between SGRs",
            "\u{1b}[1mé ✓ 日本\u{1b}[0m\t→",
            "\u{1b}[1mé ✓ 日本\u{1b}[0m\t→",
            "é ✓ 日本\t→",
        ),
        (
            "SGR next to an escaped sequence",
            "\u{1b}[31m\u{1b}[2Jx\r",
            "\u{1b}[31m\\u{1b}[2Jx\\r",
            "\\u{1b}[2Jx\\r",
        ),
        (
            "intermediate byte before the final m",
            "\u{1b}[31 mx",
            "\\u{1b}[31 mx",
            "\\u{1b}[31 mx",
        ),
        (
            "private marker",
            "\u{1b}[<31mx",
            "\\u{1b}[<31mx",
            "\\u{1b}[<31mx",
        ),
        ("DEL", "a\u{7f}b", "a\\u{7f}b", "a\\u{7f}b"),
        ("NUL", "a\u{0}b", "a\\u{0}b", "a\\u{0}b"),
        (
            "right-to-left override",
            "ok\u{202e}txt.exe",
            "ok\\u{202e}txt.exe",
            "ok\\u{202e}txt.exe",
        ),
        (
            "bidi isolate",
            "a\u{2066}b\u{2069}",
            "a\\u{2066}b\\u{2069}",
            "a\\u{2066}b\\u{2069}",
        ),
        (
            "left-to-right mark",
            "a\u{200e}b",
            "a\\u{200e}b",
            "a\\u{200e}b",
        ),
        (
            "right-to-left mark",
            "a\u{200f}b",
            "a\\u{200f}b",
            "a\\u{200f}b",
        ),
        (
            "arabic letter mark",
            "a\u{061c}b",
            "a\\u{61c}b",
            "a\\u{61c}b",
        ),
        (
            // A zero-width joiner shapes text; it reorders nothing.
            "zero-width joiner is kept",
            "a\u{200d}b",
            "a\u{200d}b",
            "a\u{200d}b",
        ),
    ];
    for &(name, input, color, plain) in table {
        assert_eq!(escape_replayed(input, true), color, "{name}, colour sink");
        assert_eq!(
            escape_replayed(input, false),
            plain,
            "{name}, no-colour sink"
        );
        // Whatever is kept, no ESC survives that is not the start of an SGR.
        let kept = escape_replayed(input, true);
        for (i, _) in kept.match_indices(ESC) {
            let tail = kept.get(i + 1..).unwrap_or_default();
            let params = tail.strip_prefix('[').map(|t| {
                t.trim_start_matches(|c: char| c.is_ascii_digit() || c == ';' || c == ':')
            });
            assert!(
                params.is_some_and(|t| t.starts_with('m')),
                "{name}: a raw ESC that is not an SGR in {kept:?}"
            );
        }
    }
    // A no-colour sink gets exactly what `escape_text` gave before.
    for &(name, input, _, plain) in table {
        assert_eq!(escape_text(input), plain, "{name}");
    }
}

/// An SGR's parameter list is kept up to 128 bytes. Past that it is no colour
/// a terminal draws: the ESC is escaped and the rest prints as text, on either
/// sink. A megabyte of parameters is the same, and costs one bounded scan.
#[test]
fn oversized_sgr_parameters_are_escaped() {
    let at_cap = "1;".repeat(64).trim_end_matches(';').to_owned() + "0";
    assert_eq!(at_cap.len(), 128);
    let sgr = format!("\u{1b}[{at_cap}mx");
    assert_eq!(escape_replayed(&sgr, true), sgr, "128 bytes: kept");
    assert_eq!(escape_replayed(&sgr, false), "x", "128 bytes: dropped");

    for params in [format!("{at_cap};"), "1;".repeat(512 * 1024)] {
        let input = format!("\u{1b}[{params}mx");
        let escaped = format!("\\u{{1b}}[{params}mx");
        assert_eq!(
            escape_replayed(&input, true),
            escaped,
            "{} bytes",
            params.len()
        );
        assert_eq!(
            escape_replayed(&input, false),
            escaped,
            "{} bytes",
            params.len()
        );
    }

    // Many sequences each just over the cap: still linear, every one escaped.
    let over = format!("\u{1b}[{at_cap};m");
    let input = over.repeat(8 * 1024);
    let out = escape_replayed(&input, true);
    assert!(!out.contains('\u{1b}'));
    assert_eq!(out.matches("\\u{1b}[").count(), 8 * 1024);
}

#[tokio::test]
async fn shown_text_never_carries_control_bytes() {
    let id = SessionId::new(T0, 1);
    let m = meta(id, 1, &["heph", "run", "//a:\u{1b}]0;pwned\u{7}"]);
    let source = FakeSource {
        sessions: vec![(
            entry(m, false, Some(ended(Some(1), NOW))),
            build_records("//a:\u{1b}[2Jb", T0, Some(1)),
        )],
    };
    let sources: [&dyn SessionSource; 1] = [&source];
    let out = run_show(&sources, &id.encode(), false).await.expect("show");
    assert!(!out.contains('\u{1b}') && !out.contains('\u{7}'), "{out:?}");
    let table = run_list(&sources, Workspace::All, false).await;
    assert!(
        !table.contains('\u{1b}') && !table.contains('\u{7}'),
        "{table:?}"
    );
}

// ---- Against the real files ----

/// A live session as its owner holds it, without a recorder: the recording
/// open and the lock taken, for as long as this lives.
struct Live {
    _recording: Recording,
    _lock: std::fs::File,
}

/// Write a live session's directory under `root`.
fn write_live(
    root: &Path,
    meta: &SessionMeta,
    records: &[SessionRecord],
    status: Option<&SessionSnapshot>,
) -> Live {
    let dir = root.join(meta.id.encode());
    let recording = Recording::create(&dir, meta, records, status).expect("recording");
    let lock = hold_lock(&dir);
    Live {
        _recording: recording,
        _lock: lock,
    }
}

/// Write an ended session's directory under `root`, sealed with `status`'s
/// end, as its owner leaves it.
fn write_ended(
    root: &Path,
    meta: &SessionMeta,
    records: &[SessionRecord],
    status: &SessionSnapshot,
) -> PathBuf {
    let end = status.end.as_ref().expect("an ended status");
    Recording::create(&root.join(meta.id.encode()), meta, records, Some(status))
        .expect("recording")
        .seal(end.at_unix_ms, end.exit_code, end.panicked)
        .expect("seal")
}

/// Hold `dir`'s lock as a live owner does.
fn hold_lock(dir: &Path) -> std::fs::File {
    let f = std::fs::File::open(dir.join(files::LOCK)).expect("open lock");
    // SAFETY: `f` owns a valid fd for the call.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "lock taken");
    f
}

/// Every path under `root` with its size, mtime and mode.
fn tree(root: &Path) -> BTreeMap<PathBuf, (u64, std::time::SystemTime, u32)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let md = std::fs::symlink_metadata(&p).expect("stat");
        out.insert(
            p.clone(),
            (
                md.len(),
                md.modified().expect("mtime"),
                md.permissions().mode(),
            ),
        );
        if md.is_dir() {
            for e in std::fs::read_dir(&p).expect("read_dir") {
                stack.push(e.expect("entry").path());
            }
        }
    }
    out
}

fn chmod_tree(root: &Path, dir_mode: u32, file_mode: u32) {
    for p in tree(root).keys() {
        let mode = if p.is_dir() { dir_mode } else { file_mode };
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }
}

#[tokio::test]
async fn viewers_never_write() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let done = SessionId::new(T0, 1);
    let live = SessionId::new(T0 + 3_000, 2);
    write_ended(
        &root,
        &meta(done, 1, &["heph", "run", "//a:b"]),
        &build_records("//a:b", T0, Some(0)),
        &ended(Some(0), T0 + 1_000),
    );
    let _live = write_live(
        &root,
        &meta(live, 2, &["heph", "run", "//c:d"]),
        &build_records("//c:d", T0 + 3_000, None),
        Some(&SessionSnapshot {
            last_event_unix_ms: Some(NOW),
            ..SessionSnapshot::default()
        }),
    );
    chmod_tree(&root, 0o555, 0o444);
    let before = tree(&root);

    let file = crate::tui::session::FileSource::new(&root);
    let sources: [&dyn SessionSource; 1] = [&file];
    let table = run_list(&sources, Workspace::All, false).await;
    assert!(
        table.contains(&done.encode()) && table.contains(&live.encode()),
        "{table}"
    );
    run_list(&sources, Workspace::All, true).await;
    for id in [done, live] {
        let out = run_show(&sources, &id.encode(), false)
            .await
            .unwrap_or_else(|e| panic!("show {id}: {e:#}"));
        assert!(out.contains(&format!("session {id}")), "{out}");
        run_show(&sources, &id.encode(), true)
            .await
            .unwrap_or_else(|e| panic!("show --json {id}: {e:#}"));
    }

    let after = tree(&root);
    chmod_tree(&root, 0o755, 0o644);
    assert_eq!(before, after, "a viewer changed the sessions directory");
}

#[tokio::test]
async fn list_never_reads_events() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let id = SessionId::new(T0, 1);
    let dir = write_ended(
        &root,
        &meta(id, 1, &["heph", "run", "//a:b"]),
        &build_records("//a:b", T0, Some(0)),
        &ended(Some(0), T0 + 1_000),
    );
    // No records table: list reads the session and status rows only.
    testing::alter(&dir, "ALTER TABLE records RENAME TO gone").expect("hide the records");
    let file = crate::tui::session::FileSource::new(&root);
    let sources: [&dyn SessionSource; 1] = [&file];
    let table = run_list(&sources, Workspace::All, false).await;
    assert!(
        table.contains(&id.encode()) && table.contains("ok"),
        "{table}"
    );
    let json = run_list(&sources, Workspace::All, true).await;
    assert!(json.contains(&id.encode()), "{json}");
    // Show, which folds the records, fails.
    run_show(&sources, &id.encode(), false)
        .await
        .expect_err("show reads the records");
}

#[tokio::test]
async fn list_json_says_panicked() {
    let id = SessionId::new(T0, 1);
    let source = FakeSource {
        sessions: vec![(
            entry(
                meta(id, 1, &["heph", "run"]),
                false,
                Some(SessionSnapshot {
                    end: Some(SessionEnd {
                        at_unix_ms: NOW,
                        exit_code: Some(0),
                        panicked: true,
                    }),
                    ..SessionSnapshot::default()
                }),
            ),
            Vec::new(),
        )],
    };
    let sources: [&dyn SessionSource; 1] = [&source];
    let json: serde_json::Value =
        serde_json::from_str(&run_list(&sources, Workspace::All, true).await).expect("json");
    let s = &json[0];
    assert_eq!(s["panicked"], true, "{s}");
    assert_eq!(s["exit_code"], 0);
    assert_eq!(s["state"], "failed");
}

/// A status row this heph cannot read: `incomplete` is unknown (`null`), not
/// "complete" (`false`).
#[tokio::test]
async fn list_json_incomplete_is_null_when_the_status_is_unreadable() {
    let id = SessionId::new(T0, 1);
    let source = FakeSource {
        sessions: vec![(
            SessionEntry {
                status_unreadable: true,
                ..entry(meta(id, 1, &["heph", "run"]), false, None)
            },
            Vec::new(),
        )],
    };
    let sources: [&dyn SessionSource; 1] = [&source];
    let json: serde_json::Value =
        serde_json::from_str(&run_list(&sources, Workspace::All, true).await).expect("json");
    let s = &json[0];
    assert_eq!(s["state"], "unknown", "{s}");
    assert_eq!(s["incomplete"], serde_json::Value::Null, "{s}");
}

#[tokio::test]
async fn list_shows_newer_format_placeholder() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let id = SessionId::new(T0, 1);
    let dir = write_ended(
        &root,
        &meta(id, 1, &["heph", "run", "//a:b"]),
        &build_records("//a:b", T0, Some(0)),
        &ended(Some(0), T0 + 1_000),
    );
    testing::alter(&dir, "PRAGMA user_version=999").expect("a newer format");

    let file = crate::tui::session::FileSource::new(&root);
    let sources: [&dyn SessionSource; 1] = [&file];
    // Shown in any workspace's view: its workspace is unknown.
    let table = run_list(&sources, Workspace::Only("/w/pacu"), false).await;
    let row = table
        .lines()
        .find(|l| l.starts_with(&id.encode()))
        .unwrap_or_else(|| panic!("placeholder row missing:\n{table}"));
    assert!(row.contains("format 999, needs newer heph"), "{row}");
    assert!(row.contains("unknown"), "{row}");

    let json: serde_json::Value =
        serde_json::from_str(&run_list(&sources, Workspace::All, true).await).expect("json");
    let s = &json[0];
    assert_eq!(s["format"], 999);
    assert_eq!(s["state"], "unknown");
    assert_eq!(s["started_unix_ms"], T0);
    // Nothing more is read of it: no pid, no progress.
    assert_eq!(s["owner_pid"], serde_json::Value::Null);
    assert_eq!(s["progress"], serde_json::Value::Null);
    assert_eq!(s["incomplete"], serde_json::Value::Null);
    assert_eq!(s["panicked"], false);

    let err = run_show(&sources, &id.encode(), false)
        .await
        .expect_err("show refuses a newer format");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("format 999") && msg.contains("format 1"),
        "{msg}"
    );
}

/// The whole command line, as `main` parses it.
#[derive(clap::Parser)]
struct TestCli {
    #[command(flatten)]
    _global: crate::commands::global::GlobalOptions,
    #[command(subcommand)]
    cmd: crate::commands::Commands,
}

/// `-D` belongs to `run`: on `session attach` it parses only to be refused
/// with the reason, before anything is opened; elsewhere clap refuses it.
#[test]
fn attach_refuses_dashboard() {
    use clap::Parser as _;
    let attach = |argv: &[&str]| -> AttachArgs {
        let cli = TestCli::try_parse_from(argv).expect("parse");
        match cli.cmd {
            crate::commands::Commands::Session(SessionArgs {
                command: SessionCommands::Attach(a),
            }) => a,
            _ => panic!("not `session attach`"),
        }
    };
    for argv in [
        &["heph", "session", "attach", "-D", "02kf5"][..],
        &["heph", "session", "attach", "02kf5", "--dashboard"],
    ] {
        let err = attach(argv)
            .refuse_dashboard()
            .expect_err("attach -D is refused");
        assert!(
            format!("{err:#}").contains("--dashboard does not apply to `session attach`"),
            "{argv:?}: {err:#}"
        );
    }
    attach(&["heph", "session", "attach", "02kf5"])
        .refuse_dashboard()
        .expect("attach without -D");
    for argv in [
        &["heph", "session", "list", "-D"][..],
        &["heph", "-D", "run", "//p:t"],
    ] {
        assert!(
            TestCli::try_parse_from(argv).is_err(),
            "{argv:?}: -D is no global flag"
        );
    }
}

#[test]
fn inflight_record_keys_subset_of_fields() -> anyhow::Result<()> {
    output::check_keys(&InflightView {
        id: "id".to_owned(),
        report: "report",
    })
}

/// A `--jq` failure keeps the output layer's exit code instead of becoming
/// this command's own 125.
#[test]
fn jq_failure_keeps_its_exit_code() {
    let e = wrap_exit(Err(output::OutputError::JqRuntime("boom".to_owned()).into()))
        .expect_err("still an error");
    assert_eq!(output::exit_code(&e), Some(2));
    let e = wrap_exit(Err(anyhow::anyhow!("no such session"))).expect_err("an error");
    assert_eq!(exit_code(&e), Some(SessionExit::OWN_FAILURE));
}

/// D30: an ended session's `show` prints one line per bounded pool, and
/// `--json` a `pools` entry each, from the session-wide `heph.session.pool.*`
/// rows derived when it finished. A live session has none yet.
#[tokio::test]
async fn show_prints_pool_usage() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let pool = |at: u64, held: u64, waiting: u64| {
        record(
            at,
            BuildEventKind::PoolUsage {
                pool: "docker".into(),
                held,
                capacity: 4,
                waiting,
                removals_pending: 0,
            },
        )
    };
    let id = SessionId::new(T0, 1);
    let mut records = build_records("//a:b", T0, None);
    records.extend([pool(T0 + 1_000, 2, 1), pool(T0 + 2_000, 4, 0)]);
    records.push(SessionRecord::session(
        T0 + 3_000,
        SessionRecordKind::End {
            exit_code: Some(0),
            panicked: false,
        },
    ));
    write_ended(
        &root,
        &meta(id, 1, &["heph", "run", "//a:b"]),
        &records,
        &ended(Some(0), T0 + 3_000),
    );
    let live_id = SessionId::new(T0 + 10, 2);
    let _live = write_live(
        &root,
        &meta(live_id, 2, &["heph", "run", "//a:c"]),
        &[pool(T0 + 20, 1, 0)],
        None,
    );

    let file = crate::tui::session::FileSource::new(&root);
    let sources: [&dyn SessionSource; 1] = [&file];
    let out = run_show(&sources, &id.encode(), false).await.expect("show");
    let line = out
        .lines()
        .find(|l| l.starts_with("docker pool (capacity 4): held "))
        .unwrap_or_else(|| panic!("no pool line: {out}"));
    assert!(line.contains(" mean while in use ("), "{line}");
    assert!(
        line.ends_with(", 4 peak; something was waiting for 1s"),
        "{line}"
    );

    let json: serde_json::Value = serde_json::from_str(
        &run_show(&sources, &id.encode(), true)
            .await
            .expect("show --json"),
    )
    .expect("json");
    let pools = json["pools"].as_array().expect("pools");
    assert_eq!(pools.len(), 1, "{json}");
    let p = &pools[0];
    assert_eq!(p["pool"], "docker");
    assert_eq!(p["unit"], "1");
    assert_eq!(p["capacity"], 4.0);
    assert_eq!(p["held_peak"], 4.0);
    let mean = p["held_mean"].as_f64().expect("mean");
    assert!(mean > 2.0 && mean < 4.0, "time-weighted: {p}");
    assert_eq!(p["waiting_us"], 1_000_000, "one second with a waiter: {p}");

    let live: serde_json::Value = serde_json::from_str(
        &run_show(&sources, &live_id.encode(), true)
            .await
            .expect("show --json"),
    )
    .expect("json");
    assert_eq!(live["pools"], serde_json::json!([]), "{live}");
    let out = run_show(&sources, &live_id.encode(), false)
        .await
        .expect("show");
    assert!(!out.contains(" pool "), "{out}");
}

/// A recording cut at its size cap says so: "incomplete" in the text, and
/// `truncated` in `--json`.
#[tokio::test]
async fn show_says_incomplete_when_truncated() {
    let id = SessionId::new(T0, 1);
    let mut records = build_records("//a:b", T0, None);
    records.push(SessionRecord::session(
        T0 + 5_000,
        SessionRecordKind::Truncated,
    ));
    let source = FakeSource {
        sessions: vec![(
            entry(
                meta(id, 1, &["heph", "run", "//a:b"]),
                true,
                Some(SessionSnapshot {
                    last_event_unix_ms: Some(NOW - 1_000),
                    truncated: true,
                    ..SessionSnapshot::default()
                }),
            ),
            records,
        )],
    };
    let sources: [&dyn SessionSource; 1] = [&source];
    let out = run_show(&sources, &id.encode(), false).await.expect("show");
    // The cap, how far into the run, and what was dropped.
    assert!(
        out.contains(
            "\nincomplete: the recording passed its 256.0 MiB size cap 5s into the run: \
             output and log lines were dropped from there"
        ),
        "{out}"
    );
    let json: serde_json::Value =
        serde_json::from_str(&run_show(&sources, &id.encode(), true).await.expect("show"))
            .expect("json");
    assert_eq!(json["truncated"], true, "{json}");
    assert_eq!(json["incomplete"], true, "{json}");
    assert_eq!(json["incomplete_reason"], "size_cap", "{json}");
    let listed = run_list(&sources, Workspace::All, true).await;
    let listed: serde_json::Value = serde_json::from_str(&listed).expect("list json");
    assert_eq!(listed[0]["incomplete"], true, "{listed}");
    let table = run_list(&sources, Workspace::All, false).await;
    assert!(table.contains("  incomplete"), "{table}");

    // Cut twice: the ceiling too, with when it stopped.
    let mut records = build_records("//a:b", T0, None);
    records.push(SessionRecord::session(
        T0 + 5_000,
        SessionRecordKind::Truncated,
    ));
    records.push(SessionRecord::session(
        T0 + 20_000,
        SessionRecordKind::Truncated,
    ));
    let twice = FakeSource {
        sessions: vec![(source.sessions[0].0.clone(), records)],
    };
    let sources: [&dyn SessionSource; 1] = [&twice];
    let out = run_show(&sources, &id.encode(), false).await.expect("show");
    assert!(
        out.contains("it reached its 1.0 GiB ceiling 20s into the run and recorded nothing after"),
        "{out}"
    );

    // Not cut: no such line.
    let whole = FakeSource {
        sessions: vec![(
            entry(
                meta(id, 1, &["heph", "run", "//a:b"]),
                false,
                Some(ended(Some(0), T0 + 1_000)),
            ),
            build_records("//a:b", T0, Some(0)),
        )],
    };
    let sources: [&dyn SessionSource; 1] = [&whole];
    let out = run_show(&sources, &id.encode(), false).await.expect("show");
    assert!(!out.contains("incomplete"), "{out}");
    let json: serde_json::Value =
        serde_json::from_str(&run_show(&sources, &id.encode(), true).await.expect("show"))
            .expect("json");
    assert_eq!(json["incomplete"], false, "{json}");
    assert_eq!(json["incomplete_reason"], serde_json::Value::Null, "{json}");
    let listed = run_list(&sources, Workspace::All, true).await;
    let listed: serde_json::Value = serde_json::from_str(&listed).expect("list json");
    assert_eq!(listed[0]["incomplete"], false, "{listed}");
}

/// A record that does not decode is skipped, counted in the text and in
/// `--json`'s `skipped`; the rest still folds.
#[tokio::test]
async fn show_reports_skipped_records() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let id = SessionId::new(T0, 1);
    let dir = write_ended(
        &root,
        &meta(id, 1, &["heph", "run", "//a:b"]),
        &build_records("//a:b", T0, Some(0)),
        &ended(Some(0), T0 + 1_000),
    );
    testing::alter(
        &dir,
        "UPDATE records SET body = 'not json' WHERE seq = (SELECT min(seq) FROM records)",
    )
    .expect("damage one record");

    let file = crate::tui::session::FileSource::new(&root);
    let sources: [&dyn SessionSource; 1] = [&file];
    let out = run_show(&sources, &id.encode(), false).await.expect("show");
    assert!(
        out.contains(
            "\n1 record could not be read and was skipped, so the progress below leaves it out: \
             damaged, or written in a shape this heph does not know (a newer heph may read it)\n"
        ),
        "{out}"
    );
    let json: serde_json::Value =
        serde_json::from_str(&run_show(&sources, &id.encode(), true).await.expect("show"))
            .expect("json");
    assert_eq!(json["skipped"], 1, "{json}");
    assert_eq!(json["pools"], serde_json::json!([]), "{json}");
}

/// Spec row `export_is_self_contained` (I6, I7): an export carries what a
/// reader elsewhere cannot look up — each approval notice's text, read by the
/// engine's reader, and the tail of each root failure's log — and opens with
/// the sessions root, the home and the workspace gone. A collateral failure's
/// log and a log outside the failure slots are not exported; `--no-logs`
/// leaves both tables empty; `-o -` writes the same file to stdout.
///
/// Viewing it through `show` comes with `<ref>` resolution; this reads the
/// foreign recording's tables, which is what `show` will render.
#[tokio::test(flavor = "multi_thread")]
async fn export_is_self_contained() {
    use crate::tui::session::export::FAILURE_LOG_CAP;
    use crate::tui::session::recording::Recording as Opened;
    use crate::tui::session::{ApprovalPrompt, ApprovalPromptNotice, ApprovalQueued, FileSource};
    use hcore::events::LeftBehindData;

    const NOTICE: &str = "drop the table";
    let ws = tempfile::tempdir().expect("tempdir");
    std::fs::write(ws.path().join(".hephconfig"), "").expect("write .hephconfig");
    let global = crate::commands::GlobalOptions::default();
    let (engine, home) =
        crate::commands::bootstrap::new_reader_engine(&global, ws.path()).expect("reader engine");
    drop(engine);
    std::fs::write(ws.path().join("PLAN"), NOTICE).expect("write the notice source");
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(NOTICE.as_bytes());
    hasher.update(&[0]);
    let notice = ApprovalPromptNotice {
        name: "plan".to_owned(),
        sources: vec![hcore::approval_notice::NoticeSource {
            addr: "//a:plan".to_owned(),
            origin: hcore::approval_notice::NoticeOrigin::File {
                path: "PLAN".to_owned(),
                out_path: "PLAN".to_owned(),
                x: false,
                hashout: format!("{:x}", hasher.digest()),
            },
        }],
    };

    // The root failure's log is longer than what an export keeps of it.
    const CUT: u64 = 100;
    let root_log = home.join("failures").join("k1").join("log.txt");
    let mut log = vec![b'a'; usize::try_from(CUT).expect("small")];
    log.extend((0..FAILURE_LOG_CAP).map(|i| b'0' + u8::try_from(i % 10).expect("digit")));
    // The slot of `//a:later` was rewritten by a later session: its log is
    // newer than this session's end, so it is not this session's.
    let later_log = home.join("failures").join("k3").join("log.txt");
    let at = |ms: u64| std::time::UNIX_EPOCH + Duration::from_millis(ms);
    for (path, bytes, mtime) in [
        (&root_log, log.as_slice(), at(T0 + 5)),
        (
            &home.join("failures").join("k2").join("log.txt"),
            b"collateral".as_slice(),
            at(T0 + 5),
        ),
        (&later_log, b"a later session's".as_slice(), at(T0 + 60_000)),
    ] {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, bytes).expect("write a log");
        std::fs::File::options()
            .write(true)
            .open(path)
            .and_then(|f| f.set_modified(mtime))
            .expect("set the log's mtime");
    }
    let outside = ws.path().join("outside.log");
    std::fs::write(&outside, b"not a failure slot").expect("write");
    let failed = |addr: &str, upstream_of: Option<&str>, log: &Path| SessionRecord {
        at_unix_ms: T0,
        kind: Some(BuildEventKind::ResultEnd {
            addr: addr.to_owned(),
            error: Some("exit status: 1".to_owned()),
            upstream_of: upstream_of.map(str::to_owned),
            exit_status: None,
            log_tail: None,
            timed_out: None,
            cancelled: false,
            left_behind: Some(Box::new(LeftBehindData {
                log_path: Some(log.display().to_string()),
                ..LeftBehindData::default()
            })),
        }),
        session: None,
    };

    let sessions = tempfile::tempdir().expect("tempdir");
    let id = SessionId::new(T0, 7);
    let mut m = meta(id, 1, &["heph", "run"]);
    m.workspace_root = Some(ws.path().display().to_string());
    m.home_dir = Some(home.display().to_string());
    let records = [
        SessionRecord::session(
            T0,
            SessionRecordKind::Approval {
                prompt: Some(Box::new(ApprovalPrompt {
                    queue: vec![ApprovalQueued {
                        seq: 1,
                        addr: "//a:gate".to_owned(),
                        queued_at_unix_ms: T0,
                        notices: vec![notice],
                    }],
                })),
                standing: None,
            },
        ),
        failed("//a:root", None, &root_log),
        failed(
            "//a:collateral",
            Some("//a:root"),
            &home.join("failures").join("k2").join("log.txt"),
        ),
        failed("//a:outside", None, &outside),
        failed("//a:later", None, &later_log),
        SessionRecord::session(
            T0 + 10,
            SessionRecordKind::End {
                exit_code: Some(1),
                panicked: false,
            },
        ),
    ];
    write_ended(sessions.path(), &m, &records, &ended(Some(1), T0 + 10));

    let out = tempfile::tempdir().expect("tempdir");
    let file = FileSource::new(sessions.path());
    let export = |output: PathBuf, no_logs: bool| {
        let args = ExportArgs {
            reference: id.encode(),
            output: Some(output),
            no_logs,
        };
        let file = &file;
        async move {
            let mut stdout = Vec::new();
            let exported = export::export_ref(
                file,
                opts(),
                &args,
                crate::engine::approval::read_notice_from_store,
                &mut stdout,
                &no_opener(),
            )
            .await
            .expect("export");
            assert!(stdout.is_empty(), "a file export writes nothing to stdout");
            assert!(!exported.snapshot);
            exported.path.expect("a path")
        }
    };
    let full = export(out.path().join("full.hephsession"), false).await;
    let bare = export(out.path().join("bare.hephsession"), true).await;
    // `-o -`: the same export, on stdout.
    let piped = out.path().join("piped.hephsession");
    {
        let rec = Opened::open_local(sessions.path(), &sessions.path().join(id.encode()))
            .expect("open local");
        let mut stdout = Vec::new();
        let exported = export::export(
            &rec,
            export::Dest::Stdout,
            Some(crate::engine::approval::read_notice_from_store),
            &crate::tui::session::store::tmp_dir(sessions.path()),
            &mut stdout,
        )
        .expect("export to stdout");
        assert!(exported.path.is_none());
        std::fs::write(&piped, stdout).expect("write");
    }

    // Nothing local is left to look anything up in.
    drop((sessions, ws));
    assert!(!home.exists(), "the home was under the workspace");
    let ftmp = tempfile::tempdir().expect("tempdir");
    for path in [&full, &piped] {
        let rec = Opened::open_foreign(path, ftmp.path()).expect("open the export");
        let Opened::Foreign(f) = &rec else {
            panic!("foreign")
        };
        assert_eq!(
            f.recorded_notice("//a:gate", 1, "plan").expect("notice"),
            Some(NOTICE.to_owned()),
            "{}",
            path.display()
        );
        let kept = log
            .get(usize::try_from(CUT).expect("small")..)
            .expect("tail");
        assert_eq!(
            f.failure_logs("//a:root").expect("logs"),
            [(kept.to_vec(), CUT)],
            "the root failure's last 1 MiB, and how much came before it"
        );
        assert!(f.failure_logs("//a:collateral").expect("logs").is_empty());
        assert!(f.failure_logs("//a:outside").expect("logs").is_empty());
        assert!(
            f.failure_logs("//a:later").expect("logs").is_empty(),
            "a log newer than the session's end is a later session's"
        );
    }
    // A foreign recording exports again: as it is, or `--no-logs`.
    for (name, logs) in [
        ("again.hephsession", true),
        ("again-bare.hephsession", false),
    ] {
        let rec = Opened::open_foreign(&full, ftmp.path()).expect("open the export");
        let again = out.path().join(name);
        let mut stdout = Vec::new();
        export::export(
            &rec,
            export::Dest::File(again.clone()),
            logs.then_some(
                crate::engine::approval::read_notice_from_store
                    as hcore::approval_notice::ReadNotice,
            ),
            ftmp.path(),
            &mut stdout,
        )
        .expect("re-export a foreign recording");
        drop(rec);
        let rec = Opened::open_foreign(&again, ftmp.path()).expect("open the re-export");
        let Opened::Foreign(f) = &rec else {
            panic!("foreign")
        };
        assert_eq!(rec.meta().expect("meta").id, id, "{name}");
        assert_eq!(
            f.recorded_notice("//a:gate", 1, "plan").expect("notice"),
            logs.then(|| NOTICE.to_owned()),
            "{name}"
        );
        assert_eq!(
            f.failure_logs("//a:root").expect("logs").len(),
            usize::from(logs),
            "{name}"
        );
    }
    let rec = Opened::open_foreign(&bare, ftmp.path()).expect("open the export");
    let Opened::Foreign(f) = &rec else {
        panic!("foreign")
    };
    assert_eq!(
        f.recorded_notice("//a:gate", 1, "plan").expect("notice"),
        None
    );
    assert!(f.failure_logs("//a:root").expect("logs").is_empty());
    assert_eq!(rec.meta().expect("meta").id, id);
    drop(rec);

    // The viewers, by path and by URL, with nothing local to read.
    let empty = FakeSource::default();
    let sources: [&dyn SessionSource; 1] = [&empty];
    let by_path = refs::opener_in(ftmp.path(), NoDownload);
    let full_arg = full.display().to_string();
    let text = refs::show_with(&sources, &full_arg, false, &by_path)
        .await
        .expect("show the export");
    assert_eq!(
        text.lines().next(),
        Some(format!("read from {full_arg}").as_str()),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "approval notice plan of request 1 for //a:gate:\n{NOTICE}\n"
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("log of //a:root (its first {CUT} B left out):\n")),
        "{text}"
    );
    assert!(!text.contains("//a:collateral (its") && !text.contains("log of //a:outside"));
    assert!(!text.contains("the process went away"), "{text}");
    let v: serde_json::Value = serde_json::from_str(
        &refs::show_with(&sources, &full_arg, true, &by_path)
            .await
            .expect("show --json"),
    )
    .expect("json");
    // A whole recording keeps the state a local one would have.
    assert_eq!(v["state"], "failed", "{v}");
    let foreign = &v["foreign"];
    assert_eq!(foreign["origin"], full_arg.as_str(), "{v}");
    assert!(foreign["partial"].is_null(), "{v}");
    assert_eq!(
        foreign["notices"],
        serde_json::json!([{"addr": "//a:gate", "seq": 1, "name": "plan", "text": NOTICE}]),
        "{v}"
    );
    assert_eq!(foreign["failure_logs"][0]["addr"], "//a:root", "{v}");
    assert_eq!(foreign["failure_logs"][0]["cut_bytes"], CUT, "{v}");
    assert_eq!(
        foreign["failure_logs"].as_array().map(Vec::len),
        Some(1),
        "{v}"
    );
    assert_eq!(v["next"]["kind"], "none", "{v}");
    // Without logs, nothing of either.
    let text = refs::show_with(&sources, &bare.display().to_string(), false, &by_path)
        .await
        .expect("show the bare export");
    assert!(!text.contains(NOTICE) && !text.contains("log of"), "{text}");
    // The same bytes served over https.
    const URL: &str = "https://example.invalid/full.hephsession";
    let by_url = refs::opener_in(
        ftmp.path(),
        refs::Serve(std::fs::read(&full).expect("read the export")),
    );
    let text = refs::show_with(&sources, URL, false, &by_url)
        .await
        .expect("show the download");
    assert_eq!(
        text.lines().next(),
        Some(format!("read from {URL}").as_str()),
        "{text}"
    );
    // attach replays it to its end and mirrors the recorded exit code.
    let (res, printed) = refs::attach_captured(&sources, &full_arg, &by_path).await;
    assert_eq!(exit_code(&res.expect_err("exit 1")), Some(1));
    assert!(
        printed.starts_with(&format!("heph: replaying {full_arg}\n")),
        "{printed}"
    );
}

/// `-o` as given, `-` for stdout, else `./heph-<id>.hephsession`.
#[test]
fn export_destination() {
    let id = SessionId::new(T0, 7);
    assert_eq!(
        export::Dest::from_arg(None, id),
        export::Dest::File(PathBuf::from(format!("heph-{}.hephsession", id.encode())))
    );
    assert_eq!(
        export::Dest::from_arg(Some(Path::new("-")), id),
        export::Dest::Stdout
    );
    // A file named `-` is spelled `./-`.
    assert_eq!(
        export::Dest::from_arg(Some(Path::new("./-")), id),
        export::Dest::File(PathBuf::from("./-"))
    );
}

/// The `.hephconfig` defaults a bad value's warning names are the ones the
/// cleanup applies.
/// A lost session sealed by cleanup carries `derivation` 0: no
/// `heph.session` span and no session-wide rows (re-derivation on open does
/// not exist yet). `list` and `show`, text and `--json`, still read it with
/// no error and say `lost`. What comes from the session-wide rows is absent,
/// not zero: `pools` is `[]` and `show` prints no pool line, although the
/// recording holds `PoolUsage` records. The documented "Why is my session
/// `lost`?" recipe runs on the sealed file and names the target that was
/// still running.
#[tokio::test]
async fn a_lost_session_sealed_by_cleanup_still_reads() {
    use crate::tui::session::cleanup::{self, Retention};
    use crate::tui::session::store;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let now_ms = hcore::events::now_unix_ms();
    let id = SessionId::new(now_ms - 5_000, 1);
    let at = id.started_unix_ms();
    let mut records = build_records("//a:done", at, None);
    records.push(record(
        at + 20,
        BuildEventKind::ResultEnd {
            left_behind: None,
            cancelled: false,
            addr: "//a:done".into(),
            error: None,
            upstream_of: None,
            exit_status: None,
            exit_code: None,
            log_tail: None,
            timed_out: None,
            spans: Vec::new(),
        },
    ));
    records.push(record(
        at + 30,
        BuildEventKind::ResultStart {
            addr: "//a:running".into(),
        },
    ));
    records.push(record(
        at + 40,
        BuildEventKind::PoolUsage {
            pool: "docker".into(),
            held: 1,
            capacity: 4,
            waiting: 0,
            removals_pending: 0,
        },
    ));
    let dir = Recording::create(
        &root.join(id.encode()),
        &meta(id, 1, &["heph", "run", "//a:..."]),
        &records,
        None,
    )
    .expect("recording")
    .abandon();
    // Past the start grace, so cleanup reads it as lost and seals it.
    let later = std::time::SystemTime::now() + 2 * cleanup::STARTING;
    let report = cleanup::cleanup(&root, later, Retention::default()).expect("cleanup");
    assert_eq!(report.sealed, vec![id], "{report:?}");

    let db = store::open_sealed(&dir.join(store::FILE)).expect("open sealed");
    let (derivation, sealed, exit_code): (i64, i64, Option<i64>) = db
        .query_row(
            "SELECT derivation, sealed, exit_code FROM session",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("session row");
    assert_eq!((derivation, sealed, exit_code), (0, 1, None));
    let session_wide: i64 = db
        .query_row(
            "SELECT (SELECT count(*) FROM spans WHERE name = 'heph.session')
                  + (SELECT count(*) FROM metrics WHERE name LIKE 'heph.session.%')",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(session_wide, 0, "cleanup seals, it does not derive");

    // The recipe, as documented.
    let doc =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/SESSIONS.md"))
            .expect("read docs/SESSIONS.md");
    let recipe = doc
        .split("**Why is my session `lost`?**")
        .nth(1)
        .and_then(|rest| rest.split("```sql\n").nth(1))
        .and_then(|rest| rest.split("```").next())
        .expect("the lost recipe's sql block");
    let mut stmt = db.prepare(recipe).expect("the recipe compiles");
    let running: Vec<(String, f64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("the recipe runs")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(running, vec![("//a:running".to_owned(), 0.03)]);
    drop(stmt);
    drop(db);

    let opts = || ViewOpts {
        now_ms: hcore::events::now_unix_ms(),
        ..opts()
    };
    let file = crate::tui::session::FileSource::new(&root);
    let sources: [&dyn SessionSource; 1] = [&file];

    let mut out = Vec::new();
    list::list(
        &sources,
        opts(),
        Workspace::All,
        &json_args(false),
        piped(&mut out),
    )
    .await
    .expect("list");
    let table = String::from_utf8(out).expect("utf-8");
    let row = table
        .lines()
        .find(|l| l.contains(&id.encode()))
        .unwrap_or_else(|| panic!("no row: {table}"));
    assert!(row.contains("lost"), "{table}");

    let mut out = Vec::new();
    list::list(
        &sources,
        opts(),
        Workspace::All,
        &json_args(true),
        piped(&mut out),
    )
    .await
    .expect("list --json");
    let json: serde_json::Value = serde_json::from_slice(&out).expect("json");
    let s = &json[0];
    assert_eq!(s["id"], id.encode(), "{json}");
    assert_eq!(s["state"], "lost", "{s}");
    assert_eq!(s["exit_code"], serde_json::Value::Null, "{s}");

    let mut out = Vec::new();
    show::show(
        &sources,
        opts(),
        &id.encode(),
        &json_args(false),
        piped(&mut out),
        &no_opener(),
    )
    .await
    .expect("show");
    let text = String::from_utf8(out).expect("utf-8");
    assert!(
        text.contains("the process went away without recording its end"),
        "{text}"
    );
    assert!(
        !text.contains(" pool "),
        "no pool line without the rows: {text}"
    );

    let mut out = Vec::new();
    show::show(
        &sources,
        opts(),
        &id.encode(),
        &json_args(true),
        piped(&mut out),
        &no_opener(),
    )
    .await
    .expect("show --json");
    let json: serde_json::Value = serde_json::from_slice(&out).expect("json");
    assert_eq!(json["state"], "lost", "{json}");
    assert_eq!(json["pools"], serde_json::json!([]), "{json}");
    assert_eq!(json["incomplete"], false, "{json}");
}

#[test]
fn retention_defaults_agree() {
    use crate::engine::config_yaml::SessionConfig;
    use crate::tui::session::cleanup::Retention;
    assert_eq!(
        Retention::default(),
        Retention {
            max_age: SessionConfig::KEEP_DEFAULT,
            max_bytes: SessionConfig::KEEP_BYTES_DEFAULT,
        }
    );
}

mod refs;
