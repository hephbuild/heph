//! Anonymous usage telemetry: collects per-run counters from the engine's build
//! events and spools a single snapshot at exit, for a detached flusher to send
//! to PostHog (see `docs/TELEMETRY.md`). Depends only on
//! heph-core (events) + heph-plugin (error); the engine calls its record_* API.
#![cfg_attr(
    test,
    expect(
        clippy::assertions_on_result_states,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

pub mod telemetry;
