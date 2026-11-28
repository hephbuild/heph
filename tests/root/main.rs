//! The root package's integration tests, linked into one test binary.
//!
//! Each file was its own `tests/*.rs` binary, and each one linked all of
//! `heph`. They are modules of one harness now; select a file with a name
//! filter: `cargo test --test root ci_gate::`.
//!
//! Five files stay separate binaries under `tests/`, because they touch state
//! that belongs to the whole process and libtest runs a binary's tests as
//! threads of one process:
//!
//!   - `supervisor.rs` re-executes its own binary with `--exact <name>`
//!     (`supervisor_*` tests), and serialises spawns behind a process-wide
//!     lock;
//!   - `handle_drop_reaps.rs`, `wait_with_output_pipe.rs` and
//!     `process_watcher_stress.rs` exercise the process-wide child watcher
//!     (reaping, the kqueue/pidfd registrations, a burst of concurrent
//!     children), and are timing-sensitive regressions for it: any other
//!     test spawning children in the same process changes what they measure;
//!   - `heph_cwd.rs` sets `HEPH_CWD`, which the cwd, root and cwd package
//!     read once per process: its test must be the first to resolve them.
//!
//! `test_layout::every_test_file_is_compiled` holds that list, and fails when
//! a file under an aggregated directory is not declared by its `main.rs`.

mod build_fmt_cli;
mod cache_tier_gate;
mod ci_failure_output;
mod ci_gate;
mod codegen_json_cli;
mod codegen_removed_cli;
mod config_schema_cli;
mod coreutils_cli;
mod darwin_linker;
mod dev_debuginfo;
mod execrunner_gate;
mod feature_unification;
mod gc_cli;
mod gen_script;
mod json_cli;
mod json_print_gate;
mod lint_gate;
mod log_cmd;
mod lsp_lifecycle;
mod plugin_agnostic_gate;
mod resources_gate;
mod run_outputs_cli;
mod stack_status;
mod target_dir_script;
mod test_layout;
mod tst_script;
mod version_json;
