//! End-to-end test of the proto transport over an in-process UDS socketpair:
//! a guest `Provider` served by the SDK, driven by a host `RemoteProvider`,
//! exercising config, streaming list, get + the bidirectional `result()`
//! callback, and cancellation. (Real subprocess spawn is a thin wrapper added
//! next; this proves the protocol.)

use futures::future::BoxFuture;
use hcore::hartifactcontent::{Content, WalkEntry};
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

struct MemContent;

impl Content for MemContent {
    fn reader(&self) -> anyhow::Result<Box<dyn Read>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }
    fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
        Ok(Box::new(std::iter::empty()))
    }
    fn hashout(&self) -> anyhow::Result<String> {
        Ok(DEP_HASH.to_string())
    }
    fn byte_size(&self) -> Option<u64> {
        Some(0)
    }
}

struct StubExec;

impl ProviderExecutor for StubExec {
    fn result<'a>(&'a self, _addr: &'a Addr) -> BoxFuture<'a, anyhow::Result<Arc<EResult>>> {
        Box::pin(async move {
            Ok(Arc::new(EResult {
                artifacts: vec![Arc::new(MemContent) as Arc<dyn Content>],
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
