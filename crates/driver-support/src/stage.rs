//! Shared read-only input staging for the OS-copy sandbox runner.
//!
//! When FUSE is unavailable the OS runner materializes every input by copying
//! its bytes into each consumer's sandbox. For inputs a target only *reads*
//! (tools, hermetic SDKs, …) that copy is pure waste: the same artifact is
//! unpacked once per consuming target. Staging hoists that work out — an
//! artifact is materialized **once** into a content-addressed, read-only stage
//! dir under `<home>/stage/`, and every consumer gets cheap symlinks into its
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
        let predicate: Option<&dyn Fn(&Path) -> bool> =
            if filters.is_empty() { None } else { Some(&pred) };
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
    make_readonly_tree(entry).with_context(|| format!("mark stage entry read-only {:?}", entry))?;
    Ok(())
}

/// Strip write bits from every file and directory under `root` (dirs become
/// `0o555`), mirroring the Go module cache. Children are processed before their
/// parents so we never lose the traversal permission needed to descend.
#[cfg(unix)]
fn make_readonly_tree(root: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for entry in walkdir::WalkDir::new(root).contents_first(true) {
        let entry = entry.with_context(|| format!("walk stage tree under {:?}", root))?;
        let md = entry
            .metadata()
            .with_context(|| format!("stat {:?} for read-only chmod", entry.path()))?;
        let ft = md.file_type();
        // Symlink perms are irrelevant (the target's mode governs) and
        // `set_permissions` would follow the link — skip.
        if ft.is_symlink() {
            continue;
        }
        let mode = if ft.is_dir() {
            0o555
        } else {
            md.permissions().mode() & !0o222
        };
        std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod read-only {:?}", entry.path()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_readonly_tree(_root: &Path) -> anyhow::Result<()> {
    anyhow::bail!("read-only input staging is only supported on unix")
}

/// Replicate the directory structure of `entry` inside `link_root`, pointing
/// every file/symlink at its staged counterpart with an absolute symlink.
/// Directories are created as real dirs so multiple inputs can merge into a
/// shared `link_root`. Parents are visited before children (walkdir's default
/// order) so each symlink's parent dir already exists.
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
        match std::os::unix::fs::symlink(ent.path(), &dst) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                std::fs::remove_file(&dst)
                    .with_context(|| format!("remove stale link {:?}", dst))?;
                std::os::unix::fs::symlink(ent.path(), &dst)
                    .with_context(|| format!("relink {:?} -> {:?}", dst, ent.path()))?;
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("symlink {:?} -> {:?}", dst, ent.path()));
            }
        }
        if let Some(list) = list.as_mut() {
            writeln!(list, "{}", dst.display())
                .with_context(|| "append to stage list file")?;
        }
    }
    Ok(())
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

        // Both sandboxes see the content through symlinks into the stage.
        for link in [&link_a, &link_b] {
            let p = link.join("pkg/lib.txt");
            let md = std::fs::symlink_metadata(&p).expect("lstat link");
            assert!(md.file_type().is_symlink(), "{:?} must be a symlink", p);
            assert_eq!(std::fs::read_to_string(&p).expect("read"), "hello");
            assert_eq!(
                std::fs::canonicalize(&p).expect("canon"),
                std::fs::canonicalize(&staged).expect("canon staged"),
                "symlink must resolve into the shared stage"
            );
        }
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
