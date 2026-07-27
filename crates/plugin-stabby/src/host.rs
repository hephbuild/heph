//! Host adapter: expose the engine's [`ProviderExecutor`] over the stable ABI so
//! a loaded plugin can call back via direct stabby vtable dispatch.

use crate::abi::{
    DynArtifact, DynExecutor, DynFunctionRegistry, DynLogSink, DynRead, DynSupervisor,
    NoteDepOutcome, QueryOutcome, ResultOutcome, StableAddr, StableArtifactContent, StableExecutor,
    StableFunctionRegistry, StableLogSink, StableRead, StableSupervisor, StatesOutcome,
};
use crate::vtable::dynify;
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
        dynify(stabby::boxed::Box::new(HostRead {
            inner: std::cell::RefCell::new(inner),
            buf: std::cell::RefCell::new(vec![0u8; READ_CHUNK]),
        }))
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

/// Host-side log sink handed to a loaded plugin. A cdylib statically links its
/// OWN `tracing`, whose global subscriber is never set, so a plugin's
/// `tracing::*` events would be dropped on the floor. The plugin installs a
/// subscriber that funnels every event here; this re-emits it on the *host's*
/// `tracing`, so plugin logs land in the host's output like any other span.
/// `level` is the `tracing::Level` as `1=ERROR .. 5=TRACE`; `target` carries the
/// plugin's module path so host filtering still works.
pub struct HostLogSink;

impl HostLogSink {
    /// Wrap as an ABI-stable [`DynLogSink`] to pass over the seam.
    pub fn wrap() -> DynLogSink {
        dynify(stabby::boxed::Box::new(HostLogSink))
    }
}

impl StableLogSink for HostLogSink {
    extern "C" fn log(&self, level: u8, target: SString, message: SString) {
        let target = target.to_string();
        let message = message.to_string();
        // Re-emit on the host subscriber. Target is set dynamically so the
        // plugin's module path is preserved for env-filter matching. The level
        // is a compile-time constant per arm, matching `tracing`'s macro shape.
        match level {
            1 => tracing::error!(target: "heph::plugin", plugin = %target, "{message}"),
            2 => tracing::warn!(target: "heph::plugin", plugin = %target, "{message}"),
            3 => tracing::info!(target: "heph::plugin", plugin = %target, "{message}"),
            4 => tracing::debug!(target: "heph::plugin", plugin = %target, "{message}"),
            _ => tracing::trace!(target: "heph::plugin", plugin = %target, "{message}"),
        }
    }
}

/// Host-side process-supervisor handle handed to a loaded plugin. Forwards onto
/// the host's tracker — the one that owns the socket to the sidecar — so children
/// a plugin spawns are reaped like any host-spawned child. See [`StableSupervisor`].
pub struct HostSupervisor;

impl HostSupervisor {
    /// Wrap as an ABI-stable [`DynSupervisor`] to pass over the seam.
    pub fn wrap() -> DynSupervisor {
        dynify(stabby::boxed::Box::new(HostSupervisor))
    }
}

/// Encode a supervisor call's outcome for the seam: empty on success, else the
/// full error chain.
fn sup_result(r: anyhow::Result<()>) -> SString {
    match r {
        Ok(()) => SString::new(),
        Err(e) => SString::from(format!("{e:#}").as_str()),
    }
}

impl StableSupervisor for HostSupervisor {
    extern "C" fn track(&self, pgid: i32) -> SString {
        sup_result(hproc::process_supervisor::tracker().track(pgid))
    }

    extern "C" fn untrack(&self, pgid: i32) -> SString {
        sup_result(hproc::process_supervisor::tracker().untrack(pgid))
    }

    extern "C" fn register_fuse_root(&self, root: SString) -> SString {
        sup_result(
            hproc::process_supervisor::tracker()
                .register_fuse_root(std::path::PathBuf::from(root.to_string())),
        )
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
        dynify(stabby::boxed::Box::new(HostFunctionRegistry { inner }))
    }
}

impl StableFunctionRegistry for HostFunctionRegistry {
    extern "C" fn call_registered<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>> {
        dynify(stabby::boxed::Box::new(async move {
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
        }))
    }
}

