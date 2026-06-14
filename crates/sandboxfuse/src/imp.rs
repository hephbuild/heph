//! Linux FUSE implementation of the sandbox overlay (fuser 0.17 API).
//!
//! Read layers are served straight from seekable tar bytes via a pre-built
//! `TarIndex`. Writes land in an upper directory (`PathBuf` on real disk)
//! via passthrough syscalls. Unlinks insert into an in-memory whiteout set.
//! Renames and copy-up read the source bytes from a layer before writing
//! them to the upper layer.
//!
//! fuser 0.17 invokes `Filesystem` methods on `&self`, so mutable state
//! lives in a `parking_lot::Mutex<State>` guarded by short critical
//! sections. Read paths (layer bytes via tar index, upper passthrough)
//! never touch the mutex.

use anyhow::Context;
use crossbeam_channel::{Sender, unbounded};
use fuser::{
    AccessFlags, BackgroundSession, BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType,
    Filesystem, FopenFlags, Generation, INodeNo, LockOwner, Notifier, OpenFlags, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyWrite, ReplyXattr, Request, SessionACL, TimeOrNow, WriteFlags,
};
use hcore::hartifactcontent::ReadSeek;
use hcore::hartifactcontent::tar_index::{IndexEntryKind, TarIndex};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Deferred inval_entry notification. The fuse kernel takes the parent
/// inode lock for both in-flight ops AND notification handling, so
/// firing `inval_entry` synchronously from inside an op handler whose
/// reply hasn't been sent deadlocks against ourselves. We post msgs to
/// a dedicated bg thread that drains them after the originating op
/// returns.
struct InvalEntry {
    parent: INodeNo,
    name: OsString,
}

const TTL: Duration = Duration::from_secs(1);
const BLOCK_SIZE: u32 = 4096;
/// Physical mode for files in the upper layer. Always FUSE-process-writable
/// so subsequent FUSE write ops can reopen the file with `O_WRONLY`
/// regardless of the logical mode requested by clients. The logical mode
/// (e.g. 0o444 from a `cp -R` of a Go module cache) is tracked separately
/// in `State.mode_overrides` and reported via `getattr`.
const UPPER_FILE_MODE: u32 = 0o600;
/// Physical mode for directories in the upper layer. Always
/// FUSE-process-traversable/writable for the same reason as `UPPER_FILE_MODE`.
const UPPER_DIR_MODE: u32 = 0o700;

/// Lazy opener for a layer's seekable byte source. Called per FUSE read
/// so that we don't pin a sqlite connection (or other backing resource)
/// for the lifetime of the mount — large dep sets would otherwise
/// exhaust the connection pool.
pub type LayerOpener = Box<dyn Fn() -> anyhow::Result<Box<dyn ReadSeek + Send>> + Send + Sync>;

/// One read-only layer. The `TarIndex` is built once; bytes are fetched
/// on demand via `open`. No backing handle is held across FUSE ops, so
/// concurrent reads are bounded by the connection pool rather than the
/// layer count.
pub struct Layer {
    pub index: TarIndex,
    open: LayerOpener,
    /// Pre-computed directory children: dir path → vec of (child name, kind).
    /// Includes implicit ancestor dirs that have no explicit tar entry.
    dirs: HashMap<PathBuf, Vec<(OsString, IndexEntryKind)>>,
}

impl Layer {
    pub fn new(index: TarIndex, open: LayerOpener) -> Self {
        let dirs = build_layer_dirs(&index);
        Self { index, open, dirs }
    }
}

fn build_layer_dirs(index: &TarIndex) -> HashMap<PathBuf, Vec<(OsString, IndexEntryKind)>> {
    let mut dirs: HashMap<PathBuf, Vec<(OsString, IndexEntryKind)>> = HashMap::new();
    dirs.entry(PathBuf::new()).or_default();
    for (path, entry) in &index.entries {
        let mut ancestor = PathBuf::new();
        let parts: Vec<&OsStr> = path.components().map(|c| c.as_os_str()).collect();
        if parts.is_empty() {
            continue;
        }
        for &comp in parts.iter().take(parts.len() - 1) {
            let comp_os: OsString = comp.to_os_string();
            let children = dirs.entry(ancestor.clone()).or_default();
            if !children.iter().any(|(n, _)| n == &comp_os) {
                children.push((comp_os.clone(), IndexEntryKind::Dir));
            }
            ancestor.push(&comp_os);
            dirs.entry(ancestor.clone()).or_default();
        }
        let Some(last) = parts.last() else { continue };
        let name = (*last).to_os_string();
        if name.is_empty() {
            continue;
        }
        let children = dirs.entry(ancestor).or_default();
        if !children.iter().any(|(n, _)| n == &name) {
            children.push((name, entry.kind.clone()));
        }
    }
    dirs
}

/// One registered overlay region on a multi-tenant LayeredFs. The `prefix`
/// is FUSE-mount-relative (e.g. `<pkg>/__target_<hash>/ws`); paths under
/// it consult the slot's layers first, falling back to the upper dir.
pub struct Slot {
    pub layers: Vec<Layer>,
}

/// Mutable state guarded by a single Mutex. Inode allocation and the
/// whiteout set are the only fields that the FS mutates after construction.
struct State {
    /// Inode → FUSE-relative path. Root inode (1) maps to the empty path.
    inode_to_path: HashMap<u64, PathBuf>,
    /// Reverse lookup: path → inode. Stable across `lookup` calls.
    path_to_inode: HashMap<PathBuf, u64>,
    next_ino: u64,
    /// Paths hidden from layer view after `unlink`/`rmdir`.
    deleted: std::collections::HashSet<PathBuf>,
    /// Logical mode bits reported via `getattr`, decoupled from the
    /// upper-layer file's physical mode. Upper files are stored 0o600
    /// (dirs 0o700) so the FUSE process can always reopen them for
    /// writes — when a client creates `LICENSE` with mode 0o444 via
    /// `cp -R`, the physical file is still writable but `stat` reports
    /// 0o444. Entries are inserted on create/mkdir/copy_up/setattr and
    /// removed on unlink/rmdir/rename.
    mode_overrides: HashMap<PathBuf, u16>,
}

