//! Where acquired material lives, and for how long.
//!
//! Two tiers, both keyed on a *resolution key* rather than on the address alone:
//!
//! - **Per process**, so two hundred consumers of one credential cause one
//!   acquisition. This matters: an `exec` source shelling out to a vendor CLI
//!   costs the better part of a second.
//! - **Across processes**, under `<home>/auth/` at mode `0700` with entries at
//!   `0600`, so signing in once a day means signing in once a day. This is what
//!   `~/.aws/cli/cache` already is — heph is not inventing a mechanism, only
//!   owning one.
//!
//! No daemon. Same reasoning as the local build cache: a disk store plus the
//! vendor CLIs' own caches already deliver the property people want.
//!
//! # Two rules about what is written
//!
//! **Material with no expiry is never written to disk.** A one-hour session token
//! cached for an hour is a convenience; a long-lived API token written to
//! `<home>/auth/` is a durable secret at rest that nothing will ever clean up.
//! The rule falls out anyway from a separate fact — material with no expiry has no
//! defined cache lifetime — and the two reasons agree.
//!
//! **The key covers the chosen source and the identity that acquired it**, not
//! just the address. Two sources of one credential yield different material, and
//! a derived credential minted under one root identity must not be served to a
//! run holding another.

use anyhow::Context as _;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// On-disk layout version. Bumping it orphans old entries rather than
/// misreading them.
const STORE_FORMAT: u32 = 1;

/// How much of a credential's remaining life must be left for it to be handed to
/// a target.
///
/// Covers clock skew between the issuer and this host, plus the gap between
/// handing material to a target and the target actually using it. A minute is
/// generous for both and cheap: it costs at most one extra acquisition per
/// credential lifetime.
///
/// A credential whose *whole* life is shorter than this is still usable — see
/// [`Material::usable_at`] — because refusing to use a 30-second token would make
/// the feature unusable for exactly the case (Vault's Kubernetes secrets engine)
/// that most needs a callback presentation.
pub const SKEW_MARGIN: Duration = Duration::from_secs(60);

/// Acquired credential material.
///
/// The one type in heph that holds a secret. It is never hashed, never packed
/// into an artifact, never put on the event stream, never printed by `inspect`,
/// and its `Debug` deliberately shows only field *names*.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Material {
    /// Named string values, reachable from a presentation as `${<name>}`.
    pub fields: BTreeMap<String, String>,
    /// Named files, reachable as `${file:<name>}`. Either a host path a
    /// `passthrough` source exposed in place, or a file this store owns.
    pub files: BTreeMap<String, PathBuf>,
    /// Absolute expiry, in Unix seconds. `None` means the source reported none
    /// and the declaration set no `ttl`.
    pub expires_at: Option<u64>,
}

