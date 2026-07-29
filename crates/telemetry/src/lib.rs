//! Anonymous usage telemetry: collects per-run counters from the engine's build
//! events and ships a single snapshot to PostHog on exit. Depends only on
//! heph-core (events) + heph-plugin (error); the engine calls its record_* API.
#![cfg_attr(
    test,
    expect(
        clippy::assertions_on_result_states,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

pub mod telemetry;