/// FUSE filesystem presenting the union of registered overlay slots
/// (read-only tar-backed layers) and the writable `upper_root` directory.
/// Inodes are assigned lazily on `lookup`. Slots may be registered and
/// unregistered while the mount is live; resolution picks the longest
/// matching prefix for any given path.
pub struct LayeredFs {
    upper_root: PathBuf,
    slots: parking_lot::RwLock<std::collections::BTreeMap<PathBuf, Slot>>,
    state: Mutex<State>,
    uid: u32,
    gid: u32,
    /// Sender into the bg notifier thread. Set after `Mount::mount`.
    /// Fire-and-forget: dropped sender shuts the thread down when the
    /// mount tears down. macFUSE 5.x caches negative lookups past our
    /// writes, so we must invalidate, but doing it synchronously inside
    /// the op handler deadlocks against the kernel's parent inode lock.
    notifier_tx: parking_lot::Mutex<Option<Sender<InvalEntry>>>,
}

/// RAII handle returned by `LayeredFs::register_slot`. Dropping it
/// removes the slot from the registry. Upper-dir bytes written during
/// the slot's lifetime stay until the sandbox cleaner removes them.
pub struct SlotGuard {
    fs: std::sync::Arc<LayeredFs>,
    prefix: PathBuf,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.fs.unregister_slot(&self.prefix);
    }
}

impl LayeredFs {
    /// Construct an empty multi-tenant FS. No slots registered initially;
    /// callers must call `register_slot` before the FUSE mount is useful
    /// for layered reads. Pure passthrough to `upper_root` works without
    /// any slots.
    pub fn new_empty(upper_root: PathBuf) -> Self {
        // SAFETY: getuid takes no args and never fails on POSIX.
        let uid = unsafe { libc::getuid() };
        // SAFETY: getgid takes no args and never fails on POSIX.
        let gid = unsafe { libc::getgid() };
        let mut inode_to_path = HashMap::new();
        let mut path_to_inode = HashMap::new();
        inode_to_path.insert(u64::from(INodeNo::ROOT), PathBuf::new());
        path_to_inode.insert(PathBuf::new(), u64::from(INodeNo::ROOT));
        Self {
            upper_root,
            slots: parking_lot::RwLock::new(std::collections::BTreeMap::new()),
            state: Mutex::new(State {
                inode_to_path,
                path_to_inode,
                next_ino: u64::from(INodeNo::ROOT) + 1,
                deleted: std::collections::HashSet::new(),
                mode_overrides: HashMap::new(),
            }),
            uid,
            gid,
            notifier_tx: parking_lot::Mutex::new(None),
        }
    }

    /// Backward-compat constructor: build a single-slot FS at the empty
    /// prefix. Used by v1 callers that mount one FUSE per unpack_root.
    pub fn new(layers: Vec<Layer>, upper_root: PathBuf) -> Self {
        let fs = Self::new_empty(upper_root);
        fs.slots.write().insert(PathBuf::new(), Slot { layers });
        fs
    }

    /// Register a new slot at `prefix`. Returns a guard that drops the
    /// slot when out of scope. The caller must hold an `Arc<LayeredFs>`
    /// so the guard can deregister even after the mount thread takes
    /// ownership of the FS.
    pub fn register_slot(
        self: &std::sync::Arc<Self>,
        prefix: PathBuf,
        layers: Vec<Layer>,
    ) -> SlotGuard {
        self.slots.write().insert(prefix.clone(), Slot { layers });
        SlotGuard {
            fs: std::sync::Arc::clone(self),
            prefix,
        }
    }

    fn unregister_slot(&self, prefix: &Path) {
        self.slots.write().remove(prefix);
    }

    /// Install the fuser `Notifier` and spawn a dedicated bg thread
    /// that drains queued `inval_entry` requests. Returns the thread
    /// `JoinHandle` so the caller (`Mount`) can join it during teardown
    /// — the thread must finish (and drop its `Notifier`, which holds a
    /// cloned FUSE channel FD) BEFORE `BackgroundSession::drop` issues
    /// umount, otherwise macFUSE blocks umount on the lingering FD.
    ///
    /// Doing the notify on a separate thread is required: fuser
    /// dispatches ops on the session thread, and calling `inval_entry`
    /// from inside an op handler before sending its reply deadlocks
    /// against the kernel's parent inode lock.
    pub fn set_notifier(&self, n: Notifier) -> std::thread::JoinHandle<()> {
        let (tx, rx) = unbounded::<InvalEntry>();
        let handle = std::thread::Builder::new()
            .name("heph-fuse-notify".into())
            .spawn(move || {
                while let Ok(msg) = rx.recv() {
                    // Best-effort: ignore errors. A failed writev means
                    // the mount is torn down; the next op will error too.
                    drop(n.inval_entry(msg.parent, &msg.name));
                }
            })
            .expect("spawn fuse notify thread");
        *self.notifier_tx.lock() = Some(tx);
        handle
    }

    /// Drop the notifier sender so the bg thread observes channel
    /// closure and exits. Called by `Mount::drop` before joining the
    /// notifier thread.
    fn close_notifier(&self) {
        self.notifier_tx.lock().take();
    }

    /// Queue a `(parent, name)` invalidation for the bg notifier thread.
    /// Fire-and-forget; the reply for the current op MUST be sent before
    /// this is called (the notifier thread will fire as soon as the
    /// channel drains, and the kernel cannot service the inval_entry
    /// until the originating op's reply is acked).
    fn invalidate_entry(&self, parent: INodeNo, name: &OsStr) {
        if let Some(tx) = self.notifier_tx.lock().as_ref() {
            drop(tx.send(InvalEntry {
                parent,
                name: name.to_os_string(),
            }));
        }
    }

    /// Find the longest registered slot prefix that is an ancestor of `rel`
    /// (or equal to it). Returns `(prefix_len_components, prefix, sub_rel)`
    /// where `sub_rel = rel.strip_prefix(prefix)`.
    fn find_slot<R>(&self, rel: &Path, f: impl FnOnce(&Path, &Slot, &Path) -> R) -> Option<R> {
        let slots = self.slots.read();
        let mut best: Option<(usize, &Path, &Slot)> = None;
        for (prefix, slot) in slots.iter() {
            if rel.starts_with(prefix) || rel == prefix.as_path() {
                let depth = prefix.components().count();
                if best.map(|(d, _, _)| depth >= d).unwrap_or(true) {
                    best = Some((depth, prefix, slot));
                }
            }
        }
        best.map(|(_, prefix, slot)| {
            let sub_rel = rel.strip_prefix(prefix).unwrap_or(rel);
            f(prefix, slot, sub_rel)
        })
    }

