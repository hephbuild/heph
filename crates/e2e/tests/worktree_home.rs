#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]
//! A linked git worktree shares the main checkout's home — and so its cache —
//! while what belongs to one working tree (sandboxes, the fswalk cache) stays
//! in the worktree's own.
//!
//! The git layouts are written by hand (`.git` dir in main, `.git` file plus
//! `<common>/worktrees/<name>/` for the worktree): heph reads them as files and
//! never runs `git`, so neither does this.

mod common;

use heph::engine::git_checkout::test_layout::{linked_worktree, main_checkout};
use heph::engine::{Config, Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::pluginbuildfile;
use heph::pluginexec;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Prints a fresh stamp: two runs that agree on it are one execution and a
/// cache hit.
const STAMPED: &str = r#"target(name = "a", driver = "bash", run = "printf '%s-%s' $$ $RANDOM > $OUT", out = "out.txt")"#;

struct Repo {
    _tmp: tempfile::TempDir,
    main: PathBuf,
    wt: PathBuf,
}

/// `<tmp>/main` and its linked worktree `<tmp>/wt`. Canonicalized up front so a
/// path the engine reports compares equal to one built here (macOS's `/var`
/// is a symlink to `/private/var`).
fn repo() -> Repo {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canonicalize tempdir");
    let main = base.join("main");
    let wt = base.join("wt");
    main_checkout(&main, "master");
    linked_worktree(&main, &wt, "wt", "feat");
    Repo {
        _tmp: tmp,
        main,
        wt,
    }
}

fn write_build(root: &Path, pkg: &str, content: &str) {
    let dir = root.join(pkg);
    std::fs::create_dir_all(&dir).expect("mkdir pkg");
    std::fs::write(dir.join("BUILD"), content).expect("write BUILD");
}

/// What `heph` started in `root` builds: the homes come from the one resolver,
/// with worktree detection on (the default).
fn engine_at(root: &Path) -> anyhow::Result<Arc<Engine>> {
    engine_with(Config::for_tests_detected(root.to_path_buf()))
}

fn engine_with(cfg: Config) -> anyhow::Result<Arc<Engine>> {
    let root = cfg.root.clone();
    let mut e = Engine::new(cfg)?;
    e.register_provider(move |init| {
        Box::new(pluginbuildfile::Provider::new(root, init.runtime.clone()))
    })?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash()))?;
    let e = Arc::new(e);
    e.install_exec_runner_host();
    Ok(e)
}

