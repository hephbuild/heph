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
/// 0.10.0: deferred values. `RunRequest`/`ManagedRunRequest` gained
/// `map<string, string> deferred` — option values the host resolved from another
/// target's output — and `Schema` gained `bool accepts_deferred`. Additive and
/// cold-path on both counts.
///
/// The bump exists for the *schema* field rather than the map. A plugin built
/// before this feature carries an old `String` decoder that rejects nothing: it
/// would decode `"${read://infra:role-arn}"` as a literal, synthesize no edge,
/// never build the producer, and run the target with the reference **text** as
/// the value — exiting 0 if the tool tolerates it. Not cache poisoning (the two
/// plugins compute different `hashin`s, so neither serves the other's artifact),
/// but a silent wrong value regardless, and host/plugin skew is reachable today
/// since the cdylibs ship as separate artifacts pinned independently of the
/// binary.
///
/// prost decodes an absent `accepts_deferred` as `false`, so an old plugin's
/// schema makes the host refuse every reference in that driver's config at parse
/// — loudly, naming the driver — rather than letting one through. Minor,
/// therefore: no plugin *must* be rebuilt, but one must be to accept a reference.
///
/// 0.9.0: `RunRequest`/`ManagedRunRequest` gained `repeated CredentialMount
/// credentials` — identities the host resolved, acquired and materialized for the
/// run, which the driver applies as runtime environment, a PATH prefix, and a
/// redaction set for its output tee. Additive and cold-path (a prost wire field,
/// not a vtable change), so an old plugin still loads.
///
/// Minor, but with a sharper consequence than 0.8.0's if a plugin is *not*
/// rebuilt: an old driver drops the mounts and runs its target with no identity,
/// which either fails much later with a message from someone else's SDK or —
/// worse — succeeds against whatever ambient identity the host happened to have.
/// Nothing is silently wrong in the *cache*, because a credential contributes
/// nothing to any hash in either direction; what is lost is the identity itself.
/// Two things bound the exposure: a reference can only enter a definition through
/// the plugin's own parser, and a plugin old enough to drop the field rejects the
/// `credentials` attribute outright. Decoding on the plugin side is fallible and
/// never lossy for the same reason — unlike a scratch mount, a credential that
/// fails to decode is an error rather than a dropped mount.
///
/// 0.8.0: `RunRequest`/`ManagedRunRequest` gained `repeated ScratchMount
/// scratch` — persistent cache directories the host resolved, locked and created
/// for the run, which the driver symlinks into the sandbox and announces through
/// an env var. Additive and cold-path: it is a prost wire field, not a vtable
/// change, so an old plugin decodes a new host's request fine and simply ignores
/// the mounts. Its targets then run without a scratch, which costs a cold cache
/// and never a wrong build — the lock is keyed on a declaration the old plugin
/// cannot see, so there is no shared directory for it to race either. Minor,
/// therefore, not major: no plugin *must* be rebuilt, but one must be to mount a
/// scratch.
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
pub const ABI_SEMVER: &str = "0.10.0";

#[cfg(feature = "convert")]
pub mod convert;
