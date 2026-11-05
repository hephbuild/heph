//! Per-target cache accounting over the real engine and BUILD files.
//!
//! The engine emits one `CacheOutcome` per cacheable target per request
//! (`LocalHit`, `RemoteHit` or `Miss`), and that — never the tier events
//! `LocalCacheMiss` / `RemoteCacheMiss` — is what a hit rate is folded from.
//! The tier events fire per tier consulted, so a remote hit is also a local
//! miss: folding them counted every remote-served target as a miss, which is
//! how CI reported a cold cache on a fully cached build.

use crate::common::{build_engine, build_engine_with_remote, run_drain_events};
use hcore::events::{CacheCounts, CacheOutcomeKind};
use heph::engine::event::{BuildEvent, BuildEventKind};
use heph::engine::{Engine, HomeDir, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

/// Three cacheable, deterministic, independent targets.
const THREE_TARGETS: &str = r#"
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "a.txt")
target(name = "b", driver = "bash", run = "echo b > $OUT", out = "b.txt")
target(name = "c", driver = "bash", run = "echo c > $OUT", out = "c.txt")
"#;
const THREE: [&str; 3] = ["//pkg:a", "//pkg:b", "//pkg:c"];

fn write_pkg(root: &Path, build: &str) {
    std::fs::create_dir_all(root.join("pkg")).expect("mkdir pkg");
    std::fs::write(root.join("pkg").join("BUILD"), build).expect("write BUILD");
}

fn pkg() -> Matcher {
    Matcher::Package(PkgBuf::from("pkg"))
}

/// `heph r //pkg/...`-ish: run every target of `pkg` on one request.
async fn run_pkg(engine: &Arc<Engine>) -> Vec<BuildEvent> {
    let (res, events) = run_drain_events(engine, &pkg()).await;
    let batch = res.expect("run pkg");
    assert!(
        batch.errors.is_empty(),
        "every target must build: {:?}",
        batch.errors
    );
    events
}

/// Every `CacheOutcome`, as `addr → outcomes` (a list, so a duplicate shows).
fn outcomes(events: &[BuildEvent]) -> BTreeMap<String, Vec<CacheOutcomeKind>> {
    let mut m: BTreeMap<String, Vec<CacheOutcomeKind>> = BTreeMap::new();
    for e in events {
        if let BuildEventKind::CacheOutcome { addr, outcome } = &e.kind {
            m.entry(addr.clone()).or_default().push(*outcome);
        }
    }
    m
}

/// `addrs` each with exactly the one outcome `kind`, and nothing else.
fn all_with(addrs: &[&str], kind: CacheOutcomeKind) -> BTreeMap<String, Vec<CacheOutcomeKind>> {
    addrs.iter().map(|a| (a.to_string(), vec![kind])).collect()
}

fn count(events: &[BuildEvent], pred: impl Fn(&BuildEventKind) -> bool) -> usize {
    events.iter().filter(|e| pred(&e.kind)).count()
}

fn fold(events: &[BuildEvent]) -> CacheCounts {
    let mut c = CacheCounts::default();
    for e in events {
        c.apply(&e.kind);
    }
    c
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_local_run_is_all_local_hits() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(root.path(), THREE_TARGETS);
    let engine = build_engine_with_remote(root.path(), None);

    run_pkg(&engine).await;
    let events = run_pkg(&engine).await;
    assert_eq!(
        outcomes(&events),
        all_with(&THREE, CacheOutcomeKind::LocalHit),
        "{events:?}"
    );
    let counts = fold(&events);
    assert_eq!((counts.hits(), counts.misses()), (3, 0));
}

