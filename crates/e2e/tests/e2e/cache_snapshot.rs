//! The working-set journal end to end: which engines record, what they
//! record from each cache tier, and that the trailer follows the last entry.
//! Then the snapshot tier: a `cacheSnapshot` dir read first, read-only, and
//! passed over for anything it can't serve.
//!
//! The journal tests drive it through `Config::record_working_set` and read
//! it back with `cache_snapshot::{list_journals, read_journal}`. The tier
//! tests build each snapshot by hand ([`Fixture`]): a cold run writes a
//! `file://` bucket, which is copied into a generation behind a marker. Then
//! the export, through `Engine::export_snapshot`: job after job over one
//! snapshot dir. Last, the `snapshot` command's lock protocol (I5), through
//! the `Consumption` it drives, with a short recorder wait.

use crate::common::{artifact_string, drain};
use futures::TryStreamExt as _;
use hcore::events::CacheOutcomeKind;
use hcore::hasync::StdCancellationToken;
use heph::engine::cache_snapshot::{
    Consumption, ExportOutcome, ExportReport, Incomplete, JournalContents, LeftOut, RECORDER_WAIT,
    RecorderAliveError, WorkingSet, list_journals, lock_snapshot, read_journal, read_marker,
    working_set_dir,
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
    engine_with(Config {
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
}

/// [`engine`] over a config of the caller's.
fn engine_with(config: Config) -> Arc<Engine> {
    let mut e = Engine::new(config).expect("engine");
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

/// I1: the walker prunes the resolved snapshot dir, so a root `**` glob
/// hashes the same with and without files under it, whatever its name (not
/// a `.heph*` one, which the home's prefix would prune anyway).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dir_under_a_root_glob_moves_no_hashin() {
    let root = tempfile::tempdir().expect("root");
    write_build(
        root.path(),
        r#"
target(
    name = "t",
    driver = "bash",
    deps = {"src": [glob("**/*", abs = True)]},
    out = "out.txt",
    run = "echo ok > $OUT",
)
"#,
    );
    std::fs::create_dir_all(root.path().join("src")).expect("mkdir src");
    std::fs::write(root.path().join("src").join("a.txt"), "a").expect("write src");

    let snap = root.path().join("snap");
    let hashin_of = |snapshot: Option<&Path>| {
        let e = engine_with(Config {
            cache_snapshot: snapshot.map(Path::to_path_buf),
            ..Config::for_tests(root.path())
        });
        async move {
            Arc::clone(&e)
                .meta(e.new_state(), &addr("//pkg:t"))
                .await
                .expect("meta")
                .hashin
        }
    };

    let before = hashin_of(Some(&snap)).await;
    let generation = snap.join("gen-a").join("pkg").join("t");
    std::fs::create_dir_all(&generation).expect("mkdir generation");
    std::fs::write(generation.join("blob"), "restored").expect("write blob");
    std::fs::write(
        snap.join("HEPH_SNAPSHOT"),
        r#"{"format":1,"current":"gen-a"}"#,
    )
    .expect("write marker");

    assert_eq!(
        hashin_of(Some(&snap)).await,
        before,
        "files under the snapshot dir moved a root glob's hashin"
    );
    // Control: without the dir configured, the same glob does see them.
    assert_ne!(
        hashin_of(None).await,
        before,
        "the glob never reached the snapshot dir, so the test proves nothing"
    );
}

// ---------------------------------------------------------------------------
// The snapshot tier.

/// The one generation every [`Fixture`] writes.
const GEN: &str = "gen-fixture";

/// Each target's output differs on every run, so an output equal to the cold
/// run's proves a cache served it, and which tier did is in the events.
const RANDOM: &str = r#"
target(name = "t", driver = "bash", run = "echo $RANDOM$RANDOM$RANDOM > $OUT", out = "out.txt")
target(name = "u", driver = "bash", run = "echo $RANDOM$RANDOM$RANDOM > $OUT", out = "out.txt")
"#;

/// Reproducible: a rebuild produces the bytes the snapshot holds.
const FOUR: &str = r#"
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "a.txt")
target(name = "b", driver = "bash", run = "echo b > $OUT", out = "b.txt")
target(name = "c", driver = "bash", run = "echo c > $OUT", out = "c.txt")
target(name = "d", driver = "bash", run = "echo d > $OUT", out = "d.txt")
"#;

/// What a fixture engine reads besides the snapshot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bucket {
    Absent,
    /// The cold run's bucket, read after the snapshot, never written.
    ReadOnly,
    /// The cold run's bucket, read after the snapshot, and uploaded to.
    Writable,
}

/// A workspace, the `file://` bucket a cold run wrote, and a snapshot dir
/// whose one generation is a copy of that bucket, behind a marker. The local
/// cache is gone, so the next engine over `root` starts cold but for the
/// tiers.
struct Fixture {
    root: tempfile::TempDir,
    bucket: tempfile::TempDir,
    parent: tempfile::TempDir,
    /// Each built addr's output from the cold run.
    cold: std::collections::BTreeMap<String, Vec<u8>>,
}

impl Fixture {
    async fn new(body: &str, addrs: &[&str]) -> Self {
        let root = tempfile::tempdir().expect("root");
        let bucket = tempfile::tempdir().expect("bucket");
        let parent = tempfile::tempdir().expect("snapshot parent");
        write_build(root.path(), body);
        let e = engine(root.path(), Some(bucket.path()), false);
        let mut cold = std::collections::BTreeMap::new();
        for a in addrs {
            cold.insert((*a).to_string(), run_one(&e, a).await.0);
        }
        drop(e);
        rm_local_cache(root.path());
        let f = Self {
            root,
            bucket,
            parent,
            cold,
        };
        copy_tree(f.bucket.path(), &f.generation());
        heph::engine::cache_snapshot::write_marker(&f.dir(), GEN, false).expect("write marker");
        f
    }

    /// `<dir>`, the configured snapshot dir.
    fn dir(&self) -> std::path::PathBuf {
        self.parent.path().join("snap")
    }

    fn generation(&self) -> std::path::PathBuf {
        self.dir().join(GEN)
    }

    fn cold(&self, a: &str) -> &[u8] {
        self.cold.get(a).map(Vec::as_slice).unwrap_or_default()
    }

    fn engine(&self, bucket: Bucket, record: bool) -> Arc<Engine> {
        self.engine_at(self.dir(), bucket, record)
    }

