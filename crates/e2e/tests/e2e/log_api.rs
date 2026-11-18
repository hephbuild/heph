//! The log API (`Engine::open_log` / `follow_log`): a target's log by
//! address, from whichever source is newest, followed live to its end,
//! across engines through the session records, and fetched from a remote
//! cache only when it is read.

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common::{build_engine, drain, run_drain};
use hcore::events::BuildEventKind;
use heph::engine::{
    Engine, FollowStep, LogFollower, LogOptions, LogSource, LogUnavailable, NoOutputYet, OpenLog,
    OutputMatcher, ResultOptions,
};
use heph::hartifactcontent::WalkEntryKind;
use heph::htaddr::parse_addr;
use heph::tui::session::{
    HeaderKind, RunLogRecord, SessionHeader, SessionId, SessionRecord, SessionRecordKind,
    SessionRoot, SessionRunLogLocator, SessionSlot,
};
use heph::{pluginbuildfile, pluginexec};
use std::io::{Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The `bash`-only engine over `root`, announcing its live logs to `slot`.
pub(crate) fn engine(root: &Path, slot: Option<&Arc<SessionSlot>>) -> anyhow::Result<Arc<Engine>> {
    let mut e = Engine::new(heph::engine::Config::for_tests(root.to_path_buf()))?;
    if let Some(slot) = slot {
        e.set_run_log_announcer(Arc::clone(slot) as _);
    }
    let r = root.to_path_buf();
    e.register_provider(move |init| {
        Box::new(pluginbuildfile::Provider::new(
            r,
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))?;
    Ok(Arc::new(e))
}

pub(crate) fn write_build(root: &Path, body: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(root.join("p"))?;
    std::fs::write(root.join("p").join("BUILD"), body)?;
    Ok(())
}

/// Build `addr`, dropping its result (it holds a read lock).
pub(crate) async fn build(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<()> {
    let rs = engine.new_state();
    let res = Arc::clone(engine)
        .result_addr(
            rs.clone(),
            &parse_addr(addr)?,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await
        .map(drop);
    drain(rs).await;
    res
}

/// `addr`'s newest log, read whole, with where it came from.
async fn open(
    engine: &Arc<Engine>,
    addr: &str,
    opts: &LogOptions,
) -> anyhow::Result<Option<(LogSource, String)>> {
    let Some(log) = engine.open_log(&parse_addr(addr)?, opts).await? else {
        return Ok(None);
    };
    Ok(Some(read_whole(log)?))
}

fn read_whole(log: OpenLog) -> anyhow::Result<(LogSource, String)> {
    let source = log.info().source.clone();
    let mut out = String::new();
    log.reader()?.read_to_string(&mut out)?;
    Ok((source, out))
}

/// The bytes of the cache entry's `Log` for `addr` at its current `hashin`.
async fn cached_log(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<String> {
    let addr = parse_addr(addr)?;
    let hashin = Arc::clone(engine)
        .meta(engine.new_state(), &addr)
        .await?
        .hashin;
    let log = engine.cached_log(&addr, &hashin)?.expect("a cached log");
    let mut out = Vec::new();
    for entry in log.walk()? {
        if let WalkEntryKind::File { mut data, .. } = entry?.kind {
            data.read_to_end(&mut out)?;
        }
    }
    Ok(String::from_utf8(out)?)
}

/// A failure slot's `log.txt`.
fn slot_log(engine: &Engine, addr: &str) -> String {
    let addr = parse_addr(addr).expect("addr");
    let slots = engine.failure_slots();
    slots.read(&addr).expect("a failure slot");
    std::fs::read_to_string(slots.slot_dir(&addr).join("log.txt")).expect("slot log")
}

/// Poll `f`, appending to `got`, until it ends (`true`) or `enough` holds
/// (`false`).
async fn poll_until(
    f: &mut LogFollower,
    got: &mut Vec<u8>,
    enough: impl Fn(&[u8]) -> bool,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match f.poll().expect("poll") {
            FollowStep::Bytes(b) => {
                assert!(!b.is_empty(), "an empty Bytes step");
                got.extend_from_slice(&b);
            }
            FollowStep::End => return true,
            FollowStep::Idle => {
                assert!(
                    Instant::now() < deadline,
                    "follow stalled at {:?}",
                    String::from_utf8_lossy(got)
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        if enough(got) {
            return false;
        }
    }
}

/// Wait until `addr`'s newest log is its run going on, holding `len` bytes:
/// the driver writes the live file after the target printed, so a marker
/// file the target writes does not say its bytes are in.
async fn wait_running(engine: &Arc<Engine>, addr: &str, len: usize) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        // Before its first byte, the run is "no output yet".
        let seen = match open(engine, addr, &LogOptions::default()).await {
            Err(e) if e.is::<NoOutputYet>() => None,
            seen => seen.expect("open"),
        };
        if let Some((LogSource::Live { running: true, .. }, text)) = &seen
            && text.len() >= len
        {
            return text.clone();
        }
        assert!(Instant::now() < deadline, "{addr} never ran: {seen:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A target that prints `before`, waits for its gate, prints `after` and
/// exits with `code`.
fn gated(name: &str, c: &Path, before: &str, after: &str, code: u8) -> String {
    let c = c.display();
    format!(
        r#"target(name = "{name}", driver = "bash", out = "{name}.txt", run = "{before}; while [ ! -e {c}/{name}.gate ]; do sleep 0.02; done; {after}; printf x > {name}.txt; exit {code}")
"#
    )
}

/// spec `log_api_picks_latest_source`: each row of "Where a log comes from"
/// (running, failed, succeeded, local hit, never ran), the newer of a
/// failure and a success, and a `cache = false` success's kept live file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn log_api_picks_latest_source() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let c = tempfile::tempdir()?;
    let root = ws.path();
    let flaky = |word: &str, code: u8| {
        format!(
            r#"target(name = "flaky", driver = "bash", out = "f.txt", run = "echo {word}; printf x > f.txt; exit {code}")
target(name = "nocache", driver = "bash", cache = False, out = "n.txt", run = "echo uncached; printf x > n.txt")
"#
        ) + &gated("run", c.path(), "echo running-1", "echo running-2", 0)
    };
    let none = LogOptions::default();

    write_build(root, &flaky("fail-1", 1))?;
    let e = engine(root, None)?;
    // Never ran: no log.
    assert_eq!(open(&e, "//p:flaky", &none).await?, None);
    assert_eq!(open(&e, "//p:never", &none).await?, None);

    // Running: its live file.
    let run = tokio::spawn({
        let e = Arc::clone(&e);
        async move { build(&e, "//p:run").await }
    });
    wait_running(&e, "//p:run", 1).await;
    let (src, text) = open(&e, "//p:run", &none).await?.expect("a running log");
    assert_eq!(
        src,
        LogSource::Live {
            running: true,
            session: None
        }
    );
    assert_eq!(text, "running-1\n");
    std::fs::write(c.path().join("run.gate"), "")?;
    run.await??;
    // Succeeded: the cache entry's log.
    assert_eq!(
        open(&e, "//p:run", &none).await?,
        Some((
            LogSource::Cache { remote: false },
            "running-1\nrunning-2\n".into()
        ))
    );

    // Failed: the slot.
    assert!(build(&e, "//p:flaky").await.is_err());
    assert_eq!(
        open(&e, "//p:flaky", &none).await?,
        Some((LogSource::FailureSlot, "fail-1\n".into()))
    );
    drop(e);

    // A failure followed by a success reads the success.
    write_build(root, &flaky("ok-2", 0))?;
    let e = engine(root, None)?;
    build(&e, "//p:flaky").await?;
    assert_eq!(
        open(&e, "//p:flaky", &none).await?,
        Some((LogSource::Cache { remote: false }, "ok-2\n".into()))
    );
    drop(e);

    // A success followed by a failure reads the failure.
    write_build(root, &flaky("fail-3", 1))?;
    let e = engine(root, None)?;
    assert!(build(&e, "//p:flaky").await.is_err());
    assert_eq!(
        open(&e, "//p:flaky", &none).await?,
        Some((LogSource::FailureSlot, "fail-3\n".into()))
    );
    drop(e);

    // A local hit of the earlier success reads its entry again.
    write_build(root, &flaky("ok-2", 0))?;
    let e = engine(root, None)?;
    build(&e, "//p:flaky").await?;
    assert_eq!(
        open(&e, "//p:flaky", &none).await?,
        Some((LogSource::Cache { remote: false }, "ok-2\n".into()))
    );

    // `cache = false`: the kept live file, while its heph lives ...
    build(&e, "//p:nocache").await?;
    assert_eq!(
        open(&e, "//p:nocache", &none).await?,
        Some((
            LogSource::Live {
                running: false,
                session: None
            },
            "uncached\n".into()
        ))
    );
    drop(e);
    // ... and no log once it is swept.
    let e = engine(root, None)?;
    assert_eq!(open(&e, "//p:nocache", &none).await?, None);
    Ok(())
}

/// spec `log_api_follow_ends_with_run`: following a running target yields
/// its chunks in order and ends when the run ends; on a pending one it waits
/// for the start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn log_api_follow_ends_with_run() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let c = tempfile::tempdir()?;
    write_build(
        ws.path(),
        &(gated("pending", c.path(), "echo one", "echo two", 0)
            + &gated("running", c.path(), "echo three", "echo four", 0)),
    )?;
    let e = engine(ws.path(), None)?;
    let none = LogOptions::default();

    // Pending: nothing to read yet, and nothing without waiting.
    let pending = parse_addr("//p:pending")?;
    assert!(e.follow_log(&pending, &none, false).await?.is_none());
    let mut f = e
        .follow_log(&pending, &none, true)
        .await?
        .expect("a follower");
    let mut got = Vec::new();
    assert_eq!(f.poll()?, FollowStep::Idle);
    assert!(f.info().is_none(), "waiting: {:?}", f.info());
    let run = tokio::spawn({
        let e = Arc::clone(&e);
        async move { build(&e, "//p:pending").await }
    });
    assert!(!poll_until(&mut f, &mut got, |g| g == b"one\n").await);
    assert!(matches!(
        f.info().map(|i| i.source),
        Some(LogSource::Live { running: true, .. })
    ));
    std::fs::write(c.path().join("pending.gate"), "")?;
    assert!(poll_until(&mut f, &mut got, |_| false).await);
    assert_eq!(String::from_utf8(got)?, "one\ntwo\n");
    run.await??;

    // Running: from its first byte to its end.
    let run = tokio::spawn({
        let e = Arc::clone(&e);
        async move { build(&e, "//p:running").await }
    });
    wait_running(&e, "//p:running", 1).await;
    let mut f = e
        .follow_log(&parse_addr("//p:running")?, &none, false)
        .await?
        .expect("a follower");
    let mut got = Vec::new();
    assert!(!poll_until(&mut f, &mut got, |g| g == b"three\n").await);
    std::fs::write(c.path().join("running.gate"), "")?;
    assert!(poll_until(&mut f, &mut got, |_| false).await);
    assert_eq!(String::from_utf8(got)?, "three\nfour\n");
    assert_eq!(f.poll()?, FollowStep::End, "an ended follower stays ended");
    run.await??;
    Ok(())
}

/// Wait for a file the target touches once it is running.
async fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A run of this engine that has written nothing is "no output yet", with
/// no session at all: the engine knows its own runs, so neither
/// `HEPH_NO_SESSION` nor a session not found reads it as "has not run".
/// Followed, it waits for the first byte, or ends with a run that writes
/// none; once over, the old answers hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn own_quiet_run_is_started_without_a_session() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let c = tempfile::tempdir()?;
    let cp = c.path().display();
    write_build(
        ws.path(),
        &format!(
            r#"target(name = "quiet", driver = "bash", out = "q.txt", run = "touch {cp}/quiet.started; while [ ! -e {cp}/quiet.gate ]; do sleep 0.02; done; echo first; while [ ! -e {cp}/quiet.gate2 ]; do sleep 0.02; done; echo second; printf x > q.txt")
target(name = "silent", driver = "bash", out = "s.txt", run = "touch {cp}/silent.started; while [ ! -e {cp}/silent.gate ]; do sleep 0.02; done; printf x > s.txt")
"#
        ),
    )?;
    // No announcer, no locator: no session anywhere.
    let e = engine(ws.path(), None)?;
    let none = LogOptions::default();

    // Running, nothing written: "no output yet", here and at its own
    // hashin, never at another one.
    let quiet = parse_addr("//p:quiet")?;
    let run = tokio::spawn({
        let e = Arc::clone(&e);
        async move { build(&e, "//p:quiet").await }
    });
    wait_file(&c.path().join("quiet.started")).await;
    let err = e
        .open_log(&quiet, &none)
        .await
        .expect_err("running, nothing written");
    let yet = err.downcast_ref::<NoOutputYet>().expect("NoOutputYet");
    assert_eq!(yet.session, None, "this engine's own run");
    assert!(err.to_string().contains("in this heph"), "{err}");
    let hashin = Arc::clone(&e).meta(e.new_state(), &quiet).await?.hashin;
    let at = |h: &str| LogOptions {
        hashin: Some(h.to_owned()),
        ..LogOptions::default()
    };
    assert!(
        e.open_log(&quiet, &at(&hashin))
            .await
            .is_err_and(|e| e.is::<NoOutputYet>())
    );
    assert!(e.open_log(&quiet, &at("other")).await?.is_none());

    // Followed: waits for the first byte, then reads the run to its end.
    let mut f = e
        .follow_log(&quiet, &none, false)
        .await?
        .expect("a follower");
    assert!(f.info().is_none(), "nothing to read yet: {:?}", f.info());
    assert!(f.started().is_some_and(|s| s.session.is_none()));
    assert_eq!(f.started_in(), None);
    assert_eq!(f.poll()?, FollowStep::Idle);
    std::fs::write(c.path().join("quiet.gate"), "")?;
    let mut got = Vec::new();
    assert!(!poll_until(&mut f, &mut got, |g| g == b"first\n").await);
    assert!(matches!(
        f.info().map(|i| i.source),
        Some(LogSource::Live {
            running: true,
            session: None
        })
    ));
    std::fs::write(c.path().join("quiet.gate2"), "")?;
    assert!(poll_until(&mut f, &mut got, |_| false).await);
    assert_eq!(String::from_utf8(got)?, "first\nsecond\n");
    run.await??;
    // Over: its cache entry's log.
    assert_eq!(
        open(&e, "//p:quiet", &none).await?,
        Some((LogSource::Cache { remote: false }, "first\nsecond\n".into()))
    );

    // A run that ends without a byte ends its follower, and leaves no log.
    let silent = parse_addr("//p:silent")?;
    let run = tokio::spawn({
        let e = Arc::clone(&e);
        async move { build(&e, "//p:silent").await }
    });
    wait_file(&c.path().join("silent.started")).await;
    assert!(
        e.open_log(&silent, &none)
            .await
            .is_err_and(|e| e.is::<NoOutputYet>())
    );
    let mut f = e
        .follow_log(&silent, &none, false)
        .await?
        .expect("a follower");
    assert_eq!(f.poll()?, FollowStep::Idle);
    std::fs::write(c.path().join("silent.gate"), "")?;
    let mut got = Vec::new();
    assert!(poll_until(&mut f, &mut got, |_| false).await);
    assert!(got.is_empty(), "{:?}", String::from_utf8_lossy(&got));
    run.await??;
    assert_eq!(open(&e, "//p:silent", &none).await?, None);
    assert!(e.follow_log(&silent, &none, false).await?.is_none());
    Ok(())
}

/// Lines a target prints: more than one follower chunk (64 KiB).
fn many_lines() -> String {
    (0..3000).fold(String::new(), |mut s, i| {
        s.push_str(&format!("line-{i:05}-{}\n", "x".repeat(60)));
        s
    })
}

/// A gated target printing [`many_lines`] before its gate.
fn chatty(name: &str, c: &Path, code: u8) -> String {
    let c = c.display();
    let x = "x".repeat(60);
    format!(
        r#"target(name = "{name}", driver = "bash", out = "{name}.txt", run = "for ((i=0;i<3000;i++)); do printf 'line-%05d-{x}\n' $i; done; while [ ! -e {c}/{name}.gate ]; do sleep 0.02; done; echo end; printf x > {name}.txt; exit {code}")
"#
    )
}

/// spec `follow_survives_live_file_unlink`: a follower mid-read when the run
/// ends and its live file is unlinked reads to the end, then what replaced
/// it by the run's hashin (the entry on success, the slot on failure), with
/// no gap, no repeated byte and no "no log". A follower that opens the
/// record after the unlink, before the end record lands, gets the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn follow_survives_live_file_unlink() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let c = tempfile::tempdir()?;
    let user = tempfile::tempdir()?;
    write_build(
        ws.path(),
        &(chatty("ok", c.path(), 0) + &chatty("bad", c.path(), 1)),
    )?;
    let (slot, sessions) = session_slot(user.path());
    let e = engine(ws.path(), Some(&slot))?;
    open_session(&slot, &e, ws.path());
    let expected = many_lines() + "end\n";
    let none = LogOptions::default();

    for (name, ok) in [("ok", true), ("bad", false)] {
        let addr = format!("//p:{name}");
        let run = tokio::spawn({
            let (e, addr) = (Arc::clone(&e), addr.clone());
            async move { build(&e, &addr).await }
        });
        wait_running(&e, &addr, many_lines().len()).await;
        let mut f = e
            .follow_log(&parse_addr(&addr)?, &none, false)
            .await?
            .expect("a follower");
        let mut got = Vec::new();
        // Mid-read: one chunk of several.
        assert!(!poll_until(&mut f, &mut got, |g| !g.is_empty()).await);
        assert!(got.len() < expected.len());
        std::fs::write(c.path().join(format!("{name}.gate")), "")?;
        assert_eq!(run.await?.is_ok(), ok);
        // The run is over and its live file unlinked.
        let left = walk_logs(&e.home.join("runs"));
        assert!(left.is_empty(), "{left:?}");
        assert!(poll_until(&mut f, &mut got, |_| false).await);
        assert_eq!(String::from_utf8(got)?, expected, "{name}: gap or repeat");
        let stored = if ok {
            cached_log(&e, &addr).await?
        } else {
            slot_log(&e, &addr)
        };
        assert_eq!(stored, expected, "{name}: what replaced the live file");

        // Opened after the unlink, the record still Live in the session (no
        // end recorded here: this slot runs no recorder of build events).
        let other = engine(ws.path(), None)?;
        let located = LogOptions {
            locator: Some(Arc::new(SessionRunLogLocator::new(sessions.clone()))),
            ..LogOptions::default()
        };
        let mut f = other
            .follow_log(&parse_addr(&addr)?, &located, false)
            .await?
            .expect("a follower");
        let mut got = Vec::new();
        assert!(poll_until(&mut f, &mut got, |_| false).await);
        assert_eq!(
            String::from_utf8(got)?,
            expected,
            "{name}: after the unlink"
        );
        let (src, text) = open(&other, &addr, &located).await?.expect("a log");
        let want = if ok {
            LogSource::Cache { remote: false }
        } else {
            LogSource::FailureSlot
        };
        assert_eq!((src, text.as_str()), (want, expected.as_str()));
    }
    slot.finish(Some(0));
    Ok(())
}

/// Every `*.log` under `dir`, one level of subdirectories down.
fn walk_logs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|d| std::fs::read_dir(d.path()).into_iter().flatten().flatten())
        .map(|f| f.path())
        .filter(|p| p.extension().is_some_and(|e| e == "log"))
        .collect()
}

/// A session slot recording under `user`'s heph dir, and that root.
fn session_slot(user: &Path) -> (Arc<SessionSlot>, SessionRoot) {
    let slot = Arc::new(SessionSlot::new());
    let root = SessionRoot::from_env(None, Some(user.as_os_str()));
    let sessions = root.clone().expect("root").expect("recording on");
    slot.configure(root, vec!["heph".into()]);
    (slot, sessions)
}

/// Open `slot`'s session for `engine`'s workspace.
fn open_session(slot: &SessionSlot, engine: &Engine, ws: &Path) -> SessionId {
    slot.set_workspace(ws, ws, &engine.home);
    slot.open(
        SessionHeader {
            kind: HeaderKind::Build,
            label: "run".into(),
        },
        false,
    )
    .expect("a session opened")
    .id
}

/// Copy the session directory `from` under `sessions` as session `id`: what
/// a process that died with that session open leaves (its lock free).
fn copy_session(from: &Path, sessions: &SessionRoot, id: SessionId) -> PathBuf {
    let to = sessions.path().join(id.encode());
    std::fs::create_dir_all(&to).expect("session dir");
    for f in std::fs::read_dir(from).expect("read session").flatten() {
        if f.file_type().expect("type").is_file() {
            std::fs::copy(f.path(), to.join(f.file_name())).expect("copy");
        }
    }
    to
}

/// Hold `dir`'s session lock, as its live owner does.
fn hold_lock(dir: &Path) -> std::fs::File {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(heph::tui::session::files::LOCK))
        .expect("lock file");
    // SAFETY: `f` is an open file descriptor for the call.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "flock");
    f
}

fn append_record(dir: &Path, record: &SessionRecord) {
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join(heph::tui::session::files::EVENTS))
        .expect("events");
    writeln!(f, "{}", serde_json::to_string(record).expect("encode")).expect("write");
}

/// spec `log_follow_across_engines`: two engines on one home, each with a
/// session. The second follows a run in the first to its end through the
/// first's `RunLog` record; a record left by a killed process (lock free)
/// reads as absent; a session whose `meta.json` names another workspace is
/// ignored; a live session that recorded no `RunLog` reads as no log.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn log_follow_across_engines() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let elsewhere = tempfile::tempdir()?;
    let c = tempfile::tempdir()?;
    let user = tempfile::tempdir()?;
    write_build(ws.path(), &gated("g", c.path(), "echo one", "echo two", 0))?;

    let (slot_a, sessions) = session_slot(user.path());
    let a = engine(ws.path(), Some(&slot_a))?;
    let id_a = open_session(&slot_a, &a, ws.path());
    let (slot_b, _) = session_slot(user.path());
    let b = engine(ws.path(), Some(&slot_b))?;
    open_session(&slot_b, &b, ws.path());
    let located = LogOptions {
        locator: Some(Arc::new(SessionRunLogLocator::new(sessions.clone()))),
        ..LogOptions::default()
    };

    // B follows A's run, which B's engine knows nothing of, to its end.
    let g = parse_addr("//p:g")?;
    let mut f = b.follow_log(&g, &located, true).await?.expect("a follower");
    let run = tokio::spawn({
        let a = Arc::clone(&a);
        async move { build(&a, "//p:g").await }
    });
    let mut got = Vec::new();
    assert!(!poll_until(&mut f, &mut got, |g| g == b"one\n").await);
    assert_eq!(
        f.info().map(|i| i.source),
        Some(LogSource::Live {
            running: true,
            session: Some(id_a.encode())
        })
    );
    std::fs::write(c.path().join("g.gate"), "")?;
    assert!(poll_until(&mut f, &mut got, |_| false).await);
    assert_eq!(String::from_utf8(got)?, "one\ntwo\n");
    run.await??;

    // A killed process's record: a live file and a record of it, in a
    // session whose lock nobody holds.
    let dir_a = slot_a.dir().expect("a's session dir");
    let dead = copy_session(&dir_a, &sessions, SessionId::new(1_700_000_000_000, 7));
    std::fs::create_dir_all(a.home.join("runs/dead"))?;
    std::fs::write(a.home.join("runs/dead/0.log"), "dead\n")?;
    append_record(
        &dead,
        &SessionRecord::session(
            hcore::events::now_unix_ms(),
            SessionRecordKind::RunLog(Box::new(RunLogRecord {
                addr: "//p:dead".into(),
                hashin: "h-dead".into(),
                file: "runs/dead/0.log".into(),
            })),
        ),
    );
    assert_eq!(open(&b, "//p:dead", &located).await?, None, "lock free");
    {
        // The same record under a held lock is followed: the lock decides.
        let _held = hold_lock(&dead);
        assert_eq!(
            open(&b, "//p:dead", &located).await?,
            Some((
                LogSource::Live {
                    running: true,
                    session: Some(SessionId::new(1_700_000_000_000, 7).encode())
                },
                "dead\n".into()
            ))
        );
    }

    // Another workspace's session, recording a file that exists here.
    let (slot_c, _) = session_slot(user.path());
    let other = engine(elsewhere.path(), Some(&slot_c))?;
    open_session(&slot_c, &other, elsewhere.path());
    std::fs::create_dir_all(a.home.join("runs/c"))?;
    std::fs::write(a.home.join("runs/c/0.log"), "elsewhere\n")?;
    hcore::session_record::RunLogAnnouncer::run_log_created(
        slot_c.as_ref(),
        RunLogRecord {
            addr: "//p:elsewhere".into(),
            hashin: "h-c".into(),
            file: "runs/c/0.log".into(),
        },
    );
    let events_c = slot_c
        .dir()
        .expect("c's dir")
        .join(heph::tui::session::files::EVENTS);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !std::fs::read_to_string(&events_c)
        .unwrap_or_default()
        .contains("//p:elsewhere")
    {
        assert!(Instant::now() < deadline, "c's record never landed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(open(&b, "//p:elsewhere", &located).await?, None);

    // A live session of this workspace that recorded no `RunLog` (an older
    // heph's): nothing of it is followed, and no log reads as no log.
    let old = copy_session(&dir_a, &sessions, SessionId::new(1_700_000_000_001, 8));
    let held = hold_lock(&old);
    assert_eq!(open(&b, "//p:never", &located).await?, None);
    assert!(
        b.follow_log(&parse_addr("//p:never")?, &located, false)
            .await?
            .is_none()
    );
    // A log heph holds is read beside it.
    assert_eq!(
        open(&b, "//p:g", &located).await?,
        Some((LogSource::Cache { remote: false }, "one\ntwo\n".into()))
    );
    // A run going on in A that has written nothing yet: "no output yet".
    let quiet = parse_addr("//p:quiet")?;
    let execute = |kind| SessionRecord {
        at_unix_ms: hcore::events::now_unix_ms(),
        kind: Some(kind),
        session: None,
    };
    append_record(
        &dir_a,
        &execute(BuildEventKind::ExecuteStart {
            addr: quiet.format(),
            driver: "bash".into(),
            cache: true,
            declared: None,
            reserved: None,
            queued_ms: None,
            admitted: None,
            output_groups: Vec::new(),
        }),
    );
    let err = b
        .open_log(&quiet, &located)
        .await
        .expect_err("running, nothing written");
    let yet = err.downcast_ref::<NoOutputYet>().expect("NoOutputYet");
    assert_eq!(yet.session, Some(id_a.encode()));
    // The dashboard's Log tab follows it: the pane says "no output yet",
    // never "unavailable".
    let screen = heph::commands::log::EngineTargetReader::with_locator(
        Arc::clone(&b),
        tokio::runtime::Handle::current(),
        located.locator.clone(),
    );
    let shown = tokio::task::spawn_blocking(move || {
        use heph::tui::dashboard::{FeedStep, LogOpened, TargetReader as _, logpane::LogPane};
        match screen.open_log("//p:quiet") {
            LogOpened::Follow { mut feed, about } => {
                assert!(matches!(feed.poll(), FeedStep::Idle), "nothing written");
                let mut pane = LogPane::default();
                pane.start_live(about);
                Ok(pane
                    .body(40, 3, &Default::default())
                    .iter()
                    .map(|l| l.to_string().trim().to_owned())
                    .collect::<Vec<_>>())
            }
            other => Err(anyhow::anyhow!("not followed: {other:?}")),
        }
    })
    .await??;
    assert_eq!(shown, ["no output yet"]);
    // Followed, it waits for the run's first byte, and ends with the run
    // when it writes none.
    let mut f = b
        .follow_log(&quiet, &located, false)
        .await?
        .expect("a follower");
    assert_eq!(f.started_in(), Some(id_a.encode().as_str()));
    assert_eq!(f.poll()?, FollowStep::Idle);
    append_record(
        &dir_a,
        &execute(BuildEventKind::ExecuteEnd {
            addr: quiet.format(),
            error: None,
        }),
    );
    let mut got = Vec::new();
    assert!(poll_until(&mut f, &mut got, |_| false).await);
    assert!(got.is_empty());
    assert_eq!(
        open(&b, "//p:quiet", &located).await?,
        None,
        "the run over without a byte, there is no log"
    );
    drop(held);
    assert_eq!(open(&b, "//p:never", &located).await?, None);

    slot_a.finish(Some(0));
    slot_b.finish(Some(0));
    slot_c.finish(Some(0));
    Ok(())
}

/// spec `log_remote_fetch_on_demand`: after a remote hit, opening the log
/// fetches the entry's `Log` artifact once; a build that never opens it
/// downloads no log; a remote manifest that differs from the local mirrored
/// one is "log unavailable" and writes nothing. (That an `in_place` fixpoint
/// copies the `Log` blob only when present is
/// `duplicate_cache_entry_copies_log_only_when_present`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn log_remote_fetch_on_demand() -> anyhow::Result<()> {
    let remote = tempfile::tempdir()?;
    let uri = format!("file://{}", remote.path().display());
    let (producer, reader, stale) = (
        tempfile::tempdir()?,
        tempfile::tempdir()?,
        tempfile::tempdir()?,
    );
    let body = r#"target(name = "t", driver = "bash", out = "t.txt", run = "echo log-$RANDOM$RANDOM; echo $RANDOM$RANDOM > t.txt")"#;
    for ws in [&producer, &reader, &stale] {
        write_build(ws.path(), body)?;
    }
    let cold = build_engine(producer.path(), &uri);
    let out = run_drain(&cold, "//p:t").await;
    let log = cached_log(&cold, "//p:t").await?;
    assert!(log.starts_with("log-"), "{log:?}");
    drop(cold);

    // Remote hits, in two other workspaces.
    let warm = build_engine(reader.path(), &uri);
    assert_eq!(run_drain(&warm, "//p:t").await, out, "a remote hit");
    let warm_stale = build_engine(stale.path(), &uri);
    assert_eq!(run_drain(&warm_stale, "//p:t").await, out, "a remote hit");

    // The build downloaded no log: not resident, and not fetched unasked.
    let unavailable = |e: anyhow::Error| -> (LogSource, String) {
        let u = e.downcast::<LogUnavailable>().expect("log unavailable");
        (u.info.source, u.reason)
    };
    let no_fetch = LogOptions::default();
    let fetch = LogOptions {
        fetch_remote: true,
        ..LogOptions::default()
    };
    let err = open(&warm, "//p:t", &no_fetch)
        .await
        .expect_err("not fetched");
    assert_eq!(unavailable(err).0, LogSource::Cache { remote: true });
    // The dashboard's LAST LINE column over the remote-hit row (what a
    // scroll over it does, on the reader's thread) fetches nothing.
    let tails =
        heph::commands::log::EngineTails::new(Arc::clone(&warm), tokio::runtime::Handle::current());
    let (size, tail) = tokio::task::spawn_blocking(move || {
        use heph::tui::dashboard::LogReader as _;
        (tails.size("//p:t"), tails.tail("//p:t", 4096))
    })
    .await?;
    assert_eq!(
        (size, tail),
        (None, None),
        "the column reads local logs only"
    );
    let err = open(&warm, "//p:t", &no_fetch)
        .await
        .expect_err("still not fetched");
    assert_eq!(unavailable(err).0, LogSource::Cache { remote: true });

    // The target screen's Log tab: fetched, once; the next open reads the
    // local copy.
    let screen = heph::commands::log::EngineTargetReader::new(
        Arc::clone(&warm),
        tokio::runtime::Handle::current(),
    );
    let opened = tokio::task::spawn_blocking(move || {
        use heph::tui::dashboard::{LogOpened, TargetReader as _};
        match screen.open_log("//p:t") {
            LogOpened::Tail { bytes, len, about } => Ok((bytes, len, about)),
            other => Err(anyhow::anyhow!("not a tail: {other:?}")),
        }
    })
    .await??;
    assert_eq!(
        opened,
        (
            log.clone().into_bytes(),
            log.len() as u64,
            "from the remote cache".to_owned()
        )
    );
    assert_eq!(
        open(&warm, "//p:t", &no_fetch).await?,
        Some((LogSource::Cache { remote: false }, log.clone()))
    );

    // The remote entry replaced by another run of the same inputs: the
    // stale mirror's log is unavailable, and nothing is written.
    std::fs::remove_dir_all(remote.path())?;
    std::fs::create_dir_all(remote.path())?;
    std::fs::remove_dir_all(heph::engine::HomeDir::for_tests(producer.path()).join("cache"))?;
    let again = build_engine(producer.path(), &uri);
    assert_ne!(run_drain(&again, "//p:t").await, out, "re-executed");
    drop(again);
    let err = open(&warm_stale, "//p:t", &fetch)
        .await
        .expect_err("a differing manifest");
    let (src, reason) = unavailable(err);
    assert_eq!(src, LogSource::Cache { remote: true });
    assert!(reason.contains("not the one"), "{reason}");
    let err = open(&warm_stale, "//p:t", &no_fetch)
        .await
        .expect_err("nothing written");
    assert_eq!(unavailable(err).0, LogSource::Cache { remote: true });
    Ok(())
}

/// A follow waiting for the next run, which turns out to be another
/// engine's remote hit: its log is not on disk and never gets there by
/// itself. A follow that may fetch reads it through; one that may not fails
/// with `LogUnavailable` rather than wait on it forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_waiting_follow_that_ends_in_a_remote_hit_fetches_or_fails() -> anyhow::Result<()> {
    let remote = tempfile::tempdir()?;
    let uri = format!("file://{}", remote.path().display());
    let (producer, reader) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let body = r#"target(name = "t", driver = "bash", out = "t.txt", run = "echo remote-log; echo x > t.txt")"#;
    for ws in [&producer, &reader] {
        write_build(ws.path(), body)?;
    }
    let cold = build_engine(producer.path(), &uri);
    run_drain(&cold, "//p:t").await;
    drop(cold);

    let t = parse_addr("//p:t")?;
    let follower = build_engine(reader.path(), &uri);
    let fetch = LogOptions {
        fetch_remote: true,
        ..LogOptions::default()
    };
    let mut fetching = follower
        .follow_log(&t, &fetch, true)
        .await?
        .expect("a follower");
    let mut refusing = follower
        .follow_log(&t, &LogOptions::default(), true)
        .await?
        .expect("a follower");
    assert_eq!(fetching.poll()?, FollowStep::Idle, "no run yet");
    assert_eq!(refusing.poll()?, FollowStep::Idle, "no run yet");

    let builder = build_engine(reader.path(), &uri);
    run_drain(&builder, "//p:t").await;

    // The refusing follow first: once the other fetched, the log is local.
    let deadline = Instant::now() + Duration::from_secs(60);
    let err = loop {
        match refusing.poll() {
            Err(e) => break e,
            Ok(FollowStep::Idle) => {
                assert!(
                    Instant::now() < deadline,
                    "the follow waits on a remote log"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok(step) => panic!("a remote log read without fetching: {step:?}"),
        }
    };
    let unavailable = err
        .downcast::<LogUnavailable>()
        .map_err(|e| e.context("not LogUnavailable"))?;
    assert_eq!(unavailable.info.source, LogSource::Cache { remote: true });

    let mut got = Vec::new();
    assert!(poll_until(&mut fetching, &mut got, |_| false).await);
    assert_eq!(String::from_utf8(got)?, "remote-log\n");
    assert_eq!(
        fetching.info(),
        None,
        "done: the fetched log was read through"
    );
    Ok(())
}
