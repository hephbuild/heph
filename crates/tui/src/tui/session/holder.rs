//! Naming the session behind a lock holder's pid.
//!
//! The engine knows only the pid stamped on a lock it waits for. Both sides
//! that put a name on it go through [`holder_session`]: a viewer (`heph session
//! list`/`show`) over the entries it already listed, and a run's own progress
//! view through [`HolderNames`], which scans the sessions directory once per
//! distinct pid.

use std::collections::HashMap;
use std::path::Path;

use super::SessionRoot;
use super::id::SessionId;
use super::source::{SessionEntry, list_dir};

/// The live session, other than `own`, whose process is `pid`.
pub fn holder_session<'a>(
    every: &[&'a SessionEntry],
    pid: u32,
    own: Option<SessionId>,
) -> Option<&'a SessionEntry> {
    every
        .iter()
        .copied()
        .find(|h| h.live && Some(h.id) != own && h.known_meta().is_some_and(|m| m.owner_pid == pid))
}

/// The session recorded for each lock holder's pid, as a run's progress view
/// names it.
///
/// [`learn`](Self::learn) resolves a pid the first time it is seen, from a
/// scan of the sessions directory, and keeps the answer — a session or none —
/// for the rest of the run: the render path only ever reads the cache
/// ([`get`](Self::get)). With no root (no `HOME`, or a view that was never
/// given one) nothing is looked up. An unreadable directory is the same as no
/// session: the notice falls back to the pid, and the build is never failed
/// or held up by it.
#[derive(Debug, Clone, Default)]
pub struct HolderNames {
    root: Option<SessionRoot>,
    /// This process's own session, never named as a holder.
    own: Option<SessionId>,
    known: HashMap<u32, Option<SessionId>>,
}

impl HolderNames {
    pub fn new(root: Option<SessionRoot>, own: Option<SessionId>) -> Self {
        Self {
            root,
            own,
            known: HashMap::new(),
        }
    }

    /// The session holding for `pid`, scanning the directory if this pid has
    /// not been seen before.
    pub fn learn(&mut self, pid: u32) -> Option<SessionId> {
        if let Some(known) = self.known.get(&pid) {
            return *known;
        }
        let found = self
            .root
            .as_ref()
            .and_then(|root| scan(root.path(), pid, self.own));
        self.known.insert(pid, found);
        found
    }

    /// What [`learn`](Self::learn) found for `pid`. Never does IO.
    pub fn get(&self, pid: u32) -> Option<SessionId> {
        self.known.get(&pid).copied().flatten()
    }
}

fn scan(root: &Path, pid: u32, own: Option<SessionId>) -> Option<SessionId> {
    let every = match list_dir(root) {
        Ok(every) => every,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "cannot name a lock holder's session");
            return None;
        }
    };
    let every: Vec<&SessionEntry> = every.iter().collect();
    holder_session(&every, pid, own).map(|h| h.id)
}
