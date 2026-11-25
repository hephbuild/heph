//! `list` over the catalog (spec "CLI", D18): who ran a target, the
//! filters, the window and its adversarial inputs, and the order.

use super::*;
use crate::tui::session::FileSource;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const ADDR: &str = "//p:t";

/// The records of a session that resolved `addr`: executing it when `ran`,
/// failing it with `error`; neither is a cache hit.
fn resolved(addr: &str, at: u64, ran: bool, error: Option<&str>) -> Vec<SessionRecord> {
    let mut v = vec![record(
        at,
        BuildEventKind::ResultStart { addr: addr.into() },
    )];
    if ran {
        v.push(record(
            at + 1,
            BuildEventKind::ExecuteStart {
                output_groups: Vec::new(),
                log_not_streamed: false,
                addr: addr.into(),
                driver: "exec".into(),
                cache: true,
                declared: None,
                reserved: None,
                queued_ms: None,
                admitted: None,
            },
        ));
    }
    v.push(record(
        at + 40,
        BuildEventKind::ResultEnd {
            spans: Vec::new(),
            left_behind: None,
            cancelled: false,
            addr: addr.into(),
            error: error.map(Into::into),
            upstream_of: None,
            exit_status: None,
            log_tail: None,
            timed_out: None,
        },
    ));
    v
}

/// Who ran a session, as the recording's session row holds it.
struct Who {
    kind: &'static str,
    name: Option<&'static str>,
    user: &'static str,
}

const HUMAN: Who = Who {
    kind: "human",
    name: None,
    user: "ana",
};
const AGENT: Who = Who {
    kind: "agent",
    name: Some("claude-code"),
    user: "bob",
};
const CI: Who = Who {
    kind: "ci",
    name: Some("github-actions"),
    user: "runner",
};

/// What a session did to its one target.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Did {
    Ran,
    Failed,
    Hit,
}

/// A sealed `heph <cmd> <addr>` session under `root`, started `ago_ms`
/// before `now` and ended a second later: exit 1 when the target failed.
fn session(
    root: &Path,
    now: u64,
    ago_ms: u64,
    cmd: &str,
    addr: &str,
    did: Did,
    who: &Who,
) -> SessionId {
    let id = SessionId::new(now - ago_ms, 7);
    let start = id.started_unix_ms();
    let records = resolved(
        addr,
        start,
        did != Did::Hit,
        (did == Did::Failed).then_some("boom"),
    );
    let exit = i32::from(did == Did::Failed);
    let dir = write_ended(
        root,
        &meta(id, 100, &["heph", cmd, addr]),
        &records,
        &ended(Some(exit), start + 1_000),
    );
    testing::alter(
        &dir,
        &format!(
            "UPDATE session SET actor_kind = '{}', actor_name = {}, user = '{}'",
            who.kind,
            who.name
                .map_or_else(|| "NULL".to_owned(), |n| format!("'{n}'")),
            who.user
        ),
    )
    .expect("set who ran it");
    id
}

/// `heph session list` as `execute` runs it: the catalog opened (healed,
/// rebuilt) first, its notes collected.
async fn list_at(
    root: &Path,
    now: u64,
    filter: &list::ListFilter,
    json: bool,
) -> (String, Vec<String>) {
    let file = FileSource::new(root);
    let sources: [&dyn SessionSource; 1] = [&file];
    let mut notes = Vec::new();
    let catalog = list::read_catalog(root, now, filter, &mut |n| notes.push(n.to_owned()));
    let mut out = Vec::new();
    list::list(
        &sources,
        ViewOpts {
            now_ms: now,
            ..opts()
        },
        Workspace::All,
        filter,
        catalog.as_ref(),
        json,
        &mut out,
    )
    .await
    .expect("list");
    (String::from_utf8(out).expect("utf-8"), notes)
}

fn since(days: u64, now: u64) -> list::ListFilter {
    list::ListFilter {
        since_ms: Some(now - days * DAY_MS),
        ..list::ListFilter::default()
    }
}

