//! A target's last failure, kept for the human reading the failure box.
//!
//! When a target's run fails, the engine copies the log the driver returned
//! (its `Log` failure artifact, [`hplugin::error::failure_log`]) into
//! `<home>/failures/<addr-key>/`: a `log.txt`, and a `manifest.json` written
//! **last** naming the address, the revision and when it failed. The failure box
//! renders the tail from the copy and points at it (`full log: <path>`).
//!
//! **Diagnostics only.** Nothing that hashes, selects content or stages into a
//! sandbox ever reads a slot, and a slot is never a cache entry: it is not in the
//! local cache, never uploaded, never part of any key. A slot that changes or
//! disappears changes what a human reads, and nothing else.
//!
//! - **One slot per address** (variant included), filled once per `(addr,
//!   hashin)` run: in `execute`'s failure arm, under the target's execute lock.
//! - **Copied, never moved.** The driver's file stays where it is; the failed
//!   sandbox is kept for diagnostics exactly as before. `std::fs::copy` reflinks
//!   where the filesystem can (`clonefile` on APFS, `copy_file_range` on btrfs and
//!   XFS). A log outside the sandbox — a symlink pointing out of it included — is
//!   not copied.
//! - **Atomic.** The slot is built in a `.tmp-` directory beside it and renamed
//!   into place, so a reader sees the old slot, the new one or none, never a
//!   partial one. A slot without a manifest, or with an unknown version, or
//!   naming another address, reads as absent.
//! - **Cleared when the target next resolves**: on a cache hit, under its result
//!   lock, and when its next run starts, under its execute lock. To keep a warm
//!   run free of a syscall per target, an engine lists `failures/` once and
//!   clears only the slots it saw or filled.
//! - **Never for credential material.** A target whose outputs are material gets
//!   its tail read into memory instead.
//! - **A failed fill never hides the failure**: it warns, and the target's own
//!   error surfaces as it would have.

use crate::engine::driver::outputartifact::{Content, OutputArtifact, Type as ArtifactType};
use hmodel::htaddr::Addr;
use hplugin::error::{FailureLog, LogTail};
use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// The directory under the home that holds every slot.
pub(crate) const FAILURES_DIR: &str = "failures";
/// Name prefix of a slot being built or being discarded. A key never starts
/// with a dot, so no key is ever taken for one.
pub(crate) const TEMP_PREFIX: &str = ".tmp-";
const MANIFEST: &str = "manifest.json";
const LOG: &str = "log.txt";
const MANIFEST_VERSION: u32 = 1;
/// Bytes of the readable part of a key.
const READABLE_MAX: usize = 48;

/// The directory name of `addr`'s slot: a short readable part for a human
/// browsing `failures/`, then a 128-bit digest of the address.
///
/// The digest makes the name injective (up to a collision of the digest, and a
/// reader checks the manifest's address anyway); the readable part is only
/// lowercase `[a-z0-9_]`, so the name is filesystem-safe, cannot be `.` or
/// `..`, never starts with a dot, never differs from another only by case, and
/// stays far under `NAME_MAX`. Flat: never nested by package.
///
/// Not [`Addr::hash_str`]: that feeds the package, name and arguments into
/// the hasher back to back with no separator, so `//a:bc` and `//ab:c` get
/// the same digest. The fields are length-prefixed here.
pub fn addr_key(addr: &Addr) -> String {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut field = |s: &str| {
        h.update(&(s.len() as u64).to_le_bytes());
        h.update(s.as_bytes());
    };
    field(addr.package.as_str());
    field(&addr.name);
    for (k, v) in &addr.args {
        field(k);
        field(v);
    }
    let digest = h.digest128();

    let mut readable = String::with_capacity(READABLE_MAX);
    for c in addr
        .package
        .as_str()
        .chars()
        .chain(std::iter::once(':'))
        .chain(addr.name.chars())
    {
        if readable.len() >= READABLE_MAX {
            break;
        }
        let c = c.to_ascii_lowercase();
        readable.push(if c.is_ascii_lowercase() || c.is_ascii_digit() {
            c
        } else {
            '_'
        });
    }
    format!("{readable}-{digest:032x}")
}

