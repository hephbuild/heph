//! Host adapter: expose the engine's [`ProviderExecutor`] over the stable ABI so
//! a loaded plugin can call back via direct stabby vtable dispatch.

use crate::abi::{
    DynExecutor, NoteDepOutcome, QueryOutcome, ResultOutcome, StableArtifact, StableExecutor,
};
use hmodel::htaddr::parse_addr;
use hplugin::provider::ProviderExecutor;
use prost::Message;
use stabby::future::DynFutureUnsync as DynFuture;
use stabby::string::String as SString;
use stabby::vec::Vec as SVec;
use std::io::Read;
use std::sync::Arc;

/// Wraps the per-request engine executor; handed to the plugin as a [`DynExecutor`].
pub struct HostExecutor {
    inner: Arc<dyn ProviderExecutor>,
}

impl HostExecutor {
    /// Wrap a per-request engine executor as an ABI-stable [`DynExecutor`].
    pub fn wrap(inner: Arc<dyn ProviderExecutor>) -> DynExecutor {
        stabby::boxed::Box::new(HostExecutor { inner }).into()
    }
}

fn is_cycle(e: &anyhow::Error) -> bool {
    hcore::hmemoizer::downcast_chain_ref::<hplugin::error::CycleError>(e).is_some()
}

impl StableExecutor for HostExecutor {
    extern "C" fn note_dep<'a>(&'a self, addr: SString) -> DynFuture<'a, NoteDepOutcome> {
        stabby::boxed::Box::new(async move {
            let parsed = match parse_addr(&addr) {
                Ok(a) => a,
                Err(e) => {
                    return NoteDepOutcome {
                        ok: false,
                        cycle: false,
                        message: format!("addr parse: {e}").into(),
                    };
                }
            };
            match self.inner.note_dep(&parsed).await {
                Ok(()) => NoteDepOutcome {
                    ok: true,
                    cycle: false,
                    message: SString::new(),
                },
                Err(e) => NoteDepOutcome {
                    ok: false,
                    cycle: is_cycle(&e),
                    message: e.to_string().into(),
                },
            }
        })
        .into()
    }

    extern "C" fn result<'a>(&'a self, addr: SString) -> DynFuture<'a, ResultOutcome> {
        stabby::boxed::Box::new(async move {
            let parsed = match parse_addr(&addr) {
                Ok(a) => a,
                Err(e) => {
                    return ResultOutcome {
                        ok: false,
                        cycle: false,
                        cancelled: false,
                        message: format!("addr parse: {e}").into(),
                        artifacts: SVec::new(),
                    };
                }
            };
            match self.inner.result(&parsed).await {
                Ok(eres) => {
                    let mut artifacts = SVec::new();
                    for (idx, art) in eres.artifacts.iter().enumerate() {
                        let hashout = eres
                            .artifacts_meta
                            .get(idx)
                            .map(|m| m.hashout.clone())
                            .or_else(|| art.hashout().ok())
                            .unwrap_or_default();
                        // Eagerly read the bytes (plugin-go reads a tiny file).
                        let bytes = match art.reader().and_then(|mut r| {
                            let mut b = Vec::new();
                            r.read_to_end(&mut b)?;
                            Ok(b)
                        }) {
                            Ok(b) => b,
                            Err(e) => {
                                return ResultOutcome {
                                    ok: false,
                                    cycle: false,
                                    cancelled: false,
                                    message: format!("artifact read: {e}").into(),
                                    artifacts: SVec::new(),
                                };
                            }
                        };
                        artifacts.push(StableArtifact {
                            hashout: hashout.into(),
                            bytes: SVec::from(bytes.as_slice()),
                        });
                    }
                    ResultOutcome {
                        ok: true,
                        cycle: false,
                        cancelled: false,
                        message: SString::new(),
                        artifacts,
                    }
                }
                Err(e) => ResultOutcome {
                    ok: false,
                    cycle: is_cycle(&e),
                    cancelled: hplugin::error::is_cancelled(&e),
                    message: e.to_string().into(),
                    artifacts: SVec::new(),
                },
            }
        })
        .into()
    }

    extern "C" fn query<'a>(
        &'a self,
        matcher_pb: SVec<u8>,
        extra_skip: SVec<SString>,
    ) -> DynFuture<'a, QueryOutcome> {
        stabby::boxed::Box::new(async move {
            let matcher = match plugin_abi::pb::Matcher::decode(&matcher_pb[..]) {
                Ok(m) => plugin_abi::convert::matcher_from_pb(m),
                Err(e) => {
                    return QueryOutcome {
                        ok: false,
                        message: format!("matcher decode: {e}").into(),
                        addrs: SVec::new(),
                    };
                }
            };
            let skip: Vec<String> = extra_skip.iter().map(|s| s.to_string()).collect();
            match self.inner.query(&matcher, &skip).await {
                Ok(addrs) => QueryOutcome {
                    ok: true,
                    message: SString::new(),
                    addrs: addrs.iter().map(|a| a.to_string().into()).collect(),
                },
                Err(e) => QueryOutcome {
                    ok: false,
                    message: e.to_string().into(),
                    addrs: SVec::new(),
                },
            }
        })
        .into()
    }
}
