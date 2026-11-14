//! Naming the session behind a lock holder's pid.
//!
//! The engine knows only the pid stamped on a lock it waits for. A viewer
//! (`heph session list`/`show`) names it from the entries it already listed,
//! through [`holder_session`]. A run's own progress view, and an attached one,
//! name it through [`HolderNames`]: the backend's receiver resolves a pid off
//! the reactor before it folds the event that first carries it, and the view
//! only ever reads the answer.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use hcore::events::{BuildEvent, BuildEventKind};

use super::SessionRoot;
use super::format::{SessionMeta, files};
use super::id::SessionId;
use super::lockfile;
use super::source::SessionEntry;

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

/// The holder pid an event carries, if folding it names one.
pub(crate) fn event_holder_pid(ev: &BuildEvent) -> Option<u32> {
    match &ev.kind {
        BuildEventKind::ResultLockWaitStart { holder_pid, .. }
        | BuildEventKind::ScratchLockWaitStart { holder_pid, .. } => *holder_pid,
        _ => None,
    }
}

/// The session recorded for each lock holder's pid, as a run's progress view
/// names it.
///
/// A cheap handle: clones share one cache, so the backend that resolves and
/// the view that renders see the same answers. [`resolve_blocking`] scans the
/// sessions directory the first time a pid is asked about and keeps the answer
/// — a session or none — for the rest of the run; it does IO, so the backend
/// calls it on the blocking pool, never on the reactor. [`get`] only reads the
/// cache, so rendering never does IO. With no root (no `HOME`, or a view that
/// was never given one) nothing is looked up. An unreadable directory is the
/// same as no session: the notice falls back to the pid, and the build is
/// never failed or held up by it.
///
/// [`resolve_blocking`]: Self::resolve_blocking
/// [`get`]: Self::get
#[derive(Debug, Clone, Default)]
pub struct HolderNames {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    root: Option<SessionRoot>,
    /// This process's own session, never named as a holder.
    own: Option<SessionId>,
    known: Mutex<HashMap<u32, Option<SessionId>>>,
}

impl HolderNames {
    pub fn new(root: Option<SessionRoot>, own: Option<SessionId>) -> Self {
        Self {
            inner: Arc::new(Inner {
                root,
                own,
                known: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Whether `pid` still needs a scan: there is a root to look in, and it
    /// has not been resolved yet. Never does IO.
    pub fn needs(&self, pid: u32) -> bool {
        self.inner.root.is_some() && !self.known().contains_key(&pid)
    }

    /// The session holding for `pid`, scanning the sessions directory if this
    /// pid has not been resolved yet. Blocking: reads each session's
    /// `meta.json` and probes the lock of the one whose pid matches, never
    /// `status.json` or `events.ndjson`.
    pub fn resolve_blocking(&self, pid: u32) -> Option<SessionId> {
        if let Some(known) = self.known().get(&pid) {
            return *known;
        }
        let found = self
            .inner
            .root
            .as_ref()
            .and_then(|root| scan(root.path(), pid, self.inner.own));
        *self.known().entry(pid).or_insert(found)
    }

    /// What [`resolve_blocking`](Self::resolve_blocking) found for `pid`.
    /// Never does IO.
    pub fn get(&self, pid: u32) -> Option<SessionId> {
        self.known().get(&pid).copied().flatten()
    }

    fn known(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Option<SessionId>>> {
        self.inner
            .known
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

fn scan(root: &Path, pid: u32, own: Option<SessionId>) -> Option<SessionId> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound {
                tracing::debug!(error = %e, "cannot name a lock holder's session");
            }
            return None;
        }
    };
    entries.filter_map(Result::ok).find_map(|entry| {
        // Not an id (a `.removing-…` tomb, anything else): not a session.
        let id = entry
            .file_name()
            .to_str()
            .and_then(|s| SessionId::parse(s).ok())?;
        if Some(id) == own {
            return None;
        }
        let dir = entry.path();
        // A missing, unreadable or too-new meta.json names no pid.
        let meta = SessionMeta::decode(&std::fs::read(dir.join(files::META)).ok()?).ok()?;
        if meta.owner_pid != pid {
            return None;
        }
        // Only a live session holds a lock; an ended one with a reused pid
        // does not.
        lockfile::is_held(&dir.join(files::LOCK))
            .ok()
            .filter(|live| *live)
            .map(|_| id)
    })
}
