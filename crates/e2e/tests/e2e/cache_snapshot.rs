//! The working-set journal end to end: which engines record, what they
//! record from each cache tier, and that the trailer follows the last entry.
//!
//! The snapshot tier, the export and the `snapshot` command are not here yet;
//! these tests drive the journal through `Config::record_working_set` and
//! read it back with `cache_snapshot::{list_journals, read_journal}`.

use crate::common::{artifact_string, drain};
use futures::TryStreamExt as _;
use hcore::events::CacheOutcomeKind;
use heph::engine::cache_snapshot::{
    JournalContents, WorkingSet, list_journals, read_journal, working_set_dir,
};
use heph::engine::event::{BuildEvent, BuildEventKind};
use heph::engine::{Config, Discovery, Engine, HomeDir, OutputMatcher, ResultOptions};
use heph::htaddr::{Addr, parse_addr};
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use heph::{pluginbuildfile, pluginexec};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// An engine over `root` — buildfile provider, bash driver — with an
/// optional `file://` remote, recording its working set when `record`.
fn engine(root: &Path, remote: Option<&Path>, record: bool) -> Arc<Engine> {
    let mut e = Engine::new(Config {
        remote_caches: remote
            .map(|dir| heph::engine::RemoteCacheDef {
                name: "shared".to_string(),
                uri: format!("file://{}", dir.display()),
                read: true,
                write: true,
                concurrency: 10,
                endpoint: None,
                region: None,
            })
            .into_iter()
            .collect(),
        record_working_set: record,
        ..Config::for_tests(root)
    })
    .expect("engine");
    e.register_provider(|init| {
        Box::new(pluginbuildfile::Provider::new(
            init.root.to_path_buf(),
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })
    .expect("register buildfile provider");
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))
        .expect("register bash driver");
    Arc::new(e)
}

fn home(root: &Path) -> HomeDir {
    HomeDir::for_tests(root)
}

fn write_build(root: &Path, body: &str) {
    std::fs::create_dir_all(root.join("pkg")).expect("mkdir pkg");
    std::fs::write(root.join("pkg").join("BUILD"), body).expect("write BUILD");
}

fn addr(s: &str) -> Addr {
    parse_addr(s).expect("parse addr")
}

/// Resolve `addrs` in one request and release it, draining its background
/// work. Returns every event it emitted.
async fn run(engine: &Arc<Engine>, addrs: &[&str]) -> Vec<BuildEvent> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = engine.new_state_with_events(true, Some(tx));
    for a in addrs {
        let res = Arc::clone(engine)
            .result_addr(
                rs.clone(),
                &addr(a),
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("build");
        drop(artifact_string(&res));
    }
    drain(rs).await;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    events
}

/// Every target's `CacheOutcome` in `events`, as `(addr, outcome)`.
fn outcomes(events: &[BuildEvent]) -> Vec<(String, CacheOutcomeKind)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::CacheOutcome { addr, outcome } => Some((addr.clone(), *outcome)),
            _ => None,
        })
        .collect()
}

/// `addr`'s `hashin`, from an engine that records nothing.
async fn hashin(root: &Path, a: &str) -> String {
    let e = engine(root, None, false);
    Arc::clone(&e)
        .meta(e.new_state(), &addr(a))
        .await
        .expect("meta")
        .hashin
}

/// Read every journal under `root`'s home once each has its trailer (the
/// last reference to a dropped engine's journal can go a moment after its
/// requests drain).
async fn read_settled(root: &Path, expect: usize) -> Vec<JournalContents> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let journals: Vec<JournalContents> = list_journals(&home(root))
            .expect("list journals")
            .iter()
            .map(|p| read_journal(p).expect("read journal"))
            .collect();
        if journals.len() == expect && journals.iter().all(|j| j.incomplete.is_none()) {
            return journals;
        }
        assert!(
            Instant::now() < deadline,
            "expected {expect} complete journals, got {journals:#?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn recorded(j: &[JournalContents]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = WorkingSet::from_journals(j.iter().cloned())
        .revisions
        .into_iter()
        .map(|r| (r.addr.format(), r.hashin))
        .collect();
    out.sort();
    out
}

