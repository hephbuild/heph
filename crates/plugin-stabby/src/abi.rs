//! The stable wire-ABI surface: stabby-stable types + traits that cross the
//! cdylib boundary. Pure stabby — no domain deps — so both host and guest depend
//! on the exact same layout. Conversions to/from the engine's native types live
//! in [`crate::host`] / [`crate::guest`].
//!
//! Every `Dyn…` handle below must be built with [`crate::vtable::dynify`], never a
//! bare `.into()`: stabby's process-global vtable registry corrupts if one thread
//! registers a first-use vtable while another reads the registry.

// Engine futures are `Send` but not `Sync` (BoxFuture), so the ABI returns the
// Send-only stabby future (`DynFuture` would additionally require `Sync`).
use stabby::future::DynFutureUnsync as DynFuture;
use stabby::string::String as SString;
use stabby::vec::Vec as SVec;

/// One `Addr` arg (`key=value`).
#[stabby::stabby]
pub struct StableArg {
    pub key: SString,
    pub val: SString,
}

/// A target address crossing the seam as its already-parsed parts (package, name,
/// args), so neither side formats-to-string then re-parses (`//pkg:name`) per
/// callback. Args are preserved — they are part of the addr's interned identity.
#[stabby::stabby]
pub struct StableAddr {
    pub package: SString,
    pub name: SString,
    pub args: SVec<StableArg>,
}

/// Outcome of a `note_dep` dep-edge registration.
#[stabby::stabby]
pub struct NoteDepOutcome {
    pub ok: bool,
    /// The edge closed a dependency cycle (typed, not message-matched).
    pub cycle: bool,
    pub message: SString,
}

/// A streaming reader over an artifact's bytes. `read_chunk` returns up to an
/// internal chunk (empty `SVec` = EOF), so the guest pulls lazily and the whole
/// artifact is never buffered in memory at the seam.
#[stabby::stabby]
pub trait StableRead {
    /// `&self` (not `&mut`) so it dispatches over the stable vtable like the rest
    /// of the ABI; implementors use interior mutability for the read cursor.
    extern "C" fn read_chunk(&self) -> SVec<u8>;
}

/// An owned, ABI-stable streaming reader handle. Not `Send`: like
/// `hcore::Content::reader`, it is opened and consumed on one thread (the
/// `Send + Sync` artifact handle is what crosses threads).
pub type DynRead = stabby::dynptr!(stabby::boxed::Box<dyn StableRead>);

/// One result artifact as a lazy handle: `open` yields a FRESH streaming reader
/// (the guest's `reader()` and `walk()` each re-read), plus cheap metadata. The
/// handle owns the underlying `Content` (host-side), keeping its cache read-guard
/// alive while the guest streams.
#[stabby::stabby]
pub trait StableArtifactContent {
    extern "C" fn open(&self) -> DynRead;
    extern "C" fn hashout(&self) -> SString;
    /// Byte size hint; `u64::MAX` means unknown.
    extern "C" fn byte_size(&self) -> u64;
    /// The artifact's on-disk path when it is a real local file (e.g. an on-disk
    /// cache artifact). EMPTY means not file-backed (synthetic/in-memory). Since
    /// host and guest share the process and filesystem, a non-empty path lets the
    /// guest open the file directly instead of pulling its bytes chunk-by-chunk
    /// through [`open`](StableArtifactContent::open) — no per-chunk vtable hop.
    extern "C" fn path(&self) -> SString;
}

/// An owned, ABI-stable artifact handle.
pub type DynArtifact = stabby::dynptr!(stabby::boxed::Box<dyn StableArtifactContent + Send + Sync>);

/// Outcome of a `result` resolution.
#[stabby::stabby]
pub struct ResultOutcome {
    pub ok: bool,
    pub cycle: bool,
    pub cancelled: bool,
    pub message: SString,
    pub artifacts: SVec<DynArtifact>,
}

/// Outcome of a `query`.
#[stabby::stabby]
pub struct QueryOutcome {
    pub ok: bool,
    pub message: SString,
    /// Canonical `//pkg:name` addr strings.
    pub addrs: SVec<SString>,
}

