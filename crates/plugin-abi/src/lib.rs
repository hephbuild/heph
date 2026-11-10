//! Wire ABI types for heph external plugins.
//!
//! This crate is the raw wire layer the plugin transport shares. Plugin authors
//! do NOT use it directly — they use the SDK (`plugin-sdk`), which sits on top.
//!
//! - [`pb`] re-exports the buffa-generated message types (source of truth,
//!   generated from `proto/plugin/v1/*.proto` via `buf`).
//! - [`convert`] holds the conversions between the [`pb`] wire types and the
//!   in-process `hplugin`/`hmodel`/`hcore` types, and the presence rules every
//!   decoder follows (see its module docs).
//!
//! The cold, low-volume Provider/Driver methods cross the cdylib boundary as
//! protobuf-encoded [`pb`] bytes; the hot `ProviderExecutor` callbacks cross as
//! native stabby vtable calls (see `plugin-stabby`).

/// The buffa-generated wire message types (`heph.plugin.v1`). Every singular
/// field has explicit presence (edition 2024): `Option<T>` for scalars, strings,
/// bytes and enums, `MessageField<T>` for messages.
pub use hproto_gen::heph::plugin::v1 as pb;

/// ABI semantic version. Since 0.17.0 it is read at load: a plugin exports it
/// (`heph_plugin_abi_semver`, a plain C symbol, so it reads the same whatever
/// stabby layout the plugin was built with), and the host refuses a plugin
/// that does not match `^`[`MIN_PLUGIN_ABI`] before calling anything else in
/// it. stabby's structural `get_stabbied` check still guards the frozen
/// surface on top of that. `scripts/abi-check.sh` fails CI if the ABI surface
/// changed without a bump, so the version history documents *why* a break
/// happened.
///
/// The convention, from 0.17.0 on (pre-1.0): an **additive** change bumps the
/// **patch**; a **break** bumps the **minor** and raises `MIN_PLUGIN_ABI` to
/// the new `x.y.0`. The 0.14.0 and 0.15.0 entries below predate it — they were
/// additive minors, and there was no version check to read them.
///
/// 0.21.0 (a break): outputs from failed targets. `CODEGEN_MODE_IN_PLACE` (3)
/// changes meaning: it now also asks the host to write back what a run that
/// exited with a code from 1 to 127 left behind (`InPlace { write_on_failure:
/// true }`), and `CODEGEN_MODE_IN_PLACE_ON_SUCCESS` (4, appended) is what 3
/// meant before. `TargetDef.failure_outputs` (14) declares what a failed run
/// keeps for its diagnosis, and `RunFailure.write_back` (5) carries a failed
/// `in_place` run's fixes beside, never inside, its `artifacts`; both decode
/// empty when absent. An unknown codegen value stays a decode error. A 0.20
/// plugin's `IN_PLACE` never asked for a write-back on failure, so
/// `MIN_PLUGIN_ABI` rises to 0.21.0 and every plugin must be rebuilt. It is a
/// Rust source break too: `CodegenMode::InPlace` became the struct variant
/// `InPlace { write_on_failure }` and `CodegenMode` no longer derives
/// `SpecEnum`, so a plugin that matched `InPlace` or parsed the mode with
/// `SpecEnum` must change its code, not just rebuild.
///
/// Also in 0.21.0, a timed-out run says so across the seam:
/// `StableCancel::cancel_timed_out(cancel_key)`, appended after `cancel`, is
/// what the host sends instead of `cancel` for a run its `timeout` stopped,
/// and the plugin cancels that run's token as timed out
/// (`Cancellable::timed_out`). An id that is not a cancel key is a plain
/// cancel. A vtable change the 0.21.0 rebuild covers; no message changes.
/// `Cancellable` gains the required `timed_out()`, a source break for a
/// plugin that implements it.
///
/// 0.20.0 (a break): `ParamType.literal` (6, `string`, in the `kind` oneof) —
/// exactly one string (`ParamType::Literal`); a closed set, e.g. a `workdir`
/// or `format` field, is a `union` of them, so a cdylib's schema reaches the
/// LSP as `"a" | "b"` rather than `string`. A peer that predates the member
/// sees no `kind` and fails the whole schema's decode (`missing field
/// ParamType.kind`), so this is not additive. Types cross one way only, plugin
/// to host (driver and provider-state schemas, function signatures; the host
/// never sends one), so an old plugin on this host would still work — it
/// never sends a literal — but a new plugin on a 0.19 host would have every
/// schema carrying a literal refused, and a patch bump would let the 0.19 host
/// load it. `MIN_PLUGIN_ABI` rises to 0.20.0 as the convention requires (the
/// lane is one minor), and every plugin must be rebuilt.
///
/// 0.19.1 (additive, a patch): target timeouts. `TargetDef.timeout_ms` (13,
/// `uint64`) — how long the run may take; the host enforces it around the
/// driver's `run` (cancel, a grace period, then abandon). Absent is no limit; a
/// present 0 is refused. Never hashed.
///
/// A timeout cancels one run, but a cancel across the seam named the whole
/// request (`StableCancel::cancel(request_id)`, which every target of the
/// request shares), so it also gets `ManagedRunRequest.cancel_key` (16,
/// `string`): one key per run call, minted by the host and starting with
/// [`CANCEL_KEY_PREFIX`]. The plugin registers the run's token under it before
/// its `run` dispatch returns; the host cancels a run by its key alone, so a
/// timeout stops that run and no sibling. A request cancel still reaches every
/// run, since each run's own token is cancelled with the request.
///
/// The skew, both ways: a 0.19.0 plugin is sent no key and its runs are
/// cancelled by `request_id` as before; it cannot declare a timeout (its
/// `TargetDef` has no such field), so its runs are cancelled only with their
/// request, and the request-wide cancel is right. A 0.19.1 plugin on a 0.19.0
/// host gets no key and falls back to `request_id`, and its `timeout_ms` is
/// dropped at decode, so its targets run unbounded: accepted, by decision —
/// there is no host capability to refuse it on. No plugin must be rebuilt.
///
/// 0.19.0 (a break): the exec-runner seam says whether the target runs on a
/// terminal. `PrepareRequest` and `RunnerRequest` (`hexecrunner::wire`) end in
/// a `tty` byte, read into `RunnerCtx::tty`, so a runner that moves the target
/// behind another transport can hand a terminal through — the oci runner
/// enters a `--shell` with `docker exec -it`. The frames are hand-encoded
/// bytes, not prost, so there is no absent-field default: a 0.18 plugin's
/// runner would ignore this host's trailing byte and never see the flag (the
/// terminal silently missing, the bug this fixes), and a 0.18 plugin driver's
/// frame is truncated to this host. `MIN_PLUGIN_ABI` rises to 0.19.0 and every
/// plugin must be rebuilt.
///
/// 0.18.0 (a break): a run's terminal failure is one `RunFailure` in
/// `RunOutFrame`'s oneof (7) — message, exit status and the failure's
/// `artifacts` (4) together — replacing `string error` (4) and the out-of-oneof
/// `ExitStatus exit` (5), both reserved. `artifacts` carries what the failed
/// run left behind (normally its `log.txt`, type LOG), with or without an exit,
/// so a cdylib driver's failure renders its log tail the way an in-process
/// driver's does; the host decodes them leniently, dropping one that does not
/// decode and keeping the message and exit. A 0.17 peer would read a failure as
/// a stream with no terminal frame, so `MIN_PLUGIN_ABI` rises to 0.18.0 and
/// every plugin must be rebuilt.
///
/// 0.17.4 (additive, a patch): target resources. `TargetDef.resources` (12,
/// `map<string, string>`) — the pools a def reserves while it runs, each amount
/// as the target wrote it (`"0.5"`, `"2G"`), which the host parses in the
/// pool's unit at link; only declared pools present, a driver's advertised
/// cost among them. `ManagedRunRequest.allotment` (15, `map<string, string>`) — every pool the
/// host granted the run (`cpu` always), each amount rendered by its pool's unit,
/// which is what the driver substitutes for `${resource:<pool>}`; heph exports
/// no environment for it. The skew, both ways: a 0.17.3 plugin sends no
/// `resources`, so its defs decode empty and reserve one cpu, and ignores
/// `allotment`; a 0.17.4 plugin on a 0.17.3 host has its `resources` dropped
/// at decode (every def costs one cpu there, a custom-pool exclusion is lost)
/// and is sent an empty `allotment`, which means "told nothing" — a
/// `${resource:}` reference then fails its run. Neither is hashed. No plugin must be rebuilt; the SDK's Rust
/// source does change (`TargetDef` gains a field and is not `#[non_exhaustive]`,
/// so a struct literal without `..Default::default()` stops compiling).
///
/// 0.17.3 (additive, a patch): `EntrypointWorkdir.TREE_PACKAGE` (4), the
/// target's package dir in the real tree; `ROOT` (2) is renamed `TREE_ROOT` at
/// the same number. An older host refuses a def carrying the new value at
/// decode rather than running it somewhere else.
///
/// 0.17.2 (additive, a patch): a run on the host's PTY. `RUN_STDIO_PTY`,
/// `RunOptions.pty_slave_fd`, `Entrypoint.offers_pty`, and the `stopped` and
/// `cont` run frames. A driver offers it by setting the bit, so an old plugin is
/// never sent a slave.
///
/// 0.17.1 (additive, a patch): entrypoints. `TargetDef.entrypoint` (11) — a
/// driver declares a target invoked rather than built; `ManagedRunRequest.options`
/// (14) — the run's caller-side cwd, base env, args and stdio, sent only for a
/// def that declared one; `RunInFrame.signal` (4) — signals the host relays,
/// which makes the request stream live until the run's terminal frame;
/// `RunOutFrame.exit` (5), outside the oneof — the structured status beside an
/// `error`; and `CreateConfig.host_capabilities` (5), whose `entrypoints` bit
/// tells a plugin the host honours the attribute. Every field is absent from and
/// ignored by a 0.17.0 peer: an old plugin never declares one so is never sent
/// options, and a new plugin on an old host sees no capability bit and refuses
/// to declare one. No plugin must be rebuilt; the SDK's Rust source does change
/// (`RunRequest` is `#[non_exhaustive]`, built with `RunRequest::new`).
///
/// 0.17.0: the wire stays protobuf — same messages, same field numbers and
/// types — but the protos are edition 2024 with explicit presence on every
/// singular field, and the codegen is buffa instead of prost. A field the
/// sender did not set is now absent rather than its zero value, and the
/// decoders tell the two apart: a field the receiver cannot do without (an
/// addr's package and name, a spec's driver, a dep's `hash`, a def's cache
/// config, a request id, a create config's root) fails the decode with
/// `missing field Message.field`, and the old "zero means unknown" sentinels
/// (an empty listed driver, an empty declared driver, an empty hashout, an
/// unspecified log level) are spelled as absence. See `convert`'s module docs.
///
/// A present-but-empty `ListedFacts.driver` is the one message a 0.16 peer
/// could validly send that this host now rejects (it used to mean "unknown",
/// which is absence now).
///
/// A hard break, though stabby cannot see it: the frozen surface did not move,
/// and a 0.16 plugin's prost encoder omits every zero-valued field, which this
/// host would read as missing required fields. So 0.17.0 also adds the
/// load-time version check: the plugin exports `heph_plugin_abi_semver`, and
/// the host refuses a plugin that does not (it predates 0.17.0) or that reports
/// a version not matching `^`[`MIN_PLUGIN_ABI`] (`>=0.17.0, <0.18.0`) — once, at load, naming
/// the plugin and both versions — instead of failing each call with a bare
/// `missing field …`. Rebuild every plugin against this ABI, as for 0.16.0.
///
/// 0.16.0: functions belong to plugins, not providers — a hard break, every
/// plugin must be rebuilt. `PluginComponents` loses `provider_name` (the
/// manifest names the plugin) and gains `functions` (each one's metadata as
/// prost `FunctionDef` bytes, decoded once at load; bad metadata fails the load)
/// plus one `DynPluginFunctions` handle per plugin, dispatched by a new
/// `FunctionMethod` id. `heph_plugin_create` takes the engine's
/// `DynFunctionRegistry` per instance, so two engines in one process never see
/// each other's functions. `StableProvider::invoke_registry` is gone, and
/// `ProviderMethod` 5, 6 and 8 (`FUNCTIONS`, `CALL_FUNCTION`,
/// `SET_FUNCTION_REGISTRY`) are reserved. `CallRegisteredRequest.provider` is
/// renamed `plugin` (same number). A 0.15 plugin fails stabby's structural
/// check on the create entry, which the host reports as a plugin built against
/// a different heph ABI.
///
/// 0.15.0: a provider function can declare targets and provider-state, not just
/// return a value. `CallFunctionResponse` gained `declared_targets` /
/// `declared_states` (2, 3) and both call requests gained
/// `accepts_declarations`, which the *caller* sets to say it carries them back
/// to the package being evaluated. Additive wire fields, no frozen-surface
/// change — minor because the capability is the caller's to offer, and a callee
/// must reach it before declaring.
///
/// Skew fails closed, and on the flag rather than on this version: an older
/// caller sends no `accepts_declarations`, which decodes to `false`, and a
/// declaring function then fails the call rather than answering with its value
/// alone — prost would skip the declaration fields, so the caller would
/// otherwise build something other than what the author declared and hash it as
/// if intended. An older callee never declares, so a new caller's flag is
/// simply unread.
///
/// 0.14.0: new optional `heph_plugin_set_log_filter` entry, taking prost
/// `pb::LogFilter` bytes: the host's log filter in a shape any language can apply
/// (an ordered level enum, opaque target strings, longest prefix wins), so a
/// plugin drops events the host would discard instead of forwarding them all.
/// Additive like 0.4.0's supervisor entry: the host tolerates a plugin without the
/// symbol, and that plugin forwards everything, which the host still filters.
///
/// 0.13.0: `ListResponse` carries `ListedFacts facts = 4` — per entry, the
/// labels, driver and `has_codegen` the provider knows without `get`, each
/// field absent (unknown) or exact — and 0.11.0's `labels`/`labels_known` are
/// removed (`reserved 2, 3`; 0.11.0 was never released). `Matcher` gained a
/// `driver` arm (9), and the host now refuses a `Matcher` with no arm it knows
/// instead of reading it as an empty `Or`.
///
/// Both directions read as unknown, never as a prune: an old plugin sends no
/// `facts`, and a pre-0.13 host reads absent `labels_known` as unknown and
/// ignores field 4. A malformed `ListedFacts` fails that `ListResponse`, and so
/// the provider's `list`, loudly. Minor: no plugin must be rebuilt, but one must
/// be for its listings to decide queries. Caveat: a new plugin that sends a
/// `driver` matcher through the query callback to a pre-0.13 host is misread
/// there as an empty `Or`.
///
/// 0.12.0: `ManagedRunInput` gained `string hashout` — the input artifact's
/// hashout, the value the consumer's hashin folded in. A guest driver's
/// `content.hashout()` returned `""` before, so a driver deriving a value from
/// it (`oci_push`'s `${image_hashout}`) worked in-process and not in the cdylib.
/// Additive and cold-path (a prost wire field, no vtable change), so an old
/// plugin still loads.
///
/// Skew fails closed in both directions: an old host sends no hashout, and an
/// old plugin ignores it. A driver that reads it must refuse an empty one, as
/// `oci_push` does. Minor, therefore.
///
/// 0.11.0: `ListResponse` gained `repeated string labels` and `bool
/// labels_known` — the labels `get` will give the target, so a label selector
/// decides membership from the listing instead of resolving every candidate's
/// spec. Additive and cold-path (prost wire fields, no vtable change), so an
/// old plugin still loads.
///
/// prost decodes an old plugin's absent `labels_known` as `false`, which the
/// host reads as "unknown" and resolves the spec exactly as before — never as
/// "this target has no labels", which would silently drop it from every label
/// selection. Minor, therefore: no plugin *must* be rebuilt, but one must be
/// to make label selection fast.
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
pub const ABI_SEMVER: &str = "0.21.0";

