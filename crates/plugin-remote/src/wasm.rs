//! wasm transport (in-process wasmtime component) — milestone M4.
//!
//! Will instantiate a plugin `.wasm` component via wasmtime, bind the WIT
//! interface (`wit/heph-plugin.wit`), wire the `AbiHost` callbacks as host
//! imports, and grant capabilities (WASI preopens) per the `launch` policy.
//! Scaffolded here so the feature wiring exists from M1; implemented in M4.