    fn assign_inode(&self, path: PathBuf) -> u64 {
        let mut state = self.state.lock();
        if let Some(&ino) = state.path_to_inode.get(&path) {
            return ino;
        }
        let ino = state.next_ino;
        state.next_ino += 1;
        state.inode_to_path.insert(ino, path.clone());
        state.path_to_inode.insert(path, ino);
        ino
    }

    fn path_for(&self, ino: u64) -> Option<PathBuf> {
        self.state.lock().inode_to_path.get(&ino).cloned()
    }

    fn parent_ino(&self, parent_path: &Path) -> u64 {
        self.state
            .lock()
            .path_to_inode
            .get(parent_path)
            .copied()
            .unwrap_or_else(|| u64::from(INodeNo::ROOT))
    }

    fn is_deleted(&self, rel: &Path) -> bool {
        self.state.lock().deleted.contains(rel)
    }

    fn mark_deleted(&self, rel: PathBuf) {
        self.state.lock().deleted.insert(rel);
    }

    fn clear_deleted(&self, rel: &Path) {
        self.state.lock().deleted.remove(rel);
    }

    fn set_mode_override(&self, rel: PathBuf, mode: u16) {
        self.state.lock().mode_overrides.insert(rel, mode & 0o7777);
    }

    fn get_mode_override(&self, rel: &Path) -> Option<u16> {
        self.state.lock().mode_overrides.get(rel).copied()
    }

    fn clear_mode_override(&self, rel: &Path) {
        self.state.lock().mode_overrides.remove(rel);
    }

    fn move_mode_override(&self, src: &Path, dst: PathBuf) {
        let mut state = self.state.lock();
        if let Some(m) = state.mode_overrides.remove(src) {
            state.mode_overrides.insert(dst, m);
        }
    }

    fn upper_abs(&self, rel: &Path) -> PathBuf {
        self.upper_root.join(rel)
    }

    /// Look up the kind + size of a path. Upper wins; if not in upper,
    /// find the longest matching slot prefix and consult that slot's
    /// layers. Returns `None` if path is whited-out or absent.
    fn stat_path(&self, rel: &Path) -> Option<NodeAttr> {
        if self.is_deleted(rel) {
            return None;
        }
        let upper = self.upper_abs(rel);
        if let Ok(md) = fs::symlink_metadata(&upper) {
            let kind = if md.file_type().is_symlink() {
                FileType::Symlink
            } else if md.file_type().is_dir() {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            let mode = self
                .get_mode_override(rel)
                .unwrap_or_else(|| (md.mode() & 0o7777) as u16);
            return Some(NodeAttr {
                kind,
                size: md.len(),
                mode,
                mtime: md.modified().unwrap_or(UNIX_EPOCH),
                atime: md.accessed().unwrap_or(UNIX_EPOCH),
                ctime: md
                    .created()
                    .or_else(|_| md.modified())
                    .unwrap_or(UNIX_EPOCH),
                origin: NodeOrigin::Upper,
            });
        }
        // No upper entry; consult the longest matching slot.
        self.find_slot(rel, |_prefix, slot, sub_rel| {
            for (idx, layer) in slot.layers.iter().enumerate() {
                if let Some(entry) = layer.index.entries.get(sub_rel) {
                    let kind = match entry.kind {
                        IndexEntryKind::File => FileType::RegularFile,
                        IndexEntryKind::Symlink(_) => FileType::Symlink,
                        IndexEntryKind::Dir => FileType::Directory,
                    };
                    return Some(NodeAttr {
                        kind,
                        size: entry.size,
                        mode: (entry.mode & 0o7777) as u16,
                        mtime: UNIX_EPOCH,
                        atime: UNIX_EPOCH,
                        ctime: UNIX_EPOCH,
                        origin: NodeOrigin::Layer(idx),
                    });
                }
                if layer.dirs.contains_key(sub_rel) {
                    return Some(NodeAttr {
                        kind: FileType::Directory,
                        size: 0,
                        mode: 0o755,
                        mtime: UNIX_EPOCH,
                        atime: UNIX_EPOCH,
                        ctime: UNIX_EPOCH,
                        origin: NodeOrigin::LayerDir,
                    });
                }
            }
            None
        })
        .flatten()
    }

    fn make_file_attr(&self, ino: u64, attr: &NodeAttr) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size: attr.size,
            blocks: attr.size.div_ceil(BLOCK_SIZE as u64),
            atime: attr.atime,
            mtime: attr.mtime,
            ctime: attr.ctime,
            crtime: attr.ctime,
            kind: attr.kind,
            perm: attr.mode,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: BLOCK_SIZE,
        }
    }

    fn ensure_upper_parents(&self, rel: &Path) -> std::io::Result<()> {
        if let Some(parent) = rel.parent() {
            let dst = self.upper_root.join(parent);
            fs::create_dir_all(&dst)?;
        }
        Ok(())
    }

    /// Copy a file from a read-only layer into the upper layer. Used on
    /// the first write to a layer-only path so subsequent operations are
    /// pure passthrough. The layer's logical mode is recorded as a mode
    /// override so the upper layer's physical mode stays writable but
    /// `getattr` still reports the source mode.
    fn copy_up(&self, rel: &Path) -> std::io::Result<()> {
        self.ensure_upper_parents(rel)?;
        let attr = self
            .stat_path(rel)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
        let dst = self.upper_abs(rel);
        match attr.origin {
            NodeOrigin::Upper => Ok(()),
            NodeOrigin::Layer(idx) => {
                // Re-locate the slot and layer by index that stat_path
                // returned. We re-find the slot (cheap, in-mem) rather
                // than threading the slot through the call.
                let kind_action = self.find_slot(rel, |_prefix, slot, sub_rel| {
                    let layer = slot
                        .layers
                        .get(idx)
                        .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EIO))?;
                    let entry = layer
                        .index
                        .entries
                        .get(sub_rel)
                        .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
                    copy_up_layer_entry(layer, entry, &dst)
                });
                kind_action.ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))??;
                self.set_mode_override(rel.to_path_buf(), attr.mode);
                Ok(())
            }
            NodeOrigin::LayerDir => {
                fs::create_dir_all(&dst)?;
                self.set_mode_override(rel.to_path_buf(), attr.mode);
                Ok(())
            }
        }
    }

    /// Core of the FUSE `create` op without the reply plumbing. Creates
    /// `rel` in the upper layer with a physically-writable mode, records
    /// the client-requested logical mode as an override, and clears any
    /// whiteout. Exposed so unit tests can exercise the create/write
    /// path without mounting FUSE.
    fn create_upper(&self, rel: &Path, logical_mode: u16) -> std::io::Result<()> {
        self.ensure_upper_parents(rel)?;
        let upper = self.upper_abs(rel);
        fs::OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .truncate(true)
            .mode(UPPER_FILE_MODE)
            .open(&upper)?;
        self.clear_deleted(rel);
        self.set_mode_override(rel.to_path_buf(), logical_mode);
        Ok(())
    }

    /// Core of the FUSE `write` op. Copies up from a layer on first
    /// write, then opens the upper file `O_WRONLY` and writes at the
    /// given offset. Safe to call repeatedly because the upper file's
    /// physical mode (`UPPER_FILE_MODE`) always permits the FUSE
    /// process to reopen for writes.
    fn write_upper(&self, rel: &Path, offset: u64, data: &[u8]) -> std::io::Result<u32> {
        let upper = self.upper_abs(rel);
        if !upper.exists() {
            self.copy_up(rel)?;
        }
        let mut f = fs::OpenOptions::new().write(true).open(&upper)?;
        f.seek(SeekFrom::Start(offset))?;
        std::io::Write::write_all(&mut f, data)?;
        Ok(data.len() as u32)
    }
}

