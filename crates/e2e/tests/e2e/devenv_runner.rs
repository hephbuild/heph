#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! The `devenv.runner` driver against a **real** `devenv`.
//!
//! The unit tests prove the pieces — the env-0 parse, the fingerprint's
//! sensitivity and stability, the mode parse. What they cannot prove is that
//! `devenv shell -- env -0` produces what the driver assumes, that the captured
//! environment actually reaches a target, or that the fingerprint is stable
//! across two independent evaluations of the same environment. That last one is
//! the quiet catastrophe of the whole feature: if it drifts, every consumer in
//! the workspace full-misses forever, nothing errors, and nobody traces it back
//! here.
//!
//! # Opt-in, not auto-skip
//!
//! Unlike the docker suite, this one does not run just because the tool is
//! present. A devenv environment nix has never evaluated costs **~2m40s**; the
//! same environment a second time costs ~4s, because nix's eval cache is keyed
//! on content. CI runners are ephemeral, so the honest expectation there is the
//! cold number, on every push, on three platforms.
//!
//! So it is gated on `HEPH_E2E_DEVENV=1` and skips loudly otherwise. Run it
//! when touching the devenv driver:
//!
//! ```sh
//! HEPH_E2E_DEVENV=1 cargo test -p e2e --test e2e devenv_runner::
//! ```
//!
//! CI sets `HEPH_E2E_DEVENV` only for `plugingo-e2e`'s Go smoke tests, so these
//! run locally, by hand, and nowhere else.

use crate::common;

use heph::engine::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin_devenv::plugindevenv;
use htestkit::WorkspaceBuilder;

/// The environment every fixture evaluates.
///
/// Deliberately identical across tests: nix caches an evaluation by content, so
/// one environment shared between them costs one evaluation rather than one
/// each. `env.*` is the cheapest thing devenv can be asked to contribute — no
/// packages means no store paths to realise.
const DEVENV_NIX: &str = r#"{ pkgs, ... }: {
  env.XR_DEVENV_MARKER = "hello-from-devenv";
}
"#;

const DEVENV_YAML: &str = r#"inputs:
  nixpkgs:
    url: github:cachix/devenv-nixpkgs/rolling
"#;

/// Skip unless explicitly opted in — see the module header for why this is not
/// a capability probe.
macro_rules! skip_unless_opted_in {
    () => {
        if std::env::var_os("HEPH_E2E_DEVENV").is_none() {
            eprintln!(
                "skipping: set HEPH_E2E_DEVENV=1 to run the devenv suite. It evaluates a real \
                 devenv environment, which costs ~2m40s the first time any machine sees it."
            );
            return Ok(());
        }
        if which_devenv().is_none() {
            eprintln!("skipping: HEPH_E2E_DEVENV is set but no `devenv` is on PATH");
            return Ok(());
        }
    };
}

fn which_devenv() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("devenv"))
        .find(|p| p.is_file())
}

fn workspace() -> htestkit::Workspace {
    workspace_recording(None)
}

/// [`workspace`], recording into `declared` the `host_paths` every run of the
/// runner driver returns, when one is given.
fn workspace_recording(declared: Option<Declared>) -> htestkit::Workspace {
    WorkspaceBuilder::new()
        .expect("workspace tempdir")
        .with_provider(|init| {
            Box::new(heph::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
                std::sync::Arc::clone(&init.functions),
            ))
        })
        .with_managed_driver(Box::new(
            heph::pluginexec::Driver::new_bash().with_host_path(),
        ))
        .with_plugin("devenv", move |init| {
            let driver = plugindevenv::Driver::new(&init.home);
            let parts = heph::engine::PluginParts::default();
            Ok(match declared {
                Some(declared) => parts.with_managed_driver(Box::new(RecordsHostPaths {
                    inner: driver,
                    declared,
                })),
                None => parts.with_managed_driver(Box::new(driver)),
            })
        })
        .build()
        .expect("build workspace")
}

/// The host paths runs declared, in run order.
type Declared = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// The runner driver, recording the `host_paths` each run returns: what the
/// engine is handed to check on every later hit.
struct RecordsHostPaths {
    inner: plugindevenv::Driver,
    declared: Declared,
}

