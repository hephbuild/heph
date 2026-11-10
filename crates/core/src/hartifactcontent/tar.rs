use crate::hartifactcontent::{StopRead, WalkEntry, WalkEntryKind};
use anyhow::Context;
use std::fs::File;
use std::io::{self, Cursor, Read, Write};
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

struct PoisonableReader {
    inner: Box<dyn Read>,
    poisoned: Arc<AtomicBool>,
}

impl Read for PoisonableReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(io::Error::other(
                "stale tar entry: next() advanced past this entry",
            ));
        }
        self.inner.read(buf)
    }
}

// entries must be declared before _archive: Rust drops fields in declaration order,
// so entries (which borrows from archive's heap) is freed before _archive.
pub struct TarWalker {
    entries: tar::Entries<'static, Box<dyn Read>>,
    _archive: Box<tar::Archive<Box<dyn Read>>>,
    current_poison: Option<Arc<AtomicBool>>,
}

impl TarWalker {
    pub fn new<R: Read + 'static>(from: R) -> anyhow::Result<Self> {
        let boxed: Box<dyn Read> = Box::new(from);
        let mut archive = Box::new(tar::Archive::new(boxed));
        let ptr: *mut tar::Archive<Box<dyn Read>> = &mut *archive;
        // SAFETY: ptr is derived from a Box-allocated archive whose heap address is stable across moves.
        // entries borrows from that heap allocation, not the Box pointer.
        // Field order in TarWalker guarantees entries is dropped before _archive.
        let raw_entries = unsafe { (*ptr).entries()? };
        // SAFETY: transmute extends the lifetime to 'static; safe because _archive (stored as a field)
        // outlives entries due to TarWalker field declaration order and Rust drop order guarantees.
        let entries: tar::Entries<'static, Box<dyn Read>> = unsafe { mem::transmute(raw_entries) };
        Ok(Self {
            entries,
            _archive: archive,
            current_poison: None,
        })
    }
}

impl Iterator for TarWalker {
    type Item = anyhow::Result<WalkEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(poison) = self.current_poison.take() {
            poison.store(true, Ordering::Release);
        }
        loop {
            let entry = match self.entries.next()? {
                Ok(e) => e,
                Err(e) => return Some(Err(e.into())),
            };
            let entry_type = entry.header().entry_type();
            let path: PathBuf = match entry.path() {
                Ok(p) => p.into_owned(),
                Err(e) => return Some(Err(e.into())),
            };
            if entry_type.is_symlink() {
                let target = match entry.link_name() {
                    Ok(Some(p)) => p.into_owned(),
                    Ok(None) => {
                        return Some(Err(anyhow::anyhow!(
                            "tar symlink {:?} has no link name",
                            path
                        )));
                    }
                    Err(e) => return Some(Err(e.into())),
                };
                if target.is_absolute() {
                    return Some(Err(anyhow::anyhow!(
                        "absolute symlink not allowed in tar: {:?} -> {:?}",
                        path,
                        target
                    )));
                }
                return Some(Ok(WalkEntry {
                    path,
                    kind: WalkEntryKind::Symlink { target },
                }));
            }
            if !entry_type.is_file() {
                continue;
            }
            let mode = match entry.header().mode() {
                Ok(m) => m,
                Err(e) => return Some(Err(e.into())),
            };
            let x = mode & 0o111 != 0;
            // Straight off the header we just parsed — no extra read.
            let size = entry.header().size().unwrap_or(0);
            let poison = Arc::new(AtomicBool::new(false));
            self.current_poison = Some(poison.clone());
            // SAFETY: entry borrows from entries which borrows from archive's stable heap allocation.
            // PoisonableReader poisons on next(), preventing reads after the stream advances.
            let reader: Box<dyn Read> = unsafe {
                let entry = mem::transmute::<
                    tar::Entry<'_, Box<dyn Read>>,
                    tar::Entry<'static, Box<dyn Read>>,
                >(entry);
                Box::new(PoisonableReader {
                    inner: Box::new(entry),
                    poisoned: poison,
                })
            };
            return Some(Ok(WalkEntry {
                path,
                kind: WalkEntryKind::File {
                    data: reader,
                    x,
                    size,
                },
            }));
        }
    }
}

enum PackEntry {
    File { source: String, at: String },
    Raw { data: Vec<u8>, at: String, x: bool },
    AppendTar { path: String },
}

pub struct TarPacker {
    entries: Vec<PackEntry>,
}

impl Default for TarPacker {
    fn default() -> Self {
        Self::new()
    }
}

impl TarPacker {
    pub fn new() -> Self {
        Self { entries: vec![] }
    }

    pub fn create_file(&mut self, path: impl Into<String>, at: impl Into<String>) {
        self.entries.push(PackEntry::File {
            source: path.into(),
            at: at.into(),
        });
    }