/// Helper: copy a layer's entry (file/symlink/dir) into `dst` on the
/// upper layer. Hoisted out of `copy_up` so the slot-find closure
/// remains compact and avoids holding the slots read-lock across IO.
fn copy_up_layer_entry(
    layer: &Layer,
    entry: &hcore::hartifactcontent::tar_index::IndexEntry,
    dst: &Path,
) -> std::io::Result<()> {
    match &entry.kind {
        IndexEntryKind::File => {
            let mut reader = (layer.open)()
                .map_err(|e| std::io::Error::other(format!("open layer reader: {e}")))?;
            reader.seek(SeekFrom::Start(entry.data_offset))?;
            let mut remaining = entry.size as usize;
            let mut out = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(UPPER_FILE_MODE)
                .open(dst)?;
            let mut buf = vec![0u8; 64 * 1024];
            while remaining > 0 {
                let want = remaining.min(buf.len());
                #[expect(clippy::indexing_slicing, reason = "want <= buf.len() by construction")]
                let n = reader.read(&mut buf[..want])?;
                if n == 0 {
                    break;
                }
                #[expect(
                    clippy::indexing_slicing,
                    reason = "n <= buf.len() by Read::read contract"
                )]
                let chunk = &buf[..n];
                std::io::Write::write_all(&mut out, chunk)?;
                remaining = remaining.saturating_sub(n);
            }
            Ok(())
        }
        IndexEntryKind::Symlink(target) => std::os::unix::fs::symlink(target, dst),
        IndexEntryKind::Dir => fs::create_dir_all(dst),
    }
}

#[derive(Clone)]
struct NodeAttr {
    kind: FileType,
    size: u64,
    mode: u16,
    mtime: SystemTime,
    atime: SystemTime,
    ctime: SystemTime,
    origin: NodeOrigin,
}

#[derive(Clone, Copy)]
enum NodeOrigin {
    Upper,
    Layer(usize),
    LayerDir,
}