#[async_trait::async_trait]
impl ManagedDriver for RecordsHostPaths {
    fn config(
        &self,
        req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        ManagedDriver::config(&self.inner, req)
    }
    fn schema(&self) -> hplugin::driver::DriverSchema {
        ManagedDriver::schema(&self.inner)
    }
    async fn parse(
        &self,
        req: hplugin::driver::ParseRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ParseResponse> {
        ManagedDriver::parse(&self.inner, req, ctoken).await
    }
    async fn apply_transitive(
        &self,
        req: hplugin::driver::ApplyTransitiveRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ApplyTransitiveResponse> {
        ManagedDriver::apply_transitive(&self.inner, req, ctoken).await
    }
    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let res = ManagedDriver::run(&self.inner, req, ctoken).await?;
        self.declared
            .lock()
            .expect("declared lock")
            .extend(res.host_paths.iter().map(|p| p.as_str().to_owned()));
        Ok(res)
    }
}

/// The `/nix/store/<hash>-` prefix of every store path `text` names, read the
/// way nix forms one: a nix32 hash part (no `e`, `o`, `t`, `u`, so a dev
/// environment's `/nix/store/eeee…` output placeholders are skipped) and a `-`.
/// By hash rather than whole path, so a path followed by a byte nix allows in a
/// name (`<path>=<flag value>`) still matches the path itself.
fn store_hash_prefixes(text: &str) -> std::collections::BTreeSet<&str> {
    const STORE: &str = "/nix/store/";
    const NIX32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
    text.match_indices(STORE)
        .filter_map(|(start, _)| {
            let end = start + STORE.len() + 33;
            let prefix = text.get(start..end)?;
            let hash = prefix.get(STORE.len()..end - start - 1)?;
            (hash.bytes().all(|b| NIX32.contains(&b)) && prefix.ends_with('-')).then_some(prefix)
        })
        .collect()
}

/// Write the devenv files plus a `devenv.runner` target over them.
fn write_env(ws: &htestkit::Workspace, mode: &str, extra: &str) {
    ws.write_file("env/devenv.nix", DEVENV_NIX);
    ws.write_file("env/devenv.yaml", DEVENV_YAML);
    ws.write_build_file(
        "env",
        &format!(
            r#"
target(
    name = "runner",
    driver = "devenv.runner",
    mode = "{mode}",
    deps = [glob("devenv.*")],
)
{extra}
"#
        ),
    );
}

/// Wrap mode, end to end: the environment devenv describes reaches the target.
///
/// The marker is set by `devenv.nix` and by nothing else, so a target that sees
/// it can only have got it through the capture.
#[tokio::test]
async fn wrap_mode_puts_the_devenv_environment_in_the_target() -> anyhow::Result<()> {
    skip_unless_opted_in!();
    let ws = workspace();
    write_env(
        &ws,
        "wrap",
        r#"
target(
    name = "consumer",
    driver = "bash",
    run = "echo \"marker=$XR_DEVENV_MARKER\" > $OUT",
    out = "result.txt",
    runner = "//env:runner",
)
"#,
    );

    let got = common::artifact_string(&*ws.run("//env:consumer").await?);
    assert!(
        got.contains("marker=hello-from-devenv"),
        "the devenv environment must reach the target; got {got:?}"
    );
    Ok(())
}

/// **The one that matters.** Two independent evaluations of the same
/// environment must produce byte-identical output.
///
/// If one ambient value leaks into the capture — a temp path, a pid, a
/// timestamp — the runner's hashout moves on every build and every consumer in
/// the workspace misses forever. Nothing fails; the build is just always cold,
/// and the cause is nowhere near the symptom.
///
/// Two separate workspaces, so the tree paths differ too: a capture that
/// embedded its own sandbox path would pass a same-directory check and fail
/// this one.
#[tokio::test]
async fn the_runner_output_is_identical_across_two_workspaces() -> anyhow::Result<()> {
    skip_unless_opted_in!();

    let mut outputs = Vec::new();
    for _ in 0..2 {
        let ws = workspace();
        write_env(&ws, "wrap", "");
        outputs.push(common::artifact_string(&*ws.run("//env:runner").await?));
    }

    let (first, second) = (&outputs[0], &outputs[1]);
    assert_eq!(
        first, second,
        "two evaluations of one environment must produce identical bytes — otherwise the \
         runner's hashout moves every build and every consumer full-misses forever"
    );
    assert!(
        first.contains("\"fingerprint\""),
        "the runner must declare a fingerprint; got {first}"
    );
    assert!(
        first.contains("hello-from-devenv"),
        "wrap mode must carry the captured environment; got {first}"
    );
    Ok(())
}

/// Session mode emits a `session` config whose launch enters devenv, and a
/// fingerprint derived the same way wrap's is — it pays one evaluation it does
/// not strictly need so that the fingerprint describes the environment rather
/// than the paperwork.
#[tokio::test]
async fn session_mode_emits_a_launch_and_the_same_fingerprint() -> anyhow::Result<()> {
    skip_unless_opted_in!();

    let wrap_ws = workspace();
    write_env(&wrap_ws, "wrap", "");
    let wrap = common::artifact_string(&*wrap_ws.run("//env:runner").await?);

    let session_ws = workspace();
    write_env(&session_ws, "session", "");
    let session = common::artifact_string(&*session_ws.run("//env:runner").await?);

    assert!(session.contains("\"runner\": \"session\""), "got {session}");
    assert!(session.contains("\"launch\""), "got {session}");
    assert!(
        session.contains("shell"),
        "the launch must enter the devenv shell; got {session}"
    );

    // Both modes derive the fingerprint from the resolved environment, so the
    // same environment fingerprints the same either way. The *configs* differ,
    // so the hashouts still differ — which is right: a target's output can
    // depend on process ancestry, so the two modes must not share cache
    // entries.
    let fp = |doc: &str| {
        doc.lines()
            .find(|l| l.contains("\"fingerprint\""))
            .map(str::trim)
            .map(str::to_string)
            .expect("a fingerprint line")
    };
    assert_eq!(fp(&wrap), fp(&session));
    assert_ne!(
        wrap, session,
        "the two modes must not emit identical bytes, or they would share a cache entry"
    );
    Ok(())
}

/// Against the real `nix`: the run declares one host path, it exists, the
/// gcroot under heph's home links it, and its closure holds every store path
/// `runner.json` names — so a `nix-collect-garbage` cannot take anything a
/// cached `runner.json` hands its consumers. The unit tests prove the same
/// with a fake `nix`; only this one proves `builtins.toFile` over
/// `builtins.storePath` really references what it was given.
#[tokio::test]
async fn the_declared_root_is_rooted_and_its_closure_covers_runner_json() -> anyhow::Result<()> {
    skip_unless_opted_in!();

    let declared = Declared::default();
    let ws = workspace_recording(Some(std::sync::Arc::clone(&declared)));
    write_env(&ws, "wrap", "");
    let doc = common::artifact_string(&*ws.run("//env:runner").await?);

    let declared = declared.lock().expect("declared lock").clone();
    let [root] = declared.as_slice() else {
        panic!("one run declares exactly one host path; got {declared:?}")
    };
    assert!(
        std::path::Path::new(root).exists(),
        "the declared path {root} exists"
    );
    let gcroot = ws
        .engine
        .home
        .as_path()
        .join("devenv-runner/gcroots")
        .join(heph::htaddr::parse_addr("//env:runner")?.hash_str());
    assert_eq!(
        std::fs::read_link(&gcroot)?,
        std::path::Path::new(root),
        "the gcroot {gcroot:?} links the declared path"
    );

    let out = std::process::Command::new("nix-store")
        .args(["--query", "--requisites", root])
        .output()?;
    assert!(
        out.status.success(),
        "nix-store --query --requisites {root}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let requisites = String::from_utf8(out.stdout)?;
    let closure = store_hash_prefixes(&requisites);
    let named = store_hash_prefixes(&doc);
    assert!(!named.is_empty(), "runner.json names store paths: {doc}");
    let outside: Vec<_> = named.difference(&closure).collect();
    assert!(
        outside.is_empty(),
        "store paths runner.json names but the root's closure lacks: {outside:?}"
    );
    Ok(())
}
