//! Host adapter: expose the engine's [`ProviderExecutor`] over the stable ABI so
//! a loaded plugin can call back via direct stabby vtable dispatch.

use crate::abi::{
    DynArtifact, DynExecutor, DynFunctionRegistry, DynLogSink, DynRead, DynSupervisor,
    NoteDepOutcome, QueryOutcome, ResultOutcome, StableAddr, StableArtifactContent, StableExecutor,
    StableFunctionRegistry, StableLogSink, StableRead, StableSupervisor, StatesOutcome,
};
use crate::seam::panic_text;
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
        //
        // `to_str`, not `to_string_lossy`: the guest opens whatever it is handed
        // and does NOT fall back to the stream on failure, so a path mangled by
        // U+FFFD substitution is a hard read error where streaming would have
        // worked. A non-UTF-8 cache path (reachable on Linux, where the cache
        // root derives from an arbitrary `PathBuf`; APFS rejects invalid UTF-8)
        // therefore reports "not file-backed" and takes the slower-but-correct
        // route. `SString` is UTF-8, so carrying the raw bytes would mean an ABI
        // change — not worth it for a case whose only cost is a stream.
        match self.content.file_path().as_deref().and_then(Path::to_str) {
            Some(p) => SString::from(p),
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

/// The host-side mirror of the guest's spawn-at-the-seam shape.
///
/// A `DynFuture` a host callback returns is polled by a *guest* runtime worker
/// (the plugin body that awaits it runs on the plugin's own runtime). Running
/// the callback body inline in that poll would execute engine futures — which
/// touch the host reactor and timer wheel — on a thread with no host runtime
/// context, which panics ("there is no reactor running") and then aborts at the
/// extern seam. So the body is spawned onto the host runtime handed in at
/// `wrap()` time, and the returned future only awaits the `JoinHandle`
/// (runtime-free to poll). Symmetric with `plugin-sdk::serve`.
///
/// The two constructors make the mode an explicit caller decision rather than
/// a silent runtime probe: [`SeamSpawn::on`] for production (the caller passes
/// the runtime it is on), [`SeamSpawn::inline`] for runtime-less in-process
/// harnesses (`futures::executor`-driven tests), where the caller drives the
/// future itself and there is no host reactor to protect.
struct SeamSpawn {
    handle: Option<tokio::runtime::Handle>,
    /// Span re-entered by the spawned body so engine logs from plugin
    /// callbacks keep their tracing context.
    span: tracing::Span,
}

impl SeamSpawn {
    /// Spawn callback bodies on `handle`, instrumented with `span`.
    fn on(handle: tokio::runtime::Handle, span: tracing::Span) -> Self {
        Self {
            handle: Some(handle),
            span,
        }
    }

    /// Run callback bodies inline on the polling thread. Only sound where the
    /// caller drives the future itself (in-process test harnesses).
    fn inline(span: tracing::Span) -> Self {
        Self { handle: None, span }
    }
}

/// Abort-on-drop await of a host-side callback task: dropping the wrapper
/// (the guest abandoned the call — e.g. its own seam task was aborted) stops
/// the spawned body instead of leaking it.
///
/// The backstop is armed on every pending poll, same as the guest's
/// `SeamTask`: this future is polled by a *guest* worker, so the completion
/// wake (host task → guest worker) crosses the stabby waker seam. A lost wake
/// here parks the plugin task on this JoinHandle forever — the guest-side
/// backstop can't help, since re-polling the guest wrapper only re-polls a
/// never-woken `JoinHandle` if the wake that was lost is this one. Same
/// defense-in-depth as `hcore::blocking::run` (docs/CONCURRENCY_MEASUREMENTS.md
/// §2, lands with #298 below this PR in the stack).
struct HostTask<T> {
    handle: tokio::task::JoinHandle<T>,
    backstop: hcore::blocking::Backstop,
}

impl<T> std::future::Future for HostTask<T> {
    type Output = Result<T, tokio::task::JoinError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        match std::pin::Pin::new(&mut this.handle).poll(cx) {
            std::task::Poll::Ready(out) => std::task::Poll::Ready(out),
            std::task::Poll::Pending => {
                this.backstop.arm(cx.waker());
                std::task::Poll::Pending
            }
        }
    }
}

