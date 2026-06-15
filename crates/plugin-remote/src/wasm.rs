//! wasm transport (in-process wasmtime component) — milestone M4.
//!
//! Instantiates a plugin `.wasm` component via wasmtime, binds the WIT
//! interface, and (eventually) wires the `AbiHost` callbacks as host imports
//! with capability-scoped WASI preopens per the `launch` policy.
//!
//! This module currently carries the de-risk vertical slice: load a component
//! that exports `greet`, link WASI, instantiate, and call it. It proves the
//! cargo-component guest ↔ wasmtime host contract end-to-end before the full
//! provider/driver WIT is brought up.

use anyhow::{Context, Result};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

mod echo_bindings {
    wasmtime::component::bindgen!({
        inline: "package component:echo;\nworld echo {\n  export greet: func(name: string) -> string;\n}",
        world: "echo",
    });
}

/// Per-instance host state: the WASI context plus the resource table wasmtime
/// uses to hand out handles to host-owned resources.
struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// De-risk slice: instantiate `wasm` (a component exporting `greet`) and call
/// `greet(name)`. Synchronous — `greet` is pure compute, no host imports beyond
/// the WASI shims the component links against.
pub fn instantiate_and_greet(wasm: &[u8], name: &str) -> Result<String> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)
        .map_err(anyhow::Error::from)
        .context("building wasmtime engine")?;
    let component = Component::from_binary(&engine, wasm)
        .map_err(anyhow::Error::from)
        .context("loading wasm component")?;

    let mut linker = Linker::<HostState>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
        .map_err(anyhow::Error::from)
        .context("linking WASI imports")?;

    let state = HostState {
        table: ResourceTable::new(),
        wasi: WasiCtxBuilder::new().build(),
    };
    let mut store = Store::new(&engine, state);

    let echo = echo_bindings::Echo::instantiate(&mut store, &component, &linker)
        .map_err(anyhow::Error::from)
        .context("instantiating echo component")?;
    echo.call_greet(&mut store, name)
        .map_err(anyhow::Error::from)
        .context("calling greet export")
}