const TWO: &str = r#"
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "a.txt")
target(name = "b", driver = "bash", run = "echo b > $OUT", out = "b.txt")
target(name = "c", driver = "bash", run = "echo c > $OUT", out = "c.txt")
"#;

/// D13: no file until a request records. A recording engine that only
/// queries, or resolves only an uncacheable target, and an engine that does
/// not record (a helper, the `snapshot` command's) building a cacheable one,
/// all leave `working-set/` absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_engine_that_never_records_leaves_no_journal() {
    let root = tempfile::tempdir().expect("root");
    write_build(
        root.path(),
        r#"
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "a.txt")
target(name = "tmp", driver = "bash", run = "echo t > $OUT", out = "t.txt", cache = False)
"#,
    );

    // Query only.
    let q = engine(root.path(), None, true);
    let addrs: Vec<Addr> = Arc::clone(&q)
        .query(
            q.new_state(),
            &Matcher::PackagePrefix(PkgBuf::from("")),
            Discovery::Complete,
        )
        .try_collect()
        .await
        .expect("query");
    assert_eq!(addrs.len(), 2);
    // A build of only an uncacheable target.
    run(&q, &["//pkg:tmp"]).await;
    drop(q);

    // A non-recording engine building a cacheable target.
    let helper = engine(root.path(), None, false);
    run(&helper, &["//pkg:a"]).await;
    drop(helper);

    assert!(
        !working_set_dir(&home(root.path())).exists(),
        "no engine here recorded, so no journal may exist"
    );
}

/// D13: with the engine dropped right after its last request — no drain
/// first, as a process exiting — every entry and the `End` line precede the
/// trailer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entries_of_the_last_request_precede_the_trailer() {
    let root = tempfile::tempdir().expect("root");
    write_build(root.path(), TWO);
    let e = engine(root.path(), None, true);
    let rs = e.new_state();
    for a in ["//pkg:a", "//pkg:b", "//pkg:c"] {
        Arc::clone(&e)
            .result_addr(
                rs.clone(),
                &addr(a),
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("build");
    }
    let path = e
        .working_set_journal()
        .and_then(|j| j.path())
        .expect("the first recording request created the journal")
        .to_path_buf();
    let bg = rs.bg_pending();
    drop(rs);
    drop(e);

    let journals = read_settled(root.path(), 1).await;
    assert_eq!(journals[0].path, path);
    assert_eq!(journals[0].revisions.len(), 3);
    assert_eq!(bg.load(Ordering::Acquire), 0);

    let text = std::fs::read_to_string(&path).expect("read journal");
    let tags: Vec<&str> = text
        .lines()
        .skip(1)
        .map(|l| {
            l.split("\"t\":\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap_or("?")
        })
        .collect();
    assert_eq!(
        tags,
        ["Revision", "Revision", "Revision", "End", "Trailer"],
        "{text}"
    );
}

/// Two engines in one process (same pid) get two journals, each locked by
/// its own file; their working sets are disjoint and both are read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_engines_in_one_process_get_two_journals() {
    let root = tempfile::tempdir().expect("root");
    write_build(root.path(), TWO);
    let e1 = engine(root.path(), None, true);
    let e2 = engine(root.path(), None, true);
    run(&e1, &["//pkg:a"]).await;
    run(&e2, &["//pkg:b"]).await;
    let (p1, p2) = (
        e1.working_set_journal()
            .and_then(|j| j.path())
            .map(Path::to_path_buf),
        e2.working_set_journal()
            .and_then(|j| j.path())
            .map(Path::to_path_buf),
    );
    assert!(p1.is_some() && p2.is_some() && p1 != p2, "{p1:?} {p2:?}");
    drop((e1, e2));

    let journals = read_settled(root.path(), 2).await;
    let pid = std::process::id().to_string();
    for j in &journals {
        let name = j.path.file_name().and_then(|n| n.to_str()).expect("name");
        assert!(name.starts_with(&format!("{pid}-")), "{name}");
        assert_eq!(j.revisions.len(), 1, "{j:#?}");
    }
    assert_eq!(
        recorded(&journals),
        vec![
            ("//pkg:a".to_string(), hashin(root.path(), "//pkg:a").await),
            ("//pkg:b".to_string(), hashin(root.path(), "//pkg:b").await),
        ]
    );
}

