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
/// `DynExecRunner` / `NamedExecRunner` join the frozen surface — the exec-runner
/// component kind, so a plugin can serve the environment a runner target
/// describes and thereby own process creation for every target under it.
///
/// A layout change to the create-entry struct, so a hard break: every plugin
/// must be rebuilt against this ABI, exactly as 0.3.0's `hooks` was. Taken
/// rather than bolting the methods onto `StableManagedDriver` because a runner
/// is not a driver — that shape forced every runner-only plugin to stub
/// `parse`/`apply_transitive`/`run`, made a runner-only plugin impossible
/// without a dummy driver, and tied a runner's registered name to a driver it
/// does not have.
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
