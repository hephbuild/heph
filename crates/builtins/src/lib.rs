//! Always-on built-in providers/drivers, depending only on the `heph-plugin`
//! contract (never on the concrete engine). The engine wires these in
//! `Engine::new`; keeping them below the engine avoids the engine↔plugin cycle.
//!
//! - `pluginfs` — the `fs` provider + driver (filesystem targets).
//! - `plugingroup` — the `group` driver (aggregate targets).
//! - `pluginscratch` — the `scratch` driver (persistent cache-directory declarations).
//! - `pluginstatictarget` — in-memory static target provider (tests/wiring).
//! - `plugintextfile` — the `textfile` driver.
//! - `plugintemplate` — the `template` driver (minijinja rendering).
//! - `pluginhostbin` — the host-binary provider + driver.

pub mod pluginfs;
pub mod plugingroup;
pub mod pluginhostbin;
pub mod pluginscratch;
pub mod pluginstatictarget;
pub mod plugintemplate;
pub mod plugintextfile;

// The `htspec` derive macros expand to code referencing `crate::htvalue` and
// `crate::htspec`; alias them here so those expansions resolve in this crate,
// the same way they did in the monolith.
pub(crate) use hcore::htvalue;
pub(crate) use hplugin::htspec;
