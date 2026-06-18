//! Host adapter: expose the engine's [`ProviderExecutor`] over the stable ABI so
//! a loaded plugin can call back via direct stabby vtable dispatch.

use crate::abi::{
    DynArtifact, DynExecutor, DynFunctionRegistry, DynRead, NoteDepOutcome, QueryOutcome,
    ResultOutcome, StableAddr, StableArtifactContent, StableExecutor, StableFunctionRegistry,
    StableRead,
};
use hcore::hartifactcontent::Content;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::provider::{FnArgs, FnCallContext, ProviderExecutor, ProviderFunctionRegistry};
use plugin_abi::pb::frame::Body;
use plugin_abi::{convert, pb};
use prost::Message;
use stabby::future::DynFutureUnsync as DynFuture;
use stabby::string::String as SString;
use stabby::vec::Vec as SVec;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

/// Chunk size for streaming an artifact across the seam — bounds peak memory per
/// in-flight read regardless of artifact size.
const READ_CHUNK: usize = 64 * 1024;

/// Host-side streaming reader: pulls ≤`READ_CHUNK` bytes from the artifact's
/// `Content` reader on each `read_chunk`, returning an empty `SVec` at EOF.
/// `RefCell` because the ABI method is `&self` (vtable dispatch); the reader is
/// consumed on one thread. The scratch `buf` is allocated once per reader and
/// reused across chunks (incl. the EOF read), not re-allocated per call.
struct HostRead {
    inner: std::cell::RefCell<Box<dyn Read>>,
    buf: std::cell::RefCell<Vec<u8>>,
}

impl StableRead for HostRead {
    extern "C" fn read_chunk(&self) -> SVec<u8> {
        let mut inner = self.inner.borrow_mut();
        let mut buf = self.buf.borrow_mut();
        // Read a single chunk into the reused buffer; partial reads are fine
        // (the guest loops until EOF). Only the `n` read bytes are copied out.
        match inner.read(&mut buf) {
            Ok(0) | Err(_) => SVec::new(),
            Ok(n) => SVec::from(buf.get(..n).unwrap_or_default()),
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
            buf: std::cell::RefCell::new(vec![0u8; READ_CHUNK]),
        })
        .into()
    }

    extern "C" fn hashout(&self) -> SString {
        self.content.hashout().unwrap_or_default().into()
    }

    extern "C" fn byte_size(&self) -> u64 {
        self.content.byte_size().unwrap_or(u64::MAX)
    }

    extern "C" fn path(&self) -> SString {
        // Non-empty only when the content is a real on-disk file (e.g. cache
        // artifact); the guest then reads it directly instead of streaming.
        match self.content.file_path() {
            Some(p) => SString::from(p.to_string_lossy().as_ref()),
            None => SString::new(),
        }
    }
}

/// Encode a unary `pb::Frame` reply carrying `body`.
fn unary(body: Body) -> SVec<u8> {
    SVec::from(
        pb::Frame {
            id: 0,
            body: Some(body),
        }
        .encode_to_vec()
        .as_slice(),
    )
}

fn err_body(message: String) -> Body {
    Body::Error(pb::Error {
        kind: pb::error::Kind::Other as i32,
        message,
    })
}

/// Wraps the host's aggregate function registry; handed to a plugin as a
/// [`DynFunctionRegistry`] so it can invoke any registered function.
pub struct HostFunctionRegistry {
    inner: Arc<ProviderFunctionRegistry>,
}

impl HostFunctionRegistry {
    /// Wrap the aggregate registry as an ABI-stable [`DynFunctionRegistry`].
    pub fn wrap(inner: Arc<ProviderFunctionRegistry>) -> DynFunctionRegistry {
        stabby::boxed::Box::new(HostFunctionRegistry { inner }).into()
    }
}

impl StableFunctionRegistry for HostFunctionRegistry {
    extern "C" fn call_registered<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>> {
        stabby::boxed::Box::new(async move {
            let req = match pb::CallRegisteredRequest::decode(&req[..]) {
                Ok(r) => r,
                Err(e) => return unary(err_body(format!("call_registered decode: {e}"))),
            };
            let Some(rf) = self.inner.get(&req.provider, &req.name) else {
                return unary(err_body(format!(
                    "unknown registered function `{}.{}`",
                    req.provider, req.name
                )));
            };
            let root = std::path::PathBuf::from(req.root);
            let ctx = FnCallContext {
                pkg: &req.pkg,
                root: Path::new(&root),
            };
            let args = FnArgs {
                positional: req
                    .positional
                    .into_iter()
                    .map(convert::value_from_pb)
                    .collect(),
                named: req
                    .named
                    .into_iter()
                    .map(|(k, v)| (k, convert::value_from_pb(v)))
                    .collect(),
            };
            match rf.func.call(&ctx, args).await {
                Ok(v) => unary(Body::CallFunctionResp(pb::CallFunctionResponse {
                    value: Some(convert::value_to_pb(&v)),
                })),
                Err(e) => unary(err_body(format!("{e:#}"))),
            }
        })
        .into()
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

/// Reconstruct an `Addr` from the seam's parts — no `//pkg:name` parse.
fn addr_from_stable(a: &StableAddr) -> Addr {
    let args = a
        .args
        .iter()
        .map(|arg| (arg.key.to_string(), arg.val.to_string()))
        .collect();
    Addr::new(
        PkgBuf::from(a.package.to_string()),
        a.name.to_string(),
        args,
    )
}

impl StableExecutor for HostExecutor {
    extern "C" fn note_dep(&self, addr: StableAddr) -> NoteDepOutcome {
        let parsed = addr_from_stable(&addr);
        // The engine's note_dep is a synchronous DepDag insert wrapped in a
        // ready future; drive it to completion without boxing a stabby future.
        match futures::executor::block_on(self.inner.note_dep(&parsed)) {
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
    }

    extern "C" fn result<'a>(&'a self, addr: StableAddr) -> DynFuture<'a, ResultOutcome> {
        stabby::boxed::Box::new(async move {
            let parsed = addr_from_stable(&addr);
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
