//! Lease table: holds the artifact read-guards alive across a `result()`
//! callback so the plugin can pull bytes lazily. Entries are dropped on
//! `release_lease` (SDK auto-fires on `Content` drop) or on disconnect.

use hcore::hartifactcontent::Content;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct LeaseTable {
    next: AtomicU64,
    leases: Mutex<HashMap<String, Vec<Arc<dyn Content>>>>,
}

impl LeaseTable {
    /// Store the artifacts for one result() callback, returning the lease id.
    pub fn insert(&self, artifacts: Vec<Arc<dyn Content>>) -> String {
        let id = format!("L{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.leases
            .lock()
            .expect("lease table")
            .insert(id.clone(), artifacts);
        id
    }

    /// Fetch the `idx`th artifact of a lease (for `open_artifact`).
    pub fn get(&self, lease_id: &str, idx: usize) -> Option<Arc<dyn Content>> {
        self.leases
            .lock()
            .expect("lease table")
            .get(lease_id)
            .and_then(|v| v.get(idx).cloned())
    }

    /// Drop all guards held under `lease_id`.
    pub fn release(&self, lease_id: &str) {
        self.leases.lock().expect("lease table").remove(lease_id);
    }
}
