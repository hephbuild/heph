use crate::engine::local_cache::{
    EntryWriter, Existence, LocalCache, MANIFEST_V1, Manifest, NotFoundError, SizedReader,
    TargetStream,
};
use anyhow::{Context, Result};
use hcore::hartifactcontent;
use hmodel::htaddr::Addr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::{fs, io};

pub struct LocalCacheFS {
    root: PathBuf,
}

impl LocalCacheFS {
    pub fn new(root: PathBuf) -> Result<Self> {
        Ok(Self { root })
    }

    /// Directory holding all of a target's revisions:
    /// `<root>/<package>/__target_<name>[_<addr_hash>]/`.
    fn target_dir(&self, addr: &Addr) -> PathBuf {
        let mut path = self.root.clone();
        path.push(addr.package.as_str());
        if addr.args.is_empty() {
            path.push(format!("__target_{}", addr.name));
        } else {
            path.push(format!("__target_{}_{}", addr.name, addr.hash_str()));
        }
        path
    }

    fn get_path(&self, addr: &Addr, hashin: &str, name: &str) -> PathBuf {
        let mut path = self.target_dir(addr);
        path.push(hashin);
        path.push(name);
        path
    }
}

/// Staging writer behind [`LocalCacheFS::writer`]: bytes land in a temp file that
/// is renamed over the final path on [`EntryWriter::commit`], so the blob appears
/// atomically. Dropping without commit — a write error, an abandoned attempt —
/// deletes the temp instead, leaving whatever was already at the destination
/// untouched rather than replacing it with a truncated blob.
struct AtomicFileWriter {
    file: Option<fs::File>,
    temp: PathBuf,
    dest: PathBuf,
    failed: bool,
    committed: bool,
}

impl io::Write for AtomicFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Some(file) = self.file.as_mut() else {
            return Err(io::Error::other("cache writer already finalized"));
        };
        let res = file.write(buf);
        if res.is_err() {
            self.failed = true;
        }
        res
    }

    fn flush(&mut self) -> io::Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        let res = file.flush();
        if res.is_err() {
            self.failed = true;
        }
        res
    }
}

impl EntryWriter for AtomicFileWriter {
    fn commit(mut self: Box<Self>) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.failed,
            "cannot commit cache file {:?}: an earlier write failed",
            self.dest
        );
        let mut file = self
            .file
            .take()
            .with_context(|| format!("cache writer for {:?} already finalized", self.dest))?;
        file.flush()
            .with_context(|| format!("flush cache file {:?}", self.temp))?;
        drop(file);
        fs::rename(&self.temp, &self.dest)
            .with_context(|| format!("rename cache file {:?} into place", self.dest))?;
        // The temp no longer exists; tell Drop there is nothing to discard.
        self.committed = true;
        Ok(())
    }
}

impl Drop for AtomicFileWriter {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // Dropped without commit (or commit failed part-way): discard the temp.
        // The destination keeps whatever complete blob it already had.
        drop(self.file.take());
        drop(fs::remove_file(&self.temp));
    }
}

impl LocalCache for LocalCacheFS {
    fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> Result<SizedReader> {
        let path = self.get_path(addr, hashin, name);
        let file = match fs::File::open(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(anyhow::anyhow!(NotFoundError))?,
            res => {
                res.with_context(|| format!("Failed to open reader for cache path: {:?}", path))?
            }
        };

        let size = file
            .metadata()
            .with_context(|| format!("stat cache file: {:?}", path))?
            .len();

        Ok(SizedReader {
            size,
            reader: Box::new(file),
            bytes: None,
        })
    }

