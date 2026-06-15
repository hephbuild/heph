//! shm transport (iceoryx2) — milestone M3.
//!
//! Will carry the same `Frame` bodies as the proto transport over two iceoryx2
//! request-response services (one per direction), with the rkyv/capnp payload
//! fast path and batched `note_dep`. Scaffolded here so the feature wiring and
//! the transport-agnostic boundary exist from M1; the implementation lands in
//! M3 (the perf-hardening milestone).