impl<T> Drop for HostTask<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl SeamSpawn {
    /// Run `fut` (a host callback body) to a future safe to hand across the
    /// seam: spawned on the runtime this wrapper was constructed with, inline
    /// otherwise. A panicking body maps to `mk_err` (host-side bug, surfaced to
    /// the guest as an error) — never an unwind out of the wrapper's poll,
    /// which runs inside the extern shim.
    async fn run<T, F>(
        &self,
        method: &'static str,
        key: String,
        fut: F,
        mk_err: fn(String) -> T,
    ) -> T
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        use tracing::Instrument;
        let Some(handle) = &self.handle else {
            return fut.instrument(self.span.clone()).await;
        };
        let task = HostTask {
            handle: hcore::hmemoizer::spawn_on_with_cycle_ctx(
                handle,
                fut.instrument(self.span.clone()),
            ),
            backstop: hcore::blocking::Backstop::new(),
        };
        match task.await {
            Ok(v) => v,
            Err(e) if e.is_panic() => {
                let payload = e.into_panic();
                mk_err(format!(
                    "host callback {method}({key}) panicked: {}",
                    panic_text(payload.as_ref())
                ))
            }
            Err(_) => mk_err(format!(
                "host callback {method}({key}) aborted: host runtime shut down"
            )),
        }
    }
}

/// Wraps the host's aggregate function registry; handed to a plugin as a
/// [`DynFunctionRegistry`] so it can invoke any registered function.
pub struct HostFunctionRegistry {
    inner: Arc<ProviderFunctionRegistry>,
    seam: SeamSpawn,
}

impl HostFunctionRegistry {
    /// The span callback bodies run under. Purpose-made rather than
    /// `Span::current()`: the registry is wired once per process, so capturing
    /// the ambient span would attribute every later call to whichever request
    /// happened to wire it first — and pin that request's span for the process
    /// lifetime.
    fn span() -> tracing::Span {
        tracing::info_span!("provider_functions")
    }

    /// Wrap the aggregate registry as an ABI-stable [`DynFunctionRegistry`],
    /// spawning callback bodies on `handle` — see [`SeamSpawn`].
    pub fn wrap(
        inner: Arc<ProviderFunctionRegistry>,
        handle: tokio::runtime::Handle,
    ) -> DynFunctionRegistry {
        dynify(stabby::boxed::Box::new(HostFunctionRegistry {
            inner,
            seam: SeamSpawn::on(handle, Self::span()),
        }))
    }

    /// Runtime-less variant for in-process test harnesses: callback bodies run
    /// inline on the polling thread, which is the caller's own driver.
    pub fn wrap_inline(inner: Arc<ProviderFunctionRegistry>) -> DynFunctionRegistry {
        dynify(stabby::boxed::Box::new(HostFunctionRegistry {
            inner,
            seam: SeamSpawn::inline(Self::span()),
        }))
    }
}

impl StableFunctionRegistry for HostFunctionRegistry {
    extern "C" fn call_registered<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>> {
        let req = match pb::CallRegisteredRequest::decode(&req[..]) {
            Ok(r) => r,
            Err(e) => {
                let body = unary(err_body(format!("call_registered decode: {e}")));
                return dynify(stabby::boxed::Box::new(async move { body }));
            }
        };
        let key = format!("{}.{}", req.provider, req.name);
        let inner = Arc::clone(&self.inner);
        let fut = async move {
            let Some(rf) = inner.get(&req.provider, &req.name) else {
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
        };
        dynify(stabby::boxed::Box::new(self.seam.run(
            "call_registered",
            key,
            fut,
            |m| unary(err_body(m)),
        )))
    }
}

/// Wraps the per-request engine executor; handed to the plugin as a [`DynExecutor`].
pub struct HostExecutor {
    inner: Arc<dyn ProviderExecutor>,
    seam: SeamSpawn,
}

impl HostExecutor {
    /// Wrap a per-request engine executor as an ABI-stable [`DynExecutor`],
    /// spawning callback bodies on `handle` (instrumented with the caller's
    /// current span) so they run on the host runtime, not on the guest worker
    /// polling them — see [`SeamSpawn`].
    pub fn wrap(inner: Arc<dyn ProviderExecutor>, handle: tokio::runtime::Handle) -> DynExecutor {
        dynify(stabby::boxed::Box::new(HostExecutor {
            inner,
            seam: SeamSpawn::on(handle, tracing::Span::current()),
        }))
    }

