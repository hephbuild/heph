//! Keyed transformable RW locks (in-memory + advisory file locks via flock),
//! with a cancellation-aware acquire path. Used by the engine's per-addr result
//! lock. Depends only on heph-core (cancellation) + heph-plugin (CancelledError).

pub mod hlock;
