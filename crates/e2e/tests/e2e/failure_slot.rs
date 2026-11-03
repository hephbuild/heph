//! The last-failure slot (`<home>/failures/<addr-key>/`): filled once per failed
//! run, read by the failure box, cleared when the target next resolves, and
//! never part of the cache. Every test has its own `TempDir` home.

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common::Workspace;
use heph::engine::error::TargetFailure;
use heph::engine::{Config, Engine, OutputMatcher, RemoteCacheDef, ResultOptions};
use heph::htaddr::parse_addr;
use heph::{pluginbuildfile, pluginexec};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Resolve `addr` on `engine` and return the failures it recorded.
async fn failures(
    engine: &Arc<Engine>,
    addr: &str,
    opts: &ResultOptions,
) -> anyhow::Result<Vec<Arc<TargetFailure>>> {
    let rs = engine.new_state();
    let res = engine
        .clone()
        .result_addr(rs.clone(), &parse_addr(addr)?, OutputMatcher::All, opts)
        .await;
    assert!(res.is_err(), "{addr} must fail");
    Ok(rs.take_failures())
}

fn slot_log(engine: &Engine, addr: &str) -> Option<String> {
    let addr = parse_addr(addr).expect("addr");
    let slots = engine.failure_slots();
    slots.read(&addr)?;
    std::fs::read_to_string(slots.slot_dir(&addr).join("log.txt")).ok()
}

fn slot_exists(engine: &Engine, addr: &str) -> bool {
    engine
        .failure_slots()
        .slot_dir(&parse_addr(addr).expect("addr"))
        .exists()
}

fn render(f: &[Arc<TargetFailure>]) -> String {
    heph::commands::errors::render_failures_to_string(f, false)
}

/// Every file under `dir` whose bytes contain `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files_containing(&p, needle));
        } else if std::fs::read(&p)
            .map(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
            .unwrap_or(false)
        {
            out.push(p);
        }
    }
    out
}

#[tokio::test]
async fn failure_box_shows_exec_log_from_slot() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", run = "echo box-marker-line; exit 3")"#,
    );
    let f = failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    let out = render(&f);
    assert!(out.contains("│ box-marker-line"), "{out}");
    let path = ws
        .engine
        .failure_slots()
        .slot_dir(&parse_addr("//p:t")?)
        .join("log.txt");
    assert!(
        out.contains(&format!("full log: {}", path.display())),
        "{out}"
    );
    assert!(path.is_absolute());
    assert_eq!(
        slot_log(&ws.engine, "//p:t").as_deref(),
        Some("box-marker-line\n")
    );
    Ok(())
}

#[tokio::test]
async fn failure_slot_filled_once_for_two_requesters() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"
target(name = "a", driver = "bash", run = "echo once; exit 1")
target(name = "grp", driver = "group", deps = ["//p:a"])
"#,
    );
    let rs = ws.engine.new_state();
    let (a, grp) = (parse_addr("//p:a")?, parse_addr("//p:grp")?);
    let opts = ResultOptions::default();
    let (ra, rg) = tokio::join!(
        ws.engine
            .clone()
            .result_addr(rs.clone(), &a, OutputMatcher::All, &opts),
        ws.engine
            .clone()
            .result_addr(rs.clone(), &grp, OutputMatcher::All, &opts),
    );
    assert!(ra.is_err() && rg.is_err());
    assert_eq!(ws.engine.failure_slots().fills(), 1);
    let f = rs.take_failures();
    assert!(render(&f).contains("│ once"), "{}", render(&f));
    Ok(())
}

