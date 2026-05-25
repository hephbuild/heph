use crate::engine::local_cache::{LocalCache, NotFoundError, SizedReader};
use crate::hartifactcontent;
use crate::htaddr::Addr;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::{fs, io};

pub struct LocalCacheFS {
    root: PathBuf,
}

impl LocalCacheFS {
    pub fn new(root: PathBuf) -> Result<Self> {
        Ok(Self { root })
    }

    fn get_path(&self, addr: &Addr, hashin: &str, name: &str) -> PathBuf {
        let mut path = self.root.clone();
        path.push(addr.package.as_str());
        if addr.args.is_empty() {
            path.push(format!("__target_{}", addr.name));
        } else {
            path.push(format!("__target_{}_{}", addr.name, addr.hash_str()));
        }
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
        Ok(())
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
            crate::htpkg::PkgBuf::from("test_pkg"),
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
    fn test_seekable_reader_fs() -> Result<()> {
        use std::io::Seek;
        use std::io::SeekFrom;
        let dir = tempdir()?;
        let cache = LocalCacheFS::new(PathBuf::from(dir.path()))?;
        let addr = crate::htaddr::Addr::new(
            crate::htpkg::PkgBuf::from("p"),
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
