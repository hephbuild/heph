//! The plugin contract: the trait + data surface every `Provider`/`Driver`
//! implements and the engine consumes. Sits below both the engine and the
//! plugins so neither needs to depend on the other's concrete types.
//!
//! - `provider` — `Provider`/`ProviderExecutor`/`ProviderFn` traits, `TargetSpec`.
//! - `driver` — `Driver` trait, `TargetAddr`, the `targetdef` target-def model,
//!   sandbox config, input/output artifact descriptors, `DriverSchema`.
//! - `eresult` — `EResult`/`ArtifactMeta`, the execution result data.
//! - `htspec` — declarative target-config spec/schema/parser (derive-backed).
//! - `error` — typed provider/driver errors.
#![cfg_attr(
    test,
    expect(
        clippy::assertions_on_result_states,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

pub mod config;
pub mod driver;
pub mod eresult;
pub mod error;
pub mod hook;
pub mod htspec;
pub mod lsp;
pub mod provider;

// The `htspec` derive macros (SpecEnum/SpecStruct/…) expand to code that
// references `crate::htvalue`. Alias it here so those expansions resolve inside
// this crate, the same way they did in the monolith.
pub(crate) use hcore::htvalue;
