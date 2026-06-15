//! End-to-end test of the proto transport over an in-process UDS socketpair:
//! a guest `Provider` served by the SDK, driven by a host `RemoteProvider`,
//! exercising config, streaming list, get + the bidirectional `result()`
//! callback, and cancellation. (Real subprocess spawn is a thin wrapper added
//! next; this proves the protocol.)

use futures::future::BoxFuture;
use hcore::hartifactcontent::{Content, WalkEntry, WalkEntryKind};
use hcore::hasync::{Cancellable, StdCancellationToken};
use hplugin::eresult::{ArtifactMeta, EResult};
use hplugin::provider::{
    ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
    ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse, Provider,
    ProviderExecutor, TargetSpec,
};
use hmodel::htaddr::Addr;
use hmodel::htmatcher::Matcher;
use hmodel::htpkg::PkgBuf;
use plugin_remote::RemoteProvider;
use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::sync::Arc;
use tokio::net::UnixStream;

const DEP_HASH: &str = "deadbeef";

fn addr(pkg: &str, name: &str) -> Addr {
    Addr::new(PkgBuf::from(pkg), name.to_string(), BTreeMap::new())
}

// ---- guest plugin ----

struct TestProvider;

impl Provider for TestProvider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "test".to_string(),
        })
    }

    fn list<'a>(
        &'a self,
        _req: ListRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>
    {
        Box::pin(async move {
            let items = vec![
                Ok(ListResponse {
                    addr: addr("//pkg", "a"),
                }),
                Ok(ListResponse {
                    addr: addr("//pkg", "b"),
                }),
            ];
            Ok(Box::new(items.into_iter())
                as Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>)
        })
    }

    fn list_packages<'a>(
        &'a self,
        _req: ListPackagesRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    > {
        Box::pin(async move {
            let items = vec![Ok(ListPackageResponse {
                pkg: PkgBuf::from("//pkg"),
            })];
            Ok(Box::new(items.into_iter())
                as Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>)
        })
    }

    fn get<'a>(
        &'a self,
        req: GetRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
        Box::pin(async move {
            // Call back into the host to resolve a dependency.
            let dep = addr("//dep", "lib");
            let eres = req
                .executor
                .result(&dep)
                .await
                .map_err(GetError::Other)?;
            // Verify the callback round-tripped the artifact metadata.
            let got = eres
                .artifacts_meta
                .first()
                .map(|m| m.hashout.clone())
                .unwrap_or_default();
            if got != DEP_HASH {
                return Err(GetError::Other(anyhow::anyhow!(
                    "callback hashout mismatch: {got:?}"
                )));
            }
            // Read package.bin out of the artifact — exercises byte streaming +
            // tar walk through the transport (the plugin-go hot read).
            let art = eres
                .artifacts
                .first()
                .ok_or_else(|| GetError::Other(anyhow::anyhow!("no artifacts")))?;
            let mut found: Option<String> = None;
            for entry in art.walk().map_err(GetError::Other)? {
                let entry = entry.map_err(GetError::Other)?;
                if entry.path.file_name().and_then(|n| n.to_str()) == Some("package.bin")
                    && let WalkEntryKind::File { mut data, .. } = entry.kind
                {
                    let mut s = String::new();
                    data.read_to_string(&mut s)
                        .map_err(|e| GetError::Other(e.into()))?;
                    found = Some(s);
                }
            }
            if found.as_deref() != Some("hello") {
                return Err(GetError::Other(anyhow::anyhow!(
                    "package.bin mismatch: {found:?}"
                )));
            }
            let spec = TargetSpec {
                addr: req.addr,
                driver: "exec".to_string(),
                ..Default::default()
            };
            Ok(GetResponse { target_spec: spec })
        })
    }

    fn probe<'a>(
        &'a self,
        _req: ProbeRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async move { Ok(ProbeResponse { states: vec![] }) })
    }
}

// ---- host stub engine executor ----

