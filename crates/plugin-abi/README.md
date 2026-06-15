# External-plugin ABI

heph plugins can run out-of-process over three transports — **proto** (prost over
a UDS socketpair, portable/polyglot), **shm** (iceoryx2 shared memory, ultra
low-overhead, used by plugin-go), and **wasm** (in-process wasmtime component,
capability-sandboxed) — all behind one logical interface. In-process plugins
(`query`, `hostbin`, `group`, `statictarget`) keep working unchanged; in-proc and
remote coexist permanently.

Full design + rationale: the approved plan (`.claude/plans/`). This file is the
quick map.

## Schemas (one contract, three encodings)

- **Source of truth: proto** — `proto/plugin/v1/*.proto`, package `heph.plugin.v1`,
  generated via `buf` (`gen` devenv command) into `gen/proto` (`proto-gen` crate,
  alias `hproto-gen`). Files: `common`, `targetdef`, `provider`, `driver`,
  `callback`, `envelope`.
- **rkyv mirror** — `crates/plugin-abi/src/shm_types.rs` mirrors only the ≤5
  hot-path messages (`ResultRequest/Response`, `NoteDep{Request,Response}`, …) for
  the shm zero-copy fast path. A parity test (`crates/plugin-abi/tests/parity.rs`)
  round-trips proto → rkyv → proto so the two cannot drift.
- **WIT mirror** — `wit/heph-plugin.wit` mirrors the interface for the wasm tier
  (validated + parity-checked in M4).

The shm `payload_encoding` is negotiated at handshake: `Rkyv` (Rust, native
zero-copy), `Capnp` (polyglot, e.g. Go), `Prost` elsewhere.

## Crates

- `crates/plugin` — the existing in-process contract (`Provider`/`Driver`/
  `ProviderExecutor`/`EResult`/`TargetSpec`/`TargetDef`). Engine-independent.
- `crates/plugin-abi` — raw wire types: re-exports the generated proto (`pb`),
  `ABI_SEMVER`, and the rkyv hot-message mirror. Deps: `hproto-gen` only (light).
- `crates/plugin-sdk` — Rust guest SDK. Authors implement the **same**
  `hplugin::Provider`/`Driver` traits; the SDK hides framing/encoding/streaming,
  manages leases, and exposes `Ctx` (cancellation) + `HostClient` (callbacks).
- `crates/plugin-remote` (M1) — host-side adapter implementing `hplugin` traits
  over a transport; registered via the existing engine factory hooks (engine core
  unchanged). Transports are cargo-feature-gated modules.

## Key invariants

- **Result/input artifacts are abstract readers, not paths.** `Content` (in
  `hcore::hartifactcontent`) exposes only `reader`/`walk`/`seekable_reader` — no
  path/fd. They cross as opaque `ArtifactHandle` + metadata; bytes are pulled
  lazily (`open_artifact`), zero-copy via fd/`memfd` when file-backed. Only driver
  `run` outputs (fresh files) cross as paths.
- **The dep-cycle check lives host-side.** Every `result`/`note_dep` registers the
  `parent → addr` DepDag edge before any await. Never memoize the callback on the
  addr in a plugin — cache the derived output, never the call. `note_dep` is the
  cache-hit fast path that registers the edge without fetching.
- **`raw_def` is opaque pass-through** — `RawDefBlob`, produced and consumed only
  by the originating driver; the host never inspects it.
