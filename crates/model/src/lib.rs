//! Target addressing + selection model: the `//package:name` address
//! (`htaddr`), package paths (`htpkg`), labels (`htlabel`), and the composable
//! matcher predicate (`htmatcher`). Self-contained — depends on no other heph crate. The
//! matcher-against-target-def evaluation lives in the engine
//! (`engine::matcher_target`), keeping this crate free of the driver/target-def
//! types.
#![cfg_attr(
    test,
    expect(
        clippy::get_unwrap,
        clippy::assertions_on_result_states,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

pub mod htaddr;
pub mod htlabel;
pub mod htmatcher;
pub mod htpkg;
pub mod htquery;
