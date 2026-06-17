//! Host adapter: expose the engine's [`ProviderExecutor`] over the stable ABI so
//! a loaded plugin can call back via direct stabby vtable dispatch.

use crate::abi::{
    DynArtifact, DynExecutor, DynRead, NoteDepOutcome, QueryOutcome, ResultOutcome,
    StableArtifactContent, StableExecutor, StableRead,
};
use hcore::hartifactcontent::Content;
use hmodel::htaddr::parse_addr;
use hplugin::provider::ProviderExecutor;
use prost::Message;
use stabby::future::DynFutureUnsync as DynFuture;
use stabby::string::String as SString;
use stabby::vec::Vec as SVec;
use std::io::Read;
use std::sync::Arc;

/// Chunk size for streaming an artifact across the seam — bounds peak memory per
/// in-flight read regardless of artifact size.
const READ_CHUNK: usize = 64 * 1024;

/// Host-side streaming reader: pulls ≤`READ_CHUNK` bytes from the artifact's
/// `Content` reader on each `read_chunk`, returning an empty `SVec` at EOF.
/// `RefCell` because the ABI method is `&self` (vtable dispatch); the reader is
/// consumed on one thread.
struct HostRead {
    inner: std::cell::RefCell<Box<dyn Read>>,
}

impl StableRead for HostRead {
    extern "C" fn read_chunk(&self) -> SVec<u8> {
        let mut buf = vec![0u8; READ_CHUNK];
        // Read a single chunk; partial reads are fine (guest loops until EOF).
        match self.inner.borrow_mut().read(&mut buf) {
            Ok(0) | Err(_) => SVec::new(),
            Ok(n) => {
                buf.truncate(n);
                SVec::from(buf.as_slice())
            }
        }
    }
}

/// Host-side artifact handle. Owns the `Arc<dyn Content>` (keeping its cache
/// read-guard alive while the guest streams) and opens a fresh reader on demand.
struct HostArtifactContent {
    content: Arc<dyn Content>,
}

impl StableArtifactContent for HostArtifactContent {
    extern "C" fn open(&self) -> DynRead {
        // A reader that errors immediately yields empty (EOF) chunks — the guest
        // then sees a truncated/empty stream rather than a hang.
        let inner: Box<dyn Read> = self
            .content
            .reader()
            .unwrap_or_else(|_| Box::new(std::io::empty()));
        stabby::boxed::Box::new(HostRead {
            inner: std::cell::RefCell::new(inner),
        })
        .into()
    }

    extern "C" fn hashout(&self) -> SString {
        self.content.hashout().unwrap_or_default().into()
    }

    extern "C" fn byte_size(&self) -> u64 {
        self.content.byte_size().unwrap_or(u64::MAX)
    }
}

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
                    // Hand each artifact across as a lazy streaming handle — the
                    // Arc<dyn Content> moves into the handle (keeping its cache
                    // read-guard alive), and bytes are pulled chunk-by-chunk by the
                    // guest. Nothing is buffered whole here.
                    let mut artifacts: SVec<DynArtifact> = SVec::new();
                    for art in eres.artifacts.iter() {
                        let handle: DynArtifact = stabby::boxed::Box::new(HostArtifactContent {
                            content: Arc::clone(art),
                        })
                        .into();
                        artifacts.push(handle);
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