/// What a slot's `manifest.json` holds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SlotManifest {
    pub version: u32,
    /// The full address, variant included — the only place a reader takes it
    /// from; the directory name is not parsed.
    pub addr: String,
    pub hashin: String,
    /// Milliseconds since the Unix epoch.
    pub failed_at: u64,
}

/// The engine's record of a failure's log on the error chain: what
/// `classify_failure` reads the tail from.
///
/// Data, not a cause: its `Display` is empty, so every chain renderer skips it
/// (as for [`hplugin::error::FailureArtifacts`]).
#[derive(Debug, Clone)]
pub struct FailureSlot {
    pub log: Option<FailureLog>,
}

impl fmt::Display for FailureSlot {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

impl std::error::Error for FailureSlot {}

/// The bytes of the log to keep.
enum LogSource {
    File(PathBuf),
    Bytes(Vec<u8>),
}

/// One failed run, as [`FailureSlots::fill`] needs it.
pub(crate) struct Fill<'a> {
    pub addr: &'a Addr,
    pub hashin: &'a str,
    /// Where the driver's log may be: the run's sandbox, and its FUSE
    /// counterparts when the sandbox was redirected. A log anywhere else is
    /// not read.
    pub roots: Vec<PathBuf>,
    pub err: &'a anyhow::Error,
    pub lines: usize,
    /// The target's outputs are credential material: no slot, the tail only.
    pub material: bool,
}

/// The slots of one home, as one engine sees them.
pub struct FailureSlots {
    root: PathBuf,
    /// Slot names this engine knows exist: what `failures/` held when it was
    /// first listed, plus the slots filled since. Listed on first use.
    known: tokio::sync::OnceCell<std::sync::Mutex<HashSet<String>>>,
    #[cfg(any(test, feature = "test-support"))]
    fills: std::sync::atomic::AtomicU64,
}

impl fmt::Debug for FailureSlots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FailureSlots")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl FailureSlots {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(FAILURES_DIR),
            known: tokio::sync::OnceCell::new(),
            #[cfg(any(test, feature = "test-support"))]
            fills: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// `<home>/failures`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory `addr`'s slot lives in, whether or not it exists.
    pub fn slot_dir(&self, addr: &Addr) -> PathBuf {
        self.root.join(addr_key(addr))
    }

    /// The manifest of `addr`'s slot, `None` when there is no complete slot
    /// for it (see [`read_manifest`]).
    pub fn read(&self, addr: &Addr) -> Option<SlotManifest> {
        read_manifest(&self.slot_dir(addr), addr)
    }

    /// How many slots this engine has filled.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fills(&self) -> u64 {
        self.fills.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The slots this engine knows of, listing `failures/` on first use.
    async fn known(&self) -> &std::sync::Mutex<HashSet<String>> {
        self.known
            .get_or_init(|| async {
                let root = self.root.clone();
                std::sync::Mutex::new(hcore::blocking::run(move || list_slots(&root)).await)
            })
            .await
    }

    /// Remove `addr`'s slot, if this engine knows of one. The first call lists
    /// `failures/`; afterwards a target with no slot costs a hash and a set
    /// lookup, never a syscall. Best-effort: a failure to remove is logged.
    pub async fn clear(&self, addr: &Addr) {
        let known = self.known().await;
        let key = {
            let mut set = known
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // The common case, a home with no failures: not even the key.
            if set.is_empty() {
                return;
            }
            let key = addr_key(addr);
            if !set.remove(&key) {
                return;
            }
            key
        };
        let dir = self.root.join(&key);
        let removed = hcore::blocking::run(move || match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            res => res.map_err(|e| (dir, e)),
        })
        .await;
        if let Err((dir, e)) = removed {
            tracing::warn!(addr = %addr, dir = %dir.display(), error = %e, "clearing a failure slot");
        }
    }

    /// Keep the failed run's log and return the context `classify_failure`
    /// reads it back from. Never fails: whatever goes wrong is logged, and the
    /// target's own error is what surfaces.
    pub(crate) async fn fill(&self, fill: Fill<'_>) -> FailureSlot {
        let Some(source) = select_log(fill.addr, fill.err) else {
            return FailureSlot { log: None };
        };
        let key = addr_key(fill.addr);
        let manifest = SlotManifest {
            version: MANIFEST_VERSION,
            addr: fill.addr.format(),
            hashin: fill.hashin.to_owned(),
            failed_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(0),
        };
        let (root, lines, material, roots) =
            (self.root.clone(), fill.lines, fill.material, fill.roots);
        let addr = fill.addr.clone();
        let res = hcore::blocking::run(move || {
            let source = match source {
                LogSource::File(path) => match resolve_in_roots(&path, &roots) {
                    Ok(Some(p)) => LogSource::File(p),
                    Ok(None) => return Filled::Nothing,
                    Err(e) => return Filled::Failed(e.into(), None),
                },
                bytes => bytes,
            };
            if material {
                return match memory_tail(&source, lines) {
                    Ok(t) => Filled::Memory(t),
                    Err(e) => Filled::Failed(e.into(), None),
                };
            }
            match fill_slot(&root, &key, &manifest, &source, |from, to| {
                std::fs::copy(from, to).map(drop)
            }) {
                Ok((path, file)) => Filled::Slot(key, FailureLog::open(path, file, lines)),
                // The slot is lost, the tail need not be.
                Err(e) => Filled::Failed(e, memory_tail(&source, lines).ok()),
            }
        })
        .await;
        let log = match res {
            Filled::Nothing => None,
            Filled::Memory(tail) => Some(FailureLog::in_memory(tail)),
            Filled::Slot(key, log) => {
                #[cfg(any(test, feature = "test-support"))]
                self.fills
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.known()
                    .await
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(key);
                Some(log)
            }
            Filled::Failed(e, tail) => {
                tracing::warn!(addr = %addr, error = %format!("{e:#}"), "keeping the failure log");
                tail.map(FailureLog::in_memory)
            }
        };
        FailureSlot { log }
    }
}