/// Build `addr` and wait for its background cache write to land, as a `heph
/// run` exiting would.
async fn run_and_settle(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<String> {
    let addr = parse_addr(addr)?;
    let bg: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let rs = engine.new_state_full(
        true,
        None,
        Arc::clone(&bg),
        Engine::DEFAULT_LOG_TAIL_LINES,
        None,
    );
    let result = engine
        .clone()
        .result_addr(
            rs.clone(),
            &addr,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    let out = common::artifact_string(&result);
    drop(result);
    drop(rs);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while bg.load(Ordering::Acquire) > 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "background work never drained"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    Ok(out)
}

#[tokio::test]
async fn worktree_hits_main_cache() -> anyhow::Result<()> {
    let r = repo();
    write_build(&r.main, "p", STAMPED);
    write_build(&r.wt, "p", STAMPED);

    let built = run_and_settle(&engine_at(&r.main)?, "//p:a").await?;

    let wt = engine_at(&r.wt)?;
    assert_eq!(wt.shared_home.as_path(), r.main.join(".heph"));
    assert_eq!(wt.checkout_home.as_path(), r.wt.join(".heph"));
    assert_eq!(
        run_and_settle(&wt, "//p:a").await?,
        built,
        "the worktree re-executed instead of reading the main checkout's cache"
    );
    assert!(
        !r.wt.join(".heph").join("cache").join("cache.db").exists(),
        "the worktree must not grow a cache of its own"
    );
    Ok(())
}

#[tokio::test]
async fn worktree_sandbox_stays_in_worktree() -> anyhow::Result<()> {
    let r = repo();
    write_build(
        &r.wt,
        "p",
        r#"target(name = "where", driver = "bash", run = "printf '%s' \"$WORKSPACE_ROOT\" > $OUT", out = "out.txt")"#,
    );

    let ws_root = PathBuf::from(run_and_settle(&engine_at(&r.wt)?, "//p:where").await?);
    assert!(
        ws_root.starts_with(r.wt.join(".heph")),
        "sandbox {} is not under the worktree's own home",
        ws_root.display()
    );
    let main_home = r.main.join(".heph");
    assert!(
        main_home.join("cache").exists(),
        "the result went to the shared home"
    );
    assert!(
        !main_home.join("sandbox").exists(),
        "no sandbox under the shared home"
    );
    Ok(())
}

/// Staged read-only inputs live in the worktree's own home, next to the
/// sandboxes they are hardlinked and symlinked into. In the shared home a
/// hardlink would fail with EXDEV across filesystems, and a symlink would
/// dangle in an OCI container that mounts only the checkout's home. FUSE is
/// forced off: staging is the OS sandbox runner's path.
#[tokio::test]
async fn worktree_staged_inputs_stay_in_worktree() -> anyhow::Result<()> {
    use heph::engine::config_yaml::FuseEnabled;
    let r = repo();
    // The read-only dep lives in its own package: staging symlinks the largest
    // subtree it owns, and the consumer writes into its own package dir.
    write_build(
        &r.wt,
        "sdk",
        r#"target(name = "tool", driver = "bash", run = "printf '#!/bin/sh\n' > $OUT && chmod +x $OUT", out = "tool.sh")"#,
    );
    write_build(
        &r.wt,
        "p",
        r#"target(name = "use", driver = "bash", deps = {"sdk": ["//sdk:tool"]}, read_only_deps = ["sdk"], run = "printf ok > $OUT", out = "out.txt")"#,
    );
    let wt = || {
        engine_with(heph::engine::Config {
            fuse: heph::engine::FuseConfig {
                enabled: Some(FuseEnabled::Off),
            },
            ..Config::for_tests_detected(r.wt.clone())
        })
    };
    // The tool is cached first: a stage entry is keyed by the artifact's
    // content hash, which a cache-backed artifact carries.
    run_and_settle(&wt()?, "//sdk:tool").await?;
    assert_eq!(run_and_settle(&wt()?, "//p:use").await?, "ok");

    let listing = |p: &Path| -> Vec<String> {
        std::fs::read_dir(p)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(
        r.wt.join(".heph").join("stage").is_dir(),
        "the tool was staged under the worktree's own home; wt home: {:?}, shared home: {:?}",
        listing(&r.wt.join(".heph")),
        listing(&r.main.join(".heph")),
    );
    assert!(
        !r.main.join(".heph").join("stage").exists(),
        "nothing is staged under the shared home"
    );
    Ok(())
}

#[tokio::test]
async fn fswalk_db_is_per_checkout() -> anyhow::Result<()> {
    let r = repo();
    write_build(&r.wt, "p", STAMPED);

    // The worktree runs first, and alone: anything that appears under the main
    // checkout's home was put there by the worktree's engine.
    run_and_settle(&engine_at(&r.wt)?, "//p:a").await?;
    assert!(
        r.wt.join(".heph").join("cache").join("fswalk.db").exists(),
        "the worktree's walk cache is its own"
    );
    assert!(
        !r.main
            .join(".heph")
            .join("cache")
            .join("fswalk.db")
            .exists(),
        "the worktree's walk cache must not land in the shared home"
    );
    Ok(())
}

#[tokio::test]
async fn gc_in_worktree_keeps_main_only_target() -> anyhow::Result<()> {
    let r = repo();
    write_build(
        &r.main,
        "p",
        r#"target(name = "only_main", driver = "bash", run = "printf '%s-%s' $$ $RANDOM > $OUT", out = "out.txt")"#,
    );
    // The worktree's branch has no such target.
    write_build(&r.wt, "p", STAMPED);

    let built = run_and_settle(&engine_at(&r.main)?, "//p:only_main").await?;

    let wt = engine_at(&r.wt)?;
    let stats = wt.clone().gc_all(wt.new_state()).await?;
    assert!(stats.orphan_sweep_skipped.is_some(), "{stats:?}");
    assert_eq!(stats.orphans_kept, 1, "{stats:?}");
    assert_eq!(stats.orphan_targets_removed, 0, "{stats:?}");

    // The main checkout's next run is still a hit.
    assert_eq!(
        run_and_settle(&engine_at(&r.main)?, "//p:only_main").await?,
        built,
        "gc in the worktree dropped the main checkout's target"
    );

    // The main checkout itself knows its home is shared while worktrees exist,
    // so a gc there does not orphan-sweep a worktree's targets either.
    let main = engine_at(&r.main)?;
    let stats = main.clone().gc_all(main.new_state()).await?;
    assert!(stats.orphan_sweep_skipped.is_some(), "{stats:?}");
    Ok(())
}

/// A stamp per execution, with remote caching off: its entry is per checkout.
const LOCAL_ONLY: &str = r#"target(name = "lo", driver = "bash", cache = {"remote": False}, run = "printf '%s-%s' $$ $RANDOM > $OUT", out = "out.txt")"#;

fn names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// A target that never goes to a remote (`cache.remote = False`) is one whose
/// bytes may name this checkout's paths — a runner's `runner.json`. Its entry
/// is per checkout: the worktree does not hit the main checkout's, builds its
/// own under its own home, and leaves main's intact. A remote-eligible target
/// beside it is still shared.
#[tokio::test]
async fn local_only_entries_are_per_checkout() -> anyhow::Result<()> {
    let r = repo();
    let build = format!("{STAMPED}\n{LOCAL_ONLY}\n");
    write_build(&r.main, "p", &build);
    write_build(&r.wt, "p", &build);

    let main_lo = run_and_settle(&engine_at(&r.main)?, "//p:lo").await?;
    let main_a = run_and_settle(&engine_at(&r.main)?, "//p:a").await?;

    let wt = engine_at(&r.wt)?;
    assert_eq!(
        run_and_settle(&wt, "//p:a").await?,
        main_a,
        "a remote-eligible target is still shared"
    );
    assert!(
        !r.wt.join(".heph").join("cache").join("cache.db").exists(),
        "no checkout store until a local-only target needs one"
    );
    let wt_lo = run_and_settle(&wt, "//p:lo").await?;
    assert_ne!(
        wt_lo, main_lo,
        "the worktree must not be served the main checkout's local-only entry"
    );
    assert!(
        r.wt.join(".heph").join("cache").join("cache.db").exists(),
        "the worktree's entry is in its own home; wt cache: {:?}",
        names(&r.wt.join(".heph").join("cache"))
    );
    // Each checkout now hits its own.
    assert_eq!(run_and_settle(&engine_at(&r.wt)?, "//p:lo").await?, wt_lo);
    assert_eq!(
        run_and_settle(&engine_at(&r.main)?, "//p:lo").await?,
        main_lo,
        "the main checkout's entry is intact"
    );
    Ok(())
}

/// Same BUILD, different source bytes: the worktree's key differs, so it
/// misses and builds its own entry; the main checkout's entry survives and
/// still hits.
///
/// `history = 2`: `cache.history` counts a target's revisions in the shared
/// store, across every checkout. At the default of 1 the worktree's write
/// trims the main checkout's revision — see the next test.
#[tokio::test]
async fn divergent_sources_miss_and_keep_mains_entry() -> anyhow::Result<()> {
    let r = repo();
    let build = r#"target(name = "d", driver = "bash", deps = [file("in.txt")], cache = {"history": 2}, run = "cat $SRC > $OUT; printf -- '-%s' $RANDOM >> $OUT", out = "out.txt")"#;
    write_build(&r.main, "p", build);
    write_build(&r.wt, "p", build);
    std::fs::write(r.main.join("p").join("in.txt"), "main")?;
    std::fs::write(r.wt.join("p").join("in.txt"), "wt")?;

    let main_out = run_and_settle(&engine_at(&r.main)?, "//p:d").await?;
    assert!(main_out.starts_with("main-"), "{main_out}");
    let wt_out = run_and_settle(&engine_at(&r.wt)?, "//p:d").await?;
    assert!(
        wt_out.starts_with("wt-"),
        "the worktree built its own: {wt_out}"
    );
    assert_eq!(
        run_and_settle(&engine_at(&r.main)?, "//p:d").await?,
        main_out,
        "the main checkout still hits its own entry"
    );
    Ok(())
}

/// Pins today's behaviour, which is a known cost of sharing: `cache.history`
/// is per target in the shared store, not per checkout. At the default of 1,
/// two checkouts building different revisions of one target evict each
/// other's, and each rebuilds on its next run.
#[tokio::test]
async fn divergent_sources_at_history_1_evict_each_other() -> anyhow::Result<()> {
    let r = repo();
    let build = r#"target(name = "d", driver = "bash", deps = [file("in.txt")], run = "cat $SRC > $OUT; printf -- '-%s' $RANDOM >> $OUT", out = "out.txt")"#;
    write_build(&r.main, "p", build);
    write_build(&r.wt, "p", build);
    std::fs::write(r.main.join("p").join("in.txt"), "main")?;
    std::fs::write(r.wt.join("p").join("in.txt"), "wt")?;

    let main_out = run_and_settle(&engine_at(&r.main)?, "//p:d").await?;
    run_and_settle(&engine_at(&r.wt)?, "//p:d").await?;
    let again = run_and_settle(&engine_at(&r.main)?, "//p:d").await?;
    assert!(again.starts_with("main-"), "{again}");
    assert_ne!(
        again, main_out,
        "history = 1 kept only the worktree's revision, so main rebuilt"
    );
    Ok(())
}

/// Every per-field split, observed from inside a run in the worktree: the
/// execute lock is under the worktree's home and the gateway under the main
/// checkout's (both held while the target runs); scratch lands in the shared
/// home; the FUSE mount, when there is one, never does.
#[tokio::test]
async fn per_checkout_wiring_in_a_worktree() -> anyhow::Result<()> {
    let r = repo();
    let wt_lock = r.wt.join(".heph").join("lock");
    let main_lock = r.main.join(".heph").join("lock");
    write_build(
        &r.wt,
        "build",
        r#"target(name = "c", driver = "scratch", env = "MYCACHE")"#,
    );
    write_build(
        &r.wt,
        "p",
        &format!(
            r#"target(name = "w", driver = "bash", out = "out.txt", scratch = ["//build:c"], run = "{{ ls '{}'; echo ---; ls '{}'; echo ---; echo \"$MYCACHE\"; }} > $OUT")"#,
            wt_lock.display(),
            main_lock.display()
        ),
    );

    let wt = engine_at(&r.wt)?;
    let out = run_and_settle(&wt, "//p:w").await?;
    let mut parts = out.split("---\n");
    let (wt_locks, main_locks, scratch) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default().trim(),
    );
    assert!(
        wt_locks.lines().any(|l| l.ends_with(".execute.lock")),
        "the execute lock is the worktree's: {out}"
    );
    assert!(
        !main_locks.lines().any(|l| l.ends_with(".execute.lock")),
        "no execute lock in the shared home: {out}"
    );
    assert!(
        main_locks.lines().any(|l| l.ends_with(".outer.lock")),
        "the gateway lock is the shared home's: {out}"
    );
    assert!(
        !wt_locks.lines().any(|l| l.ends_with(".outer.lock")),
        "no gateway lock in the worktree's home: {out}"
    );
    assert!(
        Path::new(scratch).starts_with(r.main.join(".heph").join("scratch")),
        "scratch lands in the shared home: {scratch}"
    );

    // Credential locks (and with them `auth/`'s lock namespace) are the
    // shared home's.
    assert!(main_lock.join("auth").is_dir());
    assert!(!wt_lock.join("auth").exists());

    assert!(
        !names(&r.main.join(".heph"))
            .iter()
            .any(|n| n.starts_with("sandboxfuse")),
        "a FUSE mount is never in the shared home"
    );
    if !names(&r.wt.join(".heph"))
        .iter()
        .any(|n| n.starts_with("sandboxfuse"))
    {
        eprintln!("FUSE unavailable here: its placement is not observed");
    }
    Ok(())
}

/// A credential's cached material lives in the shared home's `auth/`: one
/// login serves every checkout.
#[tokio::test]
async fn credential_cache_is_in_the_shared_home() -> anyhow::Result<()> {
    let r = repo();
    write_build(
        &r.wt,
        "auth",
        r#"target(name = "t", driver = "credential", sources = [heph.auth.env(["HOME"])], ttl = "10m", present = {"env": {"X": "${home}"}})"#,
    );
    write_build(
        &r.wt,
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False, credentials = ["//auth:t"], run = "printf ok > o.txt")"#,
    );
    assert_eq!(run_and_settle(&engine_at(&r.wt)?, "//app:a").await?, "ok");
    assert!(
        r.main.join(".heph").join("auth").is_dir(),
        "auth/ is in the shared home; shared: {:?}",
        names(&r.main.join(".heph"))
    );
    assert!(
        !r.wt.join(".heph").join("auth").exists(),
        "and not in the worktree's"
    );
    Ok(())
}

/// The negative control for the orphan skip: with nothing to share with (a
/// checkout without linked worktrees), gc removes a target that no longer
/// resolves.
#[tokio::test]
async fn gc_without_sharing_removes_an_orphan() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let main = tmp.path().canonicalize()?.join("main");
    main_checkout(&main, "master");
    write_build(&main, "p", STAMPED);
    run_and_settle(&engine_at(&main)?, "//p:a").await?;
    write_build(&main, "p", "");

    let e = engine_at(&main)?;
    let stats = e.clone().gc_all(e.new_state()).await?;
    assert!(stats.orphan_sweep_skipped.is_none(), "{stats:?}");
    assert_eq!(stats.orphan_targets_removed, 1, "{stats:?}");
    Ok(())
}

