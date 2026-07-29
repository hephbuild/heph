//! The `http_fetch` managed driver: downloads a file off the internet as a
//! cacheable target output. Depends on the contract + `heph-driver-support`.
#![cfg_attr(
    test,
    expect(
        clippy::assertions_on_result_states,
        clippy::let_underscore_must_use,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

pub mod pluginhttp;

// The `htspec` derive macros expand to `crate::htvalue` / `crate::htspec`.
pub(crate) use hcore::htvalue;
pub(crate) use hplugin::htspec;