/// A revision is recorded whichever tier served it: a local hit, a remote
/// hit, or a fresh build.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn working_set_covers_each_tier() {
    let root = tempfile::tempdir().expect("root");
    let remote = tempfile::tempdir().expect("remote");
    write_build(root.path(), TWO);

    // `b` exists on the remote only, `a` in the local cache only.
    let seed = engine(root.path(), Some(remote.path()), false);
    run(&seed, &["//pkg:b"]).await;
    drop(seed);
    std::fs::remove_dir_all(home(root.path()).join("cache")).expect("rm local cache");
    let local = engine(root.path(), None, false);
    run(&local, &["//pkg:a"]).await;
    drop(local);

    let e = engine(root.path(), Some(remote.path()), true);
    let mut got = outcomes(&run(&e, &["//pkg:a", "//pkg:b", "//pkg:c"]).await);
    got.sort_by(|x, y| x.0.cmp(&y.0));
    use CacheOutcomeKind::{LocalHit, Miss, RemoteHit};
    assert_eq!(
        got,
        vec![
            ("//pkg:a".to_string(), LocalHit),
            ("//pkg:b".to_string(), RemoteHit),
            ("//pkg:c".to_string(), Miss),
        ],
        "each tier is exercised"
    );
    drop(e);

    let journals = read_settled(root.path(), 1).await;
    let mut want = Vec::new();
    for a in ["//pkg:a", "//pkg:b", "//pkg:c"] {
        want.push((a.to_string(), hashin(root.path(), a).await));
    }
    assert_eq!(recorded(&journals), want);
}

/// Not recorded: a `cache = False` target (a tmp revision) and a
/// `remote_enabled = false` one. Their consumer is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uncacheable_and_tmp_revisions_are_not_carried() {
    let root = tempfile::tempdir().expect("root");
    write_build(
        root.path(),
        r#"
tmp = target(name = "tmp", driver = "bash", run = "echo t > $OUT", out = "t.txt", cache = False)
local = target(name = "local", driver = "bash", run = "echo l > $OUT", out = "l.txt",
               cache = {"remote": False})
target(name = "on", driver = "bash", run = "cat $SRC_TMP $SRC_LOCAL > $OUT", out = "on.txt",
       deps = {"tmp": tmp, "local": local})
"#,
    );
    let e = engine(root.path(), None, true);
    run(&e, &["//pkg:on", "//pkg:tmp", "//pkg:local"]).await;
    // A second request touching them again changes nothing.
    run(&e, &["//pkg:on", "//pkg:tmp", "//pkg:local"]).await;
    drop(e);

    let journals = read_settled(root.path(), 1).await;
    assert_eq!(
        recorded(&journals),
        vec![(
            "//pkg:on".to_string(),
            hashin(root.path(), "//pkg:on").await
        )]
    );
}

/// `heph clean` and gc leave `working-set/` alone: a journal ends only when
/// the snapshot that read it deletes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_and_gc_keep_the_journal() {
    let root = tempfile::tempdir().expect("root");
    write_build(root.path(), TWO);
    let e = engine(root.path(), None, true);
    run(&e, &["//pkg:a", "//pkg:b"]).await;
    drop(e);
    let before = read_settled(root.path(), 1).await;
    let path = before[0].path.clone();
    let bytes = std::fs::read(&path).expect("read journal");

    let other = engine(root.path(), None, false);
    let stats = Arc::clone(&other)
        .clean(
            other.new_state(),
            &Matcher::PackagePrefix(PkgBuf::from("")),
            Discovery::Complete,
        )
        .await
        .expect("clean");
    assert!(
        stats.revisions_removed > 0,
        "the clean did remove revisions"
    );
    run(&other, &["//pkg:a"]).await;
    Arc::clone(&other)
        .gc_all(other.new_state())
        .await
        .expect("gc");
    drop(other);

    assert_eq!(
        list_journals(&home(root.path())).expect("list"),
        vec![path.clone()]
    );
    assert_eq!(std::fs::read(&path).expect("read journal"), bytes);
}