/// Outcome of a `states_under` call. `State` has no canonical string form, so
/// each state crosses as prost-encoded `pb::State` bytes — mirroring how
/// `query`'s matcher crosses as `SVec<u8>`.
#[stabby::stabby]
pub struct StatesOutcome {
    pub ok: bool,
    pub message: SString,
    /// Each entry is a prost-encoded `pb::State`.
    pub states: SVec<SVec<u8>>,
}

/// The host callback surface, called by the plugin while serving `get`/`parse`.
/// Mirrors `hplugin::provider::ProviderExecutor`. Implemented host-side over the
/// real engine executor ([`crate::host`]); consumed guest-side wrapped back into
/// a `ProviderExecutor` ([`crate::guest`]). Calls are direct vtable dispatch —
/// no serialization, no message-passing.
///
/// `addr` is the canonical `//pkg:name` string; `query`'s matcher crosses as
/// prost bytes (query is rare/zero on the hot path).
#[stabby::stabby]
pub trait StableExecutor {
    /// Synchronous: registering a dep edge is a `DepDag` insert (no real await),
    /// so it skips the boxed-future + stabby future-vtable cost of the other
    /// callbacks. This is the highest-volume callback on the hot path.
    extern "C" fn note_dep(&self, addr: StableAddr) -> NoteDepOutcome;
    extern "C" fn result<'a>(&'a self, addr: StableAddr) -> DynFuture<'a, ResultOutcome>;
    extern "C" fn query<'a>(
        &'a self,
        matcher_pb: SVec<u8>,
        extra_skip: SVec<SString>,
    ) -> DynFuture<'a, QueryOutcome>;
    /// Fetch provider states for every package at or under `prefix` — the
    /// downward subtree (config lookup; no dep edge). `prefix` is a package path.
    extern "C" fn states_under<'a>(&'a self, prefix: SString) -> DynFuture<'a, StatesOutcome>;
}

/// An owned, ABI-stable handle to a host executor — what the host passes into the
/// plugin's `get`/`parse`.
pub type DynExecutor = stabby::dynptr!(stabby::boxed::Box<dyn StableExecutor + Send + Sync>);

/// The host's provider-function registry, called by a plugin that was handed the
/// aggregate registry (via `set_function_registry`) and wants to invoke one of
/// the functions in it. Mirrors a lookup + [`hplugin::provider::ProviderFn::call`]
/// on the host side. `req` is raw `pb::CallRegisteredRequest` bytes; the reply is
/// a `pb::Frame` carrying `CallFunctionResp` (the returned value) or `Error`.
#[stabby::stabby]
pub trait StableFunctionRegistry {
    extern "C" fn call_registered<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>>;
}

/// An owned, ABI-stable handle to the host's function registry — what the host
/// passes into the plugin's `set_function_registry`.
pub type DynFunctionRegistry =
    stabby::dynptr!(stabby::boxed::Box<dyn StableFunctionRegistry + Send + Sync>);

/// Static, request-less plugin metadata (config name, provider `functions` /
/// `state_schema`, driver `schema`), kept in its OWN sync trait so the evolvable
/// RPC dispatch surface ([`StableProvider`] / [`StableManagedDriver`]) stays
/// purely the async method dispatch. Composed into both handle types. `kind` is a
/// `pb::ProviderMethod` / `pb::DriverMethod` selecting which metadatum; the reply
/// is that metadatum's prost bytes (an EMPTY `SVec` encodes "none", e.g. a
/// provider with no `state_schema`). Sync because it is read once during registry
/// wiring, never on a hot path. Same append-only contract as the async dispatch.
#[stabby::stabby]
pub trait StableMeta {
    extern "C" fn meta(&self, kind: u32) -> SVec<u8>;
}

/// A pull stream of items across the seam (the streaming counterpart of the unary
/// `SVec<u8>` reply). Each `next` yields one prost length-delimited `pb::Frame`
/// (`StreamItem` for an item, or `StreamEnd{error}` to end with failure); an EMPTY
/// `SVec` means the stream is exhausted cleanly. Used for BOTH directions — a
/// request stream (host → plugin) and a response stream (plugin → host). `&self`
/// (not `&mut`) so it dispatches over the stable vtable; implementors use interior
/// mutability for the cursor. Unlike [`DynRead`], the handle is `Send + Sync`
/// (`Mutex`-guarded cursor) because list results flow into the engine, which
/// requires `Send` iterators.
#[stabby::stabby]
pub trait StableItemStream {
    extern "C" fn next(&self) -> SVec<u8>;
}

