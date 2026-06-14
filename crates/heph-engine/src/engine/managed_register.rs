//! Engine-side wiring for managed drivers. Lives in the engine (not
//! `heph-driver-support`) because it reads engine state (the FUSE runtime) and
//! supplies the default shell fallback built from `pluginexec` — a dependency
//! the contract-level driver-support crate must not have.

use crate::engine::Engine;
use heph_driver_support::driver_managed::{FuseSlot, ManagedDriver, ManagedDriverBridge};

impl Engine {
    pub fn new_managed_driver(&self, driver: Box<dyn ManagedDriver>) -> ManagedDriverBridge {
        let fuse = self.fuse.layered_fs().map(|fs| FuseSlot {
            home: self.home.clone(),
            fs,
            fuse_lower: self.fuse.lower.clone(),
            fuse_upper: self.fuse.upper.clone(),
        });
        // The pluginexec-built shell fallback lives in that plugin; the engine
        // just supplies it (driver-support must not depend on pluginexec).
        ManagedDriverBridge::new(
            driver,
            heph_plugin_exec::pluginexec::Driver::default_exec_shell_fallback(),
            self.cfg.fuse,
            fuse,
        )
    }
}