enum Filled {
    Nothing,
    Memory(LogTail),
    Slot(String, FailureLog),
    Failed(anyhow::Error, Option<LogTail>),
}

/// The `Log` artifact to keep, if the failure carries one this module can
/// read. Only `Log` is honored; the first one found is the outermost
/// attachment ([`hplugin::error::with_failure_artifacts`] puts newer ones
/// first), and any other is ignored.
fn select_log(addr: &Addr, err: &anyhow::Error) -> Option<LogSource> {
    let artifacts = hplugin::error::failure_artifacts(err)?;
    let mut chosen: Option<&OutputArtifact> = None;
    for a in &artifacts.0 {
        if a.r#type != ArtifactType::Log {
            tracing::debug!(addr = %addr, r#type = ?a.r#type, name = %a.name, "ignoring a failure artifact that is not a log");
        } else if chosen.is_some() {
            tracing::debug!(addr = %addr, name = %a.name, "ignoring a second log artifact");
        } else {
            chosen = Some(a);
        }
    }
    match &chosen?.content {
        Content::File(f) => Some(LogSource::File(PathBuf::from(&f.source_path))),
        Content::Raw(r) => Some(LogSource::Bytes(r.data.clone())),
        _ => {
            tracing::debug!(addr = %addr, "ignoring a log artifact that is neither a file nor inline bytes");
            None
        }
    }
}

/// `path` with every symlink resolved, if it is a regular file inside one of
/// `roots`. `None` for a missing file — not a second error — or one outside.
fn resolve_in_roots(path: &Path, roots: &[PathBuf]) -> io::Result<Option<PathBuf>> {
    let real = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            tracing::debug!(path = %path.display(), "the failure log is gone");
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    let inside = roots.iter().any(|root| {
        std::fs::canonicalize(root)
            .map(|r| real.starts_with(r))
            .unwrap_or(false)
    });
    if !inside {
        tracing::debug!(path = %path.display(), real = %real.display(), "the failure log is outside the sandbox; not read");
        return Ok(None);
    }
    if !std::fs::metadata(&real)?.is_file() {
        return Ok(None);
    }
    Ok(Some(real))
}