    /// A fresh engine over the workspace, with `dir` as its snapshot dir.
    fn engine_at(&self, dir: std::path::PathBuf, bucket: Bucket, record: bool) -> Arc<Engine> {
        let remote_caches = match bucket {
            Bucket::Absent => Vec::new(),
            Bucket::ReadOnly | Bucket::Writable => vec![heph::engine::RemoteCacheDef {
                name: "bucket".to_string(),
                uri: format!("file://{}", self.bucket.path().display()),
                read: true,
                write: bucket == Bucket::Writable,
                concurrency: 10,
                endpoint: None,
                region: None,
            }],
        };
        engine_with(Config {
            remote_caches,
            record_working_set: record,
            cache_snapshot: Some(dir),
            ..Config::for_tests(self.root.path())
        })
    }

    /// The snapshot's one revision directory of `//pkg:<target>`.
    fn revision(&self, target: &str) -> std::path::PathBuf {
        let mut dirs: Vec<std::path::PathBuf> =
            std::fs::read_dir(self.generation().join("pkg").join(target))
                .expect("the snapshot holds the target")
                .map(|e| e.expect("dir entry").path())
                .filter(|p| p.join("manifest-v2.borsh").is_file())
                .collect();
        assert_eq!(dirs.len(), 1, "one revision of {target}: {dirs:?}");
        dirs.remove(0)
    }

    /// The output blob of the snapshot's revision of `//pkg:<target>`.
    fn out_blob(&self, target: &str) -> std::path::PathBuf {
        let dir = self.revision(target);
        let blobs: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .expect("read revision")
            .map(|e| e.expect("dir entry").path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("out_"))
            })
            .collect();
        assert_eq!(
            blobs.len(),
            1,
            "one output blob in {}: {blobs:?}",
            dir.display()
        );
        blobs.into_iter().next().expect("one blob")
    }
}

fn rm_local_cache(root: &Path) {
    std::fs::remove_dir_all(home(root).join("cache")).expect("rm local cache");
}

/// Copy the tree at `src` to `dst`, which must not exist.
fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("mkdir copy");
    for entry in std::fs::read_dir(src).expect("read tree") {
        let entry = entry.expect("dir entry");
        let to = dst.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).expect("copy file");
        }
    }
}

/// Every file under `dir`, as `(relative path, bytes)`, sorted.
fn tree_contents(dir: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(std::path::PathBuf, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).expect("read tree") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path.strip_prefix(base).expect("under base").to_path_buf();
                out.push((rel, std::fs::read(&path).expect("read file")));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Resolve `a` with all its outputs in one request, drain it, and return the
/// output's bytes and every event the request emitted.
async fn run_one(engine: &Arc<Engine>, a: &str) -> (Vec<u8>, Vec<BuildEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = engine.new_state_with_events(true, Some(tx));
    let res = Arc::clone(engine)
        .result_addr(
            rs.clone(),
            &addr(a),
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await
        .expect("a snapshot never fails a build");
    let out = crate::common::artifact_bytes(&res);
    drop(res);
    drain(rs).await;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    (out, events)
}

/// The tier that served `a` in `events`: `"snapshot"`, a remote's name, or
/// `None` when it was not a remote hit.
fn served_by(events: &[BuildEvent], a: &str) -> Option<String> {
    events.iter().find_map(|e| match &e.kind {
        BuildEventKind::RemoteCacheHit {
            addr,
            cache,
            snapshot,
        } if addr == a => Some(if *snapshot {
            "snapshot".to_string()
        } else {
            cache.clone()
        }),
        _ => None,
    })
}

/// `a`'s cache outcome in `events`.
fn outcome_of(events: &[BuildEvent], a: &str) -> Option<CacheOutcomeKind> {
    outcomes(events)
        .into_iter()
        .find_map(|(addr, o)| (addr == a).then_some(o))
}

/// Run `f` (an engine's construction, which opens the tier) and return what
/// it returned with every `WARN` line it logged.
fn warnings_while<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    use tracing_subscriber::util::SubscriberInitExt as _;
    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|e| std::io::Error::other(e.to_string()))?
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let log = Captured::default();
    let writer = log.clone();
    let value = {
        // A second live dispatcher: see `resources::clamp_is_logged_once_per_addr_per_request`.
        let _second = tracing_subscriber::registry().set_default();
        let _guard = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
            .set_default();
        f()
    };
    let text = String::from_utf8(log.0.lock().expect("log buffer").clone()).expect("utf-8 log");
    let warnings = text
        .lines()
        .filter(|l| l.contains("WARN"))
        .map(str::to_owned)
        .collect();
    (value, warnings)
}

async fn def_hash(engine: &Arc<Engine>, a: &str) -> Vec<u8> {
    Arc::clone(engine)
        .get_def(engine.new_state(), &addr(a))
        .await
        .expect("def")
        .target_def
        .hash
        .clone()
}

/// I1: the tier moves neither a def hash nor a `hashin`, and it serves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hashes_do_not_move_with_the_tier() {
    let f = Fixture::new(FOUR, &["//pkg:a"]).await;
    let without = engine(f.root.path(), None, false);
    let with = f.engine(Bucket::Absent, false);
    for a in ["//pkg:a", "//pkg:b"] {
        assert_eq!(def_hash(&without, a).await, def_hash(&with, a).await, "{a}");
        let h = |e: &Arc<Engine>| {
            let e = Arc::clone(e);
            async move {
                Arc::clone(&e)
                    .meta(e.new_state(), &addr(a))
                    .await
                    .expect("meta")
                    .hashin
            }
        };
        assert_eq!(h(&without).await, h(&with).await, "{a}");
    }
    drop(without);
    let (out, events) = run_one(&with, "//pkg:a").await;
    assert_eq!(
        served_by(&events, "//pkg:a").as_deref(),
        Some("snapshot"),
        "{events:#?}"
    );
    assert_eq!(out, f.cold("//pkg:a"));
}

/// I4: a build that hits the tier, misses and uploads leaves `<dir>`
/// byte-identical; the upload went to the bucket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_build_never_writes_the_tier() {
    let f = Fixture::new(FOUR, &["//pkg:a"]).await;
    let before = tree_contents(&f.dir());
    let e = f.engine(Bucket::Writable, true);
    let (_, events) = run_one(&e, "//pkg:a").await;
    assert_eq!(served_by(&events, "//pkg:a").as_deref(), Some("snapshot"));
    let (_, events) = run_one(&e, "//pkg:b").await;
    assert_eq!(outcome_of(&events, "//pkg:b"), Some(CacheOutcomeKind::Miss));
    drop(e);
    assert!(
        f.bucket.path().join("pkg").join("b").is_dir(),
        "the miss was uploaded to the bucket, so a write had somewhere to go"
    );
    assert!(
        tree_contents(&f.dir()) == before,
        "the build wrote into the snapshot dir"
    );
}

