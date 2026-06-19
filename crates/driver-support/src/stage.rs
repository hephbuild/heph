//! Shared read-only input staging for the OS-copy sandbox runner.
//!
//! When FUSE is unavailable the OS runner materializes every input by copying
//! its bytes into each consumer's sandbox. For inputs a target only *reads*
//! (tools, hermetic SDKs, …) that copy is pure waste: the same artifact is
//! unpacked once per consuming target. Staging hoists that work out — an
//! artifact is materialized **once** into a content-addressed, read-only stage
//! dir under `<home>/stage/`, and every consumer gets cheap hardlinks into its
//! sandbox instead of a fresh byte-for-byte copy. This is the common-
//! subexpression elimination the FUSE path gets for free via its in-memory
//! union filesystem.
//!
//! The stage dir is marked read-only the way the Go module cache marks
//! downloaded modules read-only (`0o555` dirs, write bits stripped from files)
//! so a buggy consumer can't corrupt the shared copy. Materialization is
//! serialized with an advisory file lock so concurrent consumers (in-process
//! tasks or separate `heph` processes) cooperate: the first writes, the rest
//! reuse.

use anyhow::Context;
use hcore::hartifactcontent::{Content, unpack};
use hcore::hasync::Cancellable;
use hlock::hlock::{FLock, Lock};
use std::path::{Path, PathBuf};

/// Input annotation opting a dep into read-only staging. Value must be the
/// string `"true"`. Set by producers for artifacts the consuming target only
/// reads and never mutates (e.g. exec `tools`). Absent (the default) keeps the
/// legacy copy-into-sandbox behavior.
pub const READ_ONLY_ANNOTATION: &str = "read_only";

/// Map an arbitrary key (a source addr like `//pkg/sub:name`) to a single
/// filesystem-safe path component, so the stage tree stays human-readable.
fn sanitize_component(key: &str) -> String {
    key.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

/// Materialize `content` once into the shared stage and symlink it into
/// `link_root` (the consumer's unpack root inside the sandbox).
///
/// `key_prefix` is a debug-only path segment (typically the source addr); the
/// real cache key is the artifact's content hash, so two producers emitting
/// identical bytes share one stage entry. `list_path`/`filters` mirror
/// [`unpack`]: absolute paths of linked files are appended to the list file,
/// and a non-empty `filters` restricts the exposed set to those exact relative
/// paths. `filters` is an owned slice (not a closure) so the future stays
/// `Send` across the lock-acquire await.
///
/// Falls back to a direct (unshared) unpack into `link_root` when the artifact
/// has no content hash — without a stable key there is nothing to share on.
pub async fn stage_and_link(
    content: &dyn Content,
    stage_root: &Path,
    key_prefix: &str,
    link_root: &Path,
    list_path: Option<&Path>,
    filters: &[String],
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<()> {
    let hashout = content
        .hashout()
        .with_context(|| format!("hashout for staging ({key_prefix})"))?;
    if hashout.is_empty() {
        // No stable key → nothing to dedup on; behave like the copy path. The
        // predicate is built and consumed entirely before the first await, so
        // it never crosses a suspension point.
        let pred = |rel: &Path| filters.iter().any(|f| Path::new(f) == rel);
        let predicate: Option<&dyn Fn(&Path) -> bool> = if filters.is_empty() {
            None
        } else {
            Some(&pred)
        };
        return unpack::unpack(content, link_root, list_path, predicate)
            .with_context(|| format!("unshared unpack into {:?} (no content hash)", link_root));
    }

    let group_dir = stage_root.join(sanitize_component(key_prefix));
    let entry = group_dir.join(&hashout);
    let ready = group_dir.join(format!("{hashout}.ready"));
    let lock_path = group_dir.join(format!("{hashout}.lock"));

    // Fast path: a prior consumer already materialized this artifact.
    if !ready.exists() {
        std::fs::create_dir_all(&group_dir)
            .with_context(|| format!("create stage group dir {:?}", group_dir))?;
        let lock = FLock::new(&lock_path);
        let _guard = lock
            .lock(ctoken)
            .await
            .with_context(|| format!("acquire stage lock {:?}", lock_path))?;
        // Re-check under the lock: another writer may have finished while we
        // waited. The `.ready` marker is the witness, written last.
        if !ready.exists() {
            materialize(content, &entry)
                .with_context(|| format!("materialize stage entry {:?}", entry))?;
            std::fs::write(&ready, b"")
                .with_context(|| format!("write stage ready marker {:?}", ready))?;
        }
    }

    link_tree(&entry, link_root, list_path, filters)
        .with_context(|| format!("link staged {:?} into {:?}", entry, link_root))
}

/// Unpack `content` into a fresh `entry` and strip write permissions from the
/// whole tree. Any partial leftover from a crashed prior attempt is removed
/// first — `.ready` is absent here (checked under the lock), so whatever sits
/// at `entry` is untrustworthy.
fn materialize(content: &dyn Content, entry: &Path) -> anyhow::Result<()> {
    match hcore::fsutil::remove_dir_all(entry) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("clear partial stage entry {:?}", entry));
        }
    }
    std::fs::create_dir_all(entry).with_context(|| format!("create stage entry {:?}", entry))?;
    unpack::unpack(content, entry, None, None)
        .with_context(|| format!("unpack into stage entry {:?}", entry))?;
    // Publish read-only (Go-module-cache style) so no consumer can corrupt the
    // shared copy. `make_readonly_tree` lives next to its inverse
    // `make_readwrite_tree` (used by GC teardown) in `hcore::fsutil`.
    hcore::fsutil::make_readonly_tree(entry)
        .with_context(|| format!("mark stage entry read-only {:?}", entry))?;
    Ok(())
}