/// The CI bug: a fresh machine with an empty local cache, every target on the
/// remote. Each target misses locally — a `LocalCacheMiss` per target — and
/// is still a hit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_hits_are_not_misses() {
    let root = tempfile::tempdir().expect("root tempdir");
    let remote = tempfile::tempdir().expect("remote tempdir");
    let remote_uri = format!("file://{}", remote.path().display());
    write_pkg(root.path(), THREE_TARGETS);

    let seeder = build_engine(root.path(), &remote_uri);
    run_pkg(&seeder).await;
    drop(seeder);
    std::fs::remove_dir_all(HomeDir::for_tests(root.path()).join("cache")).expect("rm local cache");

    let engine = build_engine(root.path(), &remote_uri);
    let events = run_pkg(&engine).await;
    assert_eq!(
        outcomes(&events),
        all_with(&THREE, CacheOutcomeKind::RemoteHit),
        "{events:?}"
    );
    assert_eq!(
        count(&events, |e| matches!(
            e,
            BuildEventKind::LocalCacheMiss { .. }
        )),
        3,
        "each remote hit is also a local tier miss: {events:?}"
    );
    let counts = fold(&events);
    assert_eq!((counts.hits(), counts.misses()), (3, 0));
    assert_eq!(counts.hit_rate(), Some(1.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_run_counts_one_miss_per_target() {
    let root = tempfile::tempdir().expect("root tempdir");
    let remote = tempfile::tempdir().expect("remote tempdir");
    let remote_uri = format!("file://{}", remote.path().display());
    write_pkg(root.path(), THREE_TARGETS);

    let engine = build_engine(root.path(), &remote_uri);
    let events = run_pkg(&engine).await;
    assert_eq!(
        outcomes(&events),
        all_with(&THREE, CacheOutcomeKind::Miss),
        "{events:?}"
    );
    // The remote was consulted, and that is still one miss per target.
    assert_eq!(
        count(&events, |e| matches!(
            e,
            BuildEventKind::RemoteCacheMiss { .. }
        )),
        3,
        "{events:?}"
    );
    assert_eq!(fold(&events).hit_rate(), Some(0.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn miss_without_remote_is_still_a_miss() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(root.path(), THREE_TARGETS);

    let engine = build_engine_with_remote(root.path(), None);
    let events = run_pkg(&engine).await;
    assert_eq!(
        outcomes(&events),
        all_with(&THREE, CacheOutcomeKind::Miss),
        "{events:?}"
    );
    assert_eq!(
        count(&events, |e| matches!(
            e,
            BuildEventKind::RemoteCacheMiss { .. }
        )),
        0,
        "no remote, no remote lookup: {events:?}"
    );
}

/// Two output variants of one addr in one request — the whole target and one
/// of its groups, resolved concurrently — are one target and one outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn variants_share_one_outcome() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(
        root.path(),
        r#"target(name = "v", driver = "bash",
       out = {"x": "x.txt", "y": "y.txt"},
       run = "sleep 0.2; echo x > $OUT_X; echo y > $OUT_Y")"#,
    );
    let engine = build_engine_with_remote(root.path(), None);
    let addr = parse_addr("//pkg:v").expect("addr");

    for pass in ["cold", "warm"] {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rs = engine.new_state_with_events(false, Some(tx));
        let resolve = |outputs: OutputMatcher| {
            let (engine, rs, addr) = (Arc::clone(&engine), Arc::clone(&rs), addr.clone());
            tokio::spawn(async move {
                engine
                    .result_addr(rs, &addr, outputs, &ResultOptions::default())
                    .await
            })
        };
        let all = resolve(OutputMatcher::All);
        let x = resolve(OutputMatcher::Exact(vec!["x".to_string()]));
        all.await.expect("join all").expect("all variant");
        x.await.expect("join x").expect("x variant");
        crate::common::drain(rs).await;
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        let want = if pass == "cold" {
            CacheOutcomeKind::Miss
        } else {
            CacheOutcomeKind::LocalHit
        };
        assert_eq!(
            outcomes(&events),
            all_with(&["//pkg:v"], want),
            "{pass}: {events:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uncacheable_emits_no_outcome() {
    let root = tempfile::tempdir().expect("root tempdir");
    let remote = tempfile::tempdir().expect("remote tempdir");
    let remote_uri = format!("file://{}", remote.path().display());
    write_pkg(
        root.path(),
        r#"target(name = "n", driver = "bash", cache = False, run = "echo n > $OUT", out = "n.txt")"#,
    );
    let engine = build_engine(root.path(), &remote_uri);
    for pass in ["cold", "repeat"] {
        let events = run_pkg(&engine).await;
        assert!(outcomes(&events).is_empty(), "{pass}: {events:?}");
        assert_eq!(
            count(&events, |e| matches!(
                e,
                BuildEventKind::CacheHitRebuilt { .. }
            )),
            0,
            "{pass}: {events:?}"
        );
        assert_eq!(fold(&events).consulted(), 0, "{pass}");
    }
}

/// I5 over the real stream: on a mixed graph — cacheable and uncacheable
/// targets, with deps between them — the fold's `consulted()` is the number
/// of distinct cacheable addrs the request resolved, and each of them has
/// exactly one outcome, cold and warm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn counts_add_up_over_the_real_stream() {
    let root = tempfile::tempdir().expect("root tempdir");
    let remote = tempfile::tempdir().expect("remote tempdir");
    let remote_uri = format!("file://{}", remote.path().display());
    write_pkg(
        root.path(),
        r#"
lib = target(name = "lib", driver = "bash", run = "echo lib > $OUT", out = "lib.txt")
gen = target(name = "gen", driver = "bash", cache = False,
    run = "cat $SRC_LIB > $OUT", deps = {"lib": lib}, out = "gen.txt")
app = target(name = "app", driver = "bash",
    run = "cat $SRC_LIB > $OUT", deps = {"lib": lib}, out = "app.txt")
target(name = "tool", driver = "bash", cache = False,
    run = "cat $SRC_APP $SRC_GEN > $OUT", deps = {"app": app, "gen": gen}, out = "tool.txt")
"#,
    );
    let cacheable: BTreeSet<String> = ["//pkg:app", "//pkg:lib"]
        .into_iter()
        .map(String::from)
        .collect();
    let engine = build_engine(root.path(), &remote_uri);

    for (pass, want) in [
        ("cold", CacheOutcomeKind::Miss),
        ("warm", CacheOutcomeKind::LocalHit),
    ] {
        let events = run_pkg(&engine).await;
        let got = outcomes(&events);
        assert_eq!(
            got.keys().cloned().collect::<BTreeSet<_>>(),
            cacheable,
            "{pass}: outcomes for exactly the cacheable addrs: {events:?}"
        );
        for (addr, kinds) in &got {
            assert_eq!(kinds, &vec![want], "{pass}: {addr}: {events:?}");
        }
        // Every cacheable addr this request resolved, from its `ResultEnd`.
        let resolved: BTreeSet<String> = events
            .iter()
            .filter_map(|e| match &e.kind {
                BuildEventKind::ResultEnd { addr, .. } if cacheable.contains(addr) => {
                    Some(addr.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(resolved, cacheable, "{pass}: {events:?}");
        assert_eq!(
            fold(&events).consulted(),
            u64::try_from(resolved.len()).expect("count fits"),
            "{pass}"
        );
    }
}