fn json_ids(json: &serde_json::Value) -> Vec<String> {
    json["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|s| s["id"].as_str().expect("id").to_owned())
        .collect()
}

/// Spec row `list_who_ran`: `list --addr //p:t --since 7d` returns one row
/// per execution across sessions, newest first; plus every filter flag and
/// a session only the catalog still holds.
#[tokio::test]
async fn list_who_ran() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let now = hcore::events::now_unix_ms();
    let hour = 60 * 60 * 1000;
    let ran = session(root, now, 3 * hour, "run", ADDR, Did::Ran, &HUMAN);
    let failed = session(root, now, 2 * hour, "run", ADDR, Did::Failed, &AGENT);
    let other = session(root, now, hour, "query", "//p:other", Did::Ran, &CI);
    let old = session(root, now, 8 * DAY_MS, "run", ADDR, Did::Ran, &HUMAN);

    // The first `list` finds no catalog and rebuilds it from the recordings.
    let who_ran = list::ListFilter {
        addr: Some(ADDR.to_owned()),
        ..since(7, now)
    };
    let (table, notes) = list_at(root, now, &who_ran, false).await;
    assert!(
        notes
            .iter()
            .any(|n| n.contains("the session catalog was missing: rebuilt from 4 recordings")),
        "{notes:?}"
    );
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines.len(), 3, "header + the two runs in 7 days:\n{table}");
    assert!(lines[0].contains("OUTCOME"), "{table}");
    assert!(
        lines[1].starts_with(&failed.encode()),
        "newest first:\n{table}"
    );
    assert!(lines[1].contains("agent (claude-code)"), "{table}");
    assert!(lines[1].contains("failed"), "{table}");
    assert!(lines[2].starts_with(&ran.encode()), "{table}");
    assert!(lines[2].contains("human"), "{table}");
    assert!(lines[2].contains(" ran "), "{table}");
    assert!(!table.contains(&other.encode()), "it never resolved {ADDR}");
    assert!(!table.contains(&old.encode()), "outside 7 days");

    let (json, notes) = list_at(root, now, &who_ran, true).await;
    assert!(notes.is_empty(), "a second list repairs nothing: {notes:?}");
    let json: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert_eq!(json_ids(&json), [failed.encode(), ran.encode()]);
    let f = &json["sessions"][0];
    assert_eq!(f["target"]["addr"], ADDR);
    assert_eq!(f["target"]["outcome"], "failed");
    assert!(f["target"]["duration_ms"].is_u64(), "{f}");
    assert_eq!(
        f["actor"],
        serde_json::json!({"kind": "agent", "name": "claude-code"})
    );
    assert_eq!(f["user"], "bob");
    assert_eq!(f["command"], "run");
    assert_eq!(f["recorded"], true);
    assert_eq!(f["recording_kept"], true);
    assert_eq!(f["ended_unix_ms"], failed.started_unix_ms() + 1_000);
    assert_eq!(json["sessions"][1]["target"]["outcome"], "ran");
    assert_eq!(
        json["retention"]["catalog"]["keep_ms"],
        90 * DAY_MS,
        "{}",
        json["retention"]
    );
    assert_eq!(
        json["retention"]["catalog"]["oldest_started_unix_ms"],
        old.started_unix_ms()
    );
    assert_eq!(
        json["retention"]["recordings"]["keep_ms"],
        u64::try_from(opts().retention.max_age.as_millis()).expect("ms")
    );

    // Further back, the older run too.
    let (json, _) = list_at(
        root,
        now,
        &list::ListFilter {
            addr: Some(ADDR.to_owned()),
            ..since(30, now)
        },
        true,
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert_eq!(
        json_ids(&json),
        [failed.encode(), ran.encode(), old.encode()]
    );

    // Each filter flag on its own.
    let only = |f: list::ListFilter| async move {
        let (json, _) = list_at(root, now, &f, true).await;
        json_ids(&serde_json::from_str(&json).expect("json"))
    };
    assert_eq!(
        only(list::ListFilter {
            actor: Some(ActorKind::Agent),
            ..since(7, now)
        })
        .await,
        [failed.encode()]
    );
    assert_eq!(
        only(list::ListFilter {
            actor: Some(ActorKind::Ci),
            ..since(7, now)
        })
        .await,
        [other.encode()]
    );
    assert_eq!(
        only(list::ListFilter {
            user: Some("ana".to_owned()),
            ..since(7, now)
        })
        .await,
        [ran.encode()]
    );
    assert_eq!(
        only(list::ListFilter {
            failed: true,
            ..since(7, now)
        })
        .await,
        [failed.encode()]
    );
    assert_eq!(
        only(list::ListFilter {
            command: Some("query".to_owned()),
            ..since(7, now)
        })
        .await,
        [other.encode()]
    );
    assert_eq!(
        only(list::ListFilter {
            until_ms: Some(now - 150 * 60 * 1000),
            ..since(7, now)
        })
        .await,
        [ran.encode()],
        "--until bounds the start"
    );

    // Its directory gone (cleaned up), the catalog still answers for it.
    std::fs::remove_dir_all(root.join(ran.encode())).expect("remove the recording");
    let (table, _) = list_at(root, now, &who_ran, false).await;
    let row = table
        .lines()
        .find(|l| l.starts_with(&ran.encode()))
        .unwrap_or_else(|| panic!("the catalog row is listed:\n{table}"));
    assert!(row.contains(" ran ") && row.contains(" ok "), "{row}");
    let (json, _) = list_at(root, now, &who_ran, true).await;
    let json: serde_json::Value = serde_json::from_str(&json).expect("json");
    let gone = json["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .find(|s| s["id"] == ran.encode())
        .unwrap_or_else(|| panic!("the catalog row is listed: {json}"));
    assert_eq!(gone["recorded"], true, "it did record: {gone}");
    assert_eq!(
        gone["recording_kept"], false,
        "but there is no recording left to show: {gone}"
    );
    let (plain, _) = list_at(root, now, &since(7, now), false).await;
    assert!(plain.contains(&ran.encode()), "{plain}");
}

/// Spec row `targets_answer_cache_hits` (D18), the `list --json` half: a run
/// that executed //p:t and one that hit its cache are both listed by
/// `--addr`, with `target.outcome` `ran` and `hit`. Synthetic records; that
/// a real build's events seal into those rows is
/// `crates/e2e/tests/e2e/session_catalog.rs`.
#[tokio::test]
async fn list_json_shows_the_addr_outcome() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let now = hcore::events::now_unix_ms();
    let cold = session(root, now, 20_000, "run", ADDR, Did::Ran, &HUMAN);
    let warm = session(root, now, 10_000, "run", ADDR, Did::Hit, &HUMAN);
    let (json, _) = list_at(
        root,
        now,
        &list::ListFilter {
            addr: Some(ADDR.to_owned()),
            ..since(7, now)
        },
        true,
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert_eq!(json_ids(&json), [warm.encode(), cold.encode()]);
    assert_eq!(json["sessions"][0]["target"]["outcome"], "hit");
    assert_eq!(json["sessions"][1]["target"]["outcome"], "ran");
}

/// Live sessions first, then ended ones, each newest first: the table and
/// `--json` alike (a change from the table's oldest-first and `--json`'s
/// id order).
#[tokio::test]
async fn list_orders_live_first_then_newest_first() {
    let ids: Vec<SessionId> = (0..4u8)
        .map(|i| SessionId::new(T0 + u64::from(i) * 1_000, i))
        .collect();
    let source = FakeSource {
        sessions: vec![
            (
                entry(
                    meta(ids[0], 1, &["heph", "run"]),
                    false,
                    Some(ended(Some(0), NOW)),
                ),
                Vec::new(),
            ),
            (
                entry(meta(ids[1], 2, &["heph", "run"]), true, None),
                Vec::new(),
            ),
            (
                entry(
                    meta(ids[2], 3, &["heph", "run"]),
                    false,
                    Some(ended(Some(1), NOW)),
                ),
                Vec::new(),
            ),
            (
                entry(meta(ids[3], 4, &["heph", "run"]), true, None),
                Vec::new(),
            ),
        ],
    };
    let sources: [&dyn SessionSource; 1] = [&source];
    let want = [
        ids[3].encode(),
        ids[1].encode(),
        ids[2].encode(),
        ids[0].encode(),
    ];
    let table = run_list(&sources, Workspace::All, false).await;
    let got: Vec<&str> = table
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().next().expect("id"))
        .collect();
    assert_eq!(got, want, "{table}");
    let json: serde_json::Value =
        serde_json::from_str(&run_list(&sources, Workspace::All, true).await).expect("json");
    assert_eq!(json_ids(&json), want);
}