/// An owned, ABI-stable item-stream handle (a request or response stream).
pub type DynItemStream = stabby::dynptr!(stabby::boxed::Box<dyn StableItemStream + Send + Sync>);

/// The cold provider surface as a FROZEN generic dispatch. The `method` id
/// (`pb::ProviderMethod`) selects an RPC; payloads cross as prost-encoded
/// `pb::Frame` bytes (cheap, low-volume, lenient via protobuf). Adding an RPC is a
/// new method id + a new guest match arm — the vtable is UNTOUCHED, so an older
/// plugin and a newer host still load (stabby's type report is unchanged) and the
/// old plugin answers an unknown id with `Error{Unimplemented}`. THIS is what
/// makes the cold surface evolvable without an ABI break (see ABI_VERSIONING.md).
///
/// The slots cover the four RPC cardinalities (request × response, each unary or
/// streaming) plus the native-handle carriers (a `DynExecutor` / `DynFunctionRegistry`
/// cannot ride prost bytes, so the method carrying one gets its own frozen slot):
/// - [`invoke`](StableProvider::invoke) — unary → unary.
/// - [`invoke_server_stream`](StableProvider::invoke_server_stream) — unary →
///   stream (`list`, `list_packages`): the reply is pulled lazily, never buffered.
/// - [`invoke_client_stream`](StableProvider::invoke_client_stream) — stream → unary.
/// - [`invoke_bidi`](StableProvider::invoke_bidi) — stream → stream.
/// - [`invoke_exec`](StableProvider::invoke_exec) — unary → unary + native
///   [`DynExecutor`] (`get`, whose resolution makes hot callbacks).
/// - [`invoke_registry`](StableProvider::invoke_registry) — unary → void + native
///   [`DynFunctionRegistry`] (`set_function_registry`).
#[stabby::stabby]
pub trait StableProvider {
    extern "C" fn invoke<'a>(&'a self, method: u32, req: SVec<u8>) -> DynFuture<'a, SVec<u8>>;
    extern "C" fn invoke_server_stream<'a>(
        &'a self,
        method: u32,
        req: SVec<u8>,
    ) -> DynFuture<'a, DynItemStream>;
    extern "C" fn invoke_client_stream<'a>(
        &'a self,
        method: u32,
        req: DynItemStream,
    ) -> DynFuture<'a, SVec<u8>>;
    extern "C" fn invoke_bidi<'a>(
        &'a self,
        method: u32,
        req: DynItemStream,
    ) -> DynFuture<'a, DynItemStream>;
    extern "C" fn invoke_exec<'a>(
        &'a self,
        method: u32,
        req: SVec<u8>,
        exec: DynExecutor,
    ) -> DynFuture<'a, SVec<u8>>;
    /// Server-streaming **with** a native [`DynExecutor`] — for `list`, whose
    /// enumeration may call back (e.g. the go plugin gathering a module's variant
    /// universe via `states_under`). Combines the `invoke_server_stream` and
    /// `invoke_exec` cardinalities.
    extern "C" fn invoke_exec_server_stream<'a>(
        &'a self,
        method: u32,
        req: SVec<u8>,
        exec: DynExecutor,
    ) -> DynFuture<'a, DynItemStream>;
    extern "C" fn invoke_registry(&self, method: u32, req: SVec<u8>, reg: DynFunctionRegistry);
}

/// The cold managed-driver surface, same frozen-dispatch contract and the same
/// four cardinalities as [`StableProvider`]. `method` is a `pb::DriverMethod`.
/// `run` rides [`invoke_bidi`](StableManagedDriver::invoke_bidi): the request
/// stream carries the run request then live stdin (`pb::RunInFrame`), the response
/// stream carries live stdout/stderr then the result (`pb::RunOutFrame`). No
/// dedicated stdio slot — live IO is modeled as prost frames on the bidi stream,
/// so wiring stdin/stdout/stderr later is additive (see ABI_VERSIONING.md).
#[stabby::stabby]
pub trait StableManagedDriver {
    extern "C" fn invoke<'a>(&'a self, method: u32, req: SVec<u8>) -> DynFuture<'a, SVec<u8>>;
    extern "C" fn invoke_server_stream<'a>(
        &'a self,
        method: u32,
        req: SVec<u8>,
    ) -> DynFuture<'a, DynItemStream>;
    extern "C" fn invoke_client_stream<'a>(
        &'a self,
        method: u32,
        req: DynItemStream,
    ) -> DynFuture<'a, SVec<u8>>;
    /// `run`. Takes a [`DynExecService`] for the same reason a provider's `list`
    /// takes a [`DynExecutor`]: the call needs something only the host can do —
    /// here, creating the target's processes in the session the host resolved.
    extern "C" fn invoke_bidi<'a>(
        &'a self,
        method: u32,
        req: DynItemStream,
        exec: DynExecService,
    ) -> DynFuture<'a, DynItemStream>;
}