/// Relocatable: the wire format holds no absolute path, so a renamed `<dir>`
/// still serves every revision.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_moved_dir_still_hits() {
    let f = Fixture::new(FOUR, &["//pkg:a", "//pkg:b"]).await;
    let moved = f.parent.path().join("moved");
    std::fs::rename(f.dir(), &moved).expect("move the snapshot dir");
    let e = f.engine_at(moved, Bucket::Absent, false);
    for a in ["//pkg:a", "//pkg:b"] {
        let (out, events) = run_one(&e, a).await;
        assert_eq!(
            served_by(&events, a).as_deref(),
            Some("snapshot"),
            "{a}: {events:#?}"
        );
        assert_eq!(out, f.cold(a));
    }
}

/// A snapshot revision missing its output blob is passed over at the hit
/// decision, and the bucket serves it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_blob_falls_through_to_the_bucket() {
    let f = Fixture::new(RANDOM, &["//pkg:t"]).await;
    std::fs::remove_file(f.out_blob("t")).expect("evict blob");
    let (out, events) = run_one(&f.engine(Bucket::ReadOnly, false), "//pkg:t").await;
    assert_eq!(
        served_by(&events, "//pkg:t").as_deref(),
        Some("bucket"),
        "{events:#?}"
    );
    assert_eq!(
        out,
        f.cold("//pkg:t"),
        "the bucket served it, not a rebuild"
    );
}

/// With no bucket, only the target whose blob is gone runs; the rest hit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_blob_without_a_bucket_rebuilds_that_target_only() {
    let f = Fixture::new(FOUR, &["//pkg:a", "//pkg:b"]).await;
    std::fs::remove_file(f.out_blob("a")).expect("evict blob");
    let e = f.engine(Bucket::Absent, false);
    let (out, events) = run_one(&e, "//pkg:a").await;
    assert_eq!(outcome_of(&events, "//pkg:a"), Some(CacheOutcomeKind::Miss));
    assert_eq!(out, f.cold("//pkg:a"), "reproducible: the rebuild matches");
    let (out, events) = run_one(&e, "//pkg:b").await;
    assert_eq!(served_by(&events, "//pkg:b").as_deref(), Some("snapshot"));
    assert_eq!(out, f.cold("//pkg:b"));
}

/// The lazy pull: `top` hits the snapshot, so `dep`'s manifest is mirrored
/// locally and its multi-MiB blob left on the tiers. The snapshot then loses
/// `dep`'s blob, and a later process asking for `dep`'s output pulls it from
/// the bucket rather than rebuild it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lazy_pull_falls_through_too() {
    let f = Fixture::new(
        r#"
dep = target(
    name = "dep",
    driver = "bash",
    run = "head -c 3000000 /dev/urandom | base64 > $OUT",
    out = "dep.txt",
)

target(
    name = "top",
    driver = "bash",
    run = "echo $RANDOM$RANDOM$RANDOM$(wc -c < $SRC_DEP) > $OUT",
    deps = {"dep": dep},
    out = "top.txt",
)
"#,
        &["//pkg:top", "//pkg:dep"],
    )
    .await;
    let (top, events) = run_one(&f.engine(Bucket::ReadOnly, false), "//pkg:top").await;
    assert_eq!(top, f.cold("//pkg:top"));
    assert_eq!(served_by(&events, "//pkg:top").as_deref(), Some("snapshot"));

    std::fs::remove_file(f.out_blob("dep")).expect("evict blob");
    let (dep, _) = run_one(&f.engine(Bucket::ReadOnly, false), "//pkg:dep").await;
    assert!(
        dep == f.cold("//pkg:dep"),
        "the lazy pull must fall through to the bucket, not rebuild"
    );
}

