//! End-to-end: the `helloworld` wasm plugin (provider + driver) served through
//! the wasmtime host transport, exercising the full data path —
//!   provider.get (with a guest->host `query` callback),
//!   driver.parse (TargetDef round-trip incl. opaque raw_def),
//!   driver.run  (output artifact crossing back as inline bytes).
//!
//! Gated behind `--features wasm` (needs the devenv wasm toolchain). Run with:
//!   devenv shell -- cargo test -p plugin-remote --features wasm
#![cfg(feature = "wasm")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use futures::future::BoxFuture;
use hcore::hasync::StdCancellationToken;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hmodel::htmatcher::Matcher;
use hmodel::htpkg::PkgBuf;
use hplugin::driver::outputartifact::Content as OaContent;
use hplugin::driver::{Driver, ParseRequest, RunRequest};
use hplugin::eresult::EResult;
use hplugin::provider::{GetRequest, Provider, ProviderExecutor};
use plugin_remote::wasm::WasmPlugin;

/// Build the helloworld guest component into `out_dir`, return the `.wasm` path.
fn build_component(out_dir: &Path) -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../wasm-guests/helloworld/Cargo.toml")
        .canonicalize()
        .expect("helloworld manifest exists");
    let status = Command::new("cargo")
        .args(["component", "build"])
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target")
        .arg("wasm32-wasip1")
        .arg("--target-dir")
        .arg(out_dir)
        .status()
        .expect("cargo-component on PATH (run inside `devenv shell`)");
    assert!(status.success(), "cargo component build failed");
    let wasm = out_dir.join("wasm32-wasip1/debug/helloworld.wasm");
    assert!(wasm.exists(), "component at {}", wasm.display());
    wasm
}

/// Stub executor: `query` returns a fixed set of addrs so the guest's callback
/// has an observable effect; `result` is unused on this path.
struct StubExecutor {
    query_addrs: Vec<Addr>,
}

impl ProviderExecutor for StubExecutor {
    fn result<'a>(&'a self, _addr: &'a Addr) -> BoxFuture<'a, anyhow::Result<Arc<EResult>>> {
        Box::pin(async move { anyhow::bail!("result() not used in this test") })
    }
    fn query<'a>(
        &'a self,
        _m: &'a Matcher,
        _extra: &'a [String],
    ) -> BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
        Box::pin(async move { Ok(self.query_addrs.clone()) })
    }
}

#[derive(serde::Deserialize, PartialEq, Debug)]
struct HelloDef {
    greeting: String,
}

fn addr(pkg: &str, name: &str) -> Addr {
    Addr::new(PkgBuf::from(pkg), name.to_string(), BTreeMap::new())
}

#[tokio::test]
async fn helloworld_plugin_over_wasm() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let wasm = std::fs::read(build_component(tmp.path())).expect("read component");

    let plugin = WasmPlugin::load(&wasm, "hello", "hello").expect("load plugin");
    let provider = plugin.provider();
    let driver = plugin.driver();
    let ctoken = StdCancellationToken::new();

    let target = addr("//hello", "world");
    // Two addrs => the guest's query callback should observe a count of 2.
    let executor = Arc::new(StubExecutor {
        query_addrs: vec![addr("//hello", "world"), addr("//hello", "other")],
    });

    // --- provider.get (+ guest->host query callback) ---
    let resp = provider
        .get(
            GetRequest {
                request_id: "r1".to_string(),
                addr: target.clone(),
                states: vec![],
                executor: executor.clone(),
            },
            &ctoken,
        )
        .await
        .expect("get");
    assert_eq!(resp.target_spec.driver, "hello");
    assert!(
        resp.target_spec.labels.contains(&"queried:2".to_string()),
        "callback count folded into label, got {:?}",
        resp.target_spec.labels
    );
    assert_eq!(
        resp.target_spec.config.get("greeting"),
        Some(&Value::String("hello world".to_string()))
    );

    // --- driver.parse (TargetDef + opaque raw_def round-trip) ---
    let parsed = driver
        .parse(
            ParseRequest {
                request_id: "r2".to_string(),
                target_spec: Arc::new(resp.target_spec),
            },
            &ctoken,
        )
        .await
        .expect("parse");
    let def = &parsed.target_def;
    assert_eq!(
        def.def_de::<HelloDef>(),
        &HelloDef {
            greeting: "hello world".to_string()
        }
    );

    // --- driver.run (output artifact crosses back as inline bytes) ---
    let rid = "r3".to_string();
    let run = driver
        .run(
            RunRequest {
                request_id: &rid,
                target: def,
                tree_root_path: tmp.path().to_path_buf(),
                inputs: vec![],
                hashin: "hash",
                stdin: None,
                stdout: None,
                stderr: None,
                sandbox_dir: tmp.path().to_path_buf(),
            },
            &ctoken,
        )
        .await
        .expect("run");
    assert_eq!(run.artifacts.len(), 1);
    match &run.artifacts[0].content {
        OaContent::Raw(raw) => assert_eq!(raw.data, b"hello world"),
        _ => panic!("expected Raw output artifact"),
    }
}
