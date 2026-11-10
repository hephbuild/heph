# Plugin ABI versioning

The in-process stable-ABI plugin transport (`plugin-stabby`) crosses the cdylib
boundary as native stabby vtables (hot path) + protobuf bytes (cold path, buffa
codegen, edition 2024 with explicit presence on every field). Host and
plugin are compiled separately and linked at load time. Two gates run at load,
in order:

1. **The version check** (since 0.17.0; see the convention below). The plugin exports
   `heph_plugin_abi_semver` — a plain C function returning its `ABI_SEMVER` as a
   `'static` NUL-terminated string, deliberately *not* a stabby export so it
   reads the same whatever layout either side has. The host reads it before
   calling anything else in the plugin and accepts exactly the versions
   matching the caret requirement `^MIN_PLUGIN_ABI` (`crates/plugin-abi/src/lib.rs`;
   pre-1.0, `^0.21.0` = `>=0.21.0, <0.22.0`), compared as semver. A plugin
   without the symbol predates 0.17.0 and is refused the same way. A plugin
   from a **later minor** is unsupported and refused: the host cannot know
   what that break changed. A later *patch* loads.
2. **stabby's `get_stabbied`**: it verifies the plugin's type report (every type
   reachable from `CreateFn`) against the host's and **hard-fails on any
   difference**.

## The convention: patch = additive, minor = break (pre-1.0)

From 0.17.0 on:

- An **additive** change — one every older and newer peer in the lane still
  reads correctly — bumps the **patch**. Host and plugin may then differ in
  patch either way.
- A **break** bumps the **minor** *and* raises `MIN_PLUGIN_ABI` to the new
  `x.y.0`, which takes every plugin from the old minor out of the lane. A unit
  test (`the_host_abi_is_in_its_own_lane`) holds `ABI_SEMVER` inside
  `^MIN_PLUGIN_ABI`, so bumping one without the other fails.

The 0.14.0 and 0.15.0 history entries (additive changes bumped as minors)
predate the convention, and the version check with it.

A **wire-only** break done in place cannot use the `ABI-BREAK-ACK` escape hatch
below: nothing would detect it — stabby's check passes, and without a minor bump
and a raised `MIN_PLUGIN_ABI` the version check passes too, so the old plugin
loads and fails (or misreads) call by call.

So the frozen stabby surface in `crates/plugin-stabby/src/abi.rs` must not drift
without `ABI_SEMVER` moving, and a break the structural check cannot see — a
**wire-only** break, like 0.17.0's — must also raise `MIN_PLUGIN_ABI` to the new
`x.y.0`, or the version check lets the old plugin through to fail call by call.

`scripts/abi-check.sh` (the `abi` job in `.github/workflows/heph.yml`, run on every
PR) enforces the first half mechanically: if the frozen surface changes without
`ABI_SEMVER` moving, the check fails. A `proto/plugin/v1/` edit only warns,
pointing here — whether it is additive or a break is the author's call.

## The dispatch model — most growth is additive, no bump

The cold surface is a **frozen generic dispatch**, not one vtable slot per RPC. A
method is a `pb::ProviderMethod` / `pb::DriverMethod` id carried over the fixed
`invoke*` slots; payloads are protobuf bytes. So **adding an RPC does NOT touch the
vtable**:

- New RPC = a new `ProviderMethod`/`DriverMethod` enum value + a new guest `match`
  arm. The type report is unchanged, so an older plugin still loads; it answers an
  unknown id with `Error{Unimplemented}` and the host falls back.