/// The tail of `source`, read into memory.
fn memory_tail(source: &LogSource, lines: usize) -> io::Result<LogTail> {
    match source {
        LogSource::File(path) => {
            use std::os::unix::fs::FileExt as _;
            let file = std::fs::File::open(path)?;
            let len = file.metadata()?.len();
            hplugin::error::read_tail_at(len, |buf, off| file.read_at(buf, off), lines)
        }
        LogSource::Bytes(bytes) => hplugin::error::read_tail_at(
            bytes.len() as u64,
            |buf, off| {
                let start = usize::try_from(off).unwrap_or(usize::MAX);
                let src = bytes.get(start..).unwrap_or_default();
                let k = src.len().min(buf.len());
                buf.get_mut(..k)
                    .unwrap_or_default()
                    .copy_from_slice(src.get(..k).unwrap_or_default());
                Ok(k)
            },
            lines,
        ),
    }
}

/// A directory that is removed on drop unless [`disarm`](Self::disarm)ed:
/// a slot being built, or an old one being discarded.
struct TempDir(Option<PathBuf>);

impl TempDir {
    fn path(&self) -> &Path {
        self.0.as_deref().unwrap_or(Path::new(""))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            drop(std::fs::remove_dir_all(p));
        }
    }
}

fn temp_name(key: &str) -> String {
    format!("{TEMP_PREFIX}{key}-{}", uuid::Uuid::new_v4().as_simple())
}

/// Build the slot `key` under `root` from `source` and rename it into place,
/// replacing any slot already there. Returns the log's final path and the
/// log opened for reading — opened before the rename, so it is this fill's
/// copy whatever replaces the slot afterwards.
///
/// `copy` copies a file (`std::fs::copy` outside tests, which reflinks where
/// it can). Anything that fails before the rename leaves the old slot as it
/// was and the temporary directory removed.
fn fill_slot(
    root: &Path,
    key: &str,
    manifest: &SlotManifest,
    source: &LogSource,
    copy: impl Fn(&Path, &Path) -> io::Result<()>,
) -> anyhow::Result<(PathBuf, std::fs::File)> {
    use anyhow::Context as _;
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let mut temp = TempDir(Some(root.join(temp_name(key))));
    std::fs::create_dir(temp.path())
        .with_context(|| format!("creating {}", temp.path().display()))?;
    let log = temp.path().join(LOG);
    match source {
        LogSource::File(from) => copy(from, &log)
            .with_context(|| format!("copying {} to {}", from.display(), log.display()))?,
        LogSource::Bytes(bytes) => {
            std::fs::write(&log, bytes).with_context(|| format!("writing {}", log.display()))?
        }
    }
    let file = std::fs::File::open(&log).with_context(|| format!("opening {}", log.display()))?;
    // Last: a slot is complete exactly when its manifest exists.
    let json = serde_json::to_vec(manifest).context("encoding the failure manifest")?;
    let manifest_path = temp.path().join(MANIFEST);
    std::fs::write(&manifest_path, json)
        .with_context(|| format!("writing {}", manifest_path.display()))?;

    let dest = root.join(key);
    // Move the old slot aside first: a directory cannot be renamed over a
    // non-empty one. A reader in between sees no slot, never a mixed one.
    let old = TempDir(Some(root.join(temp_name(key))));
    match std::fs::rename(&dest, old.path()) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("moving the old slot {} aside", dest.display()));
        }
    }
    std::fs::rename(temp.path(), &dest)
        .with_context(|| format!("renaming the slot into {}", dest.display()))?;
    temp.disarm();
    drop(old);
    Ok((dest.join(LOG), file))
}

/// The manifest of the slot at `dir`, if it is complete, of a version this
/// heph reads, and for `addr`. Anything else reads as no slot.
pub fn read_manifest(dir: &Path, addr: &Addr) -> Option<SlotManifest> {
    let bytes = std::fs::read(dir.join(MANIFEST)).ok()?;
    let m: SlotManifest = serde_json::from_slice(&bytes).ok()?;
    (m.version == MANIFEST_VERSION && m.addr == addr.format() && dir.join(LOG).is_file())
        .then_some(m)
}

