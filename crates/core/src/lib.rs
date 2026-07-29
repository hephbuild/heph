// Foundational leaf crate: zero dependencies on any other heph crate. Holds the
// primitives every layer reuses (cancellation, memoization, value model, hashing,
// platform/version metadata, artifact-content tar helpers, deferred cleanup).
//
// Restriction lints in `[workspace.lints.clippy]` are production guards; test code
// legitimately uses panicking helpers. Mirror the monolith's test exemption here.
#![cfg_attr(
    test,
    expect(
        clippy::assertions_on_result_states,
        clippy::unwrap_used,
        clippy::undocumented_unsafe_blocks,
        clippy::let_underscore_must_use,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

pub mod blocking;
pub mod debug_hash;
pub mod defer;
pub mod events;
pub mod fsutil;
pub mod hartifactcontent;
pub mod hasync;
pub mod hmemoizer;
pub mod htplatform;
pub mod htvalue;
pub mod shutdown;
pub mod version;