/// The start of the ABI lane this host loads: a plugin is accepted iff its
/// version matches the caret requirement `^MIN_PLUGIN_ABI` — pre-1.0, one
/// minor (`^0.21.0` = `>=0.21.0, <0.22.0`). A patch is additive, so host and
/// plugin may differ in patch either way; a plugin from a later minor is a
/// break the host cannot know and is refused.
///
/// On every break, bump [`ABI_SEMVER`]'s minor and raise this to the new
/// `x.y.0` — a wire-only break such as 0.17.0's above all, since stabby's
/// structural check passes it and nothing but this check refuses the old
/// plugin. A test holds `ABI_SEMVER` inside `^MIN_PLUGIN_ABI`, so bumping one
/// without the other fails.
pub const MIN_PLUGIN_ABI: &str = "0.21.0";

/// [`ABI_SEMVER`], NUL-terminated: what a plugin's `heph_plugin_abi_semver`
/// entry returns a pointer to. Bumped with it; a test holds the two equal.
pub const ABI_SEMVER_C: &std::ffi::CStr = c"0.21.0";

/// The first plugin ABI that reads `ManagedRunRequest.cancel_key`. The host
/// sends a key, and cancels a run by it, only to a plugin at this version or
/// later; an older one is cancelled by `request_id`.
pub const CANCEL_KEY_SINCE: &str = "0.19.1";