/// The slot names under `root`; temporaries are not slots.
fn list_slots(root: &Path) -> HashSet<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return HashSet::new();
    };
    entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hplugin::error::{log_file_artifact, with_failure_artifacts};

    fn addr(s: &str) -> Addr {
        hmodel::htaddr::parse_addr(s).expect("addr")
    }

    fn manifest_for(a: &Addr) -> SlotManifest {
        SlotManifest {
            version: MANIFEST_VERSION,
            addr: a.format(),
            hashin: "h".into(),
            failed_at: 1,
        }
    }

    fn oracle(bytes: &[u8], n: usize) -> LogTail {
        let s = String::from_utf8_lossy(bytes);
        let (text, start_line) = hplugin::error::last_n_lines_with_start(&s, n);
        LogTail { text, start_line }
    }

    fn tail_of(bytes: &[u8], n: usize) -> LogTail {
        memory_tail(&LogSource::Bytes(bytes.to_vec()), n).expect("tail")
    }

    #[test]
    fn addr_key_is_injective_and_fs_safe() {
        let long = format!("//p:{}", "x".repeat(300));
        let pairs = [
            ("//p:a", "//p:ab"),
            ("//a/b:c", "//a_b:c"),
            ("//a:bc", "//ab:c"),
            ("//p:a@k=v", "//p:a"),
            ("//p:A", "//p:a"),
            ("//P:a", "//p:a"),
            ("//:a", "//_:a"),
            ("//p:é", "//p:e"),
            (long.as_str(), "//p:x"),
        ];
        for (l, r) in pairs {
            let (Ok(la), Ok(ra)) = (hmodel::htaddr::parse_addr(l), hmodel::htaddr::parse_addr(r))
            else {
                continue;
            };
            assert_ne!(addr_key(&la), addr_key(&ra), "{l} vs {r}");
        }
        for s in [
            "//p:a",
            "//:a",
            "//a/b/c:d",
            "//p:a@k=v",
            "//p:.hidden",
            "//p:..",
            "//p:é",
            long.as_str(),
        ] {
            let Ok(a) = hmodel::htaddr::parse_addr(s) else {
                continue;
            };
            let k = addr_key(&a);
            assert!(k.len() < 255, "{s}: {}", k.len());
            assert!(!k.starts_with('.'), "{s}: {k}");
            assert!(k != "." && k != "..");
            assert!(
                k.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'_'
                    || b == b'-'),
                "{s}: {k}"
            );
            assert_eq!(k, addr_key(&a), "stable");
        }
    }

    #[test]
    fn failure_tail_cases() {
        let cases: [(&str, &[u8]); 9] = [
            ("non-utf8", b"a\n\xff\xfe\nb\n"),
            ("empty", b""),
            ("no trailing newline", b"a\nb\nc"),
            ("fewer than n", b"one\ntwo\n"),
            ("cr only", b"a\rb\rc\r"),
            ("crlf", b"a\r\nb\r\n"),
            ("ansi", b"\x1b[31mred\x1b[0m\nplain\n"),
            ("only newlines", b"\n\n\n\n"),
            ("many", b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n"),
        ];
        for (name, bytes) in cases {
            for n in [0, 1, 3, 10] {
                assert_eq!(tail_of(bytes, n), oracle(bytes, n), "{name}, n={n}");
            }
        }
    }

    #[test]
    fn tail_reads_only_end_of_large_log() {
        // 200k lines: the backward pass must hold the tail, not the log.
        let big: Vec<u8> = (1..=200_000)
            .flat_map(|i| format!("line {i}\n").into_bytes())
            .collect();
        let mut furthest_back = u64::MAX;
        let (cut, window) = hplugin::error::tail_window(
            big.len() as u64,
            &mut |buf: &mut [u8], off: u64| {
                furthest_back = furthest_back.min(off);
                let start = usize::try_from(off).expect("off");
                let k = buf.len().min(big.len() - start);
                buf[..k].copy_from_slice(&big[start..start + k]);
                Ok(k)
            },
            10,
        )
        .expect("window");
        assert!(window.len() < 200, "held {} bytes", window.len());
        assert!(
            big.len() as u64 - furthest_back <= 8 * 1024,
            "read back {furthest_back}"
        );
        assert_eq!(cut + window.len() as u64, big.len() as u64);
        let tail = tail_of(&big, 10);
        assert_eq!(tail, oracle(&big, 10));
        assert_eq!(tail.start_line, 199_991);
    }

    #[test]
    fn slot_without_manifest_is_absent() {
        let home = tempfile::tempdir().expect("tempdir");
        let slots = FailureSlots::new(home.path());
        let a = addr("//p:a");
        let dir = slots.slot_dir(&a);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(LOG), "x").expect("log");
        assert_eq!(slots.read(&a), None, "no manifest");
        let mut m = manifest_for(&a);
        m.version = 99;
        std::fs::write(dir.join(MANIFEST), serde_json::to_vec(&m).expect("json")).expect("m");
        assert_eq!(slots.read(&a), None, "unknown version");
        m = manifest_for(&addr("//p:other"));
        std::fs::write(dir.join(MANIFEST), serde_json::to_vec(&m).expect("json")).expect("m");
        assert_eq!(slots.read(&a), None, "another address");
        std::fs::write(
            dir.join(MANIFEST),
            serde_json::to_vec(&manifest_for(&a)).expect("j"),
        )
        .expect("m");
        assert_eq!(slots.read(&a), Some(manifest_for(&a)));
    }

    #[test]
    fn slot_fill_is_atomic() {
        let home = tempfile::tempdir().expect("tempdir");
        let root = home.path().join(FAILURES_DIR);
        let a = addr("//p:a");
        let key = addr_key(&a);
        fill_slot(
            &root,
            &key,
            &manifest_for(&a),
            &LogSource::Bytes(b"old\n".to_vec()),
            |_, _| Ok(()),
        )
        .expect("first fill");
        let src = home.path().join("src.txt");
        std::fs::write(&src, "new\n").expect("src");
        let err = fill_slot(
            &root,
            &key,
            &manifest_for(&a),
            &LogSource::File(src),
            |from, to| {
                // Half a copy, then the disk fills.
                std::fs::write(to, &std::fs::read(from)?[..2])?;
                Err(io::Error::other("ENOSPC"))
            },
        );
        assert!(err.is_err());
        assert_eq!(
            std::fs::read(root.join(&key).join(LOG)).expect("old log"),
            b"old\n"
        );
        let names: Vec<String> = std::fs::read_dir(&root)
            .expect("ls")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [key], "no temporary left behind");
    }

    #[test]
    fn failure_slot_copies_never_moves() {
        let home = tempfile::tempdir().expect("tempdir");
        let src = home.path().join("log.txt");
        std::fs::write(&src, "kept\n").expect("src");
        let a = addr("//p:a");
        let (path, _file) = fill_slot(
            &home.path().join(FAILURES_DIR),
            &addr_key(&a),
            &manifest_for(&a),
            &LogSource::File(src.clone()),
            |f, t| std::fs::copy(f, t).map(drop),
        )
        .expect("fill");
        assert_eq!(std::fs::read(&src).expect("source still there"), b"kept\n");
        assert_eq!(std::fs::read(&path).expect("copy"), b"kept\n");
    }

    fn raw_log(data: &[u8]) -> OutputArtifact {
        OutputArtifact {
            group: String::new(),
            name: LOG.into(),
            r#type: ArtifactType::Log,
            content: Content::Raw(crate::engine::driver::outputartifact::ContentRaw {
                data: data.to_vec(),
                path: LOG.into(),
                x: false,
            }),
            hashout: String::new(),
        }
    }

    async fn fill_with(
        slots: &FailureSlots,
        sandbox: &Path,
        artifacts: Vec<OutputArtifact>,
    ) -> Option<String> {
        let a = addr("//p:a");
        let err = with_failure_artifacts(anyhow::anyhow!("boom"), artifacts);
        let slot = slots
            .fill(Fill {
                addr: &a,
                hashin: "h",
                roots: vec![sandbox.to_path_buf()],
                err: &err,
                lines: 10,
                material: false,
            })
            .await;
        slot.log.map(|l| l.tail().expect("tail").text)
    }

    #[tokio::test]
    async fn failure_artifact_kinds() {
        let home = tempfile::tempdir().expect("home");
        let sandbox = tempfile::tempdir().expect("sandbox");
        let outside = tempfile::tempdir().expect("outside");
        let slots = FailureSlots::new(home.path());
        let in_log = sandbox.path().join("log.txt");
        std::fs::write(&in_log, "in sandbox\n").expect("log");
        let empty = sandbox.path().join("empty.txt");
        std::fs::write(&empty, "").expect("empty");
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "outside\n").expect("secret");
        let link = sandbox.path().join("link.txt");
        std::os::unix::fs::symlink(&secret, &link).expect("symlink");
        let as_type = |t| {
            let mut a = log_file_artifact(&in_log).expect("artifact");
            a.r#type = t;
            a
        };
        let file = |p: &Path| log_file_artifact(p).expect("artifact");

        let cases: Vec<(&str, Vec<OutputArtifact>, Option<&str>)> = vec![
            ("output", vec![as_type(ArtifactType::Output)], None),
            ("support", vec![as_type(ArtifactType::SupportFile)], None),
            ("empty", vec![], None),
            ("log file", vec![file(&in_log)], Some("in sandbox")),
            ("zero-byte", vec![file(&empty)], Some("")),
            (
                "missing file",
                vec![file(&sandbox.path().join("gone.txt"))],
                None,
            ),
            ("symlink out of sandbox", vec![file(&link)], None),
            ("raw", vec![raw_log(b"inline\n")], Some("inline")),
            (
                "duplicate log",
                vec![raw_log(b"outer\n"), file(&in_log)],
                Some("outer"),
            ),
        ];
        for (name, artifacts, want) in cases {
            assert_eq!(
                fill_with(&slots, sandbox.path(), artifacts)
                    .await
                    .as_deref(),
                want,
                "{name}"
            );
        }
        assert_eq!(std::fs::read(&secret).expect("untouched"), b"outside\n");
    }

    /// Credential material never gets a slot: the tail is read into memory.
    #[tokio::test]
    async fn material_target_keeps_tail_in_memory() {
        let home = tempfile::tempdir().expect("home");
        let sandbox = tempfile::tempdir().expect("sandbox");
        let log = sandbox.path().join("log.txt");
        std::fs::write(&log, "producer said\n").expect("log");
        let slots = FailureSlots::new(home.path());
        let a = addr("//auth:fetch");
        let err = with_failure_artifacts(
            anyhow::anyhow!("boom"),
            vec![log_file_artifact(&log).expect("artifact")],
        );
        let slot = slots
            .fill(Fill {
                addr: &a,
                hashin: "h",
                roots: vec![sandbox.path().to_path_buf()],
                err: &err,
                lines: 10,
                material: true,
            })
            .await;
        let log = slot.log.expect("a tail");
        assert!(log.path().is_none());
        assert_eq!(log.tail().expect("tail").text, "producer said");
        assert!(!slots.root().exists(), "no slot, not even the directory");
    }

    #[tokio::test]
    async fn slot_fill_failure_keeps_target_error() {
        let home = tempfile::tempdir().expect("home");
        // `failures` is a file: no slot can be made under it.
        std::fs::write(home.path().join(FAILURES_DIR), "").expect("block");
        let sandbox = tempfile::tempdir().expect("sandbox");
        let slots = FailureSlots::new(home.path());
        let a = addr("//p:a");
        let err = with_failure_artifacts(
            anyhow::anyhow!("the target's own error"),
            vec![raw_log(b"why\n")],
        );
        let slot = slots
            .fill(Fill {
                addr: &a,
                hashin: "h",
                roots: vec![sandbox.path().to_path_buf()],
                err: &err,
                lines: 10,
                material: false,
            })
            .await;
        let log = slot.log.clone().expect("the tail, in memory");
        assert!(log.path().is_none());
        assert_eq!(log.tail().expect("tail").text, "why");
        let e = err.context(slot);
        assert_eq!(
            hcore::hmemoizer::chain_message(&e),
            "the target's own error"
        );
    }
}