/// Diagnosability: each remote hit names its tier, so local, snapshot and
/// bucket hits and misses fold into per-tier counts from the event stream
/// (`--json` carries the same events), and the snapshot's hits are in the
/// working set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_tier_hit_counts_are_reported() {
    let f = Fixture::new(FOUR, &["//pkg:a", "//pkg:b", "//pkg:c"]).await;
    // `c` only in the bucket; `d` nowhere.
    std::fs::remove_dir_all(f.generation().join("pkg").join("c")).expect("drop c from the tier");
    let e = f.engine(Bucket::ReadOnly, true);
    // `a` from the snapshot, into the local cache.
    run_one(&e, "//pkg:a").await;

    let mut events = Vec::new();
    for a in ["//pkg:a", "//pkg:b", "//pkg:c", "//pkg:d"] {
        events.extend(run_one(&e, a).await.1);
    }
    let mut tiers = hcore::events::TierCounts::default();
    for ev in &events {
        tiers.apply(&ev.kind);
    }
    assert_eq!(
        (tiers.local, tiers.snapshot, tiers.misses),
        (1, 1, 1),
        "{tiers:?}"
    );
    assert_eq!(
        tiers.remote,
        std::collections::BTreeMap::from([("bucket".to_string(), 1)]),
        "{tiers:?}"
    );
    let json: Vec<String> = events
        .iter()
        .filter(|ev| matches!(ev.kind, BuildEventKind::RemoteCacheHit { .. }))
        .map(|ev| serde_json::to_string(&ev.kind).expect("json"))
        .collect();
    assert!(
        json.iter().any(|j| j.contains(r#""snapshot":true"#)),
        "{json:?}"
    );
    assert!(
        json.iter().any(|j| j.contains(r#""cache":"bucket""#)),
        "{json:?}"
    );

    drop(e);
    let recorded = recorded(&read_settled(f.root.path(), 1).await);
    for a in ["//pkg:a", "//pkg:b", "//pkg:c", "//pkg:d"] {
        let h = hashin(f.root.path(), a).await;
        assert!(
            recorded.contains(&(a.to_string(), h)),
            "{a} is in the working set whatever tier served it: {recorded:?}"
        );
    }
}

/// I3: the per-tier rejections each move the lookup on to the bucket, or,
/// for an unusable dir or marker, leave the build cold with no tier.
mod degrades {
    use super::*;

    /// `a` resolved by an engine over `f` with `bucket`; its outcome must be
    /// a cold miss (no tier) for every case here.
    async fn builds_cold(e: &Arc<Engine>, a: &str) {
        let (_, events) = run_one(e, a).await;
        assert_eq!(
            outcome_of(&events, a),
            Some(CacheOutcomeKind::Miss),
            "{events:#?}"
        );
        assert_eq!(served_by(&events, a), None);
    }

    /// The first restore of a lineage: no dir, no warning, a cold build.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_dir() {
        let f = Fixture::new(FOUR, &["//pkg:a"]).await;
        let (e, warnings) =
            warnings_while(|| f.engine_at(f.parent.path().join("absent"), Bucket::Absent, false));
        assert!(warnings.is_empty(), "{warnings:?}");
        builds_cold(&e, "//pkg:a").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_dir() {
        let f = Fixture::new(FOUR, &["//pkg:a"]).await;
        let empty = f.parent.path().join("empty");
        std::fs::create_dir(&empty).expect("mkdir");
        let (e, warnings) = warnings_while(|| f.engine_at(empty, Bucket::Absent, false));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        builds_cold(&e, "//pkg:a").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_or_unreadable_marker() {
        let f = Fixture::new(FOUR, &["//pkg:a"]).await;
        let marker = f.dir().join(heph::engine::cache_snapshot::MARKER_FILE);
        for (case, contents) in [("missing", None), ("unparseable", Some("{not json"))] {
            match contents {
                None => std::fs::remove_file(&marker).expect("rm marker"),
                Some(c) => std::fs::write(&marker, c).expect("write marker"),
            }
            let (e, warnings) = warnings_while(|| f.engine(Bucket::Absent, false));
            assert_eq!(warnings.len(), 1, "{case}: {warnings:?}");
            builds_cold(&e, "//pkg:a").await;
            drop(e);
            rm_local_cache(f.root.path());
        }
    }

    /// A newer format is one warning naming it and no tier; an older one
    /// is served.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn newer_marker_format() {
        let f = Fixture::new(FOUR, &["//pkg:a"]).await;
        let marker = f.dir().join(heph::engine::cache_snapshot::MARKER_FILE);
        let newer = heph::engine::cache_snapshot::SNAPSHOT_FORMAT + 1;
        std::fs::write(
            &marker,
            format!(r#"{{"format":{newer},"heph":"x","current":"{GEN}"}}"#),
        )
        .expect("write marker");
        let (e, warnings) = warnings_while(|| f.engine(Bucket::Absent, false));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings.iter().all(|w| w.contains(&newer.to_string())),
            "the warning names the format it saw: {warnings:?}"
        );
        builds_cold(&e, "//pkg:a").await;
        drop(e);
        rm_local_cache(f.root.path());

        let older = heph::engine::cache_snapshot::SNAPSHOT_FORMAT - 1;
        std::fs::write(
            &marker,
            format!(r#"{{"format":{older},"heph":"x","current":"{GEN}"}}"#),
        )
        .expect("write marker");
        let (e, warnings) = warnings_while(|| f.engine(Bucket::Absent, false));
        assert!(warnings.is_empty(), "{warnings:?}");
        let (_, events) = run_one(&e, "//pkg:a").await;
        assert_eq!(served_by(&events, "//pkg:a").as_deref(), Some("snapshot"));
    }

    /// A manifest under the key that names another target, and one that
    /// does not deserialize: each falls through to the bucket.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_key_mismatch() {
        let f = Fixture::new(RANDOM, &["//pkg:t", "//pkg:u"]).await;
        // `u`'s revision restored under `t`'s key.
        let t = f.revision("t");
        std::fs::remove_dir_all(&t).expect("clear revision");
        copy_tree(&f.revision("u"), &t);
        // `u`'s manifest garbage.
        std::fs::write(f.revision("u").join("manifest-v2.borsh"), b"not borsh")
            .expect("garbage manifest");

        let e = f.engine(Bucket::ReadOnly, false);
        for a in ["//pkg:t", "//pkg:u"] {
            let (out, events) = run_one(&e, a).await;
            assert_eq!(
                served_by(&events, a).as_deref(),
                Some("bucket"),
                "{a}: {events:#?}"
            );
            assert_eq!(out, f.cold(a), "{a}");
        }
    }

    /// A zstd blob and a raw blob cut short after the restore: the hit stands
    /// on the snapshot's manifest, and the pull falls through to the bucket.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn truncated_blob_on_read() {
        let f = Fixture::new(
            r#"
target(name = "z", driver = "bash", run = "echo $RANDOM$RANDOM$RANDOM > $OUT", out = "out.txt")
target(name = "r", driver = "bash", run = "head -c 2000000 /dev/urandom | gzip > $OUT", out = "out.gz")
"#,
            &["//pkg:z", "//pkg:r"],
        )
        .await;
        for (target, zstd) in [("z", true), ("r", false)] {
            let blob = f.out_blob(target);
            assert_eq!(
                blob.to_string_lossy().ends_with(".zst"),
                zstd,
                "{}",
                blob.display()
            );
            let len = std::fs::metadata(&blob).expect("stat").len();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&blob)
                .expect("open blob")
                .set_len(len / 2)
                .expect("truncate");
        }
        let e = f.engine(Bucket::ReadOnly, false);
        for a in ["//pkg:z", "//pkg:r"] {
            let (out, _) = run_one(&e, a).await;
            assert!(
                out == f.cold(a),
                "{a}: the bucket must serve the whole blob"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The export, driven through its API as the `snapshot` command will drive it:
// lock, read the journals, export with an engine that records nothing, delete
// the journals.

const ALL_FOUR: [&str; 4] = ["//pkg:a", "//pkg:b", "//pkg:c", "//pkg:d"];

/// A CI job's engine: the snapshot dir configured, recording its working set.
fn job_engine(root: &Path, dir: &Path) -> Arc<Engine> {
    engine_with(Config {
        record_working_set: true,
        cache_snapshot: Some(dir.to_path_buf()),
        ..Config::for_tests(root)
    })
}

/// Export `ws` into `dir`, then delete the journals it was read from.
async fn export(root: &Path, dir: &Path, ws: &WorkingSet) -> ExportReport {
    let lock = lock_snapshot(&home(root)).expect("snapshot lock");
    let e = engine(root, None, false);
    let report = e
        .export_snapshot(&lock, dir, ws, &StdCancellationToken::new())
        .await
        .expect("export");
    for j in &ws.journals {
        let removed = std::fs::remove_file(j);
        assert!(
            removed
                .as_ref()
                .err()
                .is_none_or(|err| err.kind() == std::io::ErrorKind::NotFound),
            "deleting {}: {removed:?}",
            j.display()
        );
    }
    report
}

/// One job over `root`: build `addrs` with the snapshot at `dir`, and return
/// its working set and every build's events.
async fn job(root: &Path, dir: &Path, addrs: &[&str]) -> (WorkingSet, Vec<BuildEvent>) {
    let e = job_engine(root, dir);
    let events = run(&e, addrs).await;
    drop(e);
    let ws = WorkingSet::from_journals(read_settled(root, 1).await);
    (ws, events)
}

/// A cold first job that builds every target of [`FOUR`] and snapshots it,
/// then a fresh machine: the local cache gone.
async fn first_job(root: &Path, dir: &Path) {
    write_build(root, FOUR);
    let (ws, _) = job(root, dir, &ALL_FOUR).await;
    let report = export(root, dir, &ws).await;
    assert_eq!(report.new.count, 4, "{report:?}");
    rm_local_cache(root);
}

/// The headline: a second job on a fresh machine is served whole by the
/// snapshot the first wrote, and writes it back unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_job_runs_nothing() {
    let root = tempfile::tempdir().expect("root");
    let parent = tempfile::tempdir().expect("parent");
    let dir = parent.path().join("snap");
    first_job(root.path(), &dir).await;

    let (ws, events) = job(root.path(), &dir, &ALL_FOUR).await;
    for a in ALL_FOUR {
        assert_eq!(served_by(&events, a).as_deref(), Some("snapshot"), "{a}");
    }
    let report = export(root.path(), &dir, &ws).await;
    assert_eq!(report.kept.count, 4, "{report:?}");
    assert_eq!(report.new.count + report.dropped.count, 0, "{report:?}");
    assert!(report.left_out.is_empty(), "{report:?}");
    assert!(list_journals(&home(root.path())).expect("list").is_empty());
}

/// A complete job's snapshot is its working set: what it did not use goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unused_revisions_are_dropped() {
    let root = tempfile::tempdir().expect("root");
    let parent = tempfile::tempdir().expect("parent");
    let dir = parent.path().join("snap");
    first_job(root.path(), &dir).await;

    let (ws, _) = job(root.path(), &dir, &["//pkg:a", "//pkg:b"]).await;
    let report = export(root.path(), &dir, &ws).await;
    assert_eq!(report.kept.count, 2, "{report:?}");
    assert_eq!(report.dropped.count, 2, "{report:?}");
    assert_eq!(report.dropped_addrs, vec!["//pkg:c", "//pkg:d"]);

    rm_local_cache(root.path());
    let (_, events) = job(root.path(), &dir, &["//pkg:c"]).await;
    assert_eq!(outcome_of(&events, "//pkg:c"), Some(CacheOutcomeKind::Miss));
}

/// An incomplete job also carries over what it restored and did not touch,
/// though this machine's local cache never held it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_incomplete_run_keeps_restored_revisions() {
    let root = tempfile::tempdir().expect("root");
    let parent = tempfile::tempdir().expect("parent");
    let dir = parent.path().join("snap");
    first_job(root.path(), &dir).await;

    let (mut ws, _) = job(root.path(), &dir, &["//pkg:a"]).await;
    ws.incomplete
        .push((ws.journals[0].clone(), Incomplete::Failed));
    let report = export(root.path(), &dir, &ws).await;
    assert_eq!(report.kept.count, 1, "{report:?}");
    assert_eq!(report.carried_over.count, 3, "{report:?}");
    assert_eq!(report.incomplete.len(), 1, "{report:?}");
    assert!(read_marker(&dir).expect("marker").marker.incomplete);

    rm_local_cache(root.path());
    let (_, events) = job(root.path(), &dir, &ALL_FOUR).await;
    for a in ALL_FOUR {
        assert_eq!(served_by(&events, a).as_deref(), Some("snapshot"), "{a}");
    }
}

/// A revision gc trimmed between the build and the export is left out and
/// counted, not a failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_skips_a_gc_trimmed_revision() {
    let root = tempfile::tempdir().expect("root");
    let parent = tempfile::tempdir().expect("parent");
    let dir = parent.path().join("snap");
    write_build(root.path(), FOUR);
    let (ws, _) = job(root.path(), &dir, &ALL_FOUR).await;
    rm_local_cache(root.path());
    let report = export(root.path(), &dir, &ws).await;
    assert_eq!(
        report.left_out.get(&LeftOut::Trimmed).map(|t| t.count),
        Some(4),
        "{report:?}"
    );
    assert_eq!(report.new.count, 0, "{report:?}");
}

// ---------------------------------------------------------------------------
// The snapshot command's lock protocol (I5)
// ---------------------------------------------------------------------------

/// A wait short enough that a test refused after it stays fast.
const SHORT_WAIT: Duration = Duration::from_millis(200);

/// Drop the trailer from the journal at `path`: what a recorder killed
/// without running destructors leaves.
fn strip_trailer(path: &Path) {
    let text = std::fs::read_to_string(path).expect("read journal");
    let kept: String = text
        .lines()
        .filter(|l| !l.contains(r#""t":"Trailer""#))
        .map(|l| format!("{l}\n"))
        .collect();
    assert_ne!(kept, text, "{} had a trailer", path.display());
    std::fs::write(path, kept).expect("rewrite journal");
}

/// Take the journals with `wait`, export them to `dir` and consume them,
/// through a fresh non-recording engine, as the command does.
async fn snapshot(root: &Path, dir: &Path, wait: Duration) -> ExportReport {
    let tok = StdCancellationToken::new();
    let c = Consumption::acquire(&home(root), wait, &tok)
        .await
        .expect("acquire");
    c.export(&engine(root, None, false), dir, &tok)
        .await
        .expect("export")
}

/// A recorder mid-request, before its first flush, holds its journal: the
/// snapshot gives up after the wait, naming the pid, and consumes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuses_while_a_recorder_is_alive() {
    let root = tempfile::tempdir().expect("root");
    let parent = tempfile::tempdir().expect("parent");
    let dir = parent.path().join("snap");
    write_build(root.path(), FOUR);
    let e = job_engine(root.path(), &dir);
    let rs = e.new_state();
    let res = Arc::clone(&e)
        .result_addr(
            rs.clone(),
            &addr("//pkg:a"),
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await
        .expect("build");
    drop(res);
    let journals = list_journals(&home(root.path())).expect("list");
    assert_eq!(journals.len(), 1, "the first record created the journal");

    let started = Instant::now();
    let err = Consumption::acquire(&home(root.path()), SHORT_WAIT, &StdCancellationToken::new())
        .await
        .expect_err("a live recorder refuses the snapshot");
    assert!(started.elapsed() >= SHORT_WAIT, "it waited first");
    let alive = err
        .downcast_ref::<RecorderAliveError>()
        .unwrap_or_else(|| panic!("not a RecorderAliveError: {err:#}"));
    assert_eq!(alive.recorders.len(), 1, "{alive:?}");
    assert_eq!(alive.recorders[0].journal, journals[0]);
    assert_eq!(alive.recorders[0].pid, Some(std::process::id()));
    assert!(
        format!("{err:#}").contains(&std::process::id().to_string()),
        "the message names the pid: {err:#}"
    );
    assert!(!dir.exists(), "nothing was written");
    assert_eq!(list_journals(&home(root.path())).expect("list"), journals);

    drain(rs).await;
    drop(e);
}

/// A recorder that exits inside the wait lets the snapshot through, with its
/// journal complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waits_for_a_recorder_that_exits() {
    let root = tempfile::tempdir().expect("root");
    let parent = tempfile::tempdir().expect("parent");
    let dir = parent.path().join("snap");
    write_build(root.path(), FOUR);
    let e = job_engine(root.path(), &dir);
    run(&e, &["//pkg:a"]).await;
    assert_eq!(list_journals(&home(root.path())).expect("list").len(), 1);

    let exits = tokio::spawn(async move {
        tokio::time::sleep(SHORT_WAIT).await;
        drop(e);
    });
    let tok = StdCancellationToken::new();
    let c = Consumption::acquire(&home(root.path()), Duration::from_secs(30), &tok)
        .await
        .expect("the recorder exits inside the wait");
    exits.await.expect("recorder task");
    assert!(c.working_set().is_complete(), "{:?}", c.working_set());
    assert_eq!(c.working_set().revisions.len(), 1);

    let report = c
        .export(&engine(root.path(), None, false), &dir, &tok)
        .await
        .expect("export");
    assert!(
        matches!(report.outcome, ExportOutcome::Written { .. }),
        "{report:?}"
    );
    assert_eq!(report.new.count, 1, "{report:?}");
    assert!(list_journals(&home(root.path())).expect("list").is_empty());
}

/// A journal whose recorder died without its trailer has no lock holder: it
/// is read at once, as incomplete, never waited on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_recorder_does_not_block() {
    let root = tempfile::tempdir().expect("root");
    let parent = tempfile::tempdir().expect("parent");
    let dir = parent.path().join("snap");
    write_build(root.path(), FOUR);
    let (ws, _) = job(root.path(), &dir, &["//pkg:a", "//pkg:b"]).await;
    strip_trailer(&ws.journals[0]);

    let started = Instant::now();
    let report = snapshot(root.path(), &dir, RECORDER_WAIT).await;
    assert!(
        started.elapsed() < RECORDER_WAIT / 2,
        "a dead recorder was waited on: {:?}",
        started.elapsed()
    );
    assert_eq!(report.incomplete.len(), 1, "{report:?}");
    assert_eq!(
        report.incomplete[0].reason,
        Incomplete::NoTrailer.to_string()
    );
    assert_eq!(report.new.count, 2, "{report:?}");
    assert!(list_journals(&home(root.path())).expect("list").is_empty());
}

/// Only the journals read are consumed: a recorder that starts once the
/// snapshot has listed them keeps its journal, and a `.tmp` one is never
/// read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recorder_started_mid_snapshot_keeps_its_journal() {
    let root = tempfile::tempdir().expect("root");
    let parent = tempfile::tempdir().expect("parent");
    let dir = parent.path().join("snap");
    write_build(root.path(), FOUR);
    let (ws, _) = job(root.path(), &dir, &["//pkg:a"]).await;
    let first = ws.journals.clone();
    let tmp = working_set_dir(&home(root.path())).join(".1-unrenamed.tmp");
    std::fs::write(&tmp, "not yet a journal\n").expect("write tmp");

    let tok = StdCancellationToken::new();
    let c = Consumption::acquire(&home(root.path()), SHORT_WAIT, &tok)
        .await
        .expect("acquire");
    assert_eq!(c.working_set().journals, first);

    // A recorder starts now, and is still alive while the export runs.
    let late = job_engine(root.path(), &dir);
    run(&late, &["//pkg:b"]).await;
    let report = c
        .export(&engine(root.path(), None, false), &dir, &tok)
        .await
        .expect("export");
    assert_eq!(report.journals, first, "{report:?}");
    assert_eq!(report.new.count, 1, "only //pkg:a: {report:?}");
    assert!(tmp.exists(), "a .tmp journal is never consumed");

    drop(late);
    let left = read_settled(root.path(), 1).await;
    assert_ne!(left[0].path, first[0]);
    assert_eq!(
        recorded(&left),
        vec![("//pkg:b".to_string(), hashin(root.path(), "//pkg:b").await)]
    );
}