/// Process creation, fulfilled by the **host** on a plugin driver's behalf.
///
/// The inversion that removes a whole class of bug. A driver in a cdylib cannot
/// be handed the session its target must run in — a session is a live object and
/// the plugin links its own copy of every type — so it used to be sent an
/// approximation instead: the session's environment variables. That is exact for
/// an environment-shaped session and silently wrong for one that also rewrites
/// the command, which is every container and every held-open shell.
///
/// So the driver does not create the process. It asks the host, which holds the
/// real session and applies it in full. Nothing about the session has to cross,
/// and every runner mode works for every driver.
///
/// Handed to `run` the same way a [`DynExecutor`] is handed to a provider's
/// `list` — that precedent is why this is a parameter rather than a global.
#[stabby::stabby]
pub trait StableExecService {
    /// `req` is a prost `pb::ExecOutputRequest`; the reply is a `pb::Frame`
    /// carrying `ExecOutputResponse` or an error.
    ///
    /// Batch only. Nine of the tree's ten plugin-side call sites want exactly
    /// this, and the tenth (streaming buildx progress) degrades to it rather
    /// than justifying a second lane before something needs one.
    extern "C" fn exec_output<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>>;
}

pub type DynExecService = stabby::dynptr!(stabby::boxed::Box<dyn StableExecService + Send + Sync>);

/// The cold exec-runner surface: **one method**, which hands back a live
/// session.
///
/// A **first-class component**, not a driver that happens to answer extra
/// methods. A runner is not a driver: it does not parse, build, or have a
/// config schema, and forcing it through `StableManagedDriver` would make every
/// runner-only plugin stub `parse`/`apply_transitive`/`run` and would tie a
/// runner's registered name to a driver it does not have.
///
/// `open` returns a [`DynExecSession`] — an object, not a handle to look up
/// later. That is the same move [`ResultOutcome`] already makes with
/// [`DynArtifact`]: a cdylib is `dlopen`ed into this process, so a trait object
/// crosses the seam as a fat pointer and stays live on the plugin's side. An
/// earlier draft of this trait assumed the opposite and grew an id-keyed
/// protocol — `open_session`/`prepare_spec`/`close_session` plus a host-side
/// session table — to route around a boundary that is not there. There is no
/// session id on this seam, and no host-side session registry.
#[stabby::stabby]
pub trait StableExecRunner {
    /// `req` is a prost `pb::OpenSessionRequest`.
    extern "C" fn open<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, OpenOutcome>;
}

/// One live exec environment, owned by the plugin.
///
/// What a session *is* never crosses: a `devenv shell` held open, a socket, a
/// pid, a mux routing many targets through one of them, whatever bookkeeping
/// that needs. The host holds this object, calls `prepare` per spawn and `close`
/// once, and knows nothing else about it.
///
/// `method` is a `pb::ExecSessionMethod`; new ones are additive method ids, and
/// the vtable is untouched.
#[stabby::stabby]
pub trait StableExecSession {
    extern "C" fn invoke<'a>(&'a self, method: u32, req: SVec<u8>) -> DynFuture<'a, SVec<u8>>;
}

/// The cold hook surface: a single client-streaming RPC. A hook is a build-event
/// CONSUMER — the host streams the engine's `BuildEvent`s into the plugin
/// (`HOOK_METHOD_ON_EVENTS`: the request stream carries one event per
/// [`StableItemStream::next`], each an envelope `StreamItem` whose `item` is the
/// event's serde-JSON bytes), and the reply is a unary ack once the stream ends.
/// Same frozen-dispatch + append-only contract as [`StableProvider`]; new hook
/// RPCs are new method ids, the vtable is untouched.
#[stabby::stabby]
pub trait StableHook {
    extern "C" fn invoke_client_stream<'a>(
        &'a self,
        method: u32,
        req: DynItemStream,
    ) -> DynFuture<'a, SVec<u8>>;
}

