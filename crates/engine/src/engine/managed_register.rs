//! Engine-side wiring for managed drivers. Lives in the engine (not
//! `heph-driver-support`) because it reads engine state (the FUSE runtime) and
//! supplies the default shell fallback built from `pluginexec` — a dependency
//! the contract-level driver-support crate must not have.

use crate::engine::Engine;
use hdriver_bridge::{FuseSlot, ManagedDriverBridge};
use hdriver_support::driver_managed::ManagedDriver;

impl Engine {
    pub fn new_managed_driver(&self, driver: Box<dyn ManagedDriver>) -> ManagedDriverBridge {
        // Both homes here are the checkout's. `FuseSlot.home` is where the plain
        // sandbox paths it redirects live. The bridge's own `home` holds
        // `stage/`, whose entries are hardlinked and symlinked *into* those
        // sandboxes: in the shared home a hardlink would fail with EXDEV when
        // a worktree is on another filesystem, and a symlink would dangle
        // inside an OCI container, which mounts only the checkout's home.
        let fuse = self.fuse.layered_fs().map(|fs| FuseSlot {
            home: self.checkout_home.to_path_buf(),
            fs,
            fuse_lower: self.fuse.lower.clone(),
            fuse_upper: self.fuse.upper.clone(),
        });
        // The pluginexec-built shell fallback lives in that plugin; the engine
        // just supplies it (driver-support must not depend on pluginexec).
        ManagedDriverBridge::new(
            driver,
            hplugin_exec::pluginexec::Driver::default_exec_shell_fallback(),
            self.cfg.fuse.mode(),
            self.checkout_home.to_path_buf(),
            fuse,
        )
    }
}
