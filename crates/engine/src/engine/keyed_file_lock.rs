//! A cross-process reader/writer lock with one lock file per key.
//!
//! The scratch slot lock ([`ScratchLock`](crate::engine::scratch::ScratchLock))
//! and the credential acquisition lock
//! ([`CredentialLock`](crate::engine::credential::CredentialLock)) are both this:
//! a [`KeyedRWLock`] over `flock(2)` files named `<dir>/<key><suffix>`, or over
//! in-process locks when file locking is off. Each wraps it with its own policy:
//! which accesses it takes, whether a wait can be cancelled, whether a waiter
//! may ask who holds it.

use crate::engine::result_lock::LockBackend;
use hlock::hlock::{Ctoken, FRWLock, FWriteGuard, KeyedRWLock, MemRWLock};
use std::path::{Path, PathBuf};

/// An opaque RAII guard on one key. The concrete guard type is erased because
/// the callers only ever hold and drop it.
pub type KeyedFileGuard = Box<dyn std::any::Any + Send>;

pub(crate) enum KeyedFileLock {
    /// `flock(2)` files, `<dir>/<key><suffix>`. Mutually exclusive across
    /// processes on the same machine.
    Fs {
        lock: KeyedRWLock<String, FRWLock>,
        /// Kept so [`holder_pid`](KeyedFileLock::holder_pid) can find a key's
        /// file by path, the same way `ResultLock::Fs` keeps its own.
        dir: PathBuf,
        suffix: &'static str,
    },
    /// In-process only. Tests, and anything that has opted out of file locking.
    Mem(KeyedRWLock<String, MemRWLock>),
}

/// Path of a key's lock file. One definition, so the file `new` locks and the
/// file [`holder_pid`](KeyedFileLock::holder_pid) probes cannot drift apart.
fn lock_path(dir: &Path, key: &str, suffix: &str) -> PathBuf {
    dir.join(format!("{key}{suffix}"))
}

impl KeyedFileLock {
    /// `suffix` names the lock files: key `k` locks `<dir>/k<suffix>`.
    pub(crate) fn new(backend: LockBackend, dir: PathBuf, suffix: &'static str) -> Self {
        match backend {
            LockBackend::Fs => Self::Fs {
                lock: KeyedRWLock::new({
                    let dir = dir.clone();
                    move |key: &String| FRWLock::new(lock_path(&dir, key, suffix))
                }),
                dir,
                suffix,
            },
            LockBackend::Mem => Self::Mem(KeyedRWLock::new(|_| MemRWLock::default())),
        }
    }

    /// Take the shared guard for `key`, waiting until available.
    pub(crate) async fn read(
        &self,
        key: String,
        ctoken: Ctoken<'_>,
    ) -> anyhow::Result<KeyedFileGuard> {
        Ok(match self {
            Self::Fs { lock, .. } => Box::new(lock.read(key, ctoken).await?),
            Self::Mem(l) => Box::new(l.read(key, ctoken).await?),
        })
    }

    /// Take the exclusive guard for `key`, waiting until available.
    pub(crate) async fn write(
        &self,
        key: String,
        ctoken: Ctoken<'_>,
    ) -> anyhow::Result<KeyedFileGuard> {
        Ok(match self {
            Self::Fs { lock, .. } => Box::new(lock.write(key, ctoken).await?),
            Self::Mem(l) => Box::new(l.write(key, ctoken).await?),
        })
    }

    /// [`write`](Self::write), then stamp this process's pid into the key's lock
    /// file so [`holder_pid`](Self::holder_pid) in a blocked process can name it.
    pub(crate) async fn write_stamped(
        &self,
        key: String,
        ctoken: Ctoken<'_>,
    ) -> anyhow::Result<KeyedFileGuard> {
        Ok(match self {
            Self::Fs { lock, .. } => {
                let guard = lock.write(key, ctoken).await?;
                stamp_pid(guard.get());
                Box::new(guard)
            }
            Self::Mem(l) => Box::new(l.write(key, ctoken).await?),
        })
    }