    pub fn create_raw(&mut self, data: Vec<u8>, at: impl Into<String>, x: bool) {
        self.entries.push(PackEntry::Raw {
            data,
            at: at.into(),
            x,
        });
    }

    pub fn append_tar(&mut self, path: impl Into<String>) {
        self.entries
            .push(PackEntry::AppendTar { path: path.into() });
    }

    pub fn pack<W: Write>(self, to: W) -> anyhow::Result<()> {
        self.pack_with_stop(to, &|| false)
    }

    /// [`pack`](Self::pack), stopping with an error as soon as `stop` reads
    /// true: polled before each entry and on every chunk of an entry's bytes
    /// (see [`StopRead`]), so a cancelled pack reads and writes at most the
    /// rest of one chunk after it is asked to stop. What was written to `to`
    /// by then is a truncated archive; the caller must not publish it.
    pub fn pack_with_stop<W: Write>(self, to: W, stop: &dyn Fn() -> bool) -> anyhow::Result<()> {
        let mut builder = tar::Builder::new(to);

        for entry in self.entries {
            if stop() {
                anyhow::bail!("tar pack stopped: the job was cancelled");
            }
            match entry {
                PackEntry::File { source, at } => {
                    let symlink_meta = std::fs::symlink_metadata(&source)
                        .with_context(|| format!("lstat: {}", source))?;
                    if symlink_meta.file_type().is_symlink() {
                        let target = std::fs::read_link(&source)
                            .with_context(|| format!("readlink: {}", source))?;
                        if target.is_absolute() {
                            anyhow::bail!(
                                "absolute symlink not allowed: {} -> {}",
                                source,
                                target.display()
                            );
                        }
                        let mut header = tar::Header::new_gnu();
                        header.set_size(0);
                        header.set_entry_type(tar::EntryType::Symlink);
                        header.set_mode(0o777);
                        builder
                            .append_link(&mut header, &at, &target)
                            .with_context(|| {
                                format!("append symlink {} -> {}", at, target.display())
                            })?;
                    } else {
                        let mut src =
                            File::open(&source).with_context(|| format!("open: {}", source))?;
                        let meta = src.metadata()?;
                        let x = is_executable(&meta);
                        let mut header = tar::Header::new_gnu();
                        header.set_size(meta.len());
                        header.set_mode(if x { 0o755 } else { 0o644 });
                        header.set_cksum();
                        let src = StopRead {
                            inner: &mut src,
                            stop,
                        };
                        builder.append_data(&mut header, &at, src)?;
                    }
                }
                PackEntry::Raw { data, at, x } => {
                    let mut header = tar::Header::new_gnu();
                    header.set_size(data.len() as u64);
                    header.set_mode(if x { 0o755 } else { 0o644 });
                    header.set_cksum();
                    builder.append_data(&mut header, &at, Cursor::new(&data))?;
                }
                PackEntry::AppendTar { path } => {
                    let src = File::open(&path).with_context(|| format!("open: {}", path))?;
                    let mut archive = tar::Archive::new(src);
                    for entry in archive.entries()? {
                        if stop() {
                            anyhow::bail!("tar pack stopped in {path}: the job was cancelled");
                        }
                        let mut entry = entry?;
                        let header = entry.header().clone();
                        let entry = StopRead {
                            inner: &mut entry as &mut dyn io::Read,
                            stop,
                        };
                        builder.append(&header, entry)?;
                    }
                }
            }
        }

        builder.finish()?;
        Ok(())
    }
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn pack_relative_symlink_included() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target.txt");
        std::fs::write(&target, b"hello").unwrap();
        let link = dir.path().join("link.txt");
        symlink("target.txt", &link).unwrap();

        let mut packer = TarPacker::new();
        packer.create_file(link.to_str().unwrap(), "link.txt");
        packer.create_file(target.to_str().unwrap(), "target.txt");

        let mut buf = Vec::new();
        packer.pack(&mut buf).expect("pack");