    /// Write to a sibling temp file and rename it into place on commit.
    ///
    /// The rename makes a blob appear atomically: a concurrent reader either sees
    /// the previous complete file or the new one, never a truncated write in
    /// progress. That matters because a blob can be (re)written while another
    /// request holds only a *read* lock on the revision — a lazy remote pull
    /// materializes into an entry other callers are already reading. Both writers
    /// store the same content-addressed bytes, so whichever rename lands last is
    /// equally correct.
    fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Box<dyn EntryWriter>> {
        let path = self.get_path(addr, hashin, name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create parent directories for: {:?}", path))?;
        }
        let temp = path.with_file_name(format!(
            ".{}.{}.tmp",
            name,
            uuid::Uuid::new_v4().as_simple()
        ));
        let file = fs::File::create(&temp)
            .with_context(|| format!("Failed to create writer for cache path: {:?}", temp))?;
        Ok(Box::new(AtomicFileWriter {
            file: Some(file),
            temp,
            dest: path,
            failed: false,
            committed: false,
        }))
    }

    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
        let path = self.get_path(addr, hashin, name);
        Ok(path.exists())
    }

    fn existence(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Existence> {
        // A rename into place is the commit here — there is no queue behind it,
        // so every answer this backend can give is already committed.
        Ok(Existence::Committed(self.exists(addr, hashin, name)?))
    }

    fn exists_committed(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
        // No queue, so every answer this backend can give is committed.
        self.exists(addr, hashin, name)
    }

    fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> Result<()> {
        let path = self.get_path(addr, hashin, name);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("Failed to delete cache file: {:?}", path))?;
        }
        // Prune now-empty revision then target dir so deleting a revision's last
        // blob (the common spill-cache case: only large blobs live here) doesn't
        // leave empty directories accumulating. `remove_dir` only succeeds on an
        // empty dir, so a still-populated revision is left untouched; errors
        // (non-empty / already gone) are intentionally ignored.
        if let Some(rev_dir) = path.parent()
            && fs::remove_dir(rev_dir).is_ok()
            && let Some(target_dir) = rev_dir.parent()
        {
            drop(fs::remove_dir(target_dir));
        }
        Ok(())
    }

    fn list_targets(&self) -> Result<TargetStream> {
        let mut out = Vec::new();
        collect_targets(&self.root, &mut out)?;
        Ok(Box::new(out.into_iter().map(Ok)))
    }

    fn list_target_entries(&self, addr: &Addr) -> Result<Vec<String>> {
        let dir = self.target_dir(addr);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("read target dir {dir:?}")),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("read dir entry under {dir:?}"))?;
            if entry
                .file_type()
                .with_context(|| format!("stat {:?}", entry.path()))?
                .is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                out.push(name.to_string());
            }
        }
        Ok(out)
    }

    fn seekable_reader(
        &self,
        addr: &Addr,
        hashin: &str,
        name: &str,
    ) -> Result<Option<Box<dyn hartifactcontent::ReadSeek + Send>>> {
        let path = self.get_path(addr, hashin, name);
        match fs::File::open(&path) {
            Ok(f) => Ok(Some(Box::new(f))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(anyhow::anyhow!(NotFoundError)),
            Err(e) => {
                Err(e).with_context(|| format!("open seekable reader for cache path: {:?}", path))
            }
        }
    }

    /// Only for a blob that actually lives here. This backend is normally the
    /// *blob* half of [`LocalCacheSpill`], holding only entries over the spill
    /// threshold — everything smaller is a sqlite row and has no file at this
    /// path. Answering `Some` unconditionally would hand every small artifact a
    /// path to nothing, and `Content::file_path`'s callers open what they are
    /// given without falling back to the stream, so that is a hard read error
    /// rather than a missed optimization. One `stat(2)`, on the seam hand-off
    /// path only.
    ///
    /// [`LocalCacheSpill`]: crate::engine::local_cache_spill::LocalCacheSpill
    fn file_path(&self, addr: &Addr, hashin: &str, name: &str) -> Option<PathBuf> {
        let path = self.get_path(addr, hashin, name);
        match fs::metadata(&path) {
            Ok(m) if m.is_file() => Some(path),
            Ok(_) => None,
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            // Not-found is the ordinary answer (every sub-threshold key); anything
            // else — EACCES on the blobs dir, EIO — means the fast path is off for
            // *every* artifact while the build merely looks slow. `Path::is_file`
            // would fold that into the same silent `false`, which is the
            // invisible-degradation failure this whole path exists to undo.
            Err(e) => {
                tracing::debug!(?path, error = %e, "stat cache blob for direct-open path");
                None
            }
        }
    }
}

/// Recursively walk `dir`; for each `__target_*` directory (one per target),
/// read any one revision's `MANIFEST_V1` to recover the addr (`manifest.target`
/// is authoritative — the on-disk path hashes arg'd addrs irreversibly) and push
/// it once. Does not descend into a target dir. Targets whose manifests are all
/// missing/corrupt are skipped (best-effort; GC tolerates noise).
fn collect_targets(dir: &Path, out: &mut Vec<String>) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("read cache dir {dir:?}")),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("read dir entry under {dir:?}"))?;
        let path = entry.path();
        if !entry
            .file_type()
            .with_context(|| format!("stat {path:?}"))?
            .is_dir()
        {
            continue;
        }
        let is_target = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with("__target_"));
        if is_target {
            if let Some(addr) = read_any_target_addr(&path)? {
                out.push(addr);
            }
        } else {
            collect_targets(&path, out)?;
        }
    }
    Ok(())
}