/// Cooperative cancellation, in its OWN trait (composed into both handles like
/// [`StableMeta`]). The host calls `cancel(request_id)` when its own request token
/// fires; the plugin looks up the in-flight call by `request_id` and trips the
/// cancellation token it handed the provider/driver — so a long `get` or a running
/// subprocess (`run`) stops, exactly as for an in-process target. `request_id` is
/// the id carried in each request message; it must be unique per in-flight call.
#[stabby::stabby]
pub trait StableCancel {
    extern "C" fn cancel(&self, request_id: SString);
}

/// Owned ABI-stable handles to a loaded plugin's components. Each composes
/// [`StableMeta`] (static metadata) and [`StableCancel`] (cancellation) alongside
/// its dispatch surface.
pub type DynProvider = stabby::dynptr!(
    stabby::boxed::Box<dyn StableProvider + StableMeta + StableCancel + Send + Sync>
);
pub type DynManagedDriver = stabby::dynptr!(
    stabby::boxed::Box<dyn StableManagedDriver + StableMeta + StableCancel + Send + Sync>
);
/// A hook handle composes its dispatch surface with [`StableMeta`] (the hook name)
/// only — hooks have no per-request cancellation, so no [`StableCancel`].
pub type DynHook = stabby::dynptr!(stabby::boxed::Box<dyn StableHook + StableMeta + Send + Sync>);

pub type DynExecRunner = stabby::dynptr!(
    stabby::boxed::Box<dyn StableExecRunner + StableMeta + StableCancel + Send + Sync>
);

/// A live session handle. Composes neither [`StableMeta`] nor [`StableCancel`]:
/// a session is not a named component, and neither of its methods is worth
/// cancelling — `prepare` is a fast pure transform on the spawn path and `close`
/// is best-effort. The slow, cancellable call is `open`, which is on the runner.
pub type DynExecSession = stabby::dynptr!(stabby::boxed::Box<dyn StableExecSession + Send + Sync>);

/// Outcome of [`StableExecRunner::open`].
///
/// `info` is a prost `pb::OpenedSessionInfo` — the *only* four things the host
/// consumes about a session, each because a specific host caller needs it:
/// `base_env` so a "not found in PATH" can name which PATH it searched,
/// `max_concurrent` because admission control has to happen before the engine's
/// worker permit is taken, `pty` to gate `--shell`, and the description for
/// `heph inspect` and the in-flight report.
#[stabby::stabby]
pub struct OpenOutcome {
    pub ok: bool,
    /// Failure detail. Empty when `ok`.
    pub message: SString,
    /// Present iff `ok`.
    pub session: stabby::option::Option<DynExecSession>,
    pub info: SVec<u8>,
}

/// A named managed driver in a plugin's component bundle.
#[stabby::stabby]
pub struct NamedDriver {
    pub name: SString,
    pub driver: DynManagedDriver,
}

/// A named exec runner in a plugin's component bundle.
///
/// The name is the runner's own — deliberately NOT a driver's. A plugin can
/// export a `docker` runner with no driver at all, and a runner target built by
/// any driver can name it.
#[stabby::stabby]
pub struct NamedExecRunner {
    pub name: SString,
    pub runner: DynExecRunner,
}

/// A named hook in a plugin's component bundle.
#[stabby::stabby]
pub struct NamedHook {
    pub name: SString,
    pub hook: DynHook,
}

