//! Guest adapter: wrap a host [`DynExecutor`] back into an [`hplugin::provider::
//! ProviderExecutor`] so plugin code calls back exactly as in-process — the calls
//! are direct stabby vtable dispatch into the host, no serialization.

use anyhow::Result;
use futures::future::BoxFuture;
use hcore::hartifactcontent::{Content, WalkEntry};
use hmodel::htaddr::Addr;
use hmodel::htmatcher::Matcher;
use hplugin::eresult::{ArtifactMeta, EResult};
use hplugin::provider::ProviderExecutor;
use hplugin_stabby::abi::{DynExecutor, StableExecutorDyn};
use prost::Message;
use std::io::Read;
use std::sync::Arc;

/// Forwards `ProviderExecutor` calls to the host over the stable ABI.
pub struct GuestExecutor {
    exec: DynExecutor,
}

impl GuestExecutor {
    pub fn new(exec: DynExecutor) -> Self {
        Self { exec }
    }
}

impl ProviderExecutor for GuestExecutor {
    fn note_dep<'a>(&'a self, addr: &'a Addr) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let r = self.exec.note_dep(addr.to_string().into()).await;
            if r.ok {
                Ok(())
            } else if r.cycle {
                Err(anyhow::Error::new(hplugin::error::CycleError {
                    from: addr.clone(),
                    to: addr.clone(),
                }))
            } else {
                anyhow::bail!("{}", r.message)
            }
        })
    }

    fn result<'a>(&'a self, addr: &'a Addr) -> BoxFuture<'a, Result<Arc<EResult>>> {
        Box::pin(async move {
            let r = self.exec.result(addr.to_string().into()).await;
            if r.ok {
                let mut artifacts: Vec<Arc<dyn Content>> = Vec::with_capacity(r.artifacts.len());
                let mut meta = Vec::with_capacity(r.artifacts.len());
                for a in r.artifacts.iter() {
                    let hashout = a.hashout.to_string();
                    artifacts.push(Arc::new(StableContent {
                        bytes: a.bytes.to_vec(),
                        hashout: hashout.clone(),
                    }) as Arc<dyn Content>);
                    meta.push(ArtifactMeta { hashout });
                }
                Ok(Arc::new(EResult {
                    artifacts,
                    support_artifacts: vec![],
                    artifacts_meta: meta,
                }))
            } else if r.cycle {
                Err(anyhow::Error::new(hplugin::error::CycleError {
                    from: addr.clone(),
                    to: addr.clone(),
                }))
            } else if r.cancelled {
                Err(anyhow::Error::new(hplugin::error::CancelledError))
            } else {
                anyhow::bail!("{}", r.message)
            }
        })
    }

    fn query<'a>(
        &'a self,
        m: &'a Matcher,
        extra_skip: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<Addr>>> {
        Box::pin(async move {
            let matcher_pb = plugin_abi::convert::matcher_to_pb(m).encode_to_vec();
            let skip: stabby::vec::Vec<stabby::string::String> =
                extra_skip.iter().map(|s| s.clone().into()).collect();
            let r = self
                .exec
                .query(stabby::vec::Vec::from(matcher_pb.as_slice()), skip)
                .await;
            if r.ok {
                let mut out = Vec::with_capacity(r.addrs.len());
                for a in r.addrs.iter() {
                    out.push(hmodel::htaddr::parse_addr(a)?);
                }
                Ok(out)
            } else {
                anyhow::bail!("{}", r.message)
            }
        })
    }
}

/// A host artifact materialized guest-side from eagerly-read bytes. Artifacts are
/// tar (the only content type the cache produces today), so `walk` uses the tar
/// walker. Mirrors `plugin_sdk::serve::RemoteContent`.
struct StableContent {
    bytes: Vec<u8>,
    hashout: String,
}

impl Content for StableContent {
    fn reader(&self) -> Result<Box<dyn Read>> {
        Ok(Box::new(std::io::Cursor::new(self.bytes.clone())))
    }
    fn walk(&self) -> Result<Box<dyn Iterator<Item = Result<WalkEntry>> + '_>> {
        Ok(Box::new(hcore::hartifactcontent::tar::TarWalker::new(
            std::io::Cursor::new(self.bytes.clone()),
        )?))
    }
    fn hashout(&self) -> Result<String> {
        Ok(self.hashout.clone())
    }
    fn byte_size(&self) -> Option<u64> {
        Some(self.bytes.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hartifactcontent::WalkEntry;
    use hmodel::htaddr::parse_addr;
    use hplugin_stabby::host::HostExecutor;
    use std::io::Read;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct MockExec {
        note_deps: AtomicU32,
    }
    impl ProviderExecutor for MockExec {
        fn note_dep<'a>(&'a self, _addr: &'a Addr) -> BoxFuture<'a, Result<()>> {
            self.note_deps.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        }
        fn result<'a>(&'a self, _addr: &'a Addr) -> BoxFuture<'a, Result<Arc<EResult>>> {
            Box::pin(async {
                Ok(Arc::new(EResult {
                    artifacts: vec![Arc::new(Bytes(b"hello".to_vec())) as Arc<dyn Content>],
                    support_artifacts: vec![],
                    artifacts_meta: vec![ArtifactMeta {
                        hashout: "h1".into(),
                    }],
                }))
            })
        }
        fn query<'a>(
            &'a self,
            _m: &'a Matcher,
            _s: &'a [String],
        ) -> BoxFuture<'a, Result<Vec<Addr>>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    struct Bytes(Vec<u8>);
    impl Content for Bytes {
        fn reader(&self) -> Result<Box<dyn Read>> {
            Ok(Box::new(std::io::Cursor::new(self.0.clone())))
        }
        fn walk(&self) -> Result<Box<dyn Iterator<Item = Result<WalkEntry>> + '_>> {
            anyhow::bail!("no walk in test")
        }
        fn hashout(&self) -> Result<String> {
            Ok("h1".into())
        }
    }

    // The hot path crosses the stable ABI (host adapter -> stabby dyn -> guest
    // adapter) and back, same process — proving the conversions before the cdylib.
    #[test]
    fn hot_path_roundtrip() {
        let mock = Arc::new(MockExec {
            note_deps: AtomicU32::new(0),
        });
        let dynexec = HostExecutor::wrap(mock.clone() as Arc<dyn ProviderExecutor>);
        let guest = GuestExecutor::new(dynexec);
        let addr = parse_addr("//pkg/a:b").expect("parse addr");

        futures::executor::block_on(guest.note_dep(&addr)).expect("note_dep");
        assert_eq!(mock.note_deps.load(Ordering::Relaxed), 1);

        let eres = futures::executor::block_on(guest.result(&addr)).expect("result");
        assert_eq!(eres.artifacts.len(), 1);
        assert_eq!(eres.artifacts_meta[0].hashout, "h1");
        let mut buf = String::new();
        eres.artifacts[0]
            .reader()
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, "hello");
    }
}
