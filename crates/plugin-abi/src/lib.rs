//! Wire ABI types for heph external plugins.
//!
//! This crate is the raw wire layer the plugin transport shares. Plugin authors
//! do NOT use it directly — they use the SDK (`plugin-sdk`), which sits on top.
//!
//! - [`pb`] re-exports the prost-generated message types (source of truth,
//!   generated from `proto/plugin/v1/*.proto` via `buf`).
//! - [`convert`] holds the conversions between the [`pb`] wire types and the
//!   in-process `hplugin`/`hmodel`/`hcore` types.
//!
//! The cold, low-volume Provider/Driver methods cross the cdylib boundary as
//! prost-encoded [`pb`] bytes; the hot `ProviderExecutor` callbacks cross as
//! native stabby vtable calls (see `plugin-stabby`).

/// The prost-generated wire message types (`heph.plugin.v1`).
pub use hproto_gen::heph::plugin::v1 as pb;

/// ABI semantic version. Not read at runtime — no handshake or negotiation
/// happens; a mismatched host/plugin pair just mismatches at `dlopen` load via
/// stabby's structural `get_stabbied` check and aborts. This constant is
/// bookkeeping: `scripts/abi-check.sh` fails CI if the ABI surface changed
/// without a bump, so the version history documents *why* a break happened.
///
/// 0.6.0: `PluginComponents` gained a `runners` field, and `StableExecRunner` /
/// `DynExecRunner` / `NamedExecRunner` / `StableExecSession` / `DynExecSession` /
/// `OpenOutcome` join the frozen surface — the exec-runner component kind, so a
/// plugin can serve the environment a runner target describes and thereby decide
/// how every process under it starts.
///
/// A layout change to the create-entry struct, so a hard break: every plugin
/// must be rebuilt against this ABI, exactly as 0.3.0's `hooks` was. Taken
/// rather than bolting the methods onto `StableManagedDriver` because a runner
/// is not a driver — that shape forced every runner-only plugin to stub
/// `parse`/`apply_transitive`/`run`, made a runner-only plugin impossible
/// without a dummy driver, and tied a runner's registered name to a driver it
/// does not have.
///
/// `StableManagedDriver::invoke_bidi` gained a `DynExecService`, and
/// `StableExecService` / `DynExecService` join the frozen surface: a plugin
/// driver no longer creates its target's processes, it asks the host, which
/// holds the live session. A draft carried the session's *environment* to the
/// guest instead and had it rebuild an approximation — exact for an
/// environment-shaped session, silently wrong for anything that also rewrites
/// the command. `ManagedRunRequest.runner_opaque`, the flag that told a guest to
/// refuse what it could not rebuild, is reserved: nothing rebuilds a session any
/// more.
///
/// `StableExecRunner::open` returns a live `DynExecSession` rather than a
/// session id, which is why there is no `close_session(id)` here and no
/// host-side session registry. A cdylib is `dlopen`ed into this process, so a
/// trait object crosses as a fat pointer and stays live on the plugin's side —
/// the same move `ResultOutcome` already makes with `DynArtifact`. A draft of
/// this version assumed otherwise and defined an id-keyed
/// `open_session`/`prepare_spec`/`close_session` protocol; it was redefined in
/// place before release (pre-1.0, `ABI-BREAK-ACK:`) because the ids bought
/// nothing and pushed the runner's own bookkeeping onto the host.
///
/// 0.5.0: `StableExecutor` gained a `states` method (fetch a package's provider
/// states for cross-subtree config resolution — the go variant `vp` lookup). A
/// new method on a `#[stabby::stabby]` vtable trait changes its type-report, so
/// this is a hard break: every plugin must be rebuilt against this ABI.
///
/// 0.4.0: new optional `heph_plugin_set_supervisor` entry (`StableSupervisor` /
/// `DynSupervisor`) so a plugin's children register with the host's process
/// supervisor. Additive: no existing vtable or struct changed, and the host
/// tolerates a plugin that does not export the symbol — a minor bump signalling
/// the new capability, not a break.
///
/// 0.3.0: `PluginComponents` gained a `hooks` field (a layout change to the
/// create-entry struct) for the Hook plugin kind — a hard break, so every plugin
/// must be rebuilt against this ABI.
pub const ABI_SEMVER: &str = "0.6.0";

#[cfg(feature = "convert")]
pub mod convert;
