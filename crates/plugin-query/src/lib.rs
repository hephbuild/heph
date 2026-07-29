//! The `query` provider: exposes query-result targets (group/static composition)
//! resolved through the engine's `ProviderExecutor`. Depends only on the
//! contract + the builtins it composes (`plugingroup`).
#![cfg_attr(
    test,
    expect(
        clippy::get_unwrap,
        clippy::err_expect,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

pub mod pluginquery;
