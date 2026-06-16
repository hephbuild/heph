//! Reference out-of-process plugin: a minimal provider served over the proto
//! transport. Demonstrates the SDK surface (implement `hplugin::Provider`, call
//! back into the host via `executor`) and serves as the subprocess fixture for
//! the transport e2e test.

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

struct EchoProvider;

fn addr(pkg: &str, name: &str) -> Addr {
    Addr::new(PkgBuf::from(pkg), name.to_string(), BTreeMap::new())
}

impl Provider for EchoProvider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "echo".to_string(),
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
            let items = vec![Ok(ListPackageResponse {
                pkg: PkgBuf::from("//pkg"),
            })];
            Ok(Box::new(items.into_iter())
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
            // Exercise the bidirectional callback: resolve a dependency.
            let _eres = req
                .executor
                .result(&addr("//dep", "lib"))
                .await
                .map_err(GetError::Other)?;
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    plugin_sdk::serve_inherited(Arc::new(EchoProvider)).await
}