/// Spec "Adversarial inputs": `--since 0d` is an empty window; `--until`
/// earlier than `--since` is refused (exit 125); an RFC 3339 time is read
/// with its offset, seconds optional.
#[tokio::test]
async fn list_window_adversarial_inputs() {
    // 2026-11-20T08:00:00Z.
    let at = 1_795_161_600_000;
    for t in [
        "2026-11-20T10:00+02:00",
        "2026-11-20T10:00:00+02:00",
        "2026-11-20T08:00Z",
        "2026-11-20T08:00:00Z",
        "2026-11-20T03:00-05:00",
    ] {
        assert_eq!(list::parse_time("--since", t, NOW).expect(t), at, "{t}");
    }
    assert_eq!(
        list::parse_time("--since", "2026-11-20T10:00:00.500+02:00", NOW).expect("fraction"),
        at + 500
    );
    assert_eq!(
        list::parse_time("--since", "7d", NOW).expect("7d"),
        NOW - 7 * DAY_MS
    );
    assert_eq!(list::parse_time("--since", "0d", NOW).expect("0d"), NOW);
    for bad in [
        "",
        "yesterday",
        "2026-11-20",
        "2026-11-20T10:00",
        "-1d",
        "7w",
    ] {
        let e = list::parse_time("--since", bad, NOW).expect_err(bad);
        assert!(e.to_string().contains("--since"), "{bad:?}: {e}");
    }

    // `--since 0d`: nothing started since now.
    let (since_ms, until_ms) = list::window("0d", None, NOW).expect("0d");
    let source = FakeSource {
        sessions: vec![(
            entry(meta(SessionId::new(T0, 1), 1, &["heph", "run"]), true, None),
            Vec::new(),
        )],
    };
    let sources: [&dyn SessionSource; 1] = [&source];
    let mut out = Vec::new();
    list::list(
        &sources,
        opts(),
        Workspace::All,
        &list::ListFilter {
            since_ms: Some(since_ms),
            until_ms,
            ..list::ListFilter::default()
        },
        None,
        false,
        &mut out,
    )
    .await
    .expect("list");
    assert_eq!(String::from_utf8(out).expect("utf-8").trim(), "no sessions");

    // `--until` before `--since`: refused, and `heph session` exits 125.
    let args = ListArgs {
        all: false,
        since: "1d".to_owned(),
        until: Some("2d".to_owned()),
        addr: None,
        failed: false,
        actor: None,
        command: None,
        user: None,
        workspace: None,
        json: false,
    };
    let e = wrap_exit(args.filter(NOW).map(|_| ())).expect_err("refused");
    assert!(format!("{e:#}").contains("earlier than --since"), "{e:#}");
    assert_eq!(exit_code(&e), Some(125));
}