/// A tar artifact containing `package.bin` = "hello" — what the host would
/// hand back from a cache result; the guest must fetch + walk it.
fn pkg_tar() -> Vec<u8> {
    let mut p = hcore::hartifactcontent::tar::TarPacker::new();
    p.create_raw(b"hello".to_vec(), "package.bin", false);
    let mut buf = Vec::new();
    p.pack(&mut buf).expect("pack tar");
    buf
}

struct MemContent {
    bytes: Vec<u8>,
}

impl Content for MemContent {
    fn reader(&self) -> anyhow::Result<Box<dyn Read>> {
        Ok(Box::new(Cursor::new(self.bytes.clone())))
    }
    fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
        Ok(Box::new(std::iter::empty()))
    }
    fn hashout(&self) -> anyhow::Result<String> {
        Ok(DEP_HASH.to_string())
    }
    fn byte_size(&self) -> Option<u64> {
        Some(self.bytes.len() as u64)
    }
}

struct StubExec;

impl ProviderExecutor for StubExec {
    fn result<'a>(&'a self, _addr: &'a Addr) -> BoxFuture<'a, anyhow::Result<Arc<EResult>>> {
        Box::pin(async move {
            Ok(Arc::new(EResult {
                artifacts: vec![Arc::new(MemContent { bytes: pkg_tar() }) as Arc<dyn Content>],
                support_artifacts: vec![],
                artifacts_meta: vec![ArtifactMeta {
                    hashout: DEP_HASH.to_string(),
                }],
            }))
        })
    }

    fn query<'a>(
        &'a self,
        _m: &'a Matcher,
        _extra_skip: &'a [String],
    ) -> BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
        Box::pin(async move { Ok(vec![]) })
    }
}

fn wire() -> (RemoteProvider, Arc<plugin_abi::mux::Mux>) {
    let (a, b) = UnixStream::pair().expect("socketpair");
    let (ar, aw) = a.into_split();
    let (br, bw) = b.into_split();
    let host = RemoteProvider::connect(ar, aw, "test");
    let guest = plugin_sdk::serve(Arc::new(TestProvider), br, bw);
    (host, guest)
}

#[tokio::test]
async fn config_returns_name() {
    let (host, _guest) = wire();
    let resp = host.config(ConfigRequest {}).expect("config");
    assert_eq!(resp.name, "test");
}

#[tokio::test]
async fn list_streams_addrs() {
    let (host, _guest) = wire();
    let ctoken = StdCancellationToken::new();
    let iter = host
        .list(
            ListRequest {
                request_id: "r1".to_string(),
                package: PkgBuf::from("//pkg"),
                states: vec![],
            },
            &ctoken,
        )
        .await
        .expect("list");
    let addrs: Vec<_> = iter.map(|r| r.expect("item").addr).collect();
    assert_eq!(addrs.len(), 2);
    assert_eq!(addrs[0], addr("//pkg", "a"));
    assert_eq!(addrs[1], addr("//pkg", "b"));
}

#[tokio::test]
async fn get_invokes_result_callback() {
    let (host, _guest) = wire();
    let ctoken = StdCancellationToken::new();
    let executor: Arc<dyn ProviderExecutor> = Arc::new(StubExec);
    let resp = host
        .get(
            GetRequest {
                request_id: "r2".to_string(),
                addr: addr("//pkg", "a"),
                states: vec![],
                executor,
            },
            &ctoken,
        )
        .await
        .expect("get ok (callback round-tripped)");
    assert_eq!(resp.target_spec.driver, "exec");
    assert_eq!(resp.target_spec.addr, addr("//pkg", "a"));
}

