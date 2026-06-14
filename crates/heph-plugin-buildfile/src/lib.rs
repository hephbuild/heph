//! The `buildfile` provider: discovers packages by walking the filesystem for
//! BUILD files and evaluates them as Starlark, plus the BUILD-file LSP. Depends
//! only on the `heph-plugin` contract (the LSP via the `LspEngine` trait), so
//! the heavy `starlark` toolchain compiles here in isolation, off the engine's
//! hot path.
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

pub mod pluginbuildfile;
