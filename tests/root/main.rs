//! The root package's integration tests, linked into one test binary.
//!
//! Each file was its own `tests/*.rs` binary, and each one linked all of
//! `heph`. They are modules of one harness now; select a file with a name
//! filter: `cargo test --test root ci_gate::`.
//!
//! Four files stay separate binaries under `tests/`, because they touch state
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
//!     test spawning children in the same process changes what they measure.
//!
//! `test_layout::every_test_file_is_compiled` holds that list, and fails when
//! a file under an aggregated directory is not declared by its `main.rs`.

mod build_fmt_cli;
mod ci_gate;
mod darwin_linker;
mod execrunner_gate;
mod lint_gate;
mod lsp_lifecycle;
mod target_dir_script;
mod test_layout;
mod tst_script;