/// The prefix of every `ManagedRunRequest.cancel_key`. A plugin sees cancels
/// for request ids and for run keys through the one `cancel` entry; one with
/// this prefix that names no run in flight is for a run that already ended,
/// and is dropped rather than parked as a request cancel for a later call.
/// Engine request ids (`req-{n}`) never start with it.
pub const CANCEL_KEY_PREFIX: &str = "heph-run-cancel:";

/// Decode a wire message from a trusted peer.
///
/// buffa's default decode budgets element memory at 32 MiB, charged at the
/// in-memory size of every repeated element; a legitimate `TargetDef` with
/// ~120k inputs (under 2 MB on the wire) exceeds it. Both sides of the seam
/// are heph's own code in one process, so the budget is lifted; the recursion
/// limit, which guards the stack rather than the heap, is kept. Every decode
/// across the seam goes through here.
pub fn decode<M: buffa::Message>(bytes: &[u8]) -> Result<M, buffa::DecodeError> {
    buffa::DecodeOptions::new()
        .with_element_memory_limit(usize::MAX)
        .decode_from_slice(bytes)
}

#[cfg(feature = "convert")]
pub mod convert;

#[cfg(test)]
mod tests {
    #[test]
    fn the_c_version_is_the_version() {
        assert_eq!(super::ABI_SEMVER_C.to_str(), Ok(super::ABI_SEMVER));
    }
}
