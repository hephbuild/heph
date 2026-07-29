//! The `buildfile` provider: discovers packages by walking the filesystem for
//! BUILD files and evaluates them as Starlark, plus the BUILD-file LSP. Depends
//! only on the `heph-plugin` contract (the LSP via the `LspEngine` trait), so
//! the heavy `starlark` toolchain compiles here in isolation, off the engine's
//! hot path.
#![cfg_attr(
    test,
    expect(
        clippy::get_unwrap,
        clippy::assertions_on_result_states,
        clippy::unwrap_in_result,
        clippy::float_cmp,
        clippy::assertions_on_constants,
        clippy::cloned_ref_to_slice_refs,
        unused_imports,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

pub mod pluginbuildfile;
