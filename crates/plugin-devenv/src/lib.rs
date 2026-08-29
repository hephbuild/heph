//! The devenv plugin: run heph targets inside a [devenv](https://devenv.sh)
//! environment.

pub mod plugindevenv;

// The `htspec` derive macros expand to `crate::htvalue` / `crate::htspec`.
pub(crate) use hcore::htvalue;
pub(crate) use hplugin::htspec;
