#![expect(
    clippy::panic_in_result_fn,
    clippy::panic,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `heph.<plugin>.<fn>` from a BUILD file, end to end: what a reference to a
//! plugin or function that does not exist says, and that moving functions
//! from providers to plugins re-keyed nothing.

mod common;

use common::Workspace;
use hcore::htvalue::Value;
use heph::engine::PluginParts;
use std::sync::Arc;

fn srcs(spec: &heph::engine::provider::TargetSpec, key: &str) -> Vec<String> {
    match spec.config.get(key) {
        Some(Value::List(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => panic!("expected a string, got {other:?}"),
            })
            .collect(),
        other => panic!("expected a list under {key:?}, got {other:?}"),
    }
}

/// D10: an unknown plugin fails before evaluation with the BUILD file and
/// line, the closest plugin, the plugins that do have functions, and where to
/// look — enough for an agent to fix the call from the text alone.
#[tokio::test]
async fn unknown_plugin_in_build_file_is_actionable() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        "x = 1\ntarget(name = \"t\", driver = \"bash\", run = \"true\", deps = heph.fss.glob(\"*\"))",
    );
    let err = ws.get_spec("//p:t").await.err().expect("unknown plugin");
    let msg = format!("{err:#}");
    assert!(msg.contains("p/BUILD:2:"), "file and line: {msg}");
    assert!(
        msg.contains("heph.fss.glob: no plugin named \"fss\" (did you mean \"fs\"?)"),
        "{msg}"
    );
    assert!(msg.contains("plugins with functions: auth, fs."), "{msg}");
    assert!(msg.contains("See `heph inspect functions`"), "{msg}");

    // No plausible match: no guess, still the list.
    ws.write_build_file("q", "y = heph.zzzzzz.rule()");
    let err = ws.get_spec("//q:t").await.err().expect("unknown plugin");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("heph.zzzzzz.rule: no plugin named \"zzzzzz\"; plugins with functions"),
        "{msg}"
    );
    Ok(())
}

/// D10: an unknown function of a known plugin names the plugin, lists what it
/// does have, and suggests the closest.
#[tokio::test]
async fn unknown_function_in_build_file_is_actionable() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", "srcs = heph.fs.glb(\"*.txt\")");
    let err = ws.get_spec("//p:t").await.err().expect("unknown function");
    let msg = format!("{err:#}");
    assert!(msg.contains("p/BUILD:1:"), "file and line: {msg}");
    assert!(
        msg.contains(
            "heph.fs.glb: plugin \"fs\" has no function \"glb\"; available: base, dir, glob, \
             join, parent. Did you mean \"glob\"?"
        ),
        "{msg}"
    );
    Ok(())
}

/// The def hash of a target whose config comes from `heph.fs.glob` and
/// `heph.go.build_addr`, pinned to its value at 407d3050 — before functions
/// moved from providers to plugins. Equal means the move re-keyed nothing: the
/// values the functions return, and so every consumer's cache key, are where
/// they were.
#[tokio::test]
async fn def_hash_unchanged_for_glob_and_build_addr() -> anyhow::Result<()> {
    let ws = htestkit::WorkspaceBuilder::new()?
        .with_provider(|init| {
            Box::new(heph::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
                std::sync::Arc::clone(&init.functions),
            ))
        })
        .with_managed_driver(Box::new(heph::pluginexec::Driver::new_bash()))
        // `heph.go.*` alone: the functions, without the provider's discovery.
        .with_plugin("go", |init| {
            let go =
                hplugin_go::plugingo::Provider::new(init.root.to_path_buf(), init.runtime.clone())?;
            Ok(PluginParts::default().with_functions(go.functions()))
        })
        .build()?;
    ws.write_file("p/a.txt", "a");
    ws.write_file("p/b.txt", "b");
    ws.write_build_file(
        "p",
        r#"target(
    name = "t",
    driver = "bash",
    run = "echo " + " ".join(heph.fs.glob("*.txt")) + " " +
        heph.go.build_addr("./lib", "linux_amd64") + " > $OUT",
    out = "o.txt",
)"#,
    );
    let addr = heph::htaddr::parse_addr("//p:t")?;
    let def = Arc::clone(&ws.engine)
        .get_def(ws.engine.new_state(), &addr)
        .await?;
    let hash = String::from_utf8(def.target_def.hash.clone())?;
    assert_eq!(hash, DEF_HASH_AT_407D3050, "the def hash moved");
    Ok(())
}

/// Computed at 407d3050 (provider functions, `ProviderFunctionRegistry`, the
/// go *provider* registered for `heph.go.*`) with the same BUILD file and the
/// same buildfile provider and `bash` driver.
const DEF_HASH_AT_407D3050: &str = "2b6805fc6595e7b";

/// I1's engine-lifetime limit: a tree-reading function's value is replayed
/// within one engine at most. A file added between two engines shows up in the
/// next engine's `heph.fs.glob`.
#[tokio::test]
async fn glob_reflects_a_changed_tree_across_engines() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("p/a.txt", "a");
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", run = "true", srcs = heph.fs.glob("*.txt"))"#,
    );
    let before = ws.get_spec("//p:t").await?;
    assert_eq!(srcs(&before.spec, "srcs"), ["a.txt"]);

    ws.write_file("p/b.txt", "b");
    let next = ws.reopen()?;
    let addr = heph::htaddr::parse_addr("//p:t")?;
    let after = Arc::clone(&next).get_spec(next.new_state(), &addr).await?;
    assert_eq!(srcs(&after.spec, "srcs"), ["a.txt", "b.txt"]);
    Ok(())
}
