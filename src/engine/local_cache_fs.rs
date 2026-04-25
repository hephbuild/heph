use crate::engine::local_cache::LocalCache;
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
        path.push(addr.package.as_ref() as &std::path::Path);
        path.push(&addr.name);
        path.push(hashin);
        path.push(name);
        path
    }
}

impl LocalCache for LocalCacheFS {
    fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Box<dyn io::Read>> {
        let path = self.get_path(addr, hashin, name);
        let file = fs::File::open(&path)
            .with_context(|| format!("Failed to open reader for cache path: {:?}", path))?;
        Ok(Box::new(file))
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
        let addr = Addr {
            package: crate::htpkg::PkgBuf::from("test_pkg"),
            name: "test_target".to_string(),
            ..Default::default()
        };
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
        let mut reader = cache.reader(&addr, hashin, name)?;
        let mut content = String::new();
        reader.read_to_string(&mut content)?;
        assert_eq!(content, "hello cache");

        // Test delete
        cache.delete(&addr, hashin, name)?;
        assert!(!cache.exists(&addr, hashin, name)?);

        Ok(())
    }
}