/// Read `manifest.target` from the first revision under a target dir that has a
/// readable `MANIFEST_V1`. `None` if none do.
fn read_any_target_addr(target_dir: &Path) -> Result<Option<String>> {
    for entry in fs::read_dir(target_dir).with_context(|| format!("read {target_dir:?}"))? {
        let entry = entry.with_context(|| format!("read dir entry under {target_dir:?}"))?;
        if !entry
            .file_type()
            .with_context(|| format!("stat {:?}", entry.path()))?
            .is_dir()
        {
            continue;
        }
        let manifest_path = entry.path().join(MANIFEST_V1);
        let buf = match fs::read(&manifest_path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("read {manifest_path:?}")),
        };
        if let Ok(manifest) = borsh::from_slice::<Manifest>(&buf) {
            return Ok(Some(manifest.target));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tempfile::tempdir;

    #[test]
    fn test_local_cache_fs() -> Result<()> {
        let dir = tempdir()?;

        let cache = LocalCacheFS::new(PathBuf::from(dir.path()))?;
        let addr = Addr::new(
            hmodel::htpkg::PkgBuf::from("test_pkg"),
            "test_target".to_string(),
            Default::default(),
        );
        let hashin = "abc123hash";
        let name = "output.txt";

        // Test non-existence
        assert!(!cache.exists(&addr, hashin, name)?);

        // Test writer
        let mut writer = cache.writer(&addr, hashin, name)?;
        writer.write_all(b"hello cache")?;
        writer.commit()?;

        // Test existence
        assert!(cache.exists(&addr, hashin, name)?);

        // Test reader
        let sized = cache.reader(&addr, hashin, name)?;
        assert_eq!(sized.size, b"hello cache".len() as u64);
        let mut reader = sized.reader;
        let mut content = String::new();
        reader.read_to_string(&mut content)?;
        assert_eq!(content, "hello cache");

        // Test delete
        cache.delete(&addr, hashin, name)?;
        assert!(!cache.exists(&addr, hashin, name)?);

        Ok(())
    }

    /// `file_path` describes what is on disk, not what could be. Under
    /// [`LocalCacheSpill`] this backend is asked about every key, including the
    /// small ones that live in sqlite and have no file here — and a consumer
    /// handed a path opens it with no fallback to the byte stream, so a path to
    /// a missing file is a read error, not a slower read.
    ///
    /// [`LocalCacheSpill`]: crate::engine::local_cache_spill::LocalCacheSpill
    #[test]
    fn file_path_is_none_until_the_blob_is_on_disk() -> Result<()> {
        let dir = tempdir()?;
        let cache = LocalCacheFS::new(PathBuf::from(dir.path()))?;
        let addr = Addr::new(
            hmodel::htpkg::PkgBuf::from("pkg"),
            "t".to_string(),
            Default::default(),
        );

        assert!(
            cache.file_path(&addr, "h", "out.tar").is_none(),
            "no blob written: must not name a path that does not exist"
        );

        let mut w = cache.writer(&addr, "h", "out.tar")?;
        w.write_all(b"bytes")?;
        w.commit()?;

        let path = cache
            .file_path(&addr, "h", "out.tar")
            .expect("written blob has a path");
        assert_eq!(std::fs::read(&path)?, b"bytes", "path names the blob");

        // Reclaimed by GC: the path goes away with the bytes.
        cache.delete(&addr, "h", "out.tar")?;
        assert!(
            cache.file_path(&addr, "h", "out.tar").is_none(),
            "deleted blob must not keep answering with a path"
        );
        Ok(())
    }

    #[test]
    fn test_list_targets_and_entries_fs() -> Result<()> {
        let dir = tempdir()?;
        let cache = LocalCacheFS::new(PathBuf::from(dir.path()))?;
        let addr = hmodel::htaddr::Addr::new(
            hmodel::htpkg::PkgBuf::from("pkg"),
            "t".to_string(),
            Default::default(),
        );
        for h in ["h1", "h2"] {
            let manifest = Manifest {
                version: "1.0.0".to_string(),
                target: addr.format(),
                created_at_nanos: 0,
                hashin: h.to_string(),
                artifacts: vec![],
            };
            let mut w = cache.writer(&addr, h, MANIFEST_V1)?;
            borsh::to_writer(&mut w, &manifest)?;
            w.commit()?;
        }

        // One distinct target, recovered from the manifest's `target` field.
        let targets = cache.list_targets()?.collect::<Result<Vec<_>>>()?;
        assert_eq!(targets, vec![addr.format()]);

        let mut entries = cache.list_target_entries(&addr)?;
        entries.sort();
        assert_eq!(entries, vec!["h1".to_string(), "h2".to_string()]);
        Ok(())
    }

    #[test]
    fn test_seekable_reader_fs() -> Result<()> {
        use std::io::Seek;
        use std::io::SeekFrom;
        let dir = tempdir()?;
        let cache = LocalCacheFS::new(PathBuf::from(dir.path()))?;
        let addr = hmodel::htaddr::Addr::new(
            hmodel::htpkg::PkgBuf::from("p"),
            "t".to_string(),
            Default::default(),
        );
        let mut w = cache.writer(&addr, "h", "blob")?;
        w.write_all(b"0123456789abcdef")?;
        w.commit()?;

        let mut r = cache
            .seekable_reader(&addr, "h", "blob")?
            .expect("fs cache must support seekable_reader");
        r.seek(SeekFrom::Start(4))?;
        let mut buf = [0u8; 4];
        r.read_exact(&mut buf)?;
        assert_eq!(&buf, b"4567");
        Ok(())
    }
}
