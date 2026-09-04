//! Target-scoped credentials: a target names the credential it needs by
//! address, the *recipe* is built and hashed like any other dependency, and the
//! *value* is minted at run time, delivered as a file, redacted on the way out,
//! and never persisted anywhere.
//!
//! # The boundary
//!
//! ```text
//!   BUILD GRAPH — hashed, cached, shared  │  RUN — minted, never persisted
//!                                         │
//!   //infra/creds:r2                      │   broker.mint()
//!     secret.json — identity only  ───────┼──▶  memoized per run · TTL
//!            │ hashout                    │        │ shape
//!            ▼                            │        ▼
//!          hashin  ◀── def hash           │   <sandbox>/secrets/r2  0600
//!         cache key                       │        │ $SECRET_R2 = path
//!                                         │        ▼
//!                          value never ───┼──   target process
//!                          crosses back   │        │ redacting tee
//!                                         │        ▼
//!                                         │   log.txt · TUI · events
//! ```
//!
//! Everything the cache key is built from sits left of the line. Only the
//! recipe crosses it. Changing *who you are* — role, scope, audience — re-keys
//! every consumer, because the descriptor's hashout is in their `hashin`.
//! Rotating *the token* changes nothing, which is precisely the `pass_env` bug
//! fixed by construction rather than by discipline.
//!
//! # The rule that belongs in the user-facing docs verbatim
//!
//! > **A credential grants read access, through the shared cache, to whatever
//! > its consumers produced.**
//!
//! The descriptor hashes the role, not the subject, so an artifact built under
//! one identity is served from the shared remote cache to anyone who can reach
//! it. That is deliberate — it is the same deduplication that makes a shared
//! cache worth having. Where a target's output really does differ by who ran
//! it, `cache.subject_scoped` keys it by the run's identity instead.
//!
//! # Layout
//!
//! - [`broker`] — per-run dedup, TTL refresh, and the live-value registry.
//! - [`descriptor`] — `secret.json`, and the identity/acquisition split that
//!   the rest of the design rests on.
//! - [`shape`] — how a value reaches a tool, and the merge-key collision rule.
//! - [`expiry`] — where an expiry comes from, in precedence order.
//! - [`jwt`] — a best-effort claim reader that reads but does not verify.
//! - [`oidc`] — the federated provider: present an assertion, run the exchange
//!   pipeline, never interactively.
//! - [`protocol`] — the four helper wire formats, encode and decode.
//! - [`render`] — writing a minted value where a tool will find it, and
//!   scrubbing it off a sandbox that is kept for diagnostics.
//! - [`session`] — `heph auth login`: the one interactive flow, kept out of
//!   the build path, and the single refresh token it leaves behind.
//! - [`provider`] — the code that obtains a value: `static_env` and `exec`.
//! - [`redact`] — the multi-pattern redacting tee.
//! - [`value`] — a minted value, and the one type allowed to hold one.

// The `htspec` derives expand to code referencing `crate::htvalue` and
// `crate::htspec`; alias them so those expansions resolve here, the same way
// `builtins` does.
pub(crate) use hcore::htvalue;
pub(crate) use hplugin::htspec;

pub mod broker;
pub mod descriptor;
pub mod expiry;
pub mod jwt;
pub mod oidc;
pub mod protocol;
pub mod provider;
pub mod redact;
pub mod render;
pub mod session;
pub mod shape;
pub mod value;

pub use broker::{Broker, BrokerCtx, Grant};
pub use descriptor::{
    Acquire, Descriptor, Exchange, Identity, Protocol, ProviderKind, SECRET_JSON,
    SECRET_JSON_VERSION, SecretJson, Selection, SignIn, Source, WhenEnv,
};
pub use expiry::{Expiry, ExpirySource};
pub use provider::{MintCtx, ProviderRegistry, SecretProvider};
pub use redact::{Entry as RedactEntry, RedactStream, Redactor};
pub use render::{Rendered, Rendering, render_all, scrub};
pub use session::{Metadata, Session, TokenSet};
pub use shape::{Claim, Shape, Slot, check_collisions};
pub use value::{Credential, SecretValue};
