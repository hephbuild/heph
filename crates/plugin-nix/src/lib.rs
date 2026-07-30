//! The `nix` managed driver: builds targets via a nix expression. Depends on
//! the contract + `heph-driver-support`.

pub mod pluginnix;

// The `htspec` derive macros expand to `crate::htvalue` / `crate::htspec`.
pub(crate) use hcore::htvalue;
pub(crate) use hplugin::htspec;
