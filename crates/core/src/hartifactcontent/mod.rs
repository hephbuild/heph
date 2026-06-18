pub mod tar;
pub mod tar_index;
pub mod unpack;

use std::io;
use std::path::PathBuf;

pub struct WalkEntry {
    pub path: PathBuf,
    pub kind: WalkEntryKind,
}

pub enum WalkEntryKind {
    File { data: Box<dyn io::Read>, x: bool },
    Symlink { target: PathBuf },
}

#[derive(Clone, Copy)]
pub enum Type {
    Tar,
    Cpio,
}

/// Auto-trait for any type that is both `Read` and `Seek`, used to box
/// seekable readers behind `dyn`. Required by FUSE-backed sandbox layers
/// that index a tar once and pread by offset thereafter.
pub trait ReadSeek: io::Read + io::Seek {}
impl<T: io::Read + io::Seek> ReadSeek for T {}

impl std::fmt::Debug for dyn Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Artifact")
    }
}

pub trait Content: Send + Sync {
    fn reader(&self) -> anyhow::Result<Box<dyn io::Read>>;
    fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>>;
    fn hashout(&self) -> anyhow::Result<String>;
    /// Returns a seekable handle to the underlying bytes when the backing
    /// store supports random access (sqlite blobs, on-disk files). Backends
    /// without efficient seek (pipes, streams) return `Ok(None)` and the
    /// caller falls back to the copy path.
    fn seekable_reader(&self) -> anyhow::Result<Option<Box<dyn ReadSeek + Send>>> {
        Ok(None)
    }
    /// Cheap byte-size hint used by the engine's auto-mode router to weigh
    /// FUSE vs unpack-copy without reading the underlying bytes. `None`
    /// means the backend cannot answer cheaply; callers treat that as 0
    /// for threshold checks.
    fn byte_size(&self) -> Option<u64> {
        None
    }
    /// The on-disk path backing this content, when it is a real file on the
    /// local filesystem (e.g. an on-disk cache artifact). `None` for synthetic
    /// or non-file backends (in-memory, sqlite blobs). Lets an in-process
    /// consumer open the file directly instead of streaming its bytes — notably
    /// the stable-ABI seam, where a guest can read the file rather than pulling
    /// it chunk-by-chunk across the vtable.
    fn file_path(&self) -> Option<std::path::PathBuf> {
        None
    }
}