/// The checkout's own store is nobody else's, so gc sweeps its orphans even
/// where the shared store's orphan sweep is skipped.
#[tokio::test]
async fn gc_sweeps_the_checkout_store_in_a_worktree() -> anyhow::Result<()> {
    let r = repo();
    write_build(&r.wt, "p", LOCAL_ONLY);
    run_and_settle(&engine_at(&r.wt)?, "//p:lo").await?;
    write_build(&r.wt, "p", "");

    let wt = engine_at(&r.wt)?;
    let stats = wt.clone().gc_all(wt.new_state()).await?;
    assert!(stats.orphan_sweep_skipped.is_some(), "{stats:?}");
    assert_eq!(
        stats.orphan_targets_removed, 1,
        "the local-only orphan goes: {stats:?}"
    );
    Ok(())
}

/// Two engines in one process — the main checkout's and the worktree's — on
/// one addr at once: the shared gateway lets one execute, and the other waits
/// and hits its entry.
#[tokio::test]
async fn main_and_worktree_build_one_addr_once() -> anyhow::Result<()> {
    let r = repo();
    let slow = r#"target(name = "s", driver = "bash", run = "sleep 0.3; printf '%s-%s' $$ $RANDOM > $OUT", out = "out.txt")"#;
    write_build(&r.main, "p", slow);
    write_build(&r.wt, "p", slow);
    let (main, wt) = (engine_at(&r.main)?, engine_at(&r.wt)?);
    let (a, b) = tokio::join!(run_and_settle(&main, "//p:s"), run_and_settle(&wt, "//p:s"));
    assert_eq!(a?, b?, "one execution, one hit");
    Ok(())
}