    /// Another process holding `key` through
    /// [`write_stamped`](Self::write_stamped) right now, when one can be named.
    ///
    /// Best-effort by nature, and in exactly the two ways the result lock's
    /// probe is: it is a snapshot (the holder may release the instant after),
    /// and a pid is a hint for a human, never something the engine acts on.
    ///
    /// `None` for a key held by *readers*: `flock(2)` shared holders are not
    /// stamped and `FLock::is_path_held` reports them as unheld.
    pub(crate) fn holder_pid(&self, key: &str) -> Option<u32> {
        match self {
            Self::Fs { dir, suffix, .. } => {
                let path = lock_path(dir, key, suffix);
                match hlock::hlock::FLock::is_path_held(&path) {
                    // Our own pid means this build is serialized against itself.
                    // Reporting "held by pid <us>" would send the reader hunting
                    // a rogue process, so say nothing and let the renderer fall
                    // back to naming the access mode.
                    Ok(true) => crate::engine::result_lock::read_pid(&path)
                        .filter(|pid| *pid != std::process::id()),
                    Ok(false) => None,
                    Err(err) => {
                        // Without this the diagnostic path is itself
                        // undiagnosable — same reasoning as the result lock's.
                        tracing::debug!(error = %err, key, suffix, "probing lock liveness");
                        None
                    }
                }
            }
            // In-process only, so the holder is always this process and naming
            // it would tell a reader nothing they did not already know.
            Self::Mem(_) => None,
        }
    }
}

/// Stamp this process's pid into a held lock file, so a blocked waiter in
/// another process can name the holder.
///
/// Written through the held guard's already-open file description, and
/// newline-framed for the same reason the result lock frames its own: a shorter
/// pid written over a longer stale one would otherwise leave `<new><stale tail>`
/// — all digits, perfectly parseable, naming a process that is nobody. See
/// `result_lock::read_pid`, which is the reader for both.
///
/// Best-effort: a failed stamp costs a waiter the holder's name and nothing else.
fn stamp_pid(guard: &FWriteGuard) {
    if let Err(err) = guard.write_contents(format!("{}\n", std::process::id()).as_bytes()) {
        tracing::debug!(error = %err, "could not stamp pid into lock file");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;

    /// The scratch and credential locks keep the lock file names they had: a
    /// rename would let a new and an old `heph` take "the same" lock on two
    /// different files. Checked while held: an exclusive release unlinks the
    /// file.
    #[tokio::test]
    async fn lock_files_keep_their_names() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ct = StdCancellationToken::new();

        std::fs::create_dir_all(tmp.path().join("scratch")).expect("mkdir");
        let scratch =
            crate::engine::scratch::ScratchLock::new(LockBackend::Fs, tmp.path().join("scratch"));
        let guard = scratch
            .acquire(
                "slot".to_string(),
                hbuiltins::pluginscratch::Access::Exclusive,
                &ct,
            )
            .await
            .expect("scratch write");
        assert!(tmp.path().join("scratch/slot.scratch.lock").is_file());
        drop(guard);

        std::fs::create_dir_all(tmp.path().join("auth")).expect("mkdir");
        let credential = crate::engine::credential::CredentialLock::new(
            LockBackend::Fs,
            tmp.path().join("auth"),
        );
        let guard = credential.write("key").await.expect("credential write");
        assert!(tmp.path().join("auth/key.auth.lock").is_file());
        drop(guard);
    }

    /// Only a stamped write names its holder; a plain write leaves the file
    /// empty, so the credential lock never carries a pid.
    #[tokio::test]
    async fn only_a_stamped_write_writes_a_pid() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ct = StdCancellationToken::new();
        let lock = KeyedFileLock::new(LockBackend::Fs, tmp.path().to_path_buf(), ".x.lock");

        let plain = lock.write("a".to_string(), &ct).await.expect("write");
        let read = std::fs::read_to_string(tmp.path().join("a.x.lock")).expect("read");
        assert_eq!(read, "", "a plain write stamps nothing");
        drop(plain);

        let stamped = lock
            .write_stamped("b".to_string(), &ct)
            .await
            .expect("write");
        let read = std::fs::read_to_string(tmp.path().join("b.x.lock")).expect("read");
        assert_eq!(read, format!("{}\n", std::process::id()));
        drop(stamped);
    }
}