/// Names only. A `{material:?}` in a log line, a span field or a panic message
/// must never be the thing that leaks a token, and the only way to guarantee that
/// is for the formatting not to have the value in it.
impl std::fmt::Debug for Material {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Material")
            .field("fields", &self.fields.keys().collect::<Vec<_>>())
            .field("files", &self.files.keys().collect::<Vec<_>>())
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Material {
    /// Whether this material may still be handed to a target at `now`.
    ///
    /// Material with no expiry is always usable — that is what "no expiry" means
    /// — and separately is never written to disk, so an unbounded credential is
    /// re-acquired once per process rather than trusted forever.
    ///
    /// The margin shrinks for genuinely short-lived material: a token whose total
    /// life is under a minute would otherwise be born unusable, and those are
    /// exactly the credentials a callback presentation exists to serve.
    pub fn usable_at(&self, now: SystemTime, acquired_at: SystemTime) -> bool {
        let Some(expires_at) = self.expires_at else {
            return true;
        };
        let expiry = UNIX_EPOCH + Duration::from_secs(expires_at);
        let lifetime = expiry.duration_since(acquired_at).unwrap_or(Duration::ZERO);
        let margin = SKEW_MARGIN.min(lifetime / 2);
        expiry
            .checked_sub(margin)
            .is_some_and(|usable_until| now < usable_until)
    }

    /// Every secret string in this material, for the log redactor.
    ///
    /// Field values only: a file *path* is not a secret and redacting it would
    /// mangle ordinary output. What is in the file is the file's problem, and
    /// files are `0600` inside a sandbox that is deleted at run end.
    pub fn secrets(&self) -> impl Iterator<Item = &str> {
        self.fields.values().map(String::as_str)
    }
}

/// A hit on the disk tier.
///
/// Carries the entry's **own** acquisition time, not the read time: the skew
/// margin is a fraction of the material's lifetime, so substituting "now" would
/// silently shrink it for exactly the entries that have been on disk longest.
#[derive(Debug)]
pub struct Hit {
    pub material: Material,
    /// The winning source's label, for `heph auth explain`.
    pub source: String,
    pub acquired_at: SystemTime,
}

/// A credential's entry as it sits on disk.
#[derive(serde::Serialize, serde::Deserialize)]
struct Entry {
    format: u32,
    /// The address, for `heph auth status` and for a human reading the directory.
    addr: String,
    /// Which source produced this, for `heph auth explain`.
    source: String,
    /// Unix seconds.
    acquired_at: u64,
    material: Material,
}

/// The resolution key an entry is stored under.
///
/// Covers the address, the chosen source's configuration, and the keys of every
/// credential that source itself needed. That last part is what stops a derived
/// credential minted under one root identity from being served to a run holding
/// another — the failure it prevents is a build silently acting as the wrong
/// principal, which no amount of later auditing recovers from.
pub fn resolution_key(addr: &str, source_config: &str, parents: &[String]) -> String {
    use std::hash::{Hash as _, Hasher as _};
    let mut h = xxhash_rust::xxh3::Xxh3Default::new();
    STORE_FORMAT.hash(&mut h);
    addr.hash(&mut h);
    source_config.hash(&mut h);
    for p in parents {
        p.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// `<home>/auth`, the root of the on-disk tier.
pub fn store_root(home: &Path) -> PathBuf {
    home.join("auth")
}

/// The disk tier.
///
/// A plain directory of `0600` JSON files under a `0700` directory. Deliberately
/// **not** a keychain: there is no consent dialog to block an unattended agent, no
/// dependency on a session bus a CI container does not have, and no re-prompt
/// every time a locally built binary changes its code signature. It is also what
/// the AWS, gcloud and Azure CLIs all do — and it is the choice that removes a
/// per-platform divergence rather than creating one.
pub struct CredentialStore {
    root: PathBuf,
}

impl CredentialStore {
    pub fn new(home: &Path) -> Self {
        Self {
            root: store_root(home),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.json"))
    }

    /// Where a key's **durable** files live: a producer target's outputs, for
    /// material that carries an expiry and may therefore be reused across
    /// processes.
    pub fn files_dir(&self, key: &str) -> PathBuf {
        self.root.join("files").join(key)
    }

    /// Where a key's files live until it is known whether they may be kept.
    ///
    /// Named by pid, and swept of dead pids by the next `heph` that touches the
    /// store — the same shape the scratch audit directories use. That is what
    /// lets unbounded producer material exist at all without breaking the
    /// no-expiry rule: the rule is against a durable secret at rest that nothing
    /// will ever clean up, and this has a defined end.
    pub fn staged_files_dir(&self, key: &str) -> PathBuf {
        self.root
            .join("files")
            .join("live")
            .join(std::process::id().to_string())
            .join(key)
    }

    /// Remove staged files belonging to processes that are gone.
    ///
    /// Best-effort and cheap: one `read_dir` plus a `kill(pid, 0)` per entry,
    /// run once per process on the first staging.
    pub fn sweep_dead_staged(&self) {
        let live = self.root.join("files").join("live");
        let Ok(entries) = std::fs::read_dir(&live) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Ok(pid) = name.to_string_lossy().parse::<i32>() else {
                continue;
            };
            if pid == std::process::id() as i32 {
                continue;
            }
            // SAFETY: `kill` with signal 0 performs the permission check and
            // returns, sending nothing. It takes a plain `pid_t` and touches no
            // memory we own. ESRCH means no such process; EPERM means it exists
            // and belongs to someone else, which still counts as alive.
            let alive = unsafe { libc::kill(pid, 0) } == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
            if !alive {
                drop(std::fs::remove_dir_all(entry.path()));
            }
        }
    }

    /// Move a key's staged files into their durable home.
    ///
    /// Called only once the material is known to carry an expiry — which is the
    /// point at which it is allowed to outlive this process.
    pub fn promote_staged(&self, key: &str) -> anyhow::Result<PathBuf> {
        let from = self.staged_files_dir(key);
        let to = self.files_dir(key);
        drop(std::fs::remove_dir_all(&to));
        if let Some(parent) = to.parent() {
            ensure_private_dir(parent)?;
        }
        std::fs::rename(&from, &to)
            .with_context(|| format!("promote {} to {}", from.display(), to.display()))?;
        Ok(to)
    }

    /// Create the store root at `0700`, tightening it if it already exists with
    /// looser permissions.
    ///
    /// Tightening rather than trusting: a directory created by an older heph, or
    /// by a `mkdir -p` in someone's setup script, would otherwise be world-readable
    /// and nothing would ever say so.
    pub fn ensure_root(&self) -> anyhow::Result<()> {
        ensure_private_dir(&self.root)
    }

    /// Read a cached entry, if one exists and is still usable.
    ///
    /// A malformed or unreadable entry is treated as absent, not as an error: the
    /// worst case is one extra acquisition, and failing a build because a cache
    /// file got truncated would be a poor trade.
    pub fn get(&self, key: &str, now: SystemTime) -> Option<Hit> {
        let path = self.entry_path(key);
        let raw = std::fs::read(&path).ok()?;
        let entry: Entry = serde_json::from_slice(&raw).ok()?;
        if entry.format != STORE_FORMAT {
            return None;
        }
        let acquired_at = UNIX_EPOCH + Duration::from_secs(entry.acquired_at);
        // An entry with no expiry should not be here at all (see `put`), but if
        // one is — an older heph, a hand-written file — do not honour it: the rule
        // is that unbounded material is not trusted across processes.
        if entry.material.expires_at.is_none() {
            drop(std::fs::remove_file(&path));
            return None;
        }
        if !entry.material.usable_at(now, acquired_at) {
            drop(std::fs::remove_file(&path));
            // The entry's files go with it: they are the same material, and
            // leaving them is exactly the durable-secret-at-rest this store's
            // rules are written against.
            drop(std::fs::remove_dir_all(self.files_dir(key)));
            return None;
        }
        Some(Hit {
            material: entry.material,
            source: entry.source,
            acquired_at,
        })
    }

    /// Write an entry, if it may be written at all.
    ///
    /// Returns whether it was persisted, so a caller can say so in
    /// `heph auth explain` rather than leaving "why is this re-acquired every
    /// time?" unanswerable.
    pub fn put(
        &self,
        key: &str,
        addr: &str,
        source: &str,
        material: &Material,
        acquired_at: SystemTime,
    ) -> anyhow::Result<bool> {
        if material.expires_at.is_none() {
            return Ok(false);
        }
        self.ensure_root()?;
        let entry = Entry {
            format: STORE_FORMAT,
            addr: addr.to_string(),
            source: source.to_string(),
            acquired_at: unix_secs(acquired_at),
            material: material.clone(),
        };
        let body = serde_json::to_vec(&entry).context("serialize credential entry")?;
        write_private_file(&self.entry_path(key), &body)?;
        Ok(true)
    }

    /// Drop every cached entry. `heph auth logout`.
    pub fn clear(&self) -> anyhow::Result<()> {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("clear {}", self.root.display())),
        }
    }
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// Create (or tighten) a directory to `0700`.
pub fn ensure_private_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 {}", dir.display()))?;
    }
    Ok(())
}