/// E3: a journal left by an earlier job on a persistent runner, killed
/// before its trailer, makes the run incomplete and the snapshot bigger than
/// this job's working set. Every revision in it is still exported under its
/// own `hashin`, so the next job is never served a wrong output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_journal_makes_it_bigger_never_wrong() {
    let root = tempfile::tempdir().expect("root");
    let parent = tempfile::tempdir().expect("parent");
    let dir = parent.path().join("snap");
    write_build(root.path(), FOUR);
    let (stale, _) = job(root.path(), &dir, &["//pkg:a", "//pkg:b"]).await;
    strip_trailer(&stale.journals[0]);
    let old_a = hashin(root.path(), "//pkg:a").await;

    // This job: `//pkg:a` changed, and it builds that and `//pkg:c`.
    write_build(root.path(), &FOUR.replace("echo a >", "echo a2 >"));
    let e = job_engine(root.path(), &dir);
    run(&e, &["//pkg:a", "//pkg:c"]).await;
    drop(e);
    let new_a = hashin(root.path(), "//pkg:a").await;
    assert_ne!(old_a, new_a);

    let tok = StdCancellationToken::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let c = loop {
        let c = Consumption::acquire(&home(root.path()), RECORDER_WAIT, &tok)
            .await
            .expect("acquire");
        // The job's own journal gets its trailer a moment after the engine
        // drops; only the stale one may be incomplete.
        if c.working_set().incomplete.len() == 1 || Instant::now() > deadline {
            break c;
        }
        drop(c);
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(c.working_set().journals.len(), 2);
    let report = c
        .export(&engine(root.path(), None, false), &dir, &tok)
        .await
        .expect("export");
    assert_eq!(report.incomplete.len(), 1, "{report:?}");
    assert_eq!(
        report.incomplete[0].journal, stale.journals[0],
        "{report:?}"
    );
    // Bigger: the stale //pkg:b goes in beside this job's //pkg:a and
    // //pkg:c. The stale //pkg:a's revision is no longer in the local cache
    // (the new one replaced it), so it is left out, counted.
    assert_eq!(
        report.new.count, 3,
        "the new //pkg:a, //pkg:b and //pkg:c: {report:?}"
    );
    assert!(report.top.iter().any(|r| r.addr == "//pkg:b"), "{report:?}");
    assert_eq!(
        report.left_out.get(&LeftOut::Trimmed).map(|t| t.count),
        Some(1),
        "the old //pkg:a: {report:?}"
    );

    // A fresh machine: the changed target is served its own output, and the
    // stale entry's target is served from the snapshot, which is correct.
    rm_local_cache(root.path());
    let e = job_engine(root.path(), &dir);
    let (a, events) = run_one(&e, "//pkg:a").await;
    assert_eq!(a, b"a2\n");
    assert_eq!(served_by(&events, "//pkg:a").as_deref(), Some("snapshot"));
    let (b, events) = run_one(&e, "//pkg:b").await;
    assert_eq!(b, b"b\n");
    assert_eq!(served_by(&events, "//pkg:b").as_deref(), Some("snapshot"));
}

