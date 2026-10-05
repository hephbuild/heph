# Plugin ABI versioning

The in-process stable-ABI plugin transport (`plugin-stabby`) crosses the cdylib
boundary as native stabby vtables (hot path) + prost bytes (cold path). Host and
plugin are compiled separately and linked at load time. The compatibility gate is
stabby's `get_stabbied`: it verifies the plugin's type report (every type reachable
from `CreateFn`) against the host's at load and **hard-fails on any difference**.
So the frozen stabby surface in `crates/plugin-stabby/src/abi.rs` must not drift —
that is what `ABI_SEMVER` (`crates/plugin-abi/src/lib.rs`) tracks.

`scripts/abi-check.sh` (the `abi` job in `.github/workflows/heph.yml`, run on every
PR) enforces this mechanically: if the frozen surface changes without `ABI_SEMVER`
moving, the check fails.

## The dispatch model — most growth is additive, no bump

The cold surface is a **frozen generic dispatch**, not one vtable slot per RPC. A
method is a `pb::ProviderMethod` / `pb::DriverMethod` id carried over the fixed
`invoke*` slots; payloads are prost bytes. So **adding an RPC does NOT touch the
vtable**:

- New RPC = a new `ProviderMethod`/`DriverMethod` enum value + a new guest `match`
  arm. The type report is unchanged, so an older plugin still loads; it answers an
  unknown id with `Error{Unimplemented}` and the host falls back.
- New cold-path wire field, new `RunInFrame`/`RunOutFrame` oneof variant, new
  `pb::CreateConfig` field — all additive (prost ignores unknowns).

**None of these need an `ABI_SEMVER` bump** and none touch `abi.rs`. This additive
lane is the whole point of the dispatch collapse — grow capability without breaking
old plugins. (Bump the minor only if you want to *signal* the new capability.)

## When `ABI_SEMVER` MUST be bumped (major) — the frozen surface changed

Any change to the native stabby surface in `crates/plugin-stabby/src/abi.rs`, i.e.
anything `get_stabbied` would reject:

- Add, remove, reorder, or re-sign a method on a `#[stabby::stabby]` trait — the
  vtable slots: `StableProvider`, `StableManagedDriver` (the `invoke*` slots),
  `StableExecutor`, `StableItemStream`, `StableRead`, `StableArtifactContent`,
  `StableFunctionRegistry`, `StableMeta`, `StableCancel`, `StableHook`,
  `StableLogSink`, `StableSupervisor`, `StableRunnerHost`, `StableRunner`.
- Add, remove, or reorder a field on a `#[stabby::stabby]` struct: `StableAddr`,
  `StableArg`, `NoteDepOutcome`, `ResultOutcome`, `QueryOutcome`, `NamedDriver`,
  `NamedHook`, `NamedRunner`, `PluginComponents`.
- Change a `dynptr!` / type-alias: `DynRead`, `DynArtifact`, `DynItemStream`,
  `DynExecutor`, `DynProvider`, `DynManagedDriver`, `DynFunctionRegistry`,
  `DynHook`, `DynLogSink`, `DynSupervisor`, `DynRunnerHost`, `DynRunner`.
- Change the shape `hexecrunner::wire` encodes (both directions —
  `PrepareRequest`/`PrepareReply` for a plugin calling the host,
  `RunnerRequest`/`RunnerReply` for the host calling a plugin's runner). It is not a stabby type — the
  `StableRunnerHost` method is bytes in, bytes out — so `get_stabbied` cannot
  reject a mismatch and a plugin would misparse a `prepare` instead of failing
  to load. It is frozen with this contract for exactly that reason.
- Rename or retype `CREATE_SYMBOL` or `CreateFn` — the entry point.
- Rename or retype an optional load-time symbol: `SET_LOG_SINK_SYMBOL`,
  `SET_SUPERVISOR_SYMBOL`, `SET_RUNNER_HOST_SYMBOL`. *Adding* one is additive —
  the host tolerates its absence — but changing one that exists is not.
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

The removed numbers and names are `reserved` so they are never reused. 0.12.0
is the case in point: it removes 0.11.0's `ListResponse.labels`/`labels_known`
(2, 3) in favour of `facts` (4). An old plugin's fields 2 and 3 are skipped and
`facts` is absent — unknown; a pre-0.12 host reads absent `labels_known` as
false — unknown, so it ignores the empty `labels`. Neither side ever reads the
other's silence as "no labels". A removal whose zero value means anything but
"unknown" to the old peer — a bare `repeated` read as "none", a `bool` read as
"no" — is major. `plugin-abi`'s `removed_labels_decode_as_unknown_on_a_0_11_host`
pins this case.

An **unknown `Matcher` arm** is a decode error on the host (since 0.12.0), not a
silent `Or[]`: a plugin built against a newer schema fails the call loudly
instead of matching nothing, or everything under `Not`. An old host still
reads a new arm as an empty matcher (prost leaves the `oneof` unset); that is
why a new arm is a minor bump the host must reach before plugins send it.

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

When in doubt, bump. A false bump is cheap; a missed real break ships a host/plugin
pair that mismatches at load and aborts the plugin.