/// Replicate the directory structure of `entry` inside `link_root`,
/// **hardlinking** every staged file into place so the consumer sees a plain
/// regular file (no symlink resolution, sharing the staged inode and its
/// read-only mode). Staged symlinks are recreated as symlinks — a hardlink to
/// a symlink has inconsistent cross-platform semantics. Directories are created
/// as real dirs so multiple inputs can merge into a shared `link_root`. The
/// stage and the sandbox both live under `<home>`, i.e. one filesystem, so
/// `hard_link` never hits `EXDEV`. Parents are visited before children
/// (walkdir's default order) so each link's parent dir already exists.
#[cfg(unix)]
fn link_tree(
    entry: &Path,
    link_root: &Path,
    list_path: Option<&Path>,
    filters: &[String],
) -> anyhow::Result<()> {
    use std::io::Write as _;

    std::fs::create_dir_all(link_root)
        .with_context(|| format!("create link root {:?}", link_root))?;

    let mut list = match list_path {
        Some(p) => Some(std::io::BufWriter::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .with_context(|| format!("open list file {:?} (append)", p))?,
        )),
        None => None,
    };

    for ent in walkdir::WalkDir::new(entry) {
        let ent = ent.with_context(|| format!("walk staged tree {:?}", entry))?;
        let rel = match ent.path().strip_prefix(entry) {
            Ok(r) if r.as_os_str().is_empty() => continue, // the root itself
            Ok(r) => r,
            Err(e) => return Err(e).with_context(|| format!("strip {:?}", ent.path())),
        };
        let dst = link_root.join(rel);
        if ent.file_type().is_dir() {
            std::fs::create_dir_all(&dst)
                .with_context(|| format!("create linked dir {:?}", dst))?;
            continue;
        }
        if !filters.is_empty() && !filters.iter().any(|f| Path::new(f) == rel) {
            continue;
        }
        // A prior input may have linked the same relative path into a shared
        // root; the staged bytes are identical, so replace it.
        link_one(ent.path(), &dst, ent.file_type().is_symlink())
            .with_context(|| format!("link {:?} -> {:?}", dst, ent.path()))?;
        if let Some(list) = list.as_mut() {
            writeln!(list, "{}", dst.display()).with_context(|| "append to stage list file")?;
        }
    }
    Ok(())
}

