use crate::engine::local_cache::{
    LocalCache, MANIFEST_V1, Manifest, NotFoundError, SizedReader, TargetStream,
};
use anyhow::{Context, Result};
use hcore::hartifactcontent;
use hmodel::htaddr::Addr;
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

    fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Box<dyn io::Write>> {
        let path = self.get_path(addr, hashin, name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create parent directories for: {:?}", path))?;
        }
        let file = fs::File::create(&path)
            .with_context(|| format!("Failed to create writer for cache path: {:?}", path))?;
        Ok(Box::new(file))
    }

    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
        let path = self.get_path(addr, hashin, name);
        Ok(path.exists())
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
        drop(writer);

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
            drop(w);
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
        drop(w);

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
