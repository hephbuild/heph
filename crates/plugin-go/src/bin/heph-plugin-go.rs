//! The Go plugin as an out-of-process plugin.
//!
//! plugin-go stays Rust — this binary just relocates its construction (the `go`
//! provider plus the golist/embed/testmain managed drivers) into a process
//! served over the heph plugin transport via the SDK, instead of registering
//! them in-process on the engine. It still shells out to `go list` internally,
//! exactly as before.
//!
//! Config is passed by the host at spawn via env vars:
//! - `HEPH_PLUGIN_GO_ROOT`    — workspace root (default: cwd)
//! - `HEPH_PLUGIN_GO_BIN`     — addr of the go toolchain (default `//@heph/bin:go`)
//! - `HEPH_PLUGIN_GO_WALK_DB` — fs-walk cache db path (default: under root)

use hdriver_support::driver_managed::ManagedDriver;
use plugin_go::plugingo::{GoEmbedDriver, GoGolistDriver, GoTestmainDriver, Provider};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let root = std::env::var_os("HEPH_PLUGIN_GO_ROOT")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let go_bin =
        std::env::var("HEPH_PLUGIN_GO_BIN").unwrap_or_else(|_| "//@heph/bin:go".to_string());
    let walk_db = std::env::var_os("HEPH_PLUGIN_GO_WALK_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(".heph-plugin-go-fswalk.db"));

    // The plugin process has filesystem access, so it owns its own walker rather
    // than proxying the host's.
    let walker = Arc::new(hwalk::CachedWalker::open(&walk_db));
    let opts = hplugin::config::Options::new();
    let provider = Provider::from_options(root, &[], &[], &opts, walker)?;

    let mut managed: HashMap<String, Arc<dyn ManagedDriver>> = HashMap::new();
    managed.insert(
        "go_golist".to_string(),
        Arc::new(GoGolistDriver::new(go_bin)),
    );
    managed.insert("go_embed".to_string(), Arc::new(GoEmbedDriver));
    managed.insert("go_testmain".to_string(), Arc::new(GoTestmainDriver));

    plugin_sdk::serve_components_inherited(Some(Arc::new(provider)), managed).await
}