/// `--failed` with `--addr` keeps the sessions in which that target failed
/// ("when did this target break"), whatever else failed in them; without
/// `--addr` it keeps the sessions that failed.
#[tokio::test]
async fn list_failed_with_addr_is_the_target_failing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let now = hcore::events::now_unix_ms();
    let hour = 60 * 60 * 1000;
    // //p:t ran fine; //p:other failed, so the session exited 1.
    let other_broke = {
        let id = SessionId::new(now - 3 * hour, 7);
        let start = id.started_unix_ms();
        let mut records = resolved(ADDR, start, true, None);
        records.extend(resolved("//p:other", start + 100, true, Some("boom")));
        write_ended(
            root,
            &meta(id, 100, &["heph", "run", ADDR, "//p:other"]),
            &records,
            &ended(Some(1), start + 1_000),
        );
        id
    };
    let t_broke = session(root, now, 2 * hour, "run", ADDR, Did::Failed, &HUMAN);
    let t_ran = session(root, now, hour, "run", ADDR, Did::Ran, &HUMAN);

    let ids = |f: list::ListFilter| async move {
        let (json, _) = list_at(root, now, &f, true).await;
        json_ids(&serde_json::from_str(&json).expect("json"))
    };
    assert_eq!(
        ids(list::ListFilter {
            addr: Some(ADDR.to_owned()),
            ..since(7, now)
        })
        .await,
        [t_ran.encode(), t_broke.encode(), other_broke.encode()],
        "every session that resolved //p:t"
    );
    assert_eq!(
        ids(list::ListFilter {
            addr: Some(ADDR.to_owned()),
            failed: true,
            ..since(7, now)
        })
        .await,
        [t_broke.encode()],
        "only the session in which //p:t failed"
    );
    assert_eq!(
        ids(list::ListFilter {
            failed: true,
            ..since(7, now)
        })
        .await,
        [t_broke.encode(), other_broke.encode()],
        "without --addr, every session that failed"
    );
}

