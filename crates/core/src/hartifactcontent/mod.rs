pub mod file;
pub mod sniff;
pub mod tar;
pub mod tar_index;
pub mod unpack;
pub mod view;

pub use file::FileContent;
pub use view::{
    PathFilter, PathMapping, PathTransform, Rename, RenamePlan, SourcePaths, ViewContent,
};

use std::io;
use std::path::PathBuf;

pub struct WalkEntry {
    pub path: PathBuf,
    pub kind: WalkEntryKind,
}

pub enum WalkEntryKind {
    File {
        data: Box<dyn io::Read>,
        x: bool,
        /// Byte length of `data`.
        ///
        /// Carried because a tar header must state an entry's size *before* its
        /// bytes, so anything re-packing a walk (see
        /// [`ViewContent`](view::ViewContent)) would otherwise have to buffer
        /// each entry whole just to measure it. Every producer already knows
        /// this — a tar walker reads it from the header it just parsed, a file
        /// walker from `stat`, an in-memory one from `len` — so surfacing it
        /// makes streaming re-packs possible at no cost.
        ///
        /// Must equal the number of bytes `data` yields; a re-pack writes
        /// exactly this many.
        size: u64,
    },
    Symlink {
        target: PathBuf,
    },
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
    /// Relative paths of the file and symlink entries in this content —
    /// directory entries excluded. Intended for callers that only need the set
    /// of materialized paths (e.g. output-collision detection), not the bytes.
    ///
    /// The default enumerates via [`Content::walk`], which for stream-only
    /// backings reads (and discards) file data to advance. Seekable, indexable
    /// backings (tar-backed cache artifacts) override this with a header-only
    /// scan that seeks past data — keeping the format detail behind the trait.
    fn entry_paths(&self) -> anyhow::Result<Vec<PathBuf>> {
        self.walk()?.map(|r| r.map(|e| e.path)).collect()
    }
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
    ///
    /// Two rules, both load-bearing:
    ///
    /// 1. **Open it before dropping the `Content` it came from; never store
    ///    it.** Unlike [`reader`](Self::reader) and
    ///    [`seekable_reader`](Self::seekable_reader), which hand back an open
    ///    handle that pins its inode, a `PathBuf` is detached — whatever keeps
    ///    the bytes from being reclaimed is tied to the `Content`, not to the
    ///    path, so the file may be gone once the handle drops. The path is also
    ///    host- and revision-specific (an absolute path under a particular
    ///    user's cache), so it must never be embedded in a target's output: that
    ///    would make a content-addressed artifact machine-specific.
    /// 2. **Answer `None` rather than a path that does not exist.** Callers treat
    ///    `Some` as "open this", with no fallback to the byte stream, so a stale
    ///    path turns a working read into a hard error rather than a slow one.
    ///
    /// What backs rule 1 is per-implementation; the engine's cache artifacts
    /// document their own guarantee, and it is not the same for every `Content`.
    fn file_path(&self) -> Option<std::path::PathBuf> {
        None
    }
}
