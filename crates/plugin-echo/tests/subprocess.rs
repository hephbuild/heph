//! Real subprocess e2e: spawn the `heph-plugin-echo` binary, connect over the
//! proto transport (inherited fd 3), and drive it — config, streaming list, and
//! a `get` that triggers the bidirectional `result()` callback into a host stub.

use futures::future::BoxFuture;
use hcore::hartifactcontent::{Content, WalkEntry};
use hcore::hasync::StdCancellationToken;
use hplugin::eresult::{ArtifactMeta, EResult};
use hplugin::provider::{
    ConfigRequest, GetRequest, ListRequest, Provider, ProviderExecutor,
};
use hmodel::htaddr::Addr;
use hmodel::htmatcher::Matcher;
use hmodel::htpkg::PkgBuf;
use plugin_remote::spawn_plugin;
use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Arc;

fn addr(pkg: &str, name: &str) -> Addr {
    Addr::new(PkgBuf::from(pkg), name.to_string(), BTreeMap::new())
}

struct MemContent;
impl Content for MemContent {
    fn reader(&self) -> anyhow::Result<Box<dyn Read>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }
    fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
        Ok(Box::new(std::iter::empty()))
    }
    fn hashout(&self) -> anyhow::Result<String> {
        Ok("deadbeef".to_string())
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
                    hashout: "deadbeef".to_string(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_plugin_subprocess() {
    let exe = env!("CARGO_BIN_EXE_heph-plugin-echo");
    let (host, mut child) = spawn_plugin(Path::new(exe), &[], "echo").expect("spawn");

    // config (no round trip — name from handshake)
    assert_eq!(host.config(ConfigRequest {}).expect("config").name, "echo");

    let ctoken = StdCancellationToken::new();

    // streaming list
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
    assert_eq!(addrs, vec![addr("//pkg", "a"), addr("//pkg", "b")]);

    // get -> triggers result() callback back into the host StubExec
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
        .expect("get (callback round-tripped through the subprocess)");
    assert_eq!(resp.target_spec.driver, "exec");

    drop(child.kill());
    drop(child.wait());
}