fn engine_with_remote(root: &Path, remote_uri: &str) -> Arc<Engine> {
    let mut e = Engine::new(Config {
        remote_caches: vec![RemoteCacheDef {
            name: "shared".to_string(),
            uri: remote_uri.to_string(),
            read: true,
            write: true,
            concurrency: 10,
            endpoint: None,
            region: None,
        }],
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
    .expect("provider");
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))
        .expect("bash");
    Arc::new(e)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_target_log_not_in_cache() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let remote = tempfile::tempdir()?;
    std::fs::create_dir_all(root.path().join("p"))?;
    std::fs::write(
        root.path().join("p/BUILD"),
        r#"
target(name = "ok", driver = "bash", run = "echo fine > $OUT", out = "o.txt")
target(name = "t", driver = "bash", deps = ["//p:ok"], run = "echo never-cached-marker; exit 1")
"#,
    )?;
    let engine = engine_with_remote(root.path(), &format!("file://{}", remote.path().display()));
    let rs = engine.new_state();
    let res = engine
        .clone()
        .result_addr(
            rs.clone(),
            &parse_addr("//p:t")?,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await;
    assert!(res.is_err());
    let bg = rs.bg_pending();
    drop(rs);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while bg.load(std::sync::atomic::Ordering::Acquire) > 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "uploads never drained"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(slot_log(&engine, "//p:t").is_some(), "the slot holds it");
    let home = engine.home.to_path_buf();
    assert_eq!(
        files_containing(&home.join("cache"), "never-cached-marker"),
        Vec::<PathBuf>::new()
    );
    assert_eq!(
        files_containing(remote.path(), "never-cached-marker"),
        Vec::<PathBuf>::new()
    );
    assert!(
        !files_containing(remote.path(), "fine").is_empty()
            || std::fs::read_dir(remote.path())?.next().is_some(),
        "the dep was uploaded, so the remote was written"
    );
    Ok(())
}

#[tokio::test]
async fn failure_log_does_not_move_hashes() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", run = "echo hashes; exit 1")"#,
    );
    let hashin = |engine: Arc<Engine>| async move {
        let rs = engine.new_state();
        anyhow::Ok(engine.meta(rs, &parse_addr("//p:t")?).await?.hashin)
    };
    let before = hashin(ws.engine.clone()).await?;
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    assert!(slot_exists(&ws.engine, "//p:t"));
    let after = hashin(ws.reopen()?).await?;
    assert_eq!(before, after);
    Ok(())
}

/// A BUILD file whose `//p:t` fails or not depending on a file outside the
/// workspace, which no hash sees: `<ctl>/fail` makes it fail, printing
/// `<ctl>/fail`'s contents; `<ctl>/noout` makes it exit 0 without its output.
fn toggled(ws: &Workspace, ctl: &Path, cache: bool) {
    let c = ctl.display();
    ws.write_build_file(
        "p",
        &format!(
            r#"target(name = "t", driver = "bash", out = "o.txt", cache = {cache},
       run = "if [ -f {c}/fail ]; then cat {c}/fail; exit 1; fi; [ -f {c}/noout ] || echo ok > o.txt")"#,
            cache = if cache { "True" } else { "False" },
        ),
    );
}

#[tokio::test]
async fn failure_slot_cleared_when_target_reruns() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let ctl = tempfile::tempdir()?;
    toggled(&ws, ctl.path(), false);
    std::fs::write(ctl.path().join("fail"), "rerun-marker\n")?;
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    assert!(slot_exists(&ws.engine, "//p:t"));
    std::fs::remove_file(ctl.path().join("fail"))?;
    ws.run("//p:t").await?;
    assert!(!slot_exists(&ws.engine, "//p:t"));
    Ok(())
}

#[tokio::test]
async fn failure_slot_cleared_on_cache_hit() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let ctl = tempfile::tempdir()?;
    toggled(&ws, ctl.path(), true);
    ws.run("//p:t").await?;
    // A forced rerun at the same revision fails, as another process's would.
    std::fs::write(ctl.path().join("fail"), "hit-marker\n")?;
    let forced = ResultOptions {
        force: true,
        ..Default::default()
    };
    failures(&ws.engine, "//p:t", &forced).await?;
    assert!(slot_exists(&ws.engine, "//p:t"));
    // A fresh engine (a later `heph`) hits the cache, and clears it.
    let later = ws.reopen()?;
    let rs = later.new_state();
    later
        .clone()
        .result_addr(
            rs,
            &parse_addr("//p:t")?,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    assert!(!slot_exists(&later, "//p:t"));
    Ok(())
}

#[tokio::test]
async fn failure_slot_cleared_when_rerun_fails_without_log() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let ctl = tempfile::tempdir()?;
    toggled(&ws, ctl.path(), false);
    std::fs::write(ctl.path().join("fail"), "first-marker\n")?;
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    assert!(slot_exists(&ws.engine, "//p:t"));
    std::fs::remove_file(ctl.path().join("fail"))?;
    std::fs::write(ctl.path().join("noout"), "")?;
    let f = failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    assert!(!render(&f).contains("first-marker"), "{}", render(&f));
    assert!(!slot_exists(&ws.engine, "//p:t"));
    Ok(())
}