/// Materialize one staged entry at `dst`: hardlink a regular file, or recreate
/// a symlink (copying its target). Replaces any existing entry at `dst` so
/// inputs sharing a `link_root` can overwrite each other's identical bytes.
#[cfg(unix)]
fn link_one(staged: &Path, dst: &Path, is_symlink: bool) -> anyhow::Result<()> {
    let attempt = || -> std::io::Result<()> {
        if is_symlink {
            let target = std::fs::read_link(staged)?;
            std::os::unix::fs::symlink(target, dst)
        } else {
            std::fs::hard_link(staged, dst)
        }
    };
    match attempt() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(dst).with_context(|| format!("remove stale link {:?}", dst))?;
            attempt().with_context(|| format!("relink {:?} -> {:?}", dst, staged))
        }
        Err(e) => Err(e).with_context(|| format!("link {:?} -> {:?}", dst, staged)),
    }
}

#[cfg(not(unix))]
fn link_tree(
    _entry: &Path,
    _link_root: &Path,
    _list_path: Option<&Path>,
    _filters: &[String],
) -> anyhow::Result<()> {
    anyhow::bail!("read-only input staging is only supported on unix")
}

/// Whether `annotations` opt this input into read-only staging.
pub fn is_read_only(annotations: &std::collections::BTreeMap<String, String>) -> bool {
    annotations
        .get(READ_ONLY_ANNOTATION)
        .is_some_and(|v| v == "true")
}

/// Path component helper exposed for callers that want to predict where a key
/// lands (tests, diagnostics).
pub fn stage_group_for(stage_root: &Path, key_prefix: &str) -> PathBuf {
    stage_root.join(sanitize_component(key_prefix))
}

/// Delete every staged entry under `stage_root`. Stage entries are a pure cache
/// — a future consumer re-materializes whatever it needs — so GC just clears
/// them all. Each `<group>/<hash>` entry is removed under its advisory lock so
/// an in-flight materialization is never deleted mid-write; a contended entry
/// is left for the next sweep. Best-effort: per-entry failures are logged and
/// skipped. Returns `(entries_removed, bytes_freed)`.
pub fn clear_stage(stage_root: &Path) -> (usize, u64) {
    let groups = match std::fs::read_dir(stage_root) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (0, 0),
        Err(e) => {
            tracing::warn!(error = %e, path = %stage_root.display(), "clear_stage: read stage root");
            return (0, 0);
        }
    };
    let mut removed = 0usize;
    let mut bytes = 0u64;
    for group in groups.flatten() {
        if !group.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let gpath = group.path();
        let entries = match std::fs::read_dir(&gpath) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, path = %gpath.display(), "clear_stage: read group");
                continue;
            }
        };
        for ent in entries.flatten() {
            // Only the `<hash>` dirs are entries; the `.ready`/`.lock` sidecars
            // are reclaimed alongside their dir.
            if !ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let hash = ent.file_name().to_string_lossy().into_owned();
            // Non-blocking: a held lock means a materializer is mid-write — skip
            // and let the next sweep reclaim it.
            let lock = FLock::new(gpath.join(format!("{hash}.lock")));
            let _guard = match lock.try_lock() {
                Ok(Some(g)) => g,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), %hash, "clear_stage: lock entry");
                    continue;
                }
            };
            let entry_dir = ent.path();
            let freed = dir_size(&entry_dir);
            if let Err(e) = hcore::fsutil::remove_dir_all(&entry_dir) {
                tracing::warn!(error = %e, path = %entry_dir.display(), "clear_stage: remove entry");
                continue;
            }
            // Drop the readiness witness; the `.lock` is removed when the guard
            // releases (write-unlock deletes its lock file).
            let ready = gpath.join(format!("{hash}.ready"));
            match std::fs::remove_file(&ready) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(error = %e, path = %ready.display(), "clear_stage: remove ready")
                }
            }
            removed += 1;
            bytes = bytes.saturating_add(freed);
        }
    }
    (removed, bytes)
}