/// What a cdylib's create entry returns: an optional provider + named drivers +
/// named hooks, all as owned ABI-stable handles that the host wraps with
/// [`crate::load_stable`]. A plugin populates only what it exports — a hook-only
/// (or driver-only) bundle leaves `provider` `None`.
///
/// `meta` is reserved, prost-encoded return-side metadata (empty today). It exists
/// so a plugin can later report additive descriptive data (capabilities, abi
/// minor, …) without changing this struct's layout — a layout change would break
/// loading of older plugins. The handle fields must stay stabby (they carry the
/// live native vtables); only the data rides prost.
#[stabby::stabby]
pub struct PluginComponents {
    /// The exported provider's name (empty when `provider` is `None`).
    pub provider_name: SString,
    /// The exported provider, or `None` for a hook-only / driver-only plugin.
    pub provider: stabby::option::Option<DynProvider>,
    pub drivers: SVec<NamedDriver>,
    /// Exec runners this plugin exports. Empty for a plugin that serves no
    /// environments — which is every plugin that existed before this field.
    pub runners: SVec<NamedExecRunner>,
    /// Named build-event hooks the plugin exports. Empty for provider/driver-only
    /// plugins. A hook-only plugin leaves `provider_name` empty (its `provider` is
    /// a no-op the host drops) and carries its hooks here.
    pub hooks: SVec<NamedHook>,
    pub meta: SVec<u8>,
}

/// A host log sink the plugin forwards its `tracing` events through. A loaded
/// cdylib statically links its OWN `tracing`, whose global subscriber is never set
/// — so a plugin's `tracing::*` would be silently dropped. The host hands the
/// plugin this sink (via [`SET_LOG_SINK_SYMBOL`]); the plugin installs a tracing
/// subscriber that calls `log` for every event, and the host re-emits it on its
/// own `tracing`, so plugin logs appear in the host's output. `level` is the
/// `tracing::Level` as `1=ERROR .. 5=TRACE`; `target` is the event's target.
#[stabby::stabby]
pub trait StableLogSink {
    extern "C" fn log(&self, level: u8, target: SString, message: SString);
}

/// An owned, ABI-stable handle to the host's log sink.
pub type DynLogSink = stabby::dynptr!(stabby::boxed::Box<dyn StableLogSink + Send + Sync>);

/// The host's process-supervisor client, handed to a loaded plugin.
///
/// A cdylib statically links its OWN copy of the `proc` crate, so the supervisor
/// tracker the host initialised at startup is invisible to it — a plugin that
/// spawns children (plugin-go: every `go` compile) would register none of them,
/// and a hard kill of the host would orphan them. The host hands the plugin this
/// handle (via [`SET_SUPERVISOR_SYMBOL`]) and the plugin routes its own
/// `register_child` through it, into the host's socket to the sidecar.
///
/// Each method returns an error message, empty on success — small and rare (once
/// per spawned process), so the allocation is noise next to the fork+exec.
#[stabby::stabby]
pub trait StableSupervisor {
    extern "C" fn track(&self, pgid: i32) -> SString;
    extern "C" fn untrack(&self, pgid: i32) -> SString;
    extern "C" fn register_fuse_root(&self, root: SString) -> SString;
}

/// An owned, ABI-stable handle to the host's process supervisor.
pub type DynSupervisor = stabby::dynptr!(stabby::boxed::Box<dyn StableSupervisor + Send + Sync>);

/// The cdylib create-entry symbol name (exported with `#[stabby::export]`,
/// loaded host-side with `get_stabbied`).
pub const CREATE_SYMBOL: &[u8] = b"heph_plugin_create";

/// Optional cdylib symbol: install a host [`DynLogSink`] so the plugin's `tracing`
/// events reach the host. The host calls it right after load if present; a plugin
/// that does not export it simply gets no log forwarding.
pub const SET_LOG_SINK_SYMBOL: &[u8] = b"heph_plugin_set_log_sink";

/// The set-log-sink entry's function-pointer type.
pub type SetLogSinkFn = extern "C" fn(DynLogSink);

/// Optional cdylib symbol: install a host [`DynSupervisor`] so children the plugin
/// spawns are tracked by the host's supervisor sidecar. The host calls it right
/// after load if present; a plugin built against an older SDK does not export it
/// and keeps the previous (unregistered) behaviour.
pub const SET_SUPERVISOR_SYMBOL: &[u8] = b"heph_plugin_set_supervisor";

/// The set-supervisor entry's function-pointer type.
pub type SetSupervisorFn = extern "C" fn(DynSupervisor);

/// The create entry's function-pointer type. The config crosses as prost-encoded
/// `pb::CreateConfig` bytes (not a stabby struct), so adding config fields is an
/// additive change that does not break older plugins.
pub type CreateFn = extern "C" fn(SVec<u8>) -> PluginComponents;