#[tokio::test]
async fn failure_slot_replaced_on_refailure() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let ctl = tempfile::tempdir()?;
    toggled(&ws, ctl.path(), false);
    std::fs::write(ctl.path().join("fail"), "marker-one\n")?;
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    std::fs::write(ctl.path().join("fail"), "marker-two\n")?;
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    assert_eq!(
        slot_log(&ws.engine, "//p:t").as_deref(),
        Some("marker-two\n")
    );
    let names: Vec<String> = std::fs::read_dir(ws.engine.failure_slots().root())?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_, _>>()?;
    assert_eq!(names.len(), 1, "one slot, no temporaries: {names:?}");
    Ok(())
}

#[tokio::test]
async fn failure_slot_per_variant() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", run = "echo variant; exit 1")"#,
    );
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    failures(&ws.engine, "//p:t@v=1", &ResultOptions::default()).await?;
    let slots = ws.engine.failure_slots();
    let (plain, variant) = (parse_addr("//p:t")?, parse_addr("//p:t@v=1")?);
    assert_ne!(slots.slot_dir(&plain), slots.slot_dir(&variant));
    assert_eq!(slots.read(&plain).map(|m| m.addr), Some(plain.format()));
    assert_eq!(slots.read(&variant).map(|m| m.addr), Some(variant.format()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_failures_same_target_slot_never_mixed() -> anyhow::Result<()> {
    let ws = Workspace::new();
    // Each run prints its own id on every line.
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", cache = False,
       run = "id=$$; for i in $(seq 1 200); do echo run-$id; done; exit 1")"#,
    );
    let other = ws.reopen()?;
    let opts = ResultOptions::default();
    let (fa, fb) = tokio::join!(
        failures(&ws.engine, "//p:t", &opts),
        failures(&other, "//p:t", &opts)
    );
    let homogeneous = |s: &str| {
        let mut lines = s.lines().filter(|l| l.contains("run-"));
        let first = lines
            .next()
            .map(|l| l.rsplit('│').next().unwrap_or(l).trim().to_string());
        lines.all(|l| Some(l.rsplit('│').next().unwrap_or(l).trim().to_string()) == first)
    };
    for f in [fa?, fb?] {
        let out = render(&f);
        assert!(out.contains("run-"), "{out}");
        assert!(homogeneous(&out), "a box mixes two runs: {out}");
    }
    let log = slot_log(&ws.engine, "//p:t").expect("a complete slot");
    assert_eq!(log.lines().count(), 200);
    assert!(homogeneous(&log), "the slot mixes two runs");
    Ok(())
}

#[tokio::test]
async fn credential_target_failure_has_no_slot() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"
target(name = "fetch", driver = "bash", out = {"credential": "cred.json"}, cache = False,
       run = ["echo producer-marker", "exit 4"])
target(name = "t", driver = "auth.credential", sources = ["//auth:fetch"],
       present = {"env": {"T": "${token}"}})
"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$T\" > o.txt"])"#,
    );
    let f = failures(&ws.engine, "//app:a", &ResultOptions::default()).await?;
    assert!(!slot_exists(&ws.engine, "//auth:fetch"));
    assert!(
        !ws.engine.failure_slots().root().exists()
            || std::fs::read_dir(ws.engine.failure_slots().root())?
                .next()
                .is_none(),
        "no slot for anything"
    );
    // The consumer's box never had the producer's tail: the credential layer
    // carries the producer's failure as a message (unchanged here). The
    // in-memory tail of a material target is covered by the engine's
    // `material_target_keeps_tail_in_memory`.
    let out = render(&f);
    assert!(!out.contains("full log:"), "{out}");
    Ok(())
}

#[tokio::test]
async fn dep_failure_shows_dep_log() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"
target(name = "dep", driver = "bash", run = "echo dep-marker; exit 2", out = "d.txt")
target(name = "top", driver = "bash", deps = ["//p:dep"], run = "true")
"#,
    );
    let f = failures(&ws.engine, "//p:top", &ResultOptions::default()).await?;
    let dep = f
        .iter()
        .find(|f| f.addr.format() == "//p:dep")
        .expect("the dep's failure");
    let out = render(std::slice::from_ref(dep));
    assert!(out.contains("│ dep-marker"), "{out}");
    let path = ws
        .engine
        .failure_slots()
        .slot_dir(&parse_addr("//p:dep")?)
        .join("log.txt");
    assert!(
        out.contains(&format!("full log: {}", path.display())),
        "{out}"
    );
    Ok(())
}
