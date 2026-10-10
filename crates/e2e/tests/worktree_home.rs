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
    engine_with(Config::for_tests(root.to_path_buf()))
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
            ..Config::for_tests(r.wt.clone())
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
