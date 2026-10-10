//! The heph home end to end: a config file resolved the way `heph` resolves it
//! (`ConfigYaml` → `resolve(root)` → `Engine::new`), a target run, and the
//! assertion made on disk — where the cache landed, and where it did not.

use heph::engine::config_yaml::ConfigYaml;
use heph::engine::{ConfigYamlExt as _, DEFAULT_HOME_DIR, Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::{pluginbuildfile, pluginexec};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Resolve a config with `homeDir: home_dir` against `root`, build the engine,
/// and run `//pkg:t`.
async fn run_with_config(root: &Path, home_dir: Option<PathBuf>) -> Arc<Engine> {
    std::fs::create_dir_all(root.join("pkg")).expect("mkdir");
    std::fs::write(
        root.join("pkg").join("BUILD"),
        r#"target(name = "t", driver = "bash", out = "o.txt", run = ["echo hi > o.txt"])"#,
    )
    .expect("write BUILD");

    let file = ConfigYaml {
        home_dir,
        ..Default::default()
    };
    let config = file.resolve(root).expect("resolve config");
    let mut e = Engine::new(config).expect("engine");
    e.register_provider(|init| {
        Box::new(pluginbuildfile::Provider::new(
            init.root.to_path_buf(),
            init.runtime.clone(),
        ))
    })
    .expect("provider");
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash()))
        .expect("driver");
    let e = Arc::new(e);

    let rs = e.new_state();
    e.clone()
        .result_addr(
            rs,
            &parse_addr("//pkg:t").expect("addr"),
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await
        .expect("run //pkg:t");
    e
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_home_is_dot_heph_under_the_root() {
    let ws = tempfile::tempdir().expect("tempdir");
    let root = ws.path();
    let e = run_with_config(root, None).await;

    let home = root.join(DEFAULT_HOME_DIR);
    assert_eq!(e.shared_home.as_path(), home);
    assert_eq!(e.checkout_home.as_path(), home);
    assert!(
        home.join("cache").join("cache.db").is_file(),
        "the local cache must live under {}",
        home.display()
    );
    assert!(!root.join(".heph3").exists(), "nothing writes the old home");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_absolute_home_dir_is_used_as_written() {
    let ws = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let root = ws.path();
    let home = elsewhere.path().join("state");
    let e = run_with_config(root, Some(home.clone())).await;

    assert_eq!(e.shared_home.as_path(), home);
    assert_eq!(e.checkout_home.as_path(), home);
    assert!(
        home.join("cache").join("cache.db").is_file(),
        "the configured home must hold the cache"
    );
    assert!(
        !root.join(DEFAULT_HOME_DIR).exists(),
        "a configured home leaves no default one behind"
    );
}