/// Best-effort recursive byte size of `dir` (regular files only), for GC byte
/// accounting. Errors are swallowed — a miscount never blocks a reclaim.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return total;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            total = total.saturating_add(dir_size(&entry.path()));
        } else if ft.is_file()
            && let Ok(md) = entry.metadata()
        {
            total = total.saturating_add(md.len());
        }
    }
    total
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use hcore::hartifactcontent::WalkEntry;
    use hcore::hartifactcontent::tar::{TarPacker, TarWalker};
    use hcore::hasync::StdCancellationToken;
    use std::io::{Cursor, Read};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    /// Content backed by tar bytes with a caller-supplied hashout.
    struct TarBytes {
        bytes: Vec<u8>,
        hash: String,
    }

    impl Content for TarBytes {
        fn reader(&self) -> anyhow::Result<Box<dyn Read>> {
            Ok(Box::new(Cursor::new(self.bytes.clone())))
        }
        fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
            Ok(Box::new(TarWalker::new(Cursor::new(self.bytes.clone()))?))
        }
        fn hashout(&self) -> anyhow::Result<String> {
            Ok(self.hash.clone())
        }
    }

    fn content(hash: &str, files: &[(&str, &str, bool)]) -> TarBytes {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut packer = TarPacker::new();
        for (rel, body, x) in files {
            if *x {
                packer.create_raw(body.as_bytes().to_vec(), *rel, true);
            } else {
                let abs = dir.path().join(rel);
                if let Some(p) = abs.parent() {
                    std::fs::create_dir_all(p).expect("mkdir");
                }
                std::fs::write(&abs, body).expect("write");
                packer.create_file(
                    abs.to_str().expect("utf8 path").to_string(),
                    (*rel).to_string(),
                );
            }
        }
        let mut bytes = Vec::new();
        packer.pack(&mut bytes).expect("pack");
        TarBytes {
            bytes,
            hash: hash.to_string(),
        }
    }

    fn ct() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    #[tokio::test]
    async fn stages_once_and_links_into_two_sandboxes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stage = tmp.path().join("stage");
        let link_a = tmp.path().join("a");
        let link_b = tmp.path().join("b");
        let c = content("deadbeef", &[("pkg/lib.txt", "hello", false)]);

        stage_and_link(&c, &stage, "//pkg:lib", &link_a, None, &[], &ct())
            .await
            .expect("stage a");

        // The staged file's inode after the first materialization.
        let staged = stage.join("__pkg_lib").join("deadbeef").join("pkg/lib.txt");
        let ino1 = std::fs::metadata(&staged).expect("stat staged").ino();

        stage_and_link(&c, &stage, "//pkg:lib", &link_b, None, &[], &ct())
            .await
            .expect("stage b");

        // Second consumer reused the same stage entry (no re-materialize).
        let ino2 = std::fs::metadata(&staged).expect("stat staged").ino();
        assert_eq!(ino1, ino2, "stage entry must be materialized exactly once");

        // Both sandboxes see a plain file hardlinked to the shared staged
        // inode (same ino, not a symlink, content readable).
        for link in [&link_a, &link_b] {
            let p = link.join("pkg/lib.txt");
            let md = std::fs::symlink_metadata(&p).expect("lstat link");
            assert!(md.file_type().is_file(), "{:?} must be a regular file", p);
            assert!(
                !md.file_type().is_symlink(),
                "{:?} must not be a symlink",
                p
            );
            assert_eq!(md.ino(), ino1, "{:?} must hardlink the staged inode", p);
            assert_eq!(std::fs::read_to_string(&p).expect("read"), "hello");
        }
    }

    #[tokio::test]
    async fn sandbox_removal_succeeds_over_readonly_hardlinks() {
        // Regression: a sandbox holds hardlinks to read-only staged files.
        // Removing the sandbox must succeed (unlink needs write on the sandbox
        // dir, not on the read-only inode) and must NOT destroy the staged copy
        // (its inode survives via the remaining stage-side link).
        let tmp = tempfile::tempdir().expect("tempdir");
        let stage = tmp.path().join("stage");
        let sandbox = tmp.path().join("sandbox/ws");
        let c = content("rm1", &[("d/f.txt", "x", true)]); // +x → 0o555 staged

        stage_and_link(&c, &stage, "k", &sandbox, None, &[], &ct())
            .await
            .expect("stage");

        let staged = stage.join("k").join("rm1").join("d/f.txt");
        assert!(sandbox.join("d/f.txt").exists());

        // The OS sandbox cleaner uses exactly this call.
        hcore::fsutil::remove_dir_all(&sandbox).expect("sandbox removal must succeed");

        assert!(!sandbox.exists(), "sandbox gone");
        assert!(staged.exists(), "staged copy survives the sandbox teardown");
    }

    #[tokio::test]
    async fn staged_tree_is_read_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stage = tmp.path().join("stage");
        let link = tmp.path().join("ws");
        let c = content("cafe", &[("d/f.txt", "x", false)]);

        stage_and_link(&c, &stage, "k", &link, None, &[], &ct())
            .await
            .expect("stage");

        let entry = stage.join("k").join("cafe");
        let file_mode = std::fs::metadata(entry.join("d/f.txt"))
            .expect("stat file")
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o222, 0, "staged file must have no write bits");
        let dir_mode = std::fs::metadata(entry.join("d"))
            .expect("stat dir")
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o222, 0, "staged dir must have no write bits");
    }

    #[tokio::test]
    async fn executable_stays_runnable_through_link() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stage = tmp.path().join("stage");
        let link = tmp.path().join("ws");
        let c = content("e0", &[("run.sh", "#!/bin/sh\necho ok\n", true)]);

        stage_and_link(&c, &stage, "tool", &link, None, &[], &ct())
            .await
            .expect("stage");

        let script = link.join("run.sh");
        let out = std::process::Command::new(&script)
            .output()
            .expect("exec linked script");
        assert!(out.status.success(), "script failed: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
    }

    #[tokio::test]
    async fn staged_symlink_is_recreated_as_symlink() {
        use std::os::unix::fs::symlink;
        // Build content holding a file and a relative symlink to it.
        let src = tempfile::tempdir().expect("src");
        std::fs::write(src.path().join("target.txt"), b"hi").expect("write");
        symlink("target.txt", src.path().join("link.txt")).expect("symlink");
        let mut packer = TarPacker::new();
        packer.create_file(
            src.path().join("target.txt").to_str().unwrap().to_string(),
            "target.txt".to_string(),
        );
        packer.create_file(
            src.path().join("link.txt").to_str().unwrap().to_string(),
            "link.txt".to_string(),
        );
        let mut bytes = Vec::new();
        packer.pack(&mut bytes).expect("pack");
        let c = TarBytes {
            bytes,
            hash: "sym1".to_string(),
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let stage = tmp.path().join("stage");
        let link = tmp.path().join("ws");
        stage_and_link(&c, &stage, "k", &link, None, &[], &ct())
            .await
            .expect("stage");

        let l = link.join("link.txt");
        let md = std::fs::symlink_metadata(&l).expect("lstat");
        assert!(
            md.file_type().is_symlink(),
            "staged symlink must stay a symlink"
        );
        assert_eq!(
            std::fs::read_link(&l).expect("readlink"),
            Path::new("target.txt")
        );
        assert_eq!(std::fs::read_to_string(&l).expect("read"), "hi");
    }

    #[tokio::test]
    async fn filters_restrict_linked_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stage = tmp.path().join("stage");
        let link = tmp.path().join("ws");
        let c = content("f1", &[("a.txt", "a", false), ("b.txt", "b", false)]);

        let keep = ["a.txt".to_string()];
        stage_and_link(&c, &stage, "k", &link, None, &keep, &ct())
            .await
            .expect("stage");

        assert!(link.join("a.txt").exists(), "kept file must be linked");
        assert!(
            !link.join("b.txt").try_exists().unwrap(),
            "filtered file must not be linked"
        );
        // Both files still materialized in the (unfiltered) shared stage.
        let entry = stage.join("k").join("f1");
        assert!(entry.join("a.txt").exists());
        assert!(entry.join("b.txt").exists());
    }

    #[tokio::test]
    async fn list_file_records_linked_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stage = tmp.path().join("stage");
        let link = tmp.path().join("ws");
        let list = tmp.path().join("input.list");
        let c = content("a1", &[("x.txt", "x", false)]);

        stage_and_link(&c, &stage, "k", &link, Some(&list), &[], &ct())
            .await
            .expect("stage");

        let body = std::fs::read_to_string(&list).expect("read list");
        assert!(
            body.contains(link.join("x.txt").to_str().unwrap()),
            "list must record the linked path, got: {body}"
        );
    }

    /// Create a stage entry `<root>/<group>/<hash>/blob` plus its `.ready`
    /// witness; returns the entry dir.
    fn make_stage_entry(root: &Path, group: &str, hash: &str) -> PathBuf {
        let gdir = root.join(group);
        let entry = gdir.join(hash);
        std::fs::create_dir_all(&entry).expect("mkdir");
        std::fs::write(entry.join("blob"), b"staged-bytes").expect("blob");
        std::fs::write(gdir.join(format!("{hash}.ready")), b"").expect("ready");
        entry
    }

    #[test]
    fn clear_stage_deletes_all_entries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("stage");
        let a = make_stage_entry(&root, "g1", "aaaa");
        let b = make_stage_entry(&root, "g2", "bbbb");

        let (removed, bytes) = clear_stage(&root);

        assert_eq!(removed, 2, "every entry cleared");
        assert_eq!(bytes, 2 * b"staged-bytes".len() as u64);
        assert!(!a.exists());
        assert!(!b.exists());
        assert!(!root.join("g1/aaaa.ready").exists());
        assert!(!root.join("g1/aaaa.lock").exists());
    }

    #[test]
    fn clear_stage_missing_root_is_noop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(clear_stage(&tmp.path().join("nope")), (0, 0));
    }

    #[tokio::test]
    async fn clear_stage_skips_locked_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("stage");
        let busy = make_stage_entry(&root, "g", "held");

        // Hold the entry's lock: a materializer is mid-write → skip it.
        let lock = FLock::new(root.join("g/held.lock"));
        let _held = lock.lock(&ct()).await.expect("hold lock");

        let (removed, _bytes) = clear_stage(&root);

        assert_eq!(removed, 0, "locked entry not reclaimed");
        assert!(busy.exists(), "in-flight stage entry survives");
    }

    #[tokio::test]
    async fn no_content_hash_falls_back_to_copy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stage = tmp.path().join("stage");
        let link = tmp.path().join("ws");
        let c = content("", &[("x.txt", "x", false)]);

        stage_and_link(&c, &stage, "k", &link, None, &[], &ct())
            .await
            .expect("stage");

        let p = link.join("x.txt");
        let md = std::fs::symlink_metadata(&p).expect("lstat");
        assert!(
            md.file_type().is_file(),
            "no-hash fallback must copy a real file, not symlink"
        );
        assert!(!stage.exists(), "fallback must not create a stage entry");
    }
}
