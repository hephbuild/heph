use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use hcore::events::BuildEventKind;

use super::attach::{Outcome, outcome_result};
use super::*;
use crate::tui::session::format::{
    LockWaitEntry, RunningTarget, SessionEnd, SnapshotCounts, files,
};
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
        let mut items: Vec<SessionItem> =
            records.iter().cloned().map(SessionItem::Record).collect();
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

async fn run_list(sources: &[&dyn SessionSource], ws: Workspace<'_>, json: bool) -> String {
    let mut out = Vec::new();
    list::list(sources, opts(), ws, json, &mut out)
        .await
        .expect("list");
    String::from_utf8(out).expect("utf-8")
}

async fn run_show(sources: &[&dyn SessionSource], id: &str, json: bool) -> anyhow::Result<String> {
    let mut out = Vec::new();
    show::show(sources, opts(), id, json, &mut out).await?;
    Ok(String::from_utf8(out).expect("utf-8"))
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
                    running_unblocked: 1,
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
            SessionState::Stalled,
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
    // Unknown is what a newer heph's state decodes as.
    let s: SessionState = serde_json::from_str("\"hibernating\"").expect("decode");
    assert_eq!(s, SessionState::Unknown);
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
                                addr: "//x:y".into(),
                                error: None,
                                upstream_of: None,
                                exit_status: None,
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
        "no sessions"
    );

    let json: serde_json::Value =
        serde_json::from_str(&run_list(&sources, Workspace::Only("/w/pacu"), true).await)
            .expect("json");
    assert_eq!(json["version"], 1);
    let ids: Vec<&str> = json["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|s| s["id"].as_str().expect("id"))
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "sorted by id");
    let a_json = &json["sessions"][0];
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
    assert_eq!(a_json["progress"]["running"], 0);
    assert_eq!(a_json["panicked"], false);
    assert!(a_json.get("format").is_none(), "only on too-new rows");
    let b_json = &json["sessions"][1];
    assert_eq!(b_json["exit_code"], 0);
    assert_eq!(b_json["state"], "ok");
    assert_eq!(b_json["now"], serde_json::Value::Null, "it ended");
    assert_eq!(
        b_json["progress"],
        serde_json::json!({"done": 1, "total": 1, "total_complete": true, "running": 0, "failed": 0})
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

    // inflight goes to the source that has the session.
    let mut out = Vec::new();
    inflight(&sources, opts(), &b.encode(), false, &mut out)
        .await
        .expect("inflight");
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        format!("in-flight report for {b}\n")
    );
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
                    addr: addr.into(),
                    error: Some("boom".to_owned()),
                    upstream_of: None,
                    exit_status: None,
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
    let cases: Vec<(SessionEntry, Vec<SessionRecord>, &str, Option<&str>)> = vec![
        (e(true, Some(busy)), Vec::new(), "attach", Some(&attach)),
        (
            e(true, Some(quiet)),
            Vec::new(),
            "inflight",
            Some(&inflight),
        ),
        (
            e(false, Some(ended(Some(1), NOW))),
            fail_records("//a:b"),
            "inspect_failure",
            Some("heph inspect failure //a:b"),
        ),
        (
            e(false, Some(ended(Some(1), NOW))),
            Vec::new(),
            "rerun",
            Some("heph run //a:b"),
        ),
        (
            e(false, Some(SessionSnapshot::default())),
            Vec::new(),
            "rerun",
            Some("heph run //a:b"),
        ),
        (
            SessionEntry {
                status_unreadable: true,
                ..e(false, None)
            },
            Vec::new(),
            "list",
            Some("heph session list"),
        ),
        (
            e(false, Some(ended(Some(0), NOW))),
            Vec::new(),
            "none",
            None,
        ),
    ];
    for (n, (session, records, kind, command)) in cases.into_iter().enumerate() {
        let source = FakeSource {
            sessions: vec![(session, records)],
        };
        let sources: [&dyn SessionSource; 1] = [&source];
        let json: serde_json::Value =
            serde_json::from_str(&run_show(&sources, &id.encode(), true).await.expect("json"))
                .expect("json");
        assert_eq!(json["next"]["kind"], kind, "case {n}: {json}");
        assert_eq!(json["next"]["command"].as_str(), command, "case {n}");
        let reason = json["next"]["reason"].as_str().expect("reason");
        let out = run_show(&sources, &id.encode(), false).await.expect("show");
        let want = format!("\nNext: {}\n  {reason}\n", command.unwrap_or("nothing"));
        assert!(out.ends_with(&want), "case {n}: {out}");
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
                addr,
                error: Some("boom".to_owned()),
                upstream_of: None,
                exit_status: None,
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
    assert_eq!(listed["sessions"][0]["state"], "waiting");

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
                true,
            )
            .await
        }
    };
    let err = run(done).await.expect_err("exit 3");
    assert_eq!(exit_code(&err), Some(3));
    let err = run(lost).await.expect_err("lost");
    assert_eq!(exit_code(&err), Some(125));
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

