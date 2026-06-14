//! Always-on built-in providers/drivers, depending only on the `heph-plugin`
//! contract (never on the concrete engine). The engine wires these in
//! `Engine::new`; keeping them below the engine avoids the engine↔plugin cycle.
//!
//! - `pluginfs` — the `fs` provider + driver (filesystem targets).
//! - `plugingroup` — the `group` driver (aggregate targets).
//! - `pluginstatictarget` — in-memory static target provider (tests/wiring).
//! - `plugintextfile` — the `textfile` driver.
//! - `pluginhostbin` — the host-binary provider + driver.
#![cfg_attr(
    test,
    expect(
        clippy::get_unwrap,
        clippy::panic_in_result_fn,
        clippy::assertions_on_result_states,
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::unimplemented,
        clippy::undocumented_unsafe_blocks,
        clippy::unreachable,
        clippy::let_underscore_must_use,
        clippy::float_cmp,
        clippy::assertions_on_constants,
        clippy::cloned_ref_to_slice_refs,
        clippy::err_expect,
        unused_imports,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

pub mod pluginfs;
pub mod plugingroup;
pub mod pluginhostbin;
pub mod pluginstatictarget;
pub mod plugintextfile;

// The `htspec` derive macros expand to code referencing `crate::htvalue` and
// `crate::htspec`; alias them here so those expansions resolve in this crate,
// the same way they did in the monolith.
pub(crate) use heph_core::htvalue;
pub(crate) use heph_plugin::htspec;