/// FQ M6, catalog side: a truncated recording (`truncated=1`) and one in a
/// format this heph does not know (`user_version=99`). The catalog keeps
/// the first's `truncated` and `list` marks it `incomplete`, from the
/// recording and, once that is gone, from the catalog row alone; the second
/// is counted `heal.unknown` (no heal-failure row, so no retry), and `list`
/// still answers (exit 0) with its placeholder.
#[tokio::test]
async fn list_over_a_truncated_and_an_unknown_format_recording() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let now = hcore::events::now_unix_ms();
    let hour = 60 * 60 * 1000;
    // Cut at its ceiling: the status says so, and the session row stops
    // the explorer rows (`truncated=1`), as the recorder leaves it.
    let cut = SessionId::new(now - 2 * hour, 7);
    let start = cut.started_unix_ms();
    let dir = write_ended(
        root,
        &meta(cut, 100, &["heph", "run", ADDR]),
        &resolved(ADDR, start, true, None),
        &SessionSnapshot {
            truncated: true,
            ..ended(Some(0), start + 1_000)
        },
    );
    testing::alter(&dir, "UPDATE session SET truncated = 1, truncated_seq = 3").expect("truncated");
    let unknown = session(root, now, hour, "run", ADDR, Did::Ran, &AGENT);
    testing::alter(&root.join(unknown.encode()), "PRAGMA user_version = 99")
        .expect("an unknown format");

    let cat = crate::tui::session::catalog::open(root, now).expect("open");
    assert!(!cat.in_memory, "{:?}", cat.notes);
    assert_eq!(cat.heal.unknown, 1, "{:?}", cat.heal);
    assert_eq!(cat.heal.failed, 0, "{:?}", cat.heal);
    let count = |sql: &str| -> i64 {
        cat.conn
            .query_row(sql, [], |r| r.get(0))
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    };
    assert_eq!(count("SELECT count(*) FROM heal_failures"), 0);
    assert_eq!(
        count(&format!(
            "SELECT truncated FROM sessions WHERE session_id = '{}'",
            cut.encode()
        )),
        1
    );
    assert_eq!(
        count(&format!(
            "SELECT count(*) FROM sessions WHERE session_id = '{}'",
            unknown.encode()
        )),
        0,
        "an unknown format is never copied"
    );
    drop(cat);

    let row_of = |table: &str, id: SessionId| {
        table
            .lines()
            .find(|l| l.starts_with(&id.encode()))
            .unwrap_or_else(|| panic!("{} is listed:\n{table}", id.encode()))
            .to_owned()
    };
    let session_of = |json: &serde_json::Value, id: SessionId| {
        json["sessions"]
            .as_array()
            .expect("sessions")
            .iter()
            .find(|s| s["id"] == id.encode())
            .unwrap_or_else(|| panic!("{} is listed: {json}", id.encode()))
            .clone()
    };
    let (table, notes) = list_at(root, now, &since(7, now), false).await;
    assert!(
        row_of(&table, cut).contains("  incomplete"),
        "{table}\n{notes:?}"
    );
    assert!(
        row_of(&table, unknown).contains("format 99, needs newer heph"),
        "{table}"
    );
    let (json, _) = list_at(root, now, &since(7, now), true).await;
    let json: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert_eq!(session_of(&json, cut)["incomplete"], true, "{json}");
    assert_eq!(session_of(&json, unknown)["format"], 99, "{json}");
    assert_eq!(
        session_of(&json, unknown)["incomplete"],
        serde_json::Value::Null,
        "{json}"
    );

    // The recording gone, the catalog row alone still says it.
    std::fs::remove_dir_all(root.join(cut.encode())).expect("remove the recording");
    let (json, _) = list_at(root, now, &since(7, now), true).await;
    let json: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert_eq!(session_of(&json, cut)["incomplete"], true, "{json}");
    let (table, _) = list_at(root, now, &since(7, now), false).await;
    assert!(
        row_of(&table, cut).contains(" incomplete "),
        "the table agrees with --json:\n{table}"
    );
    let cat = crate::tui::session::catalog::open(root, now).expect("open again");
    assert_eq!(cat.heal.unknown, 1, "skipped again, never failed");
    assert_eq!(cat.heal.failed + cat.heal.deferred, 0, "{:?}", cat.heal);
}