/// D4: the build and the `snapshot` command resolve the same dir from the
/// same config, and only the build records.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_and_command_resolve_the_same_dir() {
    let root = tempfile::tempdir().expect("root");
    std::fs::write(
        root.path().join(".hephconfig"),
        "cacheSnapshot:\n  dir: snap\n",
    )
    .expect("write .hephconfig");
    let global = heph::commands::GlobalOptions::default();
    let (build, _shutdown, _) =
        heph::commands::bootstrap::new_validate_engine_at(root.path(), &global, None)
            .expect("build engine");
    let command = heph::commands::bootstrap::new_snapshot_engine_at(
        root.path(),
        &global,
        std::env::vars_os(),
    )
    .expect("snapshot engine");

    let dir = build
        .cache_snapshot_dir()
        .expect("the build has a snapshot dir");
    if std::env::var_os("HEPH_CACHE_SNAPSHOT").is_none() {
        assert!(dir.ends_with("snap"), "{}", dir.display());
    }
    assert_eq!(command.cache_snapshot_dir(), Some(dir));
    assert!(build.working_set_journal().is_some(), "the build records");
    assert!(
        command.working_set_journal().is_none(),
        "the snapshot command never records"
    );
}

// ---------------------------------------------------------------------------
// Scratch slots (O3)
// ---------------------------------------------------------------------------

