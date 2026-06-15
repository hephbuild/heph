//! Full plugin protocol over the shm (iceoryx2) transport: a guest provider
//! served on shm byte-pipe halves, driven by a host RemoteProvider over the
//! other end. Proves the entire Mux/protocol runs over shared memory unchanged.
//!
//! Gated on the `shm` feature (heavy iceoryx2 dep): `cargo test -p plugin-remote
//! --features shm --test shm_e2e`.
#![cfg(feature = "shm")]

use futures::future::BoxFuture;
use hcore::hasync::{Cancellable, StdCancellationToken};
use hmodel::htaddr::Addr;
use hmodel::htmatcher::Matcher;
use hmodel::htpkg::PkgBuf;
use hplugin::provider::{
    ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
    ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse, Provider,
    ProviderExecutor, TargetSpec,
};
use std::collections::BTreeMap;
use std::sync::Arc;

fn addr(pkg: &str, name: &str) -> Addr {
    Addr::new(PkgBuf::from(pkg), name.to_string(), BTreeMap::new())
}

struct TestProvider;

impl Provider for TestProvider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "shm-test".to_string(),
        })
    }
    fn list<'a>(
        &'a self,
        _req: ListRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>
    {
        Box::pin(async move {
            let items = vec![Ok(ListResponse {
                addr: addr("pkg", "a"),
            })];
            Ok(Box::new(items.into_iter())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListResponse>> + Send,
                >)
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
            Ok(Box::new(std::iter::empty())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                >)
        })
    }
    fn get<'a>(
        &'a self,
        req: GetRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
        Box::pin(async move {
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

struct NoopExec;
impl ProviderExecutor for NoopExec {
    fn result<'a>(
        &'a self,
        _addr: &'a Addr,
    ) -> BoxFuture<'a, anyhow::Result<Arc<hplugin::eresult::EResult>>> {
        Box::pin(async move { Ok(Arc::new(hplugin::eresult::EResult::default())) })
    }
    fn query<'a>(
        &'a self,
        _m: &'a Matcher,
        _s: &'a [String],
    ) -> BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
        Box::pin(async move { Ok(vec![]) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_over_shm() {
    let id = format!("heph_shm_e2e_{}", std::process::id());
    let h2g = format!("{id}_h2g");
    let g2h = format!("{id}_g2h");

    // Host: send h2g / recv g2h. Guest: the reverse.
    let (hr, hw) = plugin_abi::shm::connect(&h2g, &g2h).expect("host shm");
    let (gr, gw) = plugin_abi::shm::connect(&g2h, &h2g).expect("guest shm");

    let _guest = plugin_sdk::serve(Arc::new(TestProvider), gr, gw);
    let host = plugin_remote::RemoteProvider::connect(hr, hw, "shm-test");

    assert_eq!(
        host.config(ConfigRequest {}).expect("config").name,
        "shm-test"
    );

    let ctoken = StdCancellationToken::new();
    let executor: Arc<dyn ProviderExecutor> = Arc::new(NoopExec);
    let resp = host
        .get(
            GetRequest {
                request_id: "r1".to_string(),
                addr: addr("//pkg", "a"),
                states: vec![],
                executor,
            },
            &ctoken,
        )
        .await
        .expect("get over shm");
    assert_eq!(resp.target_spec.driver, "exec");
}
