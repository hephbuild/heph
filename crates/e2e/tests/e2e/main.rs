//! The crate's integration tests, linked into one test binary.
//!
//! Each file was its own `tests/*.rs` binary, and each binary statically links
//! all of `heph` — so N files meant N links of the same crate graph, which
//! dominated build time and disk. They are modules of one harness now; select a
//! file with a name filter: `cargo test -p e2e --test e2e clean::`.
//!
//! Not every file belongs here. `credential.rs`, `oci_credential.rs` and
//! `deferred.rs` stay separate binaries under `tests/` because they call
//! `std::env::set_var`, and libtest runs a binary's tests as threads of one
//! process: a variable one test sets would be visible to every other test in
//! here while it is set. The root crate's `every_test_file_is_compiled` checks
//! that each file under this directory is declared below and that no new
//! top-level test file appears without a decision either way.

#[path = "../common/mod.rs"]
mod common;

mod cache_load;
mod clean;
mod codegen_in_place;
mod deps;
mod devenv_runner;
mod discovery;
mod engine_core;
mod engine_sanity;
mod execrunner;
mod force;
mod group;
mod group_relocate;
mod home_dir;
mod http_fetch;
mod label_select;
mod listed_facts;
mod oci;
mod oci_docker;
mod oci_image;
mod oci_runner;
mod plugin_buildfile;
mod plugin_fn_declarations;
mod plugin_functions;
mod query;
mod remote_cache;
mod scratch;
mod tool_env;