/// Write a session directory as a recorder would, without one.
fn write_session(
    root: &Path,
    meta: &SessionMeta,
    records: &[SessionRecord],
    status: Option<&SessionSnapshot>,
) -> PathBuf {
    let dir = root.join(meta.id.encode());
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join(files::META),
        serde_json::to_vec(meta).expect("meta"),
    )
    .expect("meta");
    std::fs::write(dir.join(files::LOCK), b"").expect("lock");
    let mut events = Vec::new();
    for r in records {
        serde_json::to_writer(&mut events, r).expect("record");
        events.push(b'\n');
    }
    std::fs::write(dir.join(files::EVENTS), events).expect("events");
    if let Some(s) = status {
        std::fs::write(
            dir.join(files::STATUS),
            serde_json::to_vec(s).expect("status"),
        )
        .expect("status");
    }
    dir
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
    write_session(
        &root,
        &meta(done, 1, &["heph", "run", "//a:b"]),
        &build_records("//a:b", T0, Some(0)),
        Some(&ended(Some(0), T0 + 1_000)),
    );
    let live_dir = write_session(
        &root,
        &meta(live, 2, &["heph", "run", "//c:d"]),
        &build_records("//c:d", T0 + 3_000, None),
        Some(&SessionSnapshot {
            last_event_unix_ms: Some(NOW),
            ..SessionSnapshot::default()
        }),
    );
    let _lock = hold_lock(&live_dir);
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
    let dir = write_session(
        &root,
        &meta(id, 1, &["heph", "run", "//a:b"]),
        &build_records("//a:b", T0, Some(0)),
        Some(&ended(Some(0), T0 + 1_000)),
    );
    let events = dir.join(files::EVENTS);
    std::fs::set_permissions(&events, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let file = crate::tui::session::FileSource::new(&root);
    let sources: [&dyn SessionSource; 1] = [&file];
    let table = run_list(&sources, Workspace::All, false).await;
    assert!(
        table.contains(&id.encode()) && table.contains("ok"),
        "{table}"
    );
    let json = run_list(&sources, Workspace::All, true).await;
    assert!(json.contains(&id.encode()), "{json}");
    // The log really is unreadable (unless the tests run as root), so show,
    // which folds it, fails.
    if std::fs::read(&events).is_err() {
        run_show(&sources, &id.encode(), false)
            .await
            .expect_err("show reads the events");
    }
    std::fs::set_permissions(&events, std::fs::Permissions::from_mode(0o644)).expect("chmod");
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
    let s = &json["sessions"][0];
    assert_eq!(s["panicked"], true, "{s}");
    assert_eq!(s["exit_code"], 0);
    assert_eq!(s["state"], "failed");
}

#[tokio::test]
async fn list_shows_newer_format_placeholder() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let id = SessionId::new(T0, 1);
    let dir = root.join(id.encode());
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join(files::META),
        br#"{"format": 999, "whatever": true}"#,
    )
    .expect("meta");
    std::fs::write(dir.join(files::LOCK), b"").expect("lock");

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
    assert_eq!(json["sessions"][0]["format"], 999);
    assert_eq!(json["sessions"][0]["state"], "unknown");
    assert_eq!(json["sessions"][0]["started_unix_ms"], T0);
    // Nothing more is read of it: no pid, no progress.
    assert_eq!(json["sessions"][0]["owner_pid"], serde_json::Value::Null);
    assert_eq!(json["sessions"][0]["progress"], serde_json::Value::Null);
    assert_eq!(json["sessions"][0]["panicked"], false);

    let err = run_show(&sources, &id.encode(), false)
        .await
        .expect_err("show refuses a newer format");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("format 999") && msg.contains("format 1"),
        "{msg}"
    );
}
