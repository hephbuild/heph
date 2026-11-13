//! The filesystem calls the index makes, behind a trait so unit tests can
//! inject stats (case-folding and inode reuse cannot be staged on every host).

use crate::CachedWalker;
use anyhow::{Context as _, Result};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What `lstat` says about a path: the kind and the fields a record compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    pub kind: StatKind,
    pub size: u64,
    /// Nanoseconds since the epoch; negative before it.
    pub mtime_ns: i64,
    pub ino: u64,
    pub dev: u64,
    /// `st_mode`, file-type bits included.
    pub mode: u32,
}

impl FileStat {
    /// Any exec bit set: the `x` folded into a content hash.
    pub fn exec(&self) -> bool {
        self.mode & 0o111 != 0
    }

    /// Same `(dev, ino)`: the two names are one file.
    pub fn same_inode(&self, other: &FileStat) -> bool {
        self.dev == other.dev && self.ino == other.ino
    }
}

/// The kind of a path, without following a symlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatKind {
    File,
    Symlink,
    Dir,
    Other,
}

/// The index's view of the filesystem. Paths are absolute.
pub trait IndexFs: Send + Sync {
    /// `lstat(2)`.
    fn lstat(&self, path: &Path) -> io::Result<FileStat>;
    /// `readlink(2)`.
    fn read_link(&self, path: &Path) -> io::Result<PathBuf>;
    /// The content hash of a regular file as lookups read it: cached, so a
    /// changed file is hashed once (`CachedWalker::file_hash`).
    fn content_hash(&self, path: &Path) -> Result<String>;
    /// A fresh, uncached content hash (`file_hashout`): the only hash a delete
    /// may rest on (J5).
    fn fresh_hash(&self, path: &Path, x: bool) -> Result<String>;
    /// `unlink(2)`.
    fn unlink(&self, path: &Path) -> io::Result<()>;
    /// `rmdir(2)`: removes `path` only when it is an empty directory.
    fn rmdir(&self, path: &Path) -> io::Result<()>;
}

/// The real filesystem, with lookups' hashes cached by the shared walker.
pub struct RealFs {
    walker: Arc<CachedWalker>,
}

impl std::fmt::Debug for RealFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealFs").finish_non_exhaustive()
    }
}

impl RealFs {
    pub fn new(walker: Arc<CachedWalker>) -> Self {
        Self { walker }
    }
}

/// `lstat` a real path into a [`FileStat`].
pub fn lstat(path: &Path) -> io::Result<FileStat> {
    use std::os::unix::fs::MetadataExt as _;
    let m = std::fs::symlink_metadata(path)?;
    let ft = m.file_type();
    let kind = if ft.is_symlink() {
        StatKind::Symlink
    } else if ft.is_file() {
        StatKind::File
    } else if ft.is_dir() {
        StatKind::Dir
    } else {
        StatKind::Other
    };
    // Seconds and nanoseconds as the kernel reports them, so an mtime before
    // the epoch survives (`SystemTime` arithmetic would reject it).
    let mtime_ns = m
        .mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(m.mtime_nsec());
    Ok(FileStat {
        kind,
        size: m.size(),
        mtime_ns,
        ino: m.ino(),
        dev: m.dev(),
        mode: m.mode(),
    })
}

impl IndexFs for RealFs {
    fn lstat(&self, path: &Path) -> io::Result<FileStat> {
        lstat(path)
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::read_link(path)
    }

    fn content_hash(&self, path: &Path) -> Result<String> {
        Ok(self.walker.file_hash(path)?.hashout.clone())
    }

    fn fresh_hash(&self, path: &Path, x: bool) -> Result<String> {
        crate::file_hashout(path, x)
    }

    fn unlink(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn rmdir(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir(path)
    }
}

/// Whether an `lstat` error means "nothing is there".
pub fn is_absent(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// `lstat`, with "nothing there" as `None` and any other failure an error
/// naming the path.
pub fn lstat_opt(fs: &dyn IndexFs, path: &Path) -> Result<Option<FileStat>> {
    match fs.lstat(path) {
        Ok(st) => Ok(Some(st)),
        Err(e) if is_absent(&e) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("lstat {}", path.display())),
    }
}