#[expect(
    clippy::too_many_arguments,
    reason = "signatures dictated by fuser::Filesystem trait"
)]
impl LayeredFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_path) = self.path_for(u64::from(parent)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let rel = parent_path.join(name);
        let Some(attr) = self.stat_path(&rel) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let ino = self.assign_inode(rel);
        reply.entry(&TTL, &self.make_file_attr(ino, &attr), Generation(0));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let raw = u64::from(ino);
        let Some(rel) = self.path_for(raw) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if raw == u64::from(INodeNo::ROOT) {
            let now = SystemTime::now();
            reply.attr(
                &TTL,
                &self.make_file_attr(
                    raw,
                    &NodeAttr {
                        kind: FileType::Directory,
                        size: 0,
                        mode: 0o755,
                        mtime: now,
                        atime: now,
                        ctime: now,
                        origin: NodeOrigin::Upper,
                    },
                ),
            );
            return;
        }
        let Some(attr) = self.stat_path(&rel) else {
            reply.error(Errno::ENOENT);
            return;
        };
        reply.attr(&TTL, &self.make_file_attr(raw, &attr));
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let Some(rel) = self.path_for(u64::from(ino)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if self.is_deleted(&rel) {
            reply.error(Errno::ENOENT);
            return;
        }
        let upper = self.upper_abs(&rel);
        if let Ok(target) = fs::read_link(&upper) {
            reply.data(target.as_os_str().as_bytes());
            return;
        }
        let target = self.find_slot(&rel, |_prefix, slot, sub_rel| {
            for layer in &slot.layers {
                if let Some(entry) = layer.index.entries.get(sub_rel)
                    && let IndexEntryKind::Symlink(target) = &entry.kind
                {
                    return Some(target.clone());
                }
            }
            None
        });
        match target.flatten() {
            Some(t) => reply.data(t.as_os_str().as_bytes()),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(rel) = self.path_for(u64::from(ino)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if self.is_deleted(&rel) {
            reply.error(Errno::ENOENT);
            return;
        }
        let upper = self.upper_abs(&rel);
        if upper.exists() && !upper.is_symlink() {
            match read_upper(&upper, offset, size) {
                Ok(buf) => reply.data(&buf),
                Err(e) => reply.error(Errno::from(e)),
            }
            return;
        }
        // Collect the layer's seek+size + opener under the slot lock, then
        // release before doing IO (the opener may block on sqlite).
        enum ReadHit {
            Bytes(Vec<u8>),
            IsDir,
            NotFile,
        }
        let outcome = self
            .find_slot(&rel, |_prefix, slot, sub_rel| -> Option<ReadHit> {
                for layer in &slot.layers {
                    let Some(entry) = layer.index.entries.get(sub_rel) else {
                        continue;
                    };
                    let IndexEntryKind::File = entry.kind else {
                        return Some(ReadHit::NotFile);
                    };
                    if offset >= entry.size {
                        return Some(ReadHit::Bytes(Vec::new()));
                    }
                    let want = ((entry.size - offset) as usize).min(size as usize);
                    let mut buf = vec![0u8; want];
                    let mut reader = match (layer.open)() {
                        Ok(r) => r,
                        Err(_) => return Some(ReadHit::IsDir),
                    };
                    if reader
                        .seek(SeekFrom::Start(entry.data_offset + offset))
                        .is_err()
                    {
                        return Some(ReadHit::IsDir);
                    }
                    match reader.read(&mut buf) {
                        Ok(n) => {
                            buf.truncate(n);
                            return Some(ReadHit::Bytes(buf));
                        }
                        Err(_) => return Some(ReadHit::IsDir),
                    }
                }
                None
            })
            .flatten();
        match outcome {
            Some(ReadHit::Bytes(b)) => reply.data(&b),
            Some(ReadHit::IsDir) => reply.error(Errno::EIO),
            Some(ReadHit::NotFile) => reply.error(Errno::EISDIR),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let raw = u64::from(ino);
        let Some(rel) = self.path_for(raw) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let mut seen: HashMap<OsString, FileType> = HashMap::new();
        let upper_dir = self.upper_abs(&rel);
        if let Ok(entries) = fs::read_dir(&upper_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let kind = match entry.file_type() {
                    Ok(ft) if ft.is_dir() => FileType::Directory,
                    Ok(ft) if ft.is_symlink() => FileType::Symlink,
                    Ok(_) => FileType::RegularFile,
                    Err(_) => continue,
                };
                seen.insert(name, kind);
            }
        }
        // If `rel` falls inside a registered slot, union the slot's
        // layer children at the sub-path. If `rel` itself sits on a
        // path that contains slot prefixes as descendants (e.g.
        // `rel = <pkg>/__target_<hash>` with slots at `.../ws` and
        // `.../exec_tools`), we still need to expose `ws` and
        // `exec_tools` as visible children — that case is covered by
        // the upper-dir read above when the bridge has created those
        // subdirs on disk before registering the slots.
        let _ = self.find_slot(&rel, |_prefix, slot, sub_rel| {
            for layer in &slot.layers {
                let Some(children) = layer.dirs.get(sub_rel) else {
                    continue;
                };
                for (name, kind) in children {
                    if seen.contains_key(name) {
                        continue;
                    }
                    let ft = match kind {
                        IndexEntryKind::File => FileType::RegularFile,
                        IndexEntryKind::Symlink(_) => FileType::Symlink,
                        IndexEntryKind::Dir => FileType::Directory,
                    };
                    seen.insert(name.clone(), ft);
                }
            }
        });

        let mut entries: Vec<(u64, FileType, OsString)> = Vec::new();
        entries.push((raw, FileType::Directory, OsString::from(".")));
        let parent_ino = if raw == u64::from(INodeNo::ROOT) {
            raw
        } else if let Some(parent) = rel.parent() {
            self.parent_ino(parent)
        } else {
            u64::from(INodeNo::ROOT)
        };
        entries.push((parent_ino, FileType::Directory, OsString::from("..")));
        for (name, kind) in seen.into_iter() {
            let child_path = rel.join(&name);
            if self.is_deleted(&child_path) {
                continue;
            }
            let child_ino = self.assign_inode(child_path);
            entries.push((child_ino, kind, name));
        }
        let skip = usize::try_from(offset).unwrap_or(0);
        for (i, (eino, etype, name)) in entries.into_iter().enumerate().skip(skip) {
            if reply.add(INodeNo(eino), (i + 1) as u64, etype, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(parent_path) = self.path_for(u64::from(parent)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let rel = parent_path.join(name);
        match self.create_upper(&rel, (mode & 0o7777) as u16) {
            Ok(()) => {
                let ino = self.assign_inode(rel.clone());
                let Some(attr) = self.stat_path(&rel) else {
                    reply.error(Errno::EIO);
                    return;
                };
                reply.created(
                    &TTL,
                    &self.make_file_attr(ino, &attr),
                    Generation(0),
                    FileHandle(0),
                    FopenFlags::empty(),
                );
                self.invalidate_entry(parent, name);
            }
            Err(e) => reply.error(Errno::from(e)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(parent_path) = self.path_for(u64::from(parent)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let rel = parent_path.join(name);
        if let Err(e) = self.ensure_upper_parents(&rel) {
            reply.error(Errno::from(e));
            return;
        }
        let upper = self.upper_abs(&rel);
        match fs::create_dir(&upper) {
            Ok(()) => {
                if let Err(e) =
                    fs::set_permissions(&upper, fs::Permissions::from_mode(UPPER_DIR_MODE))
                {
                    reply.error(Errno::from(e));
                    return;
                }
                self.clear_deleted(&rel);
                self.set_mode_override(rel.clone(), (mode & 0o7777) as u16);
                let ino = self.assign_inode(rel.clone());
                let Some(attr) = self.stat_path(&rel) else {
                    reply.error(Errno::EIO);
                    return;
                };
                reply.entry(&TTL, &self.make_file_attr(ino, &attr), Generation(0));
                self.invalidate_entry(parent, name);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => reply.error(Errno::EEXIST),
            Err(e) => reply.error(Errno::from(e)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let Some(rel) = self.path_for(u64::from(ino)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.write_upper(&rel, offset, data) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(Errno::from(e)),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let raw = u64::from(ino);
        let Some(rel) = self.path_for(raw) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let upper = self.upper_abs(&rel);
        if !upper.exists()
            && let Err(e) = self.copy_up(&rel)
        {
            reply.error(Errno::from(e));
            return;
        }
        if let Some(new_size) = size
            && let Err(e) = (|| -> std::io::Result<()> {
                let f = fs::OpenOptions::new().write(true).open(&upper)?;
                f.set_len(new_size)?;
                Ok(())
            })()
        {
            reply.error(Errno::from(e));
            return;
        }
        if let Some(m) = mode {
            // Record the requested mode for `getattr` but leave the
            // upper file's physical mode at `UPPER_FILE_MODE`/
            // `UPPER_DIR_MODE`, so the FUSE process can still reopen
            // it for subsequent writes even if the client requested a
            // read-only mode.
            self.set_mode_override(rel.clone(), (m & 0o7777) as u16);
        }
        let Some(attr) = self.stat_path(&rel) else {
            reply.error(Errno::EIO);
            return;
        };
        reply.attr(&TTL, &self.make_file_attr(raw, &attr));
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_path) = self.path_for(u64::from(parent)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let rel = parent_path.join(name);
        let upper = self.upper_abs(&rel);
        if upper.exists()
            && let Err(e) = fs::remove_file(&upper)
            && e.kind() != std::io::ErrorKind::IsADirectory
        {
            reply.error(Errno::from(e));
            return;
        }
        self.clear_mode_override(&rel);
        if self.stat_path(&rel).is_some() {
            self.mark_deleted(rel);
        }
        reply.ok();
        self.invalidate_entry(parent, name);
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_path) = self.path_for(u64::from(parent)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let rel = parent_path.join(name);
        let upper = self.upper_abs(&rel);
        if upper.exists()
            && let Err(e) = fs::remove_dir(&upper)
        {
            reply.error(Errno::from(e));
            return;
        }
        self.clear_mode_override(&rel);
        self.mark_deleted(rel);
        reply.ok();
        self.invalidate_entry(parent, name);
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let Some(parent_path) = self.path_for(u64::from(parent)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let rel = parent_path.join(link_name);
        if let Err(e) = self.ensure_upper_parents(&rel) {
            reply.error(Errno::from(e));
            return;
        }
        let upper = self.upper_abs(&rel);
        match std::os::unix::fs::symlink(target, &upper) {
            Ok(()) => {
                self.clear_deleted(&rel);
                let ino = self.assign_inode(rel.clone());
                let Some(attr) = self.stat_path(&rel) else {
                    reply.error(Errno::EIO);
                    return;
                };
                reply.entry(&TTL, &self.make_file_attr(ino, &attr), Generation(0));
                self.invalidate_entry(parent, link_name);
            }
            Err(e) => reply.error(Errno::from(e)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (Some(parent_path), Some(newparent_path)) = (
            self.path_for(u64::from(parent)),
            self.path_for(u64::from(newparent)),
        ) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let src = parent_path.join(name);
        let dst = newparent_path.join(newname);
        let upper_src = self.upper_abs(&src);
        let upper_dst = self.upper_abs(&dst);
        if !upper_src.exists()
            && let Err(e) = self.copy_up(&src)
        {
            reply.error(Errno::from(e));
            return;
        }
        if let Err(e) = self.ensure_upper_parents(&dst) {
            reply.error(Errno::from(e));
            return;
        }
        match fs::rename(&upper_src, &upper_dst) {
            Ok(()) => {
                self.move_mode_override(&src, dst.clone());
                if self.stat_path(&src).is_some() {
                    self.mark_deleted(src);
                }
                self.clear_deleted(&dst);
                reply.ok();
                self.invalidate_entry(parent, name);
                self.invalidate_entry(newparent, newname);
            }
            Err(e) => reply.error(Errno::from(e)),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        reply.ok();
    }

    /// Silently accept setxattr. macOS `cp` propagates xattrs by default
    /// (`com.apple.quarantine`, `com.apple.macl`, etc.) and aborts with
    /// EPERM/EIO when the target rejects them. Sandbox files don't need
    /// xattrs to survive, so the simplest cure is to ack the write and
    /// drop the data on the floor.
    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    /// Report "no such attribute" instead of ENOSYS so `cp` and friends
    /// stop chasing xattrs we never stored anyway.
    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::from_i32(ENOATTR));
    }

    /// Report empty xattr list — keeps `cp`'s listxattr probe happy.
    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::from_i32(ENOATTR));
    }
}

/// macOS-style "no such attribute" errno. Linux exposes the same value as
/// `ENODATA` (the names alias the constant); we centralise on the macOS
/// spelling because the workaround is macFUSE-driven.
#[cfg(target_os = "macos")]
const ENOATTR: i32 = libc::ENOATTR;
#[cfg(not(target_os = "macos"))]
const ENOATTR: i32 = libc::ENODATA;

fn read_upper(path: &Path, offset: u64, size: u32) -> std::io::Result<Vec<u8>> {
    let mut f = fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; size as usize];
    let mut filled = 0usize;
    while filled < buf.len() {
        #[expect(
            clippy::indexing_slicing,
            reason = "filled <= buf.len() loop invariant"
        )]
        let tail = &mut buf[filled..];
        match f.read(tail)? {
            0 => break,
            n => filled += n,
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Thin wrapper that lets us share a `LayeredFs` between the FUSE
/// session thread (which moves the filesystem in) and external callers
/// that need to register/unregister slots while the mount is live.
pub struct MountFs(pub std::sync::Arc<LayeredFs>);

impl Filesystem for MountFs {
    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.0.lookup(req, parent, name, reply)
    }
    fn getattr(&self, req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        self.0.getattr(req, ino, fh, reply)
    }
    fn readlink(&self, req: &Request, ino: INodeNo, reply: ReplyData) {
        self.0.readlink(req, ino, reply)
    }
    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        flags: OpenFlags,
        lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        self.0
            .read(req, ino, fh, offset, size, flags, lock_owner, reply)
    }
    fn readdir(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        reply: ReplyDirectory,
    ) {
        self.0.readdir(req, ino, fh, offset, reply)
    }
    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        self.0.open(req, ino, flags, reply)
    }
    fn opendir(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        self.0.opendir(req, ino, flags, reply)
    }
    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        self.0.create(req, parent, name, mode, umask, flags, reply)
    }
    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        self.0.mkdir(req, parent, name, mode, umask, reply)
    }
    fn write(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        write_flags: WriteFlags,
        flags: OpenFlags,
        lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        self.0.write(
            req,
            ino,
            fh,
            offset,
            data,
            write_flags,
            flags,
            lock_owner,
            reply,
        )
    }
    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        self.0.setattr(
            req, ino, mode, uid, gid, size, atime, mtime, ctime, fh, crtime, chgtime, bkuptime,
            flags, reply,
        )
    }
    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.0.unlink(req, parent, name, reply)
    }
    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.0.rmdir(req, parent, name, reply)
    }
    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        self.0.symlink(req, parent, link_name, target, reply)
    }
    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        self.0
            .rename(req, parent, name, newparent, newname, flags, reply)
    }
    fn fsync(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.0.fsync(req, ino, fh, datasync, reply)
    }
    fn flush(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        self.0.flush(req, ino, fh, lock_owner, reply)
    }
    fn access(&self, req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        self.0.access(req, ino, mask, reply)
    }
    fn setxattr(
        &self,
        req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        self.0
            .setxattr(req, ino, name, value, flags, position, reply)
    }
    fn getxattr(&self, req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        self.0.getxattr(req, ino, name, size, reply)
    }
    fn listxattr(&self, req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        self.0.listxattr(req, ino, size, reply)
    }
    fn removexattr(&self, req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.0.removexattr(req, ino, name, reply)
    }
}

/// RAII handle for a mounted FUSE session. `Drop` triggers unmount via
/// `BackgroundSession`'s teardown.
///
/// Field order matters: `notifier_thread` and `fs` are declared BEFORE
/// `_session` so the custom `Drop` impl can close the notifier sender
/// and join the notifier thread (releasing its cloned FUSE channel FD)
/// before `_session` runs its own `Drop` and issues `unmount(2)`.
/// macFUSE blocks umount if any FD into the device is still open, so
/// the explicit ordering avoids the deadlock that wedged the process
/// in `?E+` (exit-in-progress) state.
pub struct Mount {
    notifier_thread: Option<std::thread::JoinHandle<()>>,
    fs: std::sync::Arc<LayeredFs>,
    _session: BackgroundSession,
}

impl Drop for Mount {
    fn drop(&mut self) {
        // 1. Drop the only retained `Sender` (the one stashed in
        //    `LayeredFs.notifier_tx`). The notifier thread now sees
        //    `rx.recv()` return `Err` and exits.
        self.fs.close_notifier();
        // 2. Join the notifier thread so its captured `Notifier`
        //    (which holds a cloned channel FD via fuser internals) is
        //    fully dropped before `_session` umounts. Without this,
        //    macFUSE's umount blocks indefinitely.
        if let Some(handle) = self.notifier_thread.take() {
            _ = handle.join();
        }
        // 3. `_session` drops via field order — `BackgroundSession::drop`
        //    now sees no extra FDs and umount can complete.
    }
}

// SAFETY: `_session` is never accessed after construction — the only
// `BackgroundSession` method we call (`.notifier()`) runs before the
// value is stored, and `Drop` runs with exclusive `&mut self`. On Linux
// `fuser::mnt::fuse3::MountImpl` holds a `*mut c_void` owned by libfuse
// that our code never touches, so cross-thread sharing of the inert
// wrapper is safe. macOS already auto-derives both traits.
#[cfg(target_os = "linux")]
unsafe impl Send for Mount {}
// SAFETY: same reasoning as the `Send` impl above — the inert libfuse handle
// is never accessed after construction, so cross-thread sharing is safe.
#[cfg(target_os = "linux")]
unsafe impl Sync for Mount {}

#[expect(
    clippy::self_named_constructors,
    reason = "verb matches the operation: mounting yields a Mount handle"
)]
impl Mount {
    /// Mount the given `LayeredFs` (shared via Arc so the caller keeps a
    /// handle for slot registration while the session thread owns its
    /// own ref). The mount unmounts when this `Mount` is dropped.
    pub fn mount(mountpoint: &Path, fs: std::sync::Arc<LayeredFs>) -> anyhow::Result<Self> {
        // AutoUnmount = kernel safety net: kext tears the mount down
        // when our process dies (crash, kill -9, panic). fuser 0.17
        // moved the allow_root/allow_other selector to Config.acl;
        // RootAndOwner is the narrower choice for sandbox use (only
        // root + mount owner, no other users).
        //
        // Linux note: this path still requires `user_allow_other` in
        // /etc/fuse.conf when running as non-root.
        // AutoUnmount = kernel safety net: kext/fusermount tears the
        // mount down if our process dies (SIGKILL, panic without
        // unwind, OOM). Required: a crash without AutoUnmount strands
        // the mount until manual `umount` or next startup sweep, and
        // child sandbox processes inherit a broken view.
        //
        // AutoUnmount needs acl != Owner — fuser hard-errors otherwise
        // because fusermount-side supervision is keyed on allow_other
        // /allow_root. We pick RootAndOwner (allow_root): owner + root
        // only, no other UIDs. On Linux this also needs
        // `user_allow_other` in /etc/fuse.conf for unprivileged mounts.
        //
        // Multi-threaded session (Linux only): each FUSE op served on a
        // worker, so a slow read in one layer doesn't block other ops.
        // fuser hard-errors `n_threads != 1` on macOS, and `clone_fd`
        // uses Linux-specific `FUSE_DEV_IOC_CLONE`.
        let mut config = Config::default();
        config.acl = SessionACL::RootAndOwner;
        config.mount_options.push(fuser::MountOption::AutoUnmount);
        #[cfg(target_os = "linux")]
        {
            config.n_threads = Some(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1),
            );
            config.clone_fd = true;
        }
        let fs_clone = fs.clone();
        let session = fuser::spawn_mount2(MountFs(fs), mountpoint, &config)
            .with_context(|| format!("mount FUSE at {:?}", mountpoint))?;
        // Plumb a Notifier so the FS can invalidate kernel caches after
        // creating new entries — macFUSE 5.x otherwise caches negative
        // lookups past our writes and returns stale ENOENT.
        let notifier_thread = fs_clone.set_notifier(session.notifier());
        Ok(Self {
            notifier_thread: Some(notifier_thread),
            fs: fs_clone,
            _session: session,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hartifactcontent::tar::TarPacker;
    use std::io::Cursor;

    fn pack(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut p = TarPacker::new();
        for (path, data) in files {
            p.create_raw(data.to_vec(), (*path).to_string(), false);
        }
        let mut buf = Vec::new();
        p.pack(&mut buf).expect("pack");
        buf
    }

    fn layer_from_tar(bytes: Vec<u8>) -> Layer {
        let index = TarIndex::build(Cursor::new(bytes.clone())).expect("index");
        let opener: LayerOpener = Box::new(move || Ok(Box::new(Cursor::new(bytes.clone()))));
        Layer::new(index, opener)
    }

    #[test]
    fn build_layer_dirs_materializes_implicit_parents() {
        let layer = layer_from_tar(pack(&[("a/b/c.txt", b"x"), ("a/d.txt", b"y")]));
        let root_children = layer.dirs.get(Path::new("")).expect("root");
        let names: Vec<_> = root_children.iter().map(|(n, _)| n.clone()).collect();
        assert!(names.iter().any(|n| n == "a"), "got {names:?}");
        let a_children = layer.dirs.get(Path::new("a")).expect("a dir");
        let a_names: Vec<_> = a_children.iter().map(|(n, _)| n.clone()).collect();
        assert!(a_names.iter().any(|n| n == "b"));
        assert!(a_names.iter().any(|n| n == "d.txt"));
        let b_children = layer.dirs.get(Path::new("a/b")).expect("a/b dir");
        let b_names: Vec<_> = b_children.iter().map(|(n, _)| n.clone()).collect();
        assert!(b_names.iter().any(|n| n == "c.txt"));
    }

    #[test]
    fn stat_path_resolves_through_layer() {
        let layer = layer_from_tar(pack(&[("a/b.txt", b"hi")]));
        let upper = tempfile::tempdir().expect("upper");
        let fs = LayeredFs::new(vec![layer], upper.path().to_path_buf());
        let attr = fs.stat_path(Path::new("a/b.txt")).expect("layer file");
        assert_eq!(attr.kind, FileType::RegularFile);
        assert_eq!(attr.size, 2);
        let dir = fs.stat_path(Path::new("a")).expect("layer dir");
        assert_eq!(dir.kind, FileType::Directory);
    }

    #[test]
    fn whiteout_hides_layer_file() {
        let layer = layer_from_tar(pack(&[("x.txt", b"hi")]));
        let upper = tempfile::tempdir().expect("upper");
        let fs = LayeredFs::new(vec![layer], upper.path().to_path_buf());
        assert!(fs.stat_path(Path::new("x.txt")).is_some());
        fs.mark_deleted(PathBuf::from("x.txt"));
        assert!(fs.stat_path(Path::new("x.txt")).is_none());
    }

    #[test]
    fn upper_wins_over_layer() {
        let layer = layer_from_tar(pack(&[("y.txt", b"layer")]));
        let upper = tempfile::tempdir().expect("upper");
        std::fs::write(upper.path().join("y.txt"), b"upperbytes").expect("write upper");
        let fs = LayeredFs::new(vec![layer], upper.path().to_path_buf());
        let attr = fs.stat_path(Path::new("y.txt")).expect("upper file");
        assert_eq!(attr.size, "upperbytes".len() as u64);
        assert!(matches!(attr.origin, NodeOrigin::Upper));
    }

    #[test]
    fn copy_up_materializes_layer_file_to_upper() {
        let layer = layer_from_tar(pack(&[("dir/f.txt", b"layered")]));
        let upper = tempfile::tempdir().expect("upper");
        let fs = LayeredFs::new(vec![layer], upper.path().to_path_buf());
        fs.copy_up(Path::new("dir/f.txt")).expect("copy_up");
        let bytes = std::fs::read(upper.path().join("dir/f.txt")).expect("read upper");
        assert_eq!(bytes, b"layered");
    }

    #[test]
    fn write_after_create_with_readonly_mode_succeeds() {
        // Regression: `cp -R` of a Go module cache creates files with
        // mode 0o444 (preserved from source). The FUSE write op
        // reopens the upper file with O_WRONLY; if upper physical
        // mode is 0o444, the reopen fails EACCES. Fix decouples
        // logical mode (reported via getattr) from upper physical
        // mode (always FUSE-process-writable).
        let upper = tempfile::tempdir().expect("upper");
        let fs = LayeredFs::new(vec![], upper.path().to_path_buf());
        let rel = PathBuf::from("LICENSE");

        fs.create_upper(&rel, 0o444).expect("create");

        let reported = fs.stat_path(&rel).expect("stat");
        assert_eq!(reported.mode, 0o444, "getattr reports requested mode");

        let physical_md = std::fs::metadata(upper.path().join(&rel)).expect("metadata");
        assert_eq!(
            physical_md.mode() & 0o7777,
            UPPER_FILE_MODE,
            "upper file is FUSE-process-writable regardless of logical mode"
        );

        let n = fs.write_upper(&rel, 0, b"hello").expect("write");
        assert_eq!(n, 5);
        let bytes = std::fs::read(upper.path().join(&rel)).expect("read");
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn write_after_copy_up_of_readonly_layer_file_succeeds() {
        // Same regression on the copy-up path: a layer file whose
        // tar entry has mode 0o444 must be copy-up'd into a writable
        // upper file so the subsequent write op succeeds. The
        // reported mode must still match the layer entry.
        let mut p = TarPacker::new();
        p.create_raw(b"layered".to_vec(), "ro.txt".to_string(), false);
        let mut tar_bytes = Vec::new();
        p.pack(&mut tar_bytes).expect("pack");
        let mut index = TarIndex::build(Cursor::new(tar_bytes.clone())).expect("index");
        for entry in index.entries.values_mut() {
            entry.mode = 0o444;
        }
        let opener: LayerOpener = Box::new(move || Ok(Box::new(Cursor::new(tar_bytes.clone()))));
        let layer = Layer::new(index, opener);
        let upper = tempfile::tempdir().expect("upper");
        let fs = LayeredFs::new(vec![layer], upper.path().to_path_buf());

        let rel = PathBuf::from("ro.txt");
        let n = fs.write_upper(&rel, 0, b"NEW").expect("write");
        assert_eq!(n, 3);

        let physical_md = std::fs::metadata(upper.path().join(&rel)).expect("metadata");
        assert_eq!(physical_md.mode() & 0o7777, UPPER_FILE_MODE);

        let reported = fs.stat_path(&rel).expect("stat");
        assert_eq!(reported.mode, 0o444, "getattr still reports layer mode");
    }
}