        let mut archive = tar::Archive::new(Cursor::new(&buf));
        let mut found_link = false;
        let mut found_target = false;
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().into_owned();
            let et = entry.header().entry_type();
            if path == std::path::Path::new("link.txt") {
                assert_eq!(et, tar::EntryType::Symlink);
                let link_target = entry.link_name().unwrap().unwrap().into_owned();
                assert_eq!(link_target, std::path::Path::new("target.txt"));
                found_link = true;
            } else if path == std::path::Path::new("target.txt") {
                assert!(et.is_file());
                found_target = true;
            }
        }
        assert!(found_link, "symlink entry missing");
        assert!(found_target, "target file entry missing");
    }

    #[test]
    fn walk_emits_symlink_kind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target.txt");
        std::fs::write(&target, b"hi").unwrap();
        let link = dir.path().join("link.txt");
        symlink("target.txt", &link).unwrap();

        let mut packer = TarPacker::new();
        packer.create_file(link.to_str().unwrap(), "link.txt");
        packer.create_file(target.to_str().unwrap(), "target.txt");

        let mut buf = Vec::new();
        packer.pack(&mut buf).expect("pack");

        let walker = TarWalker::new(Cursor::new(buf)).expect("walker");
        let mut saw_symlink = false;
        let mut saw_file = false;
        for entry in walker {
            let entry = entry.unwrap();
            match entry.kind {
                WalkEntryKind::Symlink { target } => {
                    assert_eq!(entry.path, std::path::Path::new("link.txt"));
                    assert_eq!(target, std::path::Path::new("target.txt"));
                    saw_symlink = true;
                }
                WalkEntryKind::File { .. } => {
                    assert_eq!(entry.path, std::path::Path::new("target.txt"));
                    saw_file = true;
                }
            }
        }
        assert!(saw_symlink && saw_file);
    }

    #[test]
    fn walk_rejects_absolute_symlink() {
        // Build a tar with an absolute symlink manually (bypassing TarPacker's check).
        let mut buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut buf);
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(0o777);
            builder
                .append_link(&mut header, "link.txt", "/etc/hosts")
                .unwrap();
            builder.finish().unwrap();
        }
        let walker = TarWalker::new(Cursor::new(buf)).expect("walker");
        let err = walker
            .into_iter()
            .find_map(|r| r.err())
            .expect("expected error");
        assert!(
            format!("{err}").contains("absolute symlink not allowed"),
            "unexpected: {err}"
        );
    }

    /// Counts what reaches it and trips `stop` once `trip_after` bytes have.
    struct TripWriter {
        written: u64,
        trip_after: u64,
        trip: Arc<AtomicBool>,
    }

    impl Write for TripWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written += buf.len() as u64;
            if self.written >= self.trip_after {
                self.trip.store(true, Ordering::Release);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A pack stopped while copying one large file stops within a chunk,
    /// rather than reading the rest of the file.
    #[test]
    fn pack_stops_inside_a_large_file() {
        const SIZE: u64 = 32 << 20;
        const TRIP_AFTER: u64 = 1 << 20;
        let dir = tempfile::tempdir().expect("tempdir");
        let big = dir.path().join("big.bin");
        File::create(&big)
            .and_then(|f| f.set_len(SIZE))
            .expect("sparse file");

        let mut packer = TarPacker::new();
        packer.create_file(big.to_str().expect("utf-8 path"), "big.bin");
        let trip = Arc::new(AtomicBool::new(false));
        let mut w = TripWriter {
            written: 0,
            trip_after: TRIP_AFTER,
            trip: Arc::clone(&trip),
        };
        let stop = || trip.load(Ordering::Acquire);

        packer
            .pack_with_stop(&mut w, &stop)
            .expect_err("a stopped pack fails");
        assert!(
            w.written < 2 * TRIP_AFTER,
            "stopped within a chunk of the trip: wrote {} of {SIZE}",
            w.written
        );
    }

    /// A pack stopped after one entry adds no further entry.
    #[test]
    fn pack_stops_between_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut packer = TarPacker::new();
        for i in 0..20 {
            let p = dir.path().join(format!("f{i}"));
            std::fs::write(&p, b"x").expect("write");
            packer.create_file(p.to_str().expect("utf-8 path"), format!("f{i}"));
        }
        // One small entry is a 512-byte header plus one 512-byte data block.
        let trip = Arc::new(AtomicBool::new(false));
        let mut w = TripWriter {
            written: 0,
            trip_after: 1024,
            trip: Arc::clone(&trip),
        };
        let stop = || trip.load(Ordering::Acquire);

        let err = packer
            .pack_with_stop(&mut w, &stop)
            .expect_err("a stopped pack fails");
        assert!(format!("{err:#}").contains("cancelled"), "{err:#}");
        // The trip lands on an entry boundary or one entry short of it,
        // depending on how the tar builder splits its writes; either way at
        // most one more entry follows it, not the other eighteen.
        assert!(
            w.written <= 2 * 1024,
            "at most one entry past the stop: wrote {} of {}",
            w.written,
            21 * 1024
        );
    }

    #[test]
    fn pack_absolute_symlink_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let link = dir.path().join("link.txt");
        symlink("/etc/hosts", &link).unwrap();

        let mut packer = TarPacker::new();
        packer.create_file(link.to_str().unwrap(), "link.txt");

        let mut buf = Vec::new();
        let err = packer.pack(&mut buf).expect_err("expected error");
        let msg = format!("{err}");
        assert!(
            msg.contains("absolute symlink not allowed"),
            "unexpected error: {msg}"
        );
    }
}
