use anyhow::Context;
use std::collections::BTreeMap;
use std::io::{Read, Seek};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum IndexEntryKind {
    File,
    Symlink(PathBuf),
    Dir,
}

#[derive(Debug, Clone)]
pub struct IndexEntry {
    /// Byte offset of file content within the archive. Symlink/dir entries
    /// keep the offset for completeness but it should not be read.
    pub data_offset: u64,
    pub size: u64,
    pub mode: u32,
    pub kind: IndexEntryKind,
}

/// In-memory map from archive-relative path → `IndexEntry`. Built once per
/// layer over a seekable tar so FUSE reads can pread into the source bytes
/// without re-walking headers.
#[derive(Debug, Default, Clone)]
pub struct TarIndex {
    pub entries: BTreeMap<PathBuf, IndexEntry>,
}

impl TarIndex {
    pub fn build<R: Read + Seek>(reader: R) -> anyhow::Result<Self> {
        let mut archive = tar::Archive::new(reader);
        let mut entries = BTreeMap::new();
        let iter = archive
            .entries_with_seek()
            .context("tar archive entries_with_seek")?;
        for entry in iter {
            let entry = entry.context("read tar entry header")?;
            let header = entry.header();
            let entry_type = header.entry_type();
            let path: PathBuf = entry.path().context("decode tar entry path")?.into_owned();
            let mode = header.mode().context("read tar entry mode")?;
            let size = header.size().context("read tar entry size")?;
            let data_offset = entry.raw_file_position();

            let kind = if entry_type.is_symlink() {
                let target = entry
                    .link_name()
                    .context("decode tar symlink target")?
                    .map(|p| p.into_owned())
                    .ok_or_else(|| {
                        anyhow::anyhow!("tar symlink entry {:?} has no link name", path)
                    })?;
                IndexEntryKind::Symlink(target)
            } else if entry_type.is_dir() {
                IndexEntryKind::Dir
            } else if entry_type.is_file() {
                IndexEntryKind::File
            } else {
                // Hardlinks, fifos, devices etc. — out of scope for sandboxes.
                continue;
            };

            entries.insert(
                path,
                IndexEntry {
                    data_offset,
                    size,
                    mode,
                    kind,
                },
            );
        }
        Ok(Self { entries })
    }

    /// Relative paths of the file and symlink entries, directory entries
    /// excluded — the header-only enumeration backing
    /// [`Content::entry_paths`](crate::hartifactcontent::Content::entry_paths)
    /// for tar-backed content.
    pub fn entry_paths(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .filter(|(_, e)| !matches!(e.kind, IndexEntryKind::Dir))
            .map(|(path, _)| path.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hartifactcontent::tar::TarPacker;
    use std::io::{Cursor, Read, Seek, SeekFrom};

    fn pack_files(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut packer = TarPacker::new();
        for (path, data) in files {
            packer.create_raw(data.to_vec(), (*path).to_string(), false);
        }
        let mut buf = Vec::new();
        packer.pack(&mut buf).expect("pack");
        buf
    }

    #[test]
    fn build_indexes_file_paths_and_sizes() {
        let buf = pack_files(&[
            ("a.txt", b"hello"),
            ("b/c.txt", b"world!!"),
            ("e.bin", &[0u8; 1024]),
        ]);
        let idx = TarIndex::build(Cursor::new(buf)).expect("build");
        assert_eq!(idx.entries.len(), 3);
        let a = idx.entries.get(std::path::Path::new("a.txt")).expect("a");
        assert!(matches!(a.kind, IndexEntryKind::File));
        assert_eq!(a.size, 5);
        let b = idx.entries.get(std::path::Path::new("b/c.txt")).expect("b");
        assert_eq!(b.size, 7);
        let e = idx.entries.get(std::path::Path::new("e.bin")).expect("e");
        assert_eq!(e.size, 1024);
    }

    #[test]
    fn data_offsets_point_to_correct_bytes() {
        let buf = pack_files(&[("a.txt", b"hello"), ("b.txt", b"world!")]);
        let idx = TarIndex::build(Cursor::new(buf.clone())).expect("build");
        let mut cur = Cursor::new(buf);
        let a = idx.entries.get(std::path::Path::new("a.txt")).expect("a");
        cur.seek(SeekFrom::Start(a.data_offset)).expect("seek");
        let mut out = vec![0u8; a.size as usize];
        cur.read_exact(&mut out).expect("read");
        assert_eq!(out, b"hello");

        let b = idx.entries.get(std::path::Path::new("b.txt")).expect("b");
        cur.seek(SeekFrom::Start(b.data_offset)).expect("seek");
        let mut out = vec![0u8; b.size as usize];
        cur.read_exact(&mut out).expect("read");
        assert_eq!(out, b"world!");
    }

    #[test]
    fn entry_paths_excludes_directories() {
        // Pack an explicit directory entry plus a file under it. entry_paths
        // must return the file (and any symlink) but not the directory.
        let mut buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut buf);
            let mut dh = tar::Header::new_gnu();
            dh.set_entry_type(tar::EntryType::Directory);
            dh.set_size(0);
            dh.set_mode(0o755);
            b.append_data(&mut dh, "d/", std::io::empty())
                .expect("append dir");
            let mut fh = tar::Header::new_gnu();
            fh.set_entry_type(tar::EntryType::Regular);
            fh.set_size(3);
            fh.set_mode(0o644);
            b.append_data(&mut fh, "d/f.txt", &b"abc"[..])
                .expect("append file");
            b.finish().expect("finish");
        }
        let idx = TarIndex::build(Cursor::new(buf)).expect("build");
        let paths = idx.entry_paths();
        assert!(
            paths.contains(&PathBuf::from("d/f.txt")),
            "file must be listed: {paths:?}"
        );
        assert!(
            !paths
                .iter()
                .any(|p| p == std::path::Path::new("d/") || p == std::path::Path::new("d")),
            "directory entry must be excluded: {paths:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn build_indexes_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target.txt");
        std::fs::write(&target, b"data").unwrap();
        let link = dir.path().join("link.txt");
        symlink("target.txt", &link).unwrap();
        let mut packer = TarPacker::new();
        packer.create_file(
            target.to_str().unwrap().to_string(),
            "target.txt".to_string(),
        );
        packer.create_file(link.to_str().unwrap().to_string(), "link.txt".to_string());
        let mut buf = Vec::new();
        packer.pack(&mut buf).expect("pack");
        let idx = TarIndex::build(Cursor::new(buf)).expect("build");
        let l = idx
            .entries
            .get(std::path::Path::new("link.txt"))
            .expect("link.txt indexed");
        match &l.kind {
            IndexEntryKind::Symlink(t) => assert_eq!(t, std::path::Path::new("target.txt")),
            other => panic!("expected symlink, got {:?}", other),
        }
    }
}
