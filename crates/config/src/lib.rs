//! Engine-free workspace configuration.
//!
//! Owns the `.hephconfig` YAML shape ([`ConfigYaml`]), the profile-layering
//! [`load_from_root`] loader, and workspace-root discovery ([`get_root`]). It is
//! deliberately free of any dependency on the engine so layers that run *before*
//! the engine boots — notably the self-upgrade check in `main` — can read the
//! config without dragging the whole engine in.
//!
//! The engine resolves a [`ConfigYaml`] into its fully-populated runtime config
//! via an extension trait on its side; everything here stops at the all-optional
//! file shape and the merge/layer logic.

// Test code legitimately uses panicking helpers, indexing, and fixture asserts;
// exempt the test cfg from the workspace restriction lints rather than rewriting
// each test. `allow` (not `expect`) since not every listed lint fires across this
// crate's small suite.
#![cfg_attr(
    test,
    allow(
        clippy::panic_in_result_fn,
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::assertions_on_result_states,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

mod config_yaml;
mod options;
mod root;

pub use config_yaml::*;
pub use options::{Options, decode_opt, deny_unknown};
pub use root::{get_cwd, get_root};
