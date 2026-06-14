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
        clippy::get_unwrap,
        clippy::panic_in_result_fn,
        clippy::assertions_on_result_states,
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::unimplemented,
        clippy::undocumented_unsafe_blocks,
        clippy::unreachable,
        clippy::let_underscore_must_use,
        clippy::float_cmp,
        clippy::assertions_on_constants,
        clippy::cloned_ref_to_slice_refs,
        clippy::err_expect,
        unused_imports,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

pub mod config;
pub mod driver;
pub mod eresult;
pub mod error;
pub mod htspec;
pub mod lsp;
pub mod provider;

// The `htspec` derive macros (SpecEnum/SpecStruct/…) expand to code that
// references `crate::htvalue`. Alias it here so those expansions resolve inside
// this crate, the same way they did in the monolith.
pub(crate) use hcore::htvalue;