/// Wraps the per-request engine executor; handed to the plugin as a [`DynExecutor`].
pub struct HostExecutor {
    inner: Arc<dyn ProviderExecutor>,
}

impl HostExecutor {
    /// Wrap a per-request engine executor as an ABI-stable [`DynExecutor`].
    pub fn wrap(inner: Arc<dyn ProviderExecutor>) -> DynExecutor {
        dynify(stabby::boxed::Box::new(HostExecutor { inner }))
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
        dynify(stabby::boxed::Box::new(async move {
            let parsed = addr_from_stable(&addr);
            match self.inner.result(&parsed).await {
                Ok(eres) => {
                    // Hand each artifact across as a lazy streaming handle — the
                    // Arc<dyn Content> moves into the handle (keeping its cache
                    // read-guard alive), and bytes are pulled chunk-by-chunk by the
                    // guest. Nothing is buffered whole here.
                    let mut artifacts: SVec<DynArtifact> = SVec::new();
                    for art in eres.artifacts.iter() {
                        let handle: DynArtifact =
                            dynify(stabby::boxed::Box::new(HostArtifactContent {
                                content: Arc::clone(art),
                            }));
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
        }))
    }

    extern "C" fn query<'a>(
        &'a self,
        matcher_pb: SVec<u8>,
        extra_skip: SVec<SString>,
    ) -> DynFuture<'a, QueryOutcome> {
        dynify(stabby::boxed::Box::new(async move {
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
        }))
    }

    extern "C" fn states_under<'a>(&'a self, prefix: SString) -> DynFuture<'a, StatesOutcome> {
        dynify(stabby::boxed::Box::new(async move {
            let prefix = PkgBuf::from(prefix.to_string());
            match self.inner.states_under(&prefix).await {
                Ok(states) => StatesOutcome {
                    ok: true,
                    message: SString::new(),
                    states: states
                        .iter()
                        .map(|s| SVec::from(convert::state_to_pb(s).encode_to_vec().as_slice()))
                        .collect(),
                },
                Err(e) => StatesOutcome {
                    ok: false,
                    message: e.to_string().into(),
                    states: SVec::new(),
                },
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{HostLogSink, HostSupervisor};
    use crate::abi::{DynRead, StableRead, StableReadDyn};
    use stabby::vec::Vec as SVec;

    /// A distinct `StableRead` implementor per `N`: each monomorphization is its
    /// own source type, so each construction is a FIRST-USE insert into stabby's
    /// process-global vtable registry. This is the mutation side of the race.
    struct Churn<const N: usize>;

    impl<const N: usize> StableRead for Churn<N> {
        extern "C" fn read_chunk(&self) -> SVec<u8> {
            SVec::from([N as u8].as_slice())
        }
    }

    /// Keep the registry mutating while other threads read it.
    macro_rules! churn {
        ($($n:literal),* $(,)?) => {
            $({
                let handle: DynRead = crate::vtable::dynify(stabby::boxed::Box::new(Churn::<$n>));
                let got = handle.read_chunk();
                assert_eq!(got.as_slice(), [$n as u8], "vtable dispatched to the wrong impl");
            })*
        };
    }

    /// The host's `wrap` constructors run on the plugin-load path, which is driven
    /// under `rayon::into_par_iter` — so they race sibling first-use vtable inserts
    /// coming from other plugins loading concurrently. Built with a bare `.into()`
    /// a `wrap` reads the registry root without the guard and can clone a node
    /// another thread just freed, coming back with a null vtable slice and
    /// aborting the process (non-unwinding, so it is not catchable). Every one of
    /// them must go through `dynify`.
    #[test]
    fn host_wrap_constructors_are_registry_safe_under_concurrent_first_use() {
        std::thread::scope(|scope| {
            for i in 0..16 {
                scope.spawn(move || {
                    if i % 2 == 0 {
                        // Reader side: repeated lookups of an already-registered
                        // vtable, which is what a freed root corrupts.
                        for _ in 0..256 {
                            let _sup = HostSupervisor::wrap();
                            let _sink = HostLogSink::wrap();
                        }
                    } else {
                        churn!(
                            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19,
                            20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
                        );
                    }
                });
            }
        });
    }
}