    /// Runtime-less variant for in-process test harnesses: callback bodies run
    /// inline on the polling thread, which is the caller's own driver.
    pub fn wrap_inline(inner: Arc<dyn ProviderExecutor>) -> DynExecutor {
        dynify(stabby::boxed::Box::new(HostExecutor {
            inner,
            seam: SeamSpawn::inline(tracing::Span::current()),
        }))
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
        let parsed = addr_from_stable(&addr);
        let key = parsed.to_string();
        let inner = Arc::clone(&self.inner);
        let fut = async move {
            match inner.result(&parsed).await {
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
        };
        dynify(stabby::boxed::Box::new(self.seam.run(
            "result",
            key,
            fut,
            |m| ResultOutcome {
                ok: false,
                cycle: false,
                cancelled: false,
                message: m.into(),
                artifacts: SVec::new(),
            },
        )))
    }

    extern "C" fn query<'a>(
        &'a self,
        matcher_pb: SVec<u8>,
        extra_skip: SVec<SString>,
    ) -> DynFuture<'a, QueryOutcome> {
        let inner = Arc::clone(&self.inner);
        let fut = async move {
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
            match inner.query(&matcher, &skip).await {
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
        };
        dynify(stabby::boxed::Box::new(self.seam.run(
            "query",
            String::new(),
            fut,
            |m| QueryOutcome {
                ok: false,
                message: m.into(),
                addrs: SVec::new(),
            },
        )))
    }

