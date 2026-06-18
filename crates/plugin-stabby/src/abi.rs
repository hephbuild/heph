//! The stable wire-ABI surface: stabby-stable types + traits that cross the
//! cdylib boundary. Pure stabby — no domain deps — so both host and guest depend
//! on the exact same layout. Conversions to/from the engine's native types live
//! in [`crate::host`] / [`crate::guest`].

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

/// A streaming byte sink across the seam — the mirror of [`StableRead`]. The host
/// owns the sink (e.g. a driver's stdout/stderr); the guest pushes chunks into it.
/// `write_chunk` returns the bytes accepted; `0` means the sink is closed and the
/// guest should stop writing.
#[stabby::stabby]
pub trait StableWrite {
    extern "C" fn write_chunk(&self, buf: SVec<u8>) -> u64;
}

/// An owned, ABI-stable streaming sink handle. Not `Send` for the same reason as
/// [`DynRead`]: opened and consumed on one thread.
pub type DynWrite = stabby::dynptr!(stabby::boxed::Box<dyn StableWrite + Send + Sync>);

/// The live stdio streams handed to a driver `run` as native streaming handles
/// (same family as artifact streaming, NOT prost bytes — live IO can't be unary).
/// Each is optional: `None` means the host did not wire that stream. Frozen at the
/// three subprocess fds, so adding live stdin/stdout/stderr streaming later just
/// populates these handles — an additive change needing no ABI bump.
#[stabby::stabby]
pub struct RunIo {
    /// host -> driver (process stdin).
    pub stdin: stabby::option::Option<DynRead>,
    /// driver -> host (process stdout).
    pub stdout: stabby::option::Option<DynWrite>,
    /// driver -> host (process stderr).
    pub stderr: stabby::option::Option<DynWrite>,
}

/// The cold provider surface as a FROZEN generic dispatch. The `method` id
/// (`pb::ProviderMethod`) selects an RPC; request/response cross as prost-encoded
/// `pb::Frame` bytes (cheap, low-volume, lenient via protobuf). Adding an RPC is a
/// new method id + a new guest match arm — the vtable is UNTOUCHED, so an older
/// plugin and a newer host still load (stabby's type report is unchanged) and the
/// old plugin answers an unknown id with `Error{Unimplemented}`. THIS is what
/// makes the cold surface evolvable without an ABI break (see ABI_VERSIONING.md).
///
/// Three slots cover the call shapes; native handles cannot ride prost bytes, so a
/// method that carries one gets its own (also frozen) slot:
/// - [`invoke`](StableProvider::invoke) — plain prost in/out.
/// - [`invoke_exec`](StableProvider::invoke_exec) — also passes the native
///   [`DynExecutor`] (`get`, whose resolution makes hot callbacks).
/// - [`invoke_registry`](StableProvider::invoke_registry) — also passes the native
///   [`DynFunctionRegistry`] (`set_function_registry`).
///
/// Replies follow the prior contract: unary methods return one prost `pb::Frame`;
/// stream methods (`list`, `list_packages`) return length-delimited `pb::Frame`s
/// (StreamItem… then StreamEnd).
#[stabby::stabby]
pub trait StableProvider {
    extern "C" fn invoke<'a>(&'a self, method: u32, req: SVec<u8>) -> DynFuture<'a, SVec<u8>>;
    extern "C" fn invoke_exec<'a>(
        &'a self,
        method: u32,
        req: SVec<u8>,
        exec: DynExecutor,
    ) -> DynFuture<'a, SVec<u8>>;
    extern "C" fn invoke_registry(&self, method: u32, req: SVec<u8>, reg: DynFunctionRegistry);
}

/// The cold managed-driver surface, same frozen-dispatch contract as
/// [`StableProvider`]. `method` is a `pb::DriverMethod`. `run` rides
/// [`invoke_io`](StableManagedDriver::invoke_io), which additionally carries the
/// native [`RunIo`] stdio handles.
#[stabby::stabby]
pub trait StableManagedDriver {
    extern "C" fn invoke<'a>(&'a self, method: u32, req: SVec<u8>) -> DynFuture<'a, SVec<u8>>;
    extern "C" fn invoke_io<'a>(
        &'a self,
        method: u32,
        req: SVec<u8>,
        io: RunIo,
    ) -> DynFuture<'a, SVec<u8>>;
}

/// Owned ABI-stable handles to a loaded plugin's components. Each composes
/// [`StableMeta`] (static metadata) alongside its dispatch surface.
pub type DynProvider =
    stabby::dynptr!(stabby::boxed::Box<dyn StableProvider + StableMeta + Send + Sync>);
pub type DynManagedDriver =
    stabby::dynptr!(stabby::boxed::Box<dyn StableManagedDriver + StableMeta + Send + Sync>);

/// Config handed to a cdylib's create entry: the workspace root, the engine's
/// home dir (where a plugin should keep its state/scratch — e.g. `<root>/.heph3`,
/// configurable; never hardcode it), and the plugin's `options:` map (from config
/// yaml) encoded as a `plugin-abi` `pb::Value` map (prost bytes). The plugin
/// decodes the options and instantiates its provider + drivers from them.
#[stabby::stabby]
pub struct CreateConfig {
    pub root: SString,
    pub home: SString,
    pub options: SVec<u8>,
}

/// A named managed driver in a plugin's component bundle.
#[stabby::stabby]
pub struct NamedDriver {
    pub name: SString,
    pub driver: DynManagedDriver,
}

/// What a cdylib's create entry returns: a provider + named drivers, all as owned
/// ABI-stable handles that the host wraps with [`crate::load_stable`]. (plugin-go
/// always exports a provider; driver-only bundles can carry an empty name.)
#[stabby::stabby]
pub struct PluginComponents {
    pub provider_name: SString,
    pub provider: DynProvider,
    pub drivers: SVec<NamedDriver>,
}

/// The cdylib create-entry symbol name (exported with `#[stabby::export]`,
/// loaded host-side with `get_stabbied`).
pub const CREATE_SYMBOL: &[u8] = b"heph_plugin_create";

/// The create entry's function-pointer type.
pub type CreateFn = extern "C" fn(CreateConfig) -> PluginComponents;
