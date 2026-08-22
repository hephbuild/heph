//! The `devenv` plugin: a driver that captures a devenv.sh environment as an
//! artifact, and an exec runner that reads it back.
//!
//! Design: `docs/EXEC_RUNNERS.md` §5. Two halves under one name, because that
//! is how a runner is selected — by the driver name of the runner target.
pub mod plugindevenv;

// The `htspec` derive macros expand to `crate::htvalue` / `crate::htspec`.
pub(crate) use hcore::htvalue;
pub(crate) use hplugin::htspec;