#[tokio::test]
async fn get_cancellation_returns_error() {
    let (host, _guest) = wire();
    let ctoken = StdCancellationToken::new();
    ctoken.cancel(); // pre-cancelled
    let executor: Arc<dyn ProviderExecutor> = Arc::new(StubExec);
    let res = host
        .get(
            GetRequest {
                request_id: "r3".to_string(),
                addr: addr("//pkg", "a"),
                states: vec![],
                executor,
            },
            &ctoken,
        )
        .await;
    let err = res.err().expect("should be cancelled");
    let msg = match err {
        GetError::Other(e) => e.to_string(),
        GetError::NotFound => "notfound".to_string(),
    };
    assert!(msg.contains("cancelled"), "unexpected error: {msg}");
}

// ---- driver path: parse + apply_transitive round-trip raw_def via def_de ----

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
struct MyCfg {
    msg: String,
    n: u32,
}

struct TestDriver;

#[async_trait::async_trait]
impl hplugin::driver::Driver for TestDriver {
    fn config(
        &self,
        _req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        Ok(hplugin::driver::ConfigResponse {
            name: "td".to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        hplugin::driver::DriverSchema::default()
    }

    async fn parse(
        &self,
        req: hplugin::driver::ParseRequest,
        _ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ParseResponse> {
        let target_def = hplugin::driver::targetdef::TargetDef {
            addr: req.target_spec.addr.clone(),
            labels: vec![],
            raw_def: Arc::new(MyCfg {
                msg: "hi".to_string(),
                n: 9,
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: hplugin::driver::targetdef::CacheConfig::off(),
            pty: false,
            hash: vec![],
            transparent: false,
        };
        Ok(hplugin::driver::ParseResponse { target_def })
    }

    async fn apply_transitive(
        &self,
        req: hplugin::driver::ApplyTransitiveRequest,
        _ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ApplyTransitiveResponse> {
        // Guest reads the wire raw_def back as the concrete type via def_de.
        let cfg = req.target_def.def_de::<MyCfg>();
        anyhow::ensure!(cfg.msg == "hi" && cfg.n == 9, "raw_def lost in transit");
        Ok(hplugin::driver::ApplyTransitiveResponse {
            target_def: req.target_def,
        })
    }

    async fn run<'a, 'io>(
        &self,
        _req: hplugin::driver::RunRequest<'a, 'io>,
        _ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::RunResponse> {
        anyhow::bail!("test driver does not run")
    }

    async fn run_shell<'a, 'io>(
        &self,
        _req: hplugin::driver::RunRequest<'a, 'io>,
        _ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::RunResponse> {
        anyhow::bail!("test driver does not run")
    }
}

#[tokio::test]
async fn driver_parse_and_apply_transitive_roundtrip() {
    let (a, b) = UnixStream::pair().expect("socketpair");
    let (ar, aw) = a.into_split();
    let (br, bw) = b.into_split();
    let host = plugin_remote::RemoteDriver::connect(ar, aw, "td");
    let _guest = plugin_sdk::serve_driver(Arc::new(TestDriver), br, bw);

    use hplugin::driver::Driver;
    assert_eq!(
        host.config(hplugin::driver::ConfigRequest {})
            .expect("config")
            .name,
        "td"
    );

    let ctoken = StdCancellationToken::new();

    // parse: TargetDef (incl. opaque raw_def) crosses back; def_de reconstructs.
    let spec = Arc::new(hplugin::provider::TargetSpec {
        addr: addr("//x", "y"),
        driver: "td".to_string(),
        ..Default::default()
    });
    let parsed = host
        .parse(
            hplugin::driver::ParseRequest {
                request_id: "r1".to_string(),
                target_spec: spec,
            },
            &ctoken,
        )
        .await
        .expect("parse");
    assert_eq!(
        parsed.target_def.def_de::<MyCfg>(),
        &MyCfg {
            msg: "hi".to_string(),
            n: 9
        }
    );

    // apply_transitive: ship the round-tripped TargetDef back; the guest reads
    // its raw_def via def_de (proving the contract works guest-side too).
    let applied = host
        .apply_transitive(
            hplugin::driver::ApplyTransitiveRequest {
                request_id: "r2".to_string(),
                target_def: parsed.target_def,
                sandbox: hplugin::driver::sandbox::Sandbox::default(),
            },
            &ctoken,
        )
        .await
        .expect("apply_transitive");
    assert_eq!(applied.target_def.def_de::<MyCfg>().n, 9);
}

// ---- managed run: host materializes, guest executes in the shared sandbox ----

struct TestManagedDriver;

#[async_trait::async_trait]
impl hdriver_support::driver_managed::ManagedDriver for TestManagedDriver {
    fn config(
        &self,
        _req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        Ok(hplugin::driver::ConfigResponse {
            name: "tmd".to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        hplugin::driver::DriverSchema::default()
    }

    async fn parse(
        &self,
        _req: hplugin::driver::ParseRequest,
        _ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ParseResponse> {
        anyhow::bail!("not used")
    }

    async fn apply_transitive(
        &self,
        _req: hplugin::driver::ApplyTransitiveRequest,
        _ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ApplyTransitiveResponse> {
        anyhow::bail!("not used")
    }

    async fn run<'a, 'io>(
        &self,
        req: hdriver_support::driver_managed::ManagedRunRequest<'a, 'io>,
        _ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hdriver_support::driver_managed::ManagedRunResponse> {
        // Execute in the (host-prepared) sandbox: write an output file.
        let out = req.sandbox_pkg_dir.join("out.txt");
        std::fs::write(&out, b"built")?;
        Ok(hdriver_support::driver_managed::ManagedRunResponse {
            artifacts: vec![hplugin::driver::outputartifact::OutputArtifact {
                group: "out".to_string(),
                name: "out.txt".to_string(),
                r#type: hplugin::driver::outputartifact::Type::Output,
                content: hplugin::driver::outputartifact::Content::File(
                    hplugin::driver::outputartifact::ContentFile {
                        source_path: out.to_string_lossy().into_owned(),
                        out_path: "out.txt".to_string(),
                        x: false,
                    },
                ),
                hashout: "h".to_string(),
            }],
        })
    }
}

#[tokio::test]
async fn managed_run_executes_remotely() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sandbox = dir.path().to_path_buf();

    let (a, b) = UnixStream::pair().expect("socketpair");
    let (ar, aw) = a.into_split();
    let (br, bw) = b.into_split();
    let host = plugin_remote::RemoteManagedDriver::connect(ar, aw, "tmd");
    let _guest = plugin_sdk::serve_managed_driver(Arc::new(TestManagedDriver), br, bw);

    let ctoken = StdCancellationToken::new();
    let request_id = "r1".to_string();
    let target = hplugin::driver::targetdef::TargetDef {
        addr: addr("//x", "y"),
        labels: vec![],
        raw_def: Arc::new(()),
        inputs: vec![],
        outputs: vec![],
        support_files: vec![],
        cache: hplugin::driver::targetdef::CacheConfig::off(),
        pty: false,
        hash: vec![],
        transparent: false,
    };
    let hashin = "hash".to_string();
    let rr = hplugin::driver::RunRequest {
        request_id: &request_id,
        target: &target,
        tree_root_path: sandbox.clone(),
        inputs: vec![],
        hashin: hashin.as_str(),
        stdin: None,
        stdout: None,
        stderr: None,
        sandbox_dir: sandbox.clone(),
    };
    let mrr = hdriver_support::driver_managed::ManagedRunRequest {
        request: rr,
        sandbox_dir: sandbox.clone(),
        sandbox_ws_dir: sandbox.clone(),
        sandbox_pkg_dir: sandbox.clone(),
        inputs: vec![],
    };

    use hdriver_support::driver_managed::ManagedDriver;
    let resp = host.run(mrr, &ctoken).await.expect("managed run");
    assert_eq!(resp.artifacts.len(), 1);
    assert_eq!(resp.artifacts[0].name, "out.txt");
    // The guest actually wrote the file into the shared sandbox.
    assert!(sandbox.join("out.txt").exists());
}
