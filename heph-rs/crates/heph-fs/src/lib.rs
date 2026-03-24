//! Filesystem abstraction for Heph

use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum FsError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Path not found: {0}")]
    NotFound(PathBuf),

    #[error("Permission denied: {0}")]
    PermissionDenied(PathBuf),
}

pub type Result<T> = std::result::Result<T, FsError>;

/// Filesystem operations trait
pub trait Fs: Send + Sync {
    /// Read entire file contents
    fn read(&self, path: &Path) -> Result<Vec<u8>>;

    /// Write data to file, creating if it doesn't exist
    fn write(&self, path: &Path, data: &[u8]) -> Result<()>;

    /// Check if path exists
    fn exists(&self, path: &Path) -> bool;

    /// Remove a file
    fn remove(&self, path: &Path) -> Result<()>;

    /// Create directory and all parent directories
    fn create_dir(&self, path: &Path) -> Result<()>;

    /// List directory contents
    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>>;

    /// Get file metadata (size, modified time, etc.)
    fn metadata(&self, path: &Path) -> Result<Metadata>;
}

/// File metadata
#[derive(Debug, Clone)]
pub struct Metadata {
    pub is_file: bool,
    pub is_dir: bool,
    pub size: u64,
}

/// Standard OS filesystem implementation
pub struct OsFs;

impl Fs for OsFs {
    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        std::fs::read(path).map_err(Into::into)
    }

    fn write(&self, path: &Path, data: &[u8]) -> Result<()> {
        std::fs::write(path, data).map_err(Into::into)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn remove(&self, path: &Path) -> Result<()> {
        std::fs::remove_file(path).map_err(Into::into)
    }

    fn create_dir(&self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path).map_err(Into::into)
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        std::fs::read_dir(path)?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<io::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    fn metadata(&self, path: &Path) -> Result<Metadata> {
        let meta = std::fs::metadata(path)?;
        Ok(Metadata {
            is_file: meta.is_file(),
            is_dir: meta.is_dir(),
            size: meta.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_read_write() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");

        let fs = OsFs;
        fs.write(&path, b"hello world").unwrap();

        let data = fs.read(&path).unwrap();
        assert_eq!(data, b"hello world");
    }

    #[test]
    fn test_exists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");

        let fs = OsFs;
        assert!(!fs.exists(&path));

        fs.write(&path, b"hello").unwrap();
        assert!(fs.exists(&path));
    }

    #[test]
    fn test_remove() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");

        let fs = OsFs;
        fs.write(&path, b"hello").unwrap();
        assert!(fs.exists(&path));

        fs.remove(&path).unwrap();
        assert!(!fs.exists(&path));
    }

    #[test]
    fn test_create_dir() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a").join("b").join("c");

        let fs = OsFs;
        fs.create_dir(&nested).unwrap();
        assert!(fs.exists(&nested));
    }

    #[test]
    fn test_list_dir() {
        let dir = TempDir::new().unwrap();

        let fs = OsFs;
        fs.write(&dir.path().join("file1.txt"), b"a").unwrap();
        fs.write(&dir.path().join("file2.txt"), b"b").unwrap();
        fs.write(&dir.path().join("file3.txt"), b"c").unwrap();

        let entries = fs.list_dir(dir.path()).unwrap();
        assert_eq!(entries.len(), 3);

        let names: Vec<String> = entries
            .iter()
            .filter_map(|p| p.file_name()?.to_str())
            .map(String::from)
            .collect();

        assert!(names.contains(&"file1.txt".to_string()));
        assert!(names.contains(&"file2.txt".to_string()));
        assert!(names.contains(&"file3.txt".to_string()));
    }

    #[test]
    fn test_metadata() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");

        let fs = OsFs;
        fs.write(&path, b"hello").unwrap();

        let meta = fs.metadata(&path).unwrap();
        assert!(meta.is_file);
        assert!(!meta.is_dir);
        assert_eq!(meta.size, 5);
    }

    #[test]
    fn test_metadata_dir() {
        let dir = TempDir::new().unwrap();
        let subdir = dir.path().join("subdir");

        let fs = OsFs;
        fs.create_dir(&subdir).unwrap();

        let meta = fs.metadata(&subdir).unwrap();
        assert!(!meta.is_file);
        assert!(meta.is_dir);
    }

    #[test]
    fn test_write_overwrites() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");

        let fs = OsFs;
        fs.write(&path, b"hello").unwrap();
        fs.write(&path, b"world").unwrap();

        let data = fs.read(&path).unwrap();
        assert_eq!(data, b"world");
    }

    #[test]
    fn test_read_nonexistent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.txt");

        let fs = OsFs;
        let result = fs.read(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_remove_nonexistent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.txt");

        let fs = OsFs;
        let result = fs.remove(&path);
        assert!(result.is_err());
    }
}