/// A traveling scratch (`c`) and one that opted out of the remote cache
/// (`l`), and a target that reports what each held and leaves `marker` in
/// both.
fn scratch_build(marker: &str) -> String {
    format!(
        r#"
target(name = "c", driver = "scratch", path = ".cache/c", env = "C", remote = True)
target(name = "l", driver = "scratch", path = ".cache/l", env = "L", remote = False)
target(name = "a", driver = "bash", out = "o.txt", scratch = ["//pkg:c", "//pkg:l"],
       run = ["echo \"c=$(cat $C/m 2>/dev/null || echo cold) l=$(cat $L/m 2>/dev/null || echo cold)\" > $OUT",
              "echo {marker} > $C/m", "echo {marker} > $L/m"])
"#
    )
}

/// One CI job's engine, or the snapshot command's (`record` false, no
/// `dir`): scratch lineage `scope` falling back to `restore`, an optional
/// `file://` bucket, and an optional snapshot dir.
fn scratch_engine(
    root: &Path,
    dir: Option<&Path>,
    bucket: Option<&Path>,
    scope: &str,
    restore: &[&str],
    record: bool,
) -> Arc<Engine> {
    engine_with(Config {
        remote_caches: bucket
            .map(|b| heph::engine::RemoteCacheDef {
                name: "shared".to_string(),
                uri: format!("file://{}", b.display()),
                read: true,
                write: true,
                concurrency: 10,
                endpoint: None,
                region: None,
            })
            .into_iter()
            .collect(),
        scratch: heph::engine::ScratchOptions {
            scope: scope.to_string(),
            restore_scopes: restore.iter().map(|s| (*s).to_string()).collect(),
            seed_on_fork: true,
        },
        record_working_set: record,
        cache_snapshot: dir.map(Path::to_path_buf),
        ..Config::for_tests(root)
    })
}

/// The slot `addr` declares, from the local store.
fn slot_of(engine: &Engine, addr: &str) -> String {
    engine
        .scratch_slots()
        .expect("scratch slots")
        .into_iter()
        .find(|s| s.meta.as_ref().is_some_and(|m| m.addr == addr))
        .expect("a slot for the address")
        .slot
}

/// Export the journals under `root` into `dir` as an engine in `scope`
/// would (the snapshot command shares the build's config), consuming them.
async fn export_scratch(root: &Path, dir: &Path, scope: &str, journals: usize) -> ExportReport {
    let ws = WorkingSet::from_journals(read_settled(root, journals).await);
    let lock = lock_snapshot(&home(root)).expect("snapshot lock");
    let e = scratch_engine(root, None, None, scope, &[], false);
    let report = e
        .export_snapshot(&lock, dir, &ws, &StdCancellationToken::new())
        .await
        .expect("export");
    for j in &ws.journals {
        std::fs::remove_file(j).expect("consume journal");
    }
    report
}

/// The scratch entries of the current generation, as
/// `<slot>/<scope>/<file>` relative to `scratch/v1/`, sorted.
fn snapshot_scratch(dir: &Path) -> Vec<String> {
    let current = read_marker(dir).expect("marker").marker.current;
    let root = dir.join(current).join("scratch").join("v1");
    let mut out = Vec::new();
    for slot in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        for scope in std::fs::read_dir(slot.path()).expect("ls slot").flatten() {
            for f in std::fs::read_dir(scope.path()).expect("ls scope").flatten() {
                out.push(format!(
                    "{}/{}/{}",
                    slot.file_name().to_string_lossy(),
                    scope.file_name().to_string_lossy(),
                    f.file_name().to_string_lossy()
                ));
            }
        }
    }
    out.sort();
    out
}

fn out_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

