//! The session catalog fed by the real engine: a session records the
//! `BuildEvent`s a real build sent, its seal writes the catalog's rows, and
//! `heph session list --addr`'s catalog query answers from them. The list
//! command's rendering of those rows is tested over synthetic recordings in
//! `src/commands/session/tests/list_catalog.rs`.

use crate::common::{build_engine_with_remote, run_drain_events};
use heph::engine::Engine;
use heph::engine::event::BuildEvent;
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::tui::session::{
    HeaderKind, Session, SessionHeader, SessionId, SessionRoot, SessionStart, catalog,
};
use std::path::Path;
use std::sync::Arc;

/// Run `addr` on `engine` inside a fresh session under `root`, feeding the
/// session every event the build sent, then end it as an exit 0 would.
async fn recorded_run(engine: &Arc<Engine>, root: &SessionRoot, addr: &str) -> SessionId {
    let session = Session::open(
        root,
        SessionStart {
            header: SessionHeader {
                kind: HeaderKind::Build,
                label: format!("run {addr}"),
            },
            argv: vec!["heph".into(), "run".into(), addr.into()],
            ..SessionStart::default()
        },
    )
    .expect("open a session");
    let (res, events): (_, Vec<BuildEvent>) =
        run_drain_events(engine, &Matcher::Addr(parse_addr(addr).expect("addr"))).await;
    res.expect("the recorded run builds");
    let tap = session.tap();
    for ev in events {
        tap.event(ev);
    }
    session.finish(Some(0), false);
    session.id()
}

/// Spec row `targets_answer_cache_hits` (D18) through the real engine: a
/// cold run executes `//p:t`, a warm one hits its cache, and a run of
/// another target never resolves it. `--addr //p:t` lists the two runs that
/// resolved it, newest first, as `hit` then `ran`, and not the third.
#[tokio::test]
async fn targets_answer_cache_hits() {
    let ws = tempfile::tempdir().expect("tempdir");
    let dir = ws.path().join("p");
    std::fs::create_dir_all(&dir).expect("create pkg dir");
    std::fs::write(
        dir.join("BUILD"),
        r#"
target(name = "t", driver = "bash", run = "echo t > $OUT", out = "t.txt")
target(name = "u", driver = "bash", run = "echo u > $OUT", out = "u.txt")
"#,
    )
    .expect("write BUILD");
    let user = tempfile::tempdir().expect("tempdir");
    let sessions = user.path().join("sessions");
    let root = SessionRoot::new(&sessions);
    let engine = build_engine_with_remote(ws.path(), None);

    let cold = recorded_run(&engine, &root, "//p:t").await;
    let warm = recorded_run(&engine, &root, "//p:t").await;
    let other = recorded_run(&engine, &root, "//p:u").await;

    let now = hcore::events::now_unix_ms();
    let cat = catalog::open(&sessions, now).expect("open the catalog");
    assert!(!cat.in_memory, "answers from the catalog: {:?}", cat.notes);
    let listed = |addr: Option<&str>| {
        catalog::list_sessions(
            &cat.conn,
            &catalog::ListQuery {
                addr,
                ..catalog::ListQuery::default()
            },
        )
        .expect("list sessions")
    };

    let all: Vec<String> = listed(None).into_iter().map(|l| l.session_id).collect();
    assert_eq!(
        all,
        [other.encode(), warm.encode(), cold.encode()],
        "every sealed session has its row"
    );

    let rows = listed(Some("//p:t"));
    let by_addr: Vec<(&str, &str, &str)> = rows
        .iter()
        .map(|l| {
            let target = l.target.as_ref().expect("a row for the target");
            (
                l.session_id.as_str(),
                target.addr.as_str(),
                target.outcome.as_str(),
            )
        })
        .collect();
    assert_eq!(
        by_addr,
        [
            (warm.encode().as_str(), "//p:t", "hit"),
            (cold.encode().as_str(), "//p:t", "ran"),
        ],
        "the warm run hit, the cold one ran, the other never resolved it"
    );
    let ran = rows
        .iter()
        .find(|l| l.session_id == cold.encode())
        .and_then(|l| l.target.as_ref())
        .expect("the cold run's row");
    assert!(
        ran.duration_us.is_some(),
        "a run that executed is timed: {ran:?}"
    );
    assert!(
        recording_kept(&sessions, cold),
        "the recording is kept beside its row"
    );
}

fn recording_kept(sessions: &Path, id: SessionId) -> bool {
    sessions.join(id.encode()).is_dir()
}
