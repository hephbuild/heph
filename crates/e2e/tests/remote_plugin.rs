//! Engine-level integration: a `RemoteProvider` (backed by an out-of-process
//! plugin served via the SDK over the proto transport) registers on a real
//! `Engine` through the normal `register_provider` factory hook and serves a
//! `get_spec` query — proving the engine is unaware the plugin is remote.

use futures::future::BoxFuture;
use hcore::hasync::Cancellable;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::provider::{
    ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
    ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse, Provider,
    TargetSpec,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::net::UnixStream;

struct TestProvider;

fn addr(pkg: &str, name: &str) -> Addr {
    Addr::new(PkgBuf::from(pkg), name.to_string(), BTreeMap::new())
}

impl Provider for TestProvider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "remote-test".to_string(),
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
                pkg: PkgBuf::from("pkg"),
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
            if req.addr.name != "a" {
                return Err(GetError::NotFound);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_provider_serves_get_spec_through_engine() -> anyhow::Result<()> {
    let (a, b) = UnixStream::pair()?;
    let (ar, aw) = a.into_split();
    let (br, bw) = b.into_split();
    // Guest plugin served over the proto transport (kept alive for the test).
    let _guest = plugin_sdk::serve(Arc::new(TestProvider), br, bw);
    let remote = plugin_remote::RemoteProvider::connect(ar, aw, "remote-test");

    // Register the remote provider on a real Engine via the normal hook.
    let ws = htestkit::WorkspaceBuilder::new()?
        .with_provider(move |_| Box::new(remote))
        .build()?;

    // Resolve a target spec through the engine — it routes to the remote plugin.
    let spec = ws.get_spec("//pkg:a").await?;
    assert_eq!(spec.driver, "exec");

    Ok(())
}