/// Write a file at `0600`, creating it with those permissions rather than
/// widening then narrowing.
///
/// The order matters: `write` then `chmod` leaves a window in which the file
/// exists at the umask's permissions with the secret already in it. `OpenOptions`
/// with `mode` closes it, because the mode is applied by `open(2)` itself.
pub fn write_private_file(path: &Path, body: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    // Truncate through a fresh create so a shorter secret cannot leave a tail of
    // a longer previous one behind.
    drop(std::fs::remove_file(path));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    f.write_all(body)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn material(expires_in: Option<u64>, now: SystemTime) -> Material {
        Material {
            fields: BTreeMap::from([("token".to_string(), "s3cr3t".to_string())]),
            files: BTreeMap::new(),
            expires_at: expires_in.map(|d| unix_secs(now) + d),
        }
    }

    #[test]
    fn debug_shows_names_and_never_values() {
        let m = material(Some(3600), SystemTime::now());
        let rendered = format!("{m:?}");
        assert!(rendered.contains("token"), "{rendered}");
        assert!(
            !rendered.contains("s3cr3t"),
            "a Debug impl must not be the thing that leaks a token: {rendered}"
        );
    }

    #[test]
    fn material_is_not_handed_out_inside_the_skew_margin() {
        let now = SystemTime::now();
        let acquired = now - Duration::from_secs(3000);
        // 3600s lifetime, 3000 elapsed: 600 left, well past the margin.
        let m = Material {
            expires_at: Some(unix_secs(acquired) + 3600),
            ..material(None, now)
        };
        assert!(m.usable_at(now, acquired));
        // 30 seconds left is inside the one-minute margin.
        let late = acquired + Duration::from_secs(3570);
        assert!(!m.usable_at(late, acquired));
    }

    #[test]
    fn a_very_short_lived_token_is_still_usable_when_fresh() {
        // Vault's Kubernetes engine leases tokens that live for minutes; a flat
        // margin would make them born unusable, which would rule out exactly the
        // case the callback presentation exists for.
        let now = SystemTime::now();
        let m = Material {
            expires_at: Some(unix_secs(now) + 40),
            ..material(None, now)
        };
        assert!(m.usable_at(now, now));
        assert!(!m.usable_at(now + Duration::from_secs(25), now));
    }

    #[test]
    fn material_with_no_expiry_is_never_written_to_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CredentialStore::new(dir.path());
        let now = SystemTime::now();
        let wrote = store
            .put("k", "//auth:a", "env(T)", &material(None, now), now)
            .expect("put");
        assert!(!wrote, "an unbounded secret must not be left at rest");
        assert!(store.get("k", now).is_none());
    }

    #[test]
    fn a_bounded_entry_round_trips_and_is_dropped_once_expired() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CredentialStore::new(dir.path());
        let now = SystemTime::now();
        assert!(
            store
                .put(
                    "k",
                    "//auth:a",
                    "exec(aws)",
                    &material(Some(3600), now),
                    now
                )
                .expect("put")
        );
        let hit = store.get("k", now).expect("hit");
        assert_eq!(
            hit.material.fields.get("token").map(String::as_str),
            Some("s3cr3t")
        );
        assert_eq!(hit.source, "exec(aws)");
        // The entry's own time, not the read time.
        assert!(hit.acquired_at <= now);
        // Past its expiry, the entry is both a miss and gone.
        assert!(store.get("k", now + Duration::from_secs(3601)).is_none());
        assert!(store.get("k", now).is_none(), "an expired entry is removed");
    }

    #[cfg(unix)]
    #[test]
    fn the_store_is_private_on_disk() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CredentialStore::new(dir.path());
        let now = SystemTime::now();
        store
            .put("k", "//auth:a", "env(T)", &material(Some(60), now), now)
            .expect("put");
        let dmode = std::fs::metadata(store.root())
            .expect("stat root")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o700, "store root must be 0700");
        let fmode = std::fs::metadata(store.root().join("k.json"))
            .expect("stat entry")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(fmode, 0o600, "entries must be 0600");
    }

    #[test]
    fn a_key_separates_sources_and_root_identities() {
        let a = resolution_key("//auth:aws", "exec(aws)", &[]);
        let b = resolution_key("//auth:aws", "oidc(github_actions)", &[]);
        let c = resolution_key("//auth:aws", "exec(aws)", &["parent-1".to_string()]);
        assert_ne!(a, b, "two sources yield different material");
        assert_ne!(
            a, c,
            "material minted under one root identity must not be served to another"
        );
    }

    #[test]
    fn a_truncated_entry_reads_as_absent_rather_than_failing_the_build() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CredentialStore::new(dir.path());
        store.ensure_root().expect("root");
        std::fs::write(store.root().join("k.json"), b"{not json").expect("write");
        assert!(store.get("k", SystemTime::now()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_pre_existing_loose_directory_is_tightened() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let root = store_root(dir.path());
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        CredentialStore::new(dir.path())
            .ensure_root()
            .expect("ensure");
        let mode = std::fs::metadata(&root).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
