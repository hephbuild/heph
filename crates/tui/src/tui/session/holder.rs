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
use super::format::files;
use super::id::SessionId;
use super::source::SessionEntry;
use super::{lockfile, store};

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
/// calls it off the reactor, never on it. [`get`] only reads the cache, so
/// rendering never does IO. With no root (no `HOME`, or a view that was never
/// given one) nothing is looked up. An unreadable directory is the same as no
/// session: the notice falls back to the pid, and the build is never failed or
/// held up by it.
///
/// A pid is [`claim`]ed before its scan starts, and a claimed pid is never
/// scanned again, even while its scan is still running: a scan that outlives
/// the receiver's wait (a hung `~/.heph` on NFS) is not restarted on every
/// later event. Its answer, if it ever comes, still lands in the cache, and
/// names the pid from then on.
///
/// [`resolve_blocking`]: Self::resolve_blocking
/// [`get`]: Self::get
/// [`claim`]: Self::claim
#[derive(Debug, Clone, Default)]
pub struct HolderNames {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    root: Option<SessionRoot>,
    /// This process's own session, never named as a holder.
    own: Option<SessionId>,
    known: Mutex<HashMap<u32, Answer>>,
    /// A test's stand-in for the directory scan.
    #[cfg(test)]
    fake: Option<FakeLookup>,
}

/// What is known about one pid.
#[derive(Debug, Clone, Copy)]
enum Answer {
    /// Its scan is running (or could not be started): not named, and not
    /// scanned again.
    Resolving,
    /// Its scan finished: the session, or none.
    Found(Option<SessionId>),
}

/// A test's stand-in for the directory scan, so a test can make it slow.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct FakeLookup(pub(crate) Arc<dyn Fn(u32) -> Option<SessionId> + Send + Sync>);

#[cfg(test)]
impl std::fmt::Debug for FakeLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FakeLookup")
    }
}

impl HolderNames {
    pub fn new(root: Option<SessionRoot>, own: Option<SessionId>) -> Self {
        Self {
            inner: Arc::new(Inner {
                root,
                own,
                known: Mutex::new(HashMap::new()),
                #[cfg(test)]
                fake: None,
            }),
        }
    }

    /// Names resolved by `lookup` instead of a directory scan.
    #[cfg(test)]
    pub(crate) fn with_lookup(lookup: FakeLookup) -> Self {
        Self {
            inner: Arc::new(Inner {
                root: None,
                own: None,
                known: Mutex::new(HashMap::new()),
                fake: Some(lookup),
            }),
        }
    }

    fn can_look(&self) -> bool {
        #[cfg(test)]
        if self.inner.fake.is_some() {
            return true;
        }
        self.inner.root.is_some()
    }

    /// Whether `pid` still needs a scan: there is a root to look in, and it
    /// has been neither claimed nor resolved. Never does IO.
    pub fn needs(&self, pid: u32) -> bool {
        self.can_look() && !self.known().contains_key(&pid)
    }

    /// Claim `pid`'s scan: true when it [`needs`](Self::needs) one, and from
    /// then on it no longer does, whether or not the scan ever finishes.
    /// Never does IO.
    pub fn claim(&self, pid: u32) -> bool {
        if !self.can_look() {
            return false;
        }
        let mut known = self.known();
        if known.contains_key(&pid) {
            return false;
        }
        known.insert(pid, Answer::Resolving);
        true
    }

    /// Record what a session's `LockHolder` record says names `pid` (D6): a
    /// viewer names a holder exactly as the run did, with no scan of its own.
    /// Never does IO.
    pub fn insert(&self, pid: u32, session: Option<SessionId>) {
        self.known().insert(pid, Answer::Found(session));
    }

    /// The session holding for `pid`, scanning the sessions directory unless
    /// this pid has been resolved already. Blocking: probes each session's
    /// lock and reads the session row of the live ones, never their records.
    pub fn resolve_blocking(&self, pid: u32) -> Option<SessionId> {
        if let Some(Answer::Found(found)) = self.known().get(&pid) {
            return *found;
        }
        let found = self.lookup(pid);
        match *self.known().entry(pid).or_insert(Answer::Resolving) {
            // Another scan of the pid finished first: keep its answer.
            Answer::Found(first) => first,
            ref mut slot @ Answer::Resolving => {
                *slot = Answer::Found(found);
                found
            }
        }
    }

    fn lookup(&self, pid: u32) -> Option<SessionId> {
        #[cfg(test)]
        if let Some(fake) = &self.inner.fake {
            return (fake.0)(pid);
        }
        self.inner
            .root
            .as_ref()
            .and_then(|root| scan(root.path(), pid, self.inner.own))
    }

    /// What [`resolve_blocking`](Self::resolve_blocking) found for `pid`.
    /// Never does IO.
    pub fn get(&self, pid: u32) -> Option<SessionId> {
        match self.known().get(&pid) {
            Some(Answer::Found(found)) => *found,
            Some(Answer::Resolving) | None => None,
        }
    }

    fn known(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Answer>> {
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
        // Only a live session holds a lock; an ended one with a reused pid
        // does not. Probed first: a live recording is read in place, where
        // an ended unsealed one would be copied to be read.
        if !lockfile::is_held(&dir.join(files::LOCK)).ok()? {
            return None;
        }
        // A missing, unreadable or too-new recording names no pid.
        let meta = store::read_meta_in(root, &dir).ok()?;
        (meta.owner_pid == pid).then_some(id)
    })
}
