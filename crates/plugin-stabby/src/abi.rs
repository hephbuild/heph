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

/// The cold provider surface, called by the host. Direct stabby vtable dispatch —
/// no async mux, no channels, no duplex (that machinery was the entire cold-path
/// cost; see ai-docs/PERFORMANCE.md). Requests/responses cross as prost-encoded
/// `pb::Frame` bytes (cheap, low-volume, lenient via protobuf); the host decodes
/// the response `Body`. `get` additionally takes the native [`DynExecutor`] so the
/// plugin's hot callbacks during resolution are direct calls.
///
/// Stream methods (`list`, `list_packages`) return all items length-delimited:
/// a sequence of prost length-delimited `pb::Frame`s (StreamItem… then StreamEnd).
#[stabby::stabby]
pub trait StableProvider {
    /// The provider's registered name.
    extern "C" fn config(&self) -> SString;
    extern "C" fn list<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>>;
    extern "C" fn list_packages<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>>;
    extern "C" fn get<'a>(&'a self, req: SVec<u8>, exec: DynExecutor) -> DynFuture<'a, SVec<u8>>;
    extern "C" fn probe<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>>;
}

/// The cold managed-driver surface (same transport contract as [`StableProvider`]).
#[stabby::stabby]
pub trait StableManagedDriver {
    extern "C" fn config(&self) -> SString;
    extern "C" fn parse<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>>;
    extern "C" fn apply_transitive<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>>;
    /// `shell` selects `run_shell` over `run`.
    extern "C" fn run<'a>(&'a self, req: SVec<u8>, shell: bool) -> DynFuture<'a, SVec<u8>>;
}

/// Owned ABI-stable handles to a loaded plugin's components.
pub type DynProvider = stabby::dynptr!(stabby::boxed::Box<dyn StableProvider + Send + Sync>);
pub type DynManagedDriver =
    stabby::dynptr!(stabby::boxed::Box<dyn StableManagedDriver + Send + Sync>);

/// Config handed to a cdylib's create entry: the workspace root plus the
/// plugin's `options:` map (from config yaml), encoded as a `plugin-abi`
/// `pb::Value` map (prost bytes). The plugin decodes the options and instantiates
/// its provider + drivers from them.
#[stabby::stabby]
pub struct CreateConfig {
    pub root: SString,
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