/// `list` over the real catalog, as `execute` runs it: the ids it printed
/// (`--json`) and the hints it says on stderr after them.
async fn list_hints(
    root: &Path,
    now: u64,
    filter: &list::ListFilter,
    since: &str,
) -> (Vec<String>, Vec<String>) {
    let file = FileSource::new(root);
    let sources: [&dyn SessionSource; 1] = [&file];
    let catalog = list::read_catalog(root, now, filter, &mut |_| {});
    let mut out = Vec::new();
    let shown = list::list(
        &sources,
        ViewOpts {
            now_ms: now,
            ..opts()
        },
        Workspace::All,
        filter,
        catalog.as_ref(),
        true,
        &mut out,
    )
    .await
    .expect("list");
    let json: serde_json::Value = serde_json::from_slice(&out).expect("json");
    (
        json_ids(&json),
        list::hints(shown, filter, catalog.as_ref(), since, now),
    )
}

/// What `list` did not search is said on stderr: under `--addr`, the live
/// sessions (their targets reach the catalog when they end); and, when the
/// window matched nothing, how much further back the catalog reaches, as the
/// `--since` that reads it.
#[tokio::test]
async fn list_says_what_it_did_not_search() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let now = hcore::events::now_unix_ms();
    let old = session(root, now, 20 * DAY_MS, "run", ADDR, Did::Ran, &HUMAN);
    let reach = "heph: no session matched since 7d; the catalog holds sessions from up to 20 \
                 days ago (it keeps 90 days): --since 20d reads them\n";

    let (ids, hints) = list_hints(root, now, &since(7, now), "7d").await;
    assert!(ids.is_empty(), "{ids:?}");
    assert_eq!(hints, [reach]);
    let (ids, hints) = list_hints(root, now, &since(20, now), "20d").await;
    assert_eq!(ids, [old.encode()], "the hinted --since reads it");
    assert!(hints.is_empty(), "{hints:?}");

    let live_id = SessionId::new(now - 60_000, 9);
    let _live = write_live(
        root,
        &meta(live_id, 4242, &["heph", "run", ADDR]),
        &resolved(ADDR, now - 60_000, true, None),
        None,
    );
    let unsearched =
        "heph: --addr searches ended sessions only; 1 live session is not searched until it ends\n";
    let by_addr = |days| list::ListFilter {
        addr: Some(ADDR.to_owned()),
        ..since(days, now)
    };
    let (ids, hints) = list_hints(root, now, &by_addr(7), "7d").await;
    assert!(ids.is_empty(), "the live session is not searched: {ids:?}");
    assert_eq!(hints, [unsearched, reach]);
    let (ids, hints) = list_hints(root, now, &by_addr(30), "30d").await;
    assert_eq!(ids, [old.encode()]);
    assert_eq!(hints, [unsearched]);
    let (ids, hints) = list_hints(root, now, &since(7, now), "7d").await;
    assert_eq!(ids, [live_id.encode()], "without --addr it is listed");
    assert!(hints.is_empty(), "{hints:?}");
}