- New cold-path wire field, new `RunInFrame`/`RunOutFrame` oneof variant, new
  `pb::CreateConfig` field — all additive (protobuf decoders skip unknown
  fields), **provided the new field's absence reads as the safe default**. Every
  field has explicit presence, so an older peer's message simply lacks it: a
  field the receiver decodes as *required* (`missing field …` on `None`, see
  `plugin-abi`'s `convert` docs) fails every call from an older peer, and is a
  break, not an addition.

**None of these need a break** and none touch `abi.rs`. This additive lane is the
whole point of the dispatch collapse — grow capability without breaking old
plugins. Bump the **patch** to record the addition; `MIN_PLUGIN_ABI` stays, so
every plugin in the minor keeps loading.

## When `ABI_SEMVER` MUST take a breaking bump — the frozen surface changed

A break: the minor (pre-1.0; the major from 1.0) moves and `MIN_PLUGIN_ABI`
is raised to the new `x.y.0`.

Any change to the native stabby surface in `crates/plugin-stabby/src/abi.rs`, i.e.
anything `get_stabbied` would reject:

- Add, remove, reorder, or re-sign a method on a `#[stabby::stabby]` trait — the
  vtable slots: `StableProvider`, `StableManagedDriver` (the `invoke*` slots),
  `StableExecutor`, `StableItemStream`, `StableRead`, `StableArtifactContent`,
  `StableFunctionRegistry`, `StablePluginFunctions`, `StableMeta`, `StableCancel`,
  `StableHook`, `StableLogSink`, `StableSupervisor`, `StableRunnerHost`,
  `StableRunner`.
- Add, remove, or reorder a field on a `#[stabby::stabby]` struct: `StableAddr`,
  `StableArg`, `NoteDepOutcome`, `ResultOutcome`, `QueryOutcome`, `NamedDriver`,
  `NamedFunction`, `NamedHook`, `NamedRunner`, `PluginComponents`.
- Change a `dynptr!` / type-alias: `DynRead`, `DynArtifact`, `DynItemStream`,
  `DynExecutor`, `DynProvider`, `DynManagedDriver`, `DynFunctionRegistry`,
  `DynPluginFunctions`, `DynHook`, `DynLogSink`, `DynSupervisor`, `DynRunnerHost`,
  `DynRunner`.
- Change the shape `hexecrunner::wire` encodes (both directions —
  `PrepareRequest`/`PrepareReply` for a plugin calling the host,
  `RunnerRequest`/`RunnerReply` for the host calling a plugin's runner). It is not a stabby type — the
  `StableRunnerHost` method is bytes in, bytes out — so `get_stabbied` cannot
  reject a mismatch and a plugin would misparse a `prepare` instead of failing
  to load. It is frozen with this contract for exactly that reason.
- Rename or retype `CREATE_SYMBOL` or `CreateFn` — the entry point.
- Rename or retype `ABI_SEMVER_SYMBOL` / `AbiSemverFn` — never: the version
  entry is how every later host tells this plugin's ABI, so it stays a plain C
  `fn() -> *const c_char` for good.
- Any wire-only break (a field made required, a meaning changed) — and raise
  `MIN_PLUGIN_ABI` to the new version with it, since only the version check can
  refuse the old plugin at load.
- Rename or retype an optional load-time symbol: `SET_LOG_SINK_SYMBOL`,
  `SET_SUPERVISOR_SYMBOL`, `SET_RUNNER_HOST_SYMBOL`, `SET_LOG_FILTER_SYMBOL`.
  *Adding* one is additive — the host tolerates its absence — but changing one
  that exists is not.
- Change the meaning of `pb::LogFilter` (its level order, the longest-prefix
  match, or the forward-on-doubt rules in `common.proto`): it is bytes in, so
  like the `hexecrunner::wire` shape above nothing rejects a mismatch — a plugin
  would silently drop events the host wants. The same holds for the host's
  translation of its own filter into one: it must never send a narrower filter
  than it applies (`hostlog::plain_directives`, and the test beside it that
  checks it against `EnvFilter`).
- Change the `stabby` dependency version in any crate that links the boundary
  (`plugin-stabby`, `plugin-sdk`, `plugin-go-cdylib`). stabby keys its type reports
  to its own version; a mismatch fails `get_stabbied`.

A **removed** or **renumbered** proto field (vs an added one) is wire-breaking →
also major — with one exception, below.

**Removing a field is minor only when its absence decodes to a safe
*meaning* in both directions** — not merely to bytes the other side can
skip. Skipping is what protobuf always does; the question is what the reader
then believes. Both must hold:

- the new side, reading an old peer that still sends the field, skips it and
  reads the rest as "unknown", never as a definite answer;
- the old peer, reading the new side's silence, takes the field's zero value,
  and that zero value means "unknown" in the old schema, never "no" or
  "empty".

The removed numbers and names are `reserved` so they are never reused. 0.13.0
is the case in point: it removes 0.11.0's `ListResponse.labels`/`labels_known`
(2, 3) in favour of `facts` (4). An old plugin's fields 2 and 3 are skipped and
`facts` is absent — unknown; a pre-0.13 host reads absent `labels_known` as
false — unknown, so it ignores the empty `labels`. Neither side ever reads the
other's silence as "no labels". A removal whose zero value means anything but
"unknown" to the old peer — a bare `repeated` read as "none", a `bool` read as
"no" — is major. `plugin-abi`'s `removed_labels_decode_as_unknown_on_a_0_11_host`
pins this case.

## A new capability the *caller* must offer: a flag, pointed so zero means "no"

A field added to a reply is skipped by a reader that predates it — silently, and
with nothing to notice. That is harmless for a value the old reader never needed
(0.12.0's `hashout`) and wrong for one it was supposed to act on: 0.15.0's
`declared_targets`/`declared_states`, a declaring provider function's targets,
would have been dropped by an old host and the build configured as if the author
had declared nothing, hashed as if intended.

So the capability is advertised by the **caller**, on the request
(`accepts_declarations`), and the producing side tests the flag rather than a
version. Point the flag so the proto3 zero value is the *refusal*: absent means
"I cannot carry these", which is exactly what an older caller means by not
sending it. The callee then fails loudly instead of answering with a reply the
caller would misread. The mirror image — a flag whose zero means "yes, go ahead"
— reintroduces the silent drop, and is wrong however it reads in the field name.
`plugin-abi`'s `a_declaration_crosses_whole_or_not_at_all` and
`an_absent_accepts_declarations_reads_as_refusal` pin both halves.

Minor, not major: nothing frozen moved and no old pairing breaks, but a callee
must reach the version before it can declare.

An **unknown `Matcher` arm** is a decode error on the host (since 0.13.0), not a
silent `Or[]`: a plugin built against a newer schema fails the call loudly
instead of matching nothing, or everything under `Not`. An old host still
reads a new arm as an empty matcher (its decoder leaves the `oneof` unset); that is
why a new arm is a bump the host must reach before plugins send it (a minor
before 0.17.0; under the convention, a patch — though a plugin a patch ahead of
its host can then send an arm that host misreads, so a new arm is safer as a
break).

## A new optional load-time symbol: an additive bump

Adding a `SET_*_SYMBOL` (0.4.0 the supervisor, 0.14.0 the log filter) edits
`abi.rs`, so `scripts/abi-check.sh` wants `ABI_SEMVER` to move — but an additive
bump (a **patch**, since 0.17.0; those two were minors) is enough, because each
pairing still works: a new host skips a symbol an old
plugin lacks (`get_stabbied` errs, the call is skipped), an old host never
looks for a new plugin's extra export, and the plugin must behave sensibly
without the call (the log filter: forward everything).

## What does NOT require a bump

- A new `ProviderMethod`/`DriverMethod` id + guest arm, a new proto field/variant —
  the additive lane above.
- Comments / doc changes with no layout effect.
- Host-only (`crate::host`) or guest-only (`crate::guest`, `plugin-sdk/serve`)
  adapter internals that don't touch the shared `abi` types.
- Engine-internal types that never cross the seam.

## Intentional breaks (pre-1.0)

Below 1.0 the contract may be redefined in place without bumping (no plugins are
released against it). To land a frozen-surface change without moving `ABI_SEMVER`,
put `ABI-BREAK-ACK: <reason>` in a commit message on the PR — the guard treats that
as an acknowledged break and passes. (Past 1.0, bump instead.)

This covers only a break stabby's structural check will refuse anyway. A
**wire-only** break cannot be landed in place: nothing would detect it, so it
always takes the minor bump and the raised `MIN_PLUGIN_ABI`.

When in doubt, bump. A false bump is cheap; a missed real break ships a host/plugin
pair that mismatches at load and aborts the plugin.

## A refused plugin says why

A plugin from an ABI the host does not load is refused once, at load, naming
the plugin and both versions: "plugin <path> was built against a different heph
ABI (plugin <its version>; host <ABI_SEMVER>, which loads ^<MIN_PLUGIN_ABI>);
rebuild it against this heph, or reinstall plugins from
the same release as this heph" — "it reports no ABI version, so it predates
0.17.0" for a plugin without the symbol. A plugin whose version is accepted but
whose create entry still mismatches is not an abort either: the host wraps
`get_stabbied`'s failure as "… different heph ABI (host <ABI_SEMVER>); rebuild it
against this heph, …". An *optional* entry
(`SET_*_SYMBOL`) the plugin exports with a mismatched type is skipped with a
`warn!` naming the symbol — only a plugin that does not export it at all is
skipped in silence.

## History of breaks

- **0.21.0** (break) — outputs from failed targets. `CODEGEN_MODE_IN_PLACE`
  (3) changes meaning: it now also asks the host to write back what a run that
  exited with a code from 1 to 127 left behind (`InPlace { write_on_failure:
  true }`), and the appended `CODEGEN_MODE_IN_PLACE_ON_SUCCESS` (4) is what 3
  meant before. `TargetDef.failure_outputs` (14, `FailureOutput`s of
  `PathContent`) declares what a failed run keeps for its diagnosis, never an
  output and never hashed. `RunFailure.write_back` (5, `OutputArtifactRef`s)
  carries a failed `in_place` run's fixes as the host's `FailureWriteBack`
  context, beside and never inside its `artifacts`, decoded as leniently. Both
  new fields decode empty when absent, and an unknown codegen value stays a
  decode error, never read as a tree write. A 0.20 plugin's `IN_PLACE` never
  asked for a write-back on failure, so `MIN_PLUGIN_ABI` rises to 0.21.0:
  every plugin must be rebuilt.
- **0.20.0** (break) — `ParamType.literal` (6, `string`), a member of the
  `kind` oneof: exactly one string (`ParamType::Literal`). A closed set is a
  `union` of literals, so a cdylib driver's `workdir`-style field reaches the
  LSP as `"a" | "b"` rather than `string`; a oneof member has presence, so
  `literal = ""` is a literal, not a missing kind. Types travel plugin → host
  only (driver and provider-state schemas, function signatures). An old plugin
  never sends a literal, but a 0.19 host would see a new plugin's literal as
  no `kind` at all and refuse the schema (`missing field ParamType.kind`), so
  the change is not additive: a patch would let a 0.19 host load the plugin
  and fail it per schema. `MIN_PLUGIN_ABI` rises to 0.20.0: every plugin must
  be rebuilt.
- **0.19.1** (additive patch) — target timeouts. `TargetDef.timeout_ms` (13,
  `uint64`): how long the run may take, which the host enforces around the
  driver's `run`. Absent is no limit; a present 0 is refused at decode. Never
  hashed. A timeout cancels one run, but a cancel across the seam named the
  whole request (`StableCancel::cancel(request_id)`, shared by every target of
  the request: it tripped every in-flight call of that driver and parked the id
  for the next one), so `ManagedRunRequest.cancel_key` (16, `string`) gives each
  run call a key of its own. The host mints it (unique in the process, starting
  with `plugin_abi::CANCEL_KEY_PREFIX`) and cancels the run by it alone; the SDK
  registers the run's token under it before the `run` dispatch returns, so a
  cancel is never early and a key is never parked — one naming no run in flight
  is for a run that ended, and is dropped. A request cancel still reaches every
  run: each run's token is cancelled with its request. `request_id` is still
  sent and still keys the runner callback. The skew, both ways: a 0.19.0 plugin
  is sent no key (the host reads its reported version) and is cancelled by
  `request_id` as before; it cannot declare a timeout (its `TargetDef` has no
  such field), so its runs are cancelled only with their request, where the
  request-wide cancel is right. A 0.19.1 plugin on a 0.19.0 host gets no key and
  falls back to `request_id`, and its `timeout_ms` is dropped at decode, so its
  targets run unbounded: accepted, by decision — no host capability bit refuses
  it. `TargetDef` struct literals in SDK code need the new field (a source
  break, not a wire break). `MIN_PLUGIN_ABI` stays 0.19.0.
- **0.19.0** (break) — the exec-runner seam says whether the target runs on a
  terminal. `PrepareRequest` and `RunnerRequest` (`hexecrunner::wire`) end in
  a `tty` byte, read into `RunnerCtx::tty`, so a runner that moves the target
  behind another transport can hand a terminal through: the oci runner enters
  a `--shell` with `docker exec -it`. The frames are hand-encoded bytes, not
  prost, so there is no absent-field default. A 0.18 plugin's runner would
  ignore this host's trailing byte and never see the flag (the terminal silently
  missing, the bug this fixes), and a 0.18 plugin driver's frame is truncated
  to this host. `MIN_PLUGIN_ABI` rises to 0.19.0: every plugin must be
  rebuilt.
- **0.18.0** (break) — a run's terminal failure is one `RunFailure` in
  `RunOutFrame`'s oneof (7): the message, the exit status and the failure's
  `artifacts` (4, `OutputArtifactRef`s — normally the run's `log.txt`). It
  replaces `string error` (4) and the out-of-oneof `ExitStatus exit` (5), both
  now reserved. Any failure may carry artifacts, with or without an exit. The
  host rebuilds them as the same `FailureArtifacts` context the plugin
  attached, so a cdylib driver's failure box shows the log tail as an
  in-process driver's does; an artifact that does not decode is dropped and the
  message and exit are kept. A 0.17 peer would see no terminal frame
  on a failure, so `MIN_PLUGIN_ABI` rises to 0.18.0: every plugin must be
  rebuilt.
- **0.17.4** (additive patch) — target resources. `TargetDef.resources` (12,
  `map<string, string>`): the pools a def reserves, each amount as written in
  its pool's unit (only declared pools present, empty means one cpu; a
  driver advertising its targets' cost, e.g. `cpu: "0.1"`, sends it here as
  an ordinary entry). `ManagedRunRequest.allotment` (15, `map<string, string>`): each pool the host
  granted, rendered by its unit, which `${resource:<pool>}` substitutes; absent
  means "told nothing". A 0.17.3 plugin's defs reserve one cpu and it ignores the
  allotment; a 0.17.4 plugin on a 0.17.3 host loses its `resources` at decode and
  is told nothing. Neither field is hashed. `TargetDef` struct literals in SDK
  code need the new field (a source break, not a wire break). `MIN_PLUGIN_ABI`
  stays 0.17.0.
- **0.17.3** (additive patch) — `EntrypointWorkdir.TREE_PACKAGE` (4): an
  entrypoint run in its package's dir in the real tree. `ROOT` (2) is renamed
  `TREE_ROOT`, same number, so the wire is unchanged. A def carrying the new
  value sent to an older host is refused at decode (`Entrypoint.workdir` must be
  one the build knows), never run elsewhere. `MIN_PLUGIN_ABI` stays 0.17.0.
- **0.17.2** (additive patch) — a run on the host's PTY. `RUN_STDIO_PTY` (3),
  `RunOptions.pty_slave_fd` (8, a descriptor whose ownership moves with the
  message, so it is decoded once and only by a transport sharing the host's
  process), `Entrypoint.offers_pty` (3), and the run frames `RunInFrame.cont` (5)
  and `RunOutFrame.stopped` (6). A driver offers a PTY by setting the bit, so an
  older plugin is never sent a slave and its run inherits, with a warning.
  `MIN_PLUGIN_ABI` stays 0.17.0; no plugin must be rebuilt.
- **0.17.1** (additive patch) — entrypoints. `TargetDef.entrypoint` (11),
  `ManagedRunRequest.options` (14, sent only for a def that declared an
  entrypoint), `RunInFrame.signal` (4, an OS-local signo), `RunOutFrame.exit`
  (5, outside the oneof so `error` stays the terminal frame) and
  `CreateConfig.host_capabilities` (5; absent = no capability). The run's request
  stream is now live: the host ends it only after reading the terminal frame,
  and a plugin never waits for its end before sending that frame — a plugin on
  a 0.17.0 host sees the end right after `start` and reads it as "no signals".
  `MIN_PLUGIN_ABI` stays 0.17.0; no plugin must be rebuilt.
- **0.17.0** — explicit presence, buffa codegen. The wire stays protobuf with
  the same messages, field numbers and types, but every proto is edition 2024
  with explicit presence on every singular field, generated by buffa instead of
  prost. A field the sender did not set is absent instead of its zero value,
  and the decoders tell the two apart: a field the receiver cannot do without
  (an addr's package and name, a spec's driver, a dep's or input's hash flags,
  a def's cache config and hash, a request id, a create config's root/home/name)
  fails the decode with `missing field Message.field`; the zero-value sentinels
  (an empty listed driver = unknown, an empty declared driver = the provider
  default, an empty hashout = none, `LEVEL_UNSPECIFIED`) are spelled as absence,
  and a present empty value there is malformed. A present-empty
  `ListedFacts.driver` is the one message a 0.16 peer could validly send that
  this host now rejects. The frozen create surface did not move, so
  `get_stabbied` alone would still accept a 0.16 plugin — whose prost encoder
  omits every zero, which this host reads as missing required fields, call by
  call. So 0.17.0 also starts the **load-time version check**: the new required
  `heph_plugin_abi_semver` entry (`ABI_SEMVER_SYMBOL`), and `MIN_PLUGIN_ABI` =
  0.17.0, accepting `^0.17.0`. A 0.16 plugin exports no version and is refused
  once, at load, as predating 0.17.0. Every plugin must be rebuilt, as for
  0.16.0. From here on: additive = patch, break = minor + raised
  `MIN_PLUGIN_ABI` (see "The convention" above).
- **0.16.0** — functions belong to plugins. `PluginComponents` lost
  `provider_name` (the manifest names the plugin) and gained `functions`
  (`NamedFunction`: name + `FunctionDef` metadata, decoded once at load; bad
  metadata fails the load) and `function_handle` (one `DynPluginFunctions` per
  plugin, dispatched by `pb::FunctionMethod`). `CreateFn` takes the engine's
  `DynFunctionRegistry` per instance. `StableProvider::invoke_registry` is gone;
  `ProviderMethod` 5, 6 and 8 are reserved. `CallRegisteredRequest.provider` is
  `plugin` (same number). `CreateConfig.name` (4) carries the manifest name.