    extern "C" fn states_under<'a>(&'a self, prefix: SString) -> DynFuture<'a, StatesOutcome> {
        let prefix = PkgBuf::from(prefix.to_string());
        let key = prefix.as_str().to_string();
        let inner = Arc::clone(&self.inner);
        let fut = async move {
            match inner.states_under(&prefix).await {
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
        };
        dynify(stabby::boxed::Box::new(self.seam.run(
            "states_under",
            key,
            fut,
            |m| StatesOutcome {
                ok: false,
                message: m.into(),
                states: SVec::new(),
            },
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::{HostArtifactContent, HostLogSink, HostSupervisor};
    use crate::abi::{DynRead, StableArtifactContent, StableRead, StableReadDyn};
    use stabby::vec::Vec as SVec;

    /// `Content` naming a path the seam has to render as a `SString`.
    struct PathContent(std::path::PathBuf);

    impl hcore::hartifactcontent::Content for PathContent {
        fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
            Ok(Box::new(std::io::empty()))
        }
        fn walk(
            &self,
        ) -> anyhow::Result<
            Box<dyn Iterator<Item = anyhow::Result<hcore::hartifactcontent::WalkEntry>> + '_>,
        > {
            Ok(Box::new(std::iter::empty()))
        }
        fn hashout(&self) -> anyhow::Result<String> {
            Ok("h".to_string())
        }
        fn file_path(&self) -> Option<std::path::PathBuf> {
            Some(self.0.clone())
        }
    }

    /// A UTF-8 cache path crosses intact — the ordinary case.
    #[test]
    fn path_carries_a_utf8_cache_path() {
        let h = HostArtifactContent {
            content: std::sync::Arc::new(PathContent("/cache/blobs/out.tar".into())),
        };
        assert_eq!(h.path().to_string(), "/cache/blobs/out.tar");
    }

    /// A non-UTF-8 cache path must report "not file-backed" (empty), so the guest
    /// streams. `SString` is UTF-8, and the guest opens whatever it is handed
    /// with no fallback — so lossy conversion would substitute U+FFFD and turn a
    /// working read into `ENOENT`. Correct-and-slower beats wrong.
    ///
    /// Unix-only: the byte sequence has no `OsString` equivalent elsewhere. It is
    /// reachable on Linux (ext4/xfs accept arbitrary bytes) and not on macOS
    /// (APFS rejects invalid UTF-8), which is exactly why it needs a test rather
    /// than a green CI run.
    #[cfg(unix)]
    #[test]
    fn path_refuses_a_non_utf8_cache_path_instead_of_mangling_it() {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::ffi::OsStr::from_bytes(b"/cache/blobs/\xff\xfe/out.tar");
        let h = HostArtifactContent {
            content: std::sync::Arc::new(PathContent(std::path::PathBuf::from(raw))),
        };
        assert_eq!(
            h.path().to_string(),
            "",
            "a path that cannot round-trip must read as 'not file-backed'"
        );
    }

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

    /// Abort-on-drop on the host mirror: the guest abandoning a callback future
    /// (dropping it — e.g. its own seam task was aborted) must stop the spawned
    /// host body, not leak it on the engine runtime. Same shape as the guest's
    /// `dropped_seam_future_aborts_the_spawned_body`: side-effect flag +
    /// deadline poll. The host spawn is lazy (first poll of the returned
    /// future), so the future is polled once before the drop.
    #[test]
    fn dropped_callback_future_aborts_the_spawned_host_body() {
        use crate::abi::{StableAddr, StableExecutorDyn};
        use futures::future::BoxFuture;
        use hmodel::htaddr::Addr;
        use hplugin::provider::ProviderExecutor;
        use std::future::Future as _;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        struct SetOnDrop(Arc<AtomicBool>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        struct Hanger {
            started: Arc<AtomicBool>,
            stopped: Arc<AtomicBool>,
        }
        impl ProviderExecutor for Hanger {
            fn result<'a>(
                &'a self,
                _addr: &'a Addr,
            ) -> BoxFuture<'a, anyhow::Result<Arc<hplugin::eresult::EResult>>> {
                let started = Arc::clone(&self.started);
                let stopped = Arc::clone(&self.stopped);
                Box::pin(async move {
                    // Dropped only when this body's future is dropped — i.e.
                    // when the spawned host task is aborted.
                    let _guard = SetOnDrop(stopped);
                    started.store(true, Ordering::SeqCst);
                    futures::future::pending::<()>().await;
                    anyhow::bail!("unreachable: pending never resolves")
                })
            }
            fn query<'a>(
                &'a self,
                _m: &'a hmodel::htmatcher::Matcher,
                _s: &'a [String],
            ) -> BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
                Box::pin(async { anyhow::bail!("unused") })
            }
        }

        let started = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("rt");
        let dynexec = super::HostExecutor::wrap(
            Arc::new(Hanger {
                started: Arc::clone(&started),
                stopped: Arc::clone(&stopped),
            }) as Arc<dyn ProviderExecutor>,
            rt.handle().clone(),
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        {
            let fut = dynexec.result(StableAddr {
                package: "p".into(),
                name: "t".into(),
                args: stabby::vec::Vec::new(),
            });
            futures::pin_mut!(fut);
            let waker = futures::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            assert!(
                fut.as_mut().poll(&mut cx).is_pending(),
                "callback body must still be running"
            );
            while !started.load(Ordering::SeqCst) {
                assert!(Instant::now() < deadline, "spawned host body never started");
                std::thread::sleep(Duration::from_millis(5));
            }
            // The guest abandons the call: the future drops at end of scope.
        }
        while !stopped.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "host body still running after the callback future was dropped"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
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