/// A slot a job used is carried by its snapshot and pulled from it by the
/// next job on a fresh machine with no bucket at all; a slot that opted out
/// of the remote cache is not carried.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scratch_slots_are_carried_unless_opted_out() {
    let parent = tempfile::tempdir().expect("parent");
    let dir = parent.path().join("snap");

    // Job 1: cold everywhere, no bucket. Using the slot is what puts it in
    // the working set: there was nothing to pull and nothing was pushed.
    let one = tempfile::tempdir().expect("machine one");
    write_build(one.path(), &scratch_build("one"));
    let e = scratch_engine(one.path(), Some(&dir), None, "main", &[], true);
    let (out, _) = run_one(&e, "//pkg:a").await;
    assert_eq!(out_text(&out), "c=cold l=cold");
    let (carried, opted_out) = (slot_of(&e, "//pkg:c"), slot_of(&e, "//pkg:l"));
    drop(e);
    let report = export_scratch(one.path(), &dir, "main", 1).await;
    assert_eq!(report.scratch.new.count, 1, "{report:?}");
    assert!(report.scratch.new.bytes > 0, "{report:?}");
    assert_eq!(report.scratch.skipped, 0, "{report:?}");
    let entries = snapshot_scratch(&dir);
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert!(
        entries
            .iter()
            .all(|e| e.starts_with(&format!("{carried}/main/0000000000000000."))),
        "{entries:?}"
    );
    assert!(
        !entries.iter().any(|e| e.starts_with(&opted_out)),
        "{entries:?}"
    );

    // Job 2: a fresh machine. The target changed, so it runs, and its
    // scratch comes from the snapshot tier.
    let two = tempfile::tempdir().expect("machine two");
    write_build(two.path(), &scratch_build("two"));
    let e = scratch_engine(two.path(), Some(&dir), None, "main", &[], true);
    let head = e
        .scratch_remote_head(&carried, "main", &[])
        .await
        .expect("the snapshot serves the slot");
    assert_eq!(head.cache, "snapshot");
    let (out, _) = run_one(&e, "//pkg:a").await;
    assert_eq!(out_text(&out), "c=one l=cold");
    drop(e);

    // Its export publishes the changed slot as the next generation, and
    // the previous one goes with the rest of what the job did not keep.
    let report = export_scratch(two.path(), &dir, "main", 1).await;
    assert_eq!(report.scratch.new.count, 1, "{report:?}");
    assert_eq!(report.scratch.dropped.count, 1, "{report:?}");
    let entries = snapshot_scratch(&dir);
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert!(
        entries
            .iter()
            .all(|e| e.starts_with(&format!("{carried}/main/0000000000000001."))),
        "{entries:?}"
    );
}

/// A slot `heph tool scratch push` published is in the working set though
/// no request recorded the push, and is exported in the job's own scope
/// only: as the pushed entry when unchanged (no new generation), and as
/// `parent + 1` once a later job changed it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pushed_slot_is_exported_in_its_own_scope() {
    let parent = tempfile::tempdir().expect("parent");
    let dir = parent.path().join("snap");
    let bucket = tempfile::tempdir().expect("bucket");

    // The base branch published generation 0 of `main` to the bucket.
    let base = tempfile::tempdir().expect("base machine");
    write_build(base.path(), &scratch_build("base"));
    let e = scratch_engine(base.path(), None, Some(bucket.path()), "main", &[], false);
    run_one(&e, "//pkg:a").await;
    let slot = slot_of(&e, "//pkg:c");
    let head = heph::engine::scratch_store::head_dir(&e.home, &slot, "main");
    e.scratch_push(&slot, "main", &head, None, "base")
        .await
        .expect("push main");
    drop(e);

    // A `feat` job: its build seeds the slot from `main` in the bucket...
    let feat = tempfile::tempdir().expect("feat machine");
    write_build(feat.path(), &scratch_build("feat"));
    let e = scratch_engine(
        feat.path(),
        Some(&dir),
        Some(bucket.path()),
        "feat",
        &["main"],
        true,
    );
    let (out, _) = run_one(&e, "//pkg:a").await;
    assert_eq!(out_text(&out), "c=base l=cold");
    drop(e);
    // ...and a separate process pushes it, as `heph tool scratch push` does.
    let e = scratch_engine(
        feat.path(),
        Some(&dir),
        Some(bucket.path()),
        "feat",
        &["main"],
        true,
    );
    let head = heph::engine::scratch_store::head_dir(&e.home, &slot, "feat");
    let from = heph::engine::scratch_remote::read_local_meta(&e.home, &slot, "feat");
    assert_eq!(from.as_ref().map(|m| m.scope.as_str()), Some("main"));
    let (generation, _) = e
        .scratch_push(&slot, "feat", &head, from.as_ref(), "feat-1")
        .await
        .expect("push feat");
    assert_eq!(generation, 0, "a fork starts its own lineage");
    drop(e);
    let journals = read_settled(feat.path(), 2).await;
    assert!(
        journals.iter().any(|j| j.revisions.is_empty()
            && j.scratch
                .iter()
                .any(|s| s.slot == slot && s.scope == "feat")),
        "the push alone records the slot: {journals:#?}"
    );

    let report = export_scratch(feat.path(), &dir, "feat", 2).await;
    assert_eq!(
        report.scratch.kept.count, 1,
        "unchanged since the push: {report:?}"
    );
    assert_eq!(report.scratch.new.count, 0, "{report:?}");
    let entries = snapshot_scratch(&dir);
    assert_eq!(entries.len(), 2, "{entries:?}");
    let prefix = format!("{slot}/feat/0000000000000000.");
    assert!(
        entries.iter().all(|e| e.starts_with(&prefix)),
        "{entries:?}"
    );
    // The entry is the pushed one, byte for byte.
    let current = read_marker(&dir).expect("marker").marker.current;
    for e in &entries {
        let rel = format!("scratch/v1/{e}");
        assert_eq!(
            std::fs::read(dir.join(&current).join(&rel)).expect("snapshot entry"),
            std::fs::read(bucket.path().join(&rel)).expect("bucket entry"),
            "{rel}"
        );
    }

    // The next `feat` job, on a fresh machine with only the snapshot: it
    // pulls generation 0, changes the slot, and exports generation 1.
    let next = tempfile::tempdir().expect("next machine");
    write_build(next.path(), &scratch_build("feat-2"));
    let e = scratch_engine(next.path(), Some(&dir), None, "feat", &["main"], true);
    let (out, _) = run_one(&e, "//pkg:a").await;
    assert_eq!(out_text(&out), "c=feat l=cold");
    drop(e);
    let report = export_scratch(next.path(), &dir, "feat", 1).await;
    assert_eq!(report.scratch.new.count, 1, "{report:?}");
    let entries = snapshot_scratch(&dir);
    let prefix = format!("{slot}/feat/0000000000000001.");
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert!(
        entries.iter().all(|e| e.starts_with(&prefix)),
        "{entries:?}"
    );
}
