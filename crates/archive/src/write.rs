//! Writing an archive reproducibly: [`write`].
//!
//! The same files, given in any order, on any machine, at any time, by any
//! user, produce the same bytes. Nothing about the host reaches them:
//!
//! - **Entries are sorted by name**, component by component (`a/b` before
//!   `a-c`), so neither the order the caller found the files in nor the
//!   filesystem's directory order shows.
//! - **Owners are flattened**: uid/gid 0 and empty owner names. A numeric 0
//!   with `raphael` beside it would still record who built it.
//! - **Modes are normalized**: `0o755` when any execute bit is set, else
//!   `0o644`; a symlink is `0o777`. The rest is umask, which is ambient state.
//! - **Time is the caller's**: every tar entry carries
//!   [`WriteOptions::mtime`], never a file's own. A zip entry always carries
//!   1980-01-01 00:00:00, the earliest time zip can record.
//! - **Compressor settings are fixed**: gzip at [`GZIP_LEVEL`] with no file
//!   name and a zero timestamp in its header, zstd at [`ZSTD_LEVEL`], zip
//!   entries deflated at [`ZIP_DEFLATE_LEVEL`] (a symlink's target stored).
//! - **A symlink is stored as a symlink**, its target as read, never followed.
//!
//! Only files and symlinks are written: a directory is implied by the paths
//! below it, so an empty directory is not carried. A directory, device, fifo
//! or socket given as a member is an error.
//!
//! **Names are checked before anything is written**, for every format: a
//! name is relative and `/`-separated, with no empty, `.` or `..` component
//! and no NUL byte, so no reader of the archive can be steered outside its
//! destination by it (zip-slip). A name need not be UTF-8 for a tar; a zip
//! entry's must be, and one that is not is refused.
//!
//! **A file is read for exactly the length it was stat'ed at.** The header
//! is written first, and a file that grew or shrank under the writer would
//! otherwise leave an entry whose bytes disagree with it; the entry fails
//! instead.
//!
//! The `tar` coreutil writes through here, so its archives and the
//! `archive.create` driver's agree byte for byte given the same mtime.

use crate::{Format, file_mode};
use anyhow::{Context as _, bail};
use hcore::hartifactcontent::StopRead;
use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

/// gzip's level for a `tar.gz`: gzip's own default.
pub const GZIP_LEVEL: u32 = 6;
/// zstd's level for a `tar.zst`: zstd's own default.
pub const ZSTD_LEVEL: i32 = 3;
/// Deflate's level for a zip entry: zlib's own default.
pub const ZIP_DEFLATE_LEVEL: i64 = 6;

/// A symlink's mode: its own bits are not portable, and nothing reads them.
const LINK_MODE: u32 = 0o777;

/// A zip entry of this many bytes or more is written with zip64 sizes.
///
/// 16 MiB below `u32::MAX`: the header is written before the data, so the
/// choice has to cover the *compressed* size too, and deflate can grow
/// incompressible input. zlib's bound for a 4 GiB input is about 1.3 MiB of
/// growth (`deflateBound`), so the headroom is over ten times that. The
/// choice depends only on the file's length, so the same content still gives
/// the same bytes.
pub const ZIP64_FROM: u64 = u32::MAX as u64 - (16 << 20);

/// One entry of an archive being written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// The entry's path inside the archive: relative, `/`-separated.
    pub name: PathBuf,
    /// The file or symlink on disk that becomes the entry.
    pub source: PathBuf,
}

/// What [`write`] stamps that is not in the files themselves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteOptions {
    /// Seconds since the Unix epoch, stamped on every tar entry. Zip ignores
    /// it (see the module docs).
    pub mtime: u64,
}

/// Write `members` into `out` as one `format` archive, sorted by name, and
/// return `out` once everything is written (a buffered caller still flushes
/// it). `cancelled` is polled between entries and on every read of a file's
/// bytes. Every name is checked (see the module docs) before the first byte
/// is written.
pub fn write<W: Write + Seek>(
    out: W,
    format: Format,
    mut members: Vec<Member>,
    opts: WriteOptions,
    cancelled: &dyn Fn() -> bool,
) -> anyhow::Result<W> {
    for m in &members {
        check_name(&m.name).with_context(|| format!("entry name for {}", m.source.display()))?;
    }
    // Stable, and by `Path`'s ordering: component by component.
    members.sort_by(|a, b| a.name.cmp(&b.name));
    match format {
        Format::Tar => write_tar(out, &members, opts.mtime, cancelled),
        Format::TarGz => {
            // No file name and a zero timestamp: gzip normally records both,
            // which is why gzipping the same bytes twice gives two files.
            let gz = flate2::GzBuilder::new()
                .mtime(0)
                .write(out, flate2::Compression::new(GZIP_LEVEL));
            let mut gz = write_tar(gz, &members, opts.mtime, cancelled)?;
            flush_before_finish(&mut gz)?;
            gz.finish().context("finish the gzip stream")
        }
        Format::TarZst => {
            let zst = zstd::stream::Encoder::new(out, ZSTD_LEVEL).context("open zstd stream")?;
            let mut zst = write_tar(zst, &members, opts.mtime, cancelled)?;
            flush_before_finish(&mut zst)?;
            zst.finish().context("finish the zstd stream")
        }
        Format::Zip => write_zip(out, &members, cancelled),
        Format::TarXz | Format::Gz => {
            anyhow::bail!("{} is read only: nothing writes it", format.as_str())
        }
    }
}

/// A compressor is flushed once before it is finished. The flush ends a block
/// (an empty stored block for deflate), so it is in the bytes: the `tar`
/// coreutil has always flushed there, and its archives must not change
/// (`tar_create_bytes_are_pinned` in `crates/coreutils` holds this).
fn flush_before_finish(w: &mut impl Write) -> anyhow::Result<()> {
    w.flush().context("flush the compressor")
}

fn write_tar<W: Write>(
    out: W,
    members: &[Member],
    mtime: u64,
    cancelled: &dyn Fn() -> bool,
) -> anyhow::Result<W> {
    let mut builder = tar::Builder::new(out);
    builder.follow_symlinks(false);
    for m in members {
        stop_check(cancelled)?;
        append_tar(&mut builder, m, mtime, cancelled)
            .with_context(|| format!("add {} as {}", m.source.display(), m.name.display()))?;
    }
    builder.into_inner().context("finish the tar stream")
}

/// Add `m` with every machine-specific field flattened.
fn append_tar<W: Write>(
    builder: &mut tar::Builder<W>,
    m: &Member,
    mtime: u64,
    cancelled: &dyn Fn() -> bool,
) -> anyhow::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_mtime(mtime);
    header.set_uid(0);
    header.set_gid(0);
    header.set_username("").context("clear the owner name")?;
    header.set_groupname("").context("clear the group name")?;
    match kind(&m.source)? {
        Kind::Symlink(target) => {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(LINK_MODE);
            header.set_cksum();
            builder
                .append_link(&mut header, &m.name, &target)
                .context("write a symlink entry")
        }
        Kind::File { mode, len } => {
            header.set_mode(mode);
            header.set_size(len);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            let mut f = ExactLen::open(&m.source, len)?;
            builder
                .append_data(&mut header, &m.name, StopRead::new(&mut f, cancelled))
                .context("write a file entry")
        }
    }
}

fn write_zip<W: Write + Seek>(
    out: W,
    members: &[Member],
    cancelled: &dyn Fn() -> bool,
) -> anyhow::Result<W> {
    let mut zip = zip::ZipWriter::new(out);
    for m in members {
        stop_check(cancelled)?;
        append_zip(&mut zip, m, cancelled)
            .with_context(|| format!("add {} as {}", m.source.display(), m.name.display()))?;
    }
    zip.finish().context("finish the zip")
}

/// Add `m` deflated, dated 1980-01-01, with its normalized mode.
fn append_zip<W: Write + Seek>(
    zip: &mut zip::ZipWriter<W>,
    m: &Member,
    cancelled: &dyn Fn() -> bool,
) -> anyhow::Result<()> {
    use zip::write::SimpleFileOptions;

    let name = m.name.to_str().with_context(|| {
        format!(
            "zip entry name {:?} is not valid UTF-8 (a tar can carry it; a zip cannot)",
            m.name
        )
    })?;
    // Set explicitly: the crate's default is the current time when its `time`
    // feature is on, and another crate in the tree may turn it on.
    let base = SimpleFileOptions::default().last_modified_time(zip::DateTime::default());
    match kind(&m.source)? {
        Kind::Symlink(target) => {
            let target = target
                .to_str()
                .with_context(|| format!("symlink target {target:?} is not valid UTF-8"))?;
            zip.add_symlink(name, target, base.unix_permissions(LINK_MODE))
                .context("write a symlink entry")
        }
        Kind::File { mode, len } => {
            let opts = base
                .compression_method(zip::CompressionMethod::Deflated)
                .compression_level(Some(ZIP_DEFLATE_LEVEL))
                .unix_permissions(mode)
                .large_file(needs_zip64(len));
            zip.start_file(name, opts).context("start a file entry")?;
            let mut f = ExactLen::open(&m.source, len)?;
            io::copy(&mut StopRead::new(&mut f, cancelled), zip).context("write a file entry")?;
            Ok(())
        }
    }
}

/// Whether a zip entry of `len` bytes is written with zip64 sizes: see
/// [`ZIP64_FROM`].
fn needs_zip64(len: u64) -> bool {
    len >= ZIP64_FROM
}

/// A member name is relative and `/`-separated, with no empty, `.` or `..`
/// component and no NUL byte. Checked on the bytes: a tar name need not be
/// UTF-8.
fn check_name(name: &Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;

    let bytes = name.as_os_str().as_bytes();
    if bytes.is_empty() {
        bail!("an entry name must not be empty");
    }
    if bytes.contains(&0) {
        bail!("entry name {name:?} contains a NUL byte");
    }
    if bytes.starts_with(b"/") {
        bail!("entry name {name:?} must be relative");
    }
    if bytes
        .split(|b| *b == b'/')
        .any(|c| c.is_empty() || c == b"." || c == b"..")
    {
        bail!("entry name {name:?} must not contain empty, `.` or `..` components");
    }
    Ok(())
}

/// A file read for exactly the `len` bytes its entry's header was written
/// with. Fewer (it shrank) or more (it grew) fails the read: tar pads by
/// what was read and zip records what was read, so either would otherwise
/// leave an entry that disagrees with its header, or a file that is not the
/// one that was stat'ed.
struct ExactLen {
    file: File,
    len: u64,
    left: u64,
}

impl ExactLen {
    fn open(path: &Path, len: u64) -> anyhow::Result<Self> {
        Ok(Self {
            file: File::open(path).context("open")?,
            len,
            left: len,
        })
    }
}

impl Read for ExactLen {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            // At the stated end: one more byte means the file grew.
            let mut probe = [0u8; 1];
            return match self.file.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(io::Error::other(format!(
                    "the file grew past the {} bytes it had when its header was written",
                    self.len
                ))),
            };
        }
        let cap = usize::try_from(self.left).map_or(buf.len(), |left| left.min(buf.len()));
        let n = match buf.get_mut(..cap) {
            Some(window) => self.file.read(window)?,
            None => 0,
        };
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "the file shrank: it ended {} bytes short of the {} it had when its header \
                     was written",
                    self.left, self.len
                ),
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// What a member is on disk, with what of it reaches the archive.
enum Kind {
    Symlink(PathBuf),
    File { mode: u32, len: u64 },
}

fn kind(source: &Path) -> anyhow::Result<Kind> {
    use std::os::unix::fs::PermissionsExt as _;

    let meta = std::fs::symlink_metadata(source).context("stat")?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        Ok(Kind::Symlink(
            std::fs::read_link(source).context("read the link")?,
        ))
    } else if ft.is_file() {
        Ok(Kind::File {
            mode: file_mode(meta.permissions().mode()),
            len: meta.len(),
        })
    } else if ft.is_dir() {
        bail!("a directory is not a member: name the files under it")
    } else {
        bail!("only files and symlinks are archived, not devices, fifos or sockets")
    }
}

fn stop_check(cancelled: &dyn Fn() -> bool) -> anyhow::Result<()> {
    if cancelled() {
        bail!("stopped between entries: the job was cancelled");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{UnpackOptions, unpack};
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt as _;

    fn never() -> bool {
        false
    }

    fn put(dir: &Path, rel: &str, body: &str, mode: u32) -> Member {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        Member {
            name: PathBuf::from(rel),
            source: path,
        }
    }

    fn link(dir: &Path, rel: &str, target: &str) -> Member {
        let path = dir.join(rel);
        std::os::unix::fs::symlink(target, &path).unwrap();
        Member {
            name: PathBuf::from(rel),
            source: path,
        }
    }

    /// A small tree with every kind of member: files of two modes, a nested
    /// one, a symlink, and a name over tar's 100-byte field. `reverse` creates
    /// it, and lists its members, in the opposite order.
    fn tree(dir: &Path, reverse: bool) -> Vec<Member> {
        let long = format!("deep/{}.txt", "y".repeat(110));
        let mut files = vec![
            ("a.txt", "aye", 0o600),
            ("bin/tool", "#!/bin/sh\n", 0o750),
            (long.as_str(), "long", 0o644),
        ];
        if reverse {
            files.reverse();
        }
        let mut members: Vec<Member> = files
            .into_iter()
            .map(|(rel, body, mode)| put(dir, rel, body, mode))
            .collect();
        let l = link(dir, "link", "a.txt");
        if reverse {
            members.insert(0, l);
        } else {
            members.push(l);
        }
        members
    }

    /// Moves every file's on-disk mtime, which must not reach the bytes.
    fn touch_all(members: &[Member]) {
        let when = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1 << 30);
        for m in members {
            if let Ok(f) = File::options().write(true).open(&m.source) {
                f.set_modified(when).unwrap();
            }
        }
    }

    fn bytes(format: Format, members: Vec<Member>, mtime: u64) -> Vec<u8> {
        write(
            Cursor::new(Vec::new()),
            format,
            members,
            WriteOptions { mtime },
            &never,
        )
        .unwrap()
        .into_inner()
    }

    /// The property the writer exists for: two trees made in a different
    /// order (so different inodes, directory order and on-disk times), handed
    /// over in a different order, give the same bytes in every format.
    #[test]
    fn the_same_tree_writes_the_same_bytes_in_every_format() {
        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();
        let first = tree(one.path(), false);
        let second = tree(two.path(), true);
        touch_all(&second);
        for format in Format::WRITABLE {
            assert_eq!(
                bytes(format, first.clone(), 0),
                bytes(format, second.clone(), 0),
                "{}",
                format.as_str()
            );
        }
    }

    #[test]
    fn the_mtime_is_the_callers_and_only_tar_carries_it() {
        let dir = tempfile::tempdir().unwrap();
        let members = tree(dir.path(), false);
        for format in [Format::Tar, Format::TarGz, Format::TarZst] {
            assert_ne!(
                bytes(format, members.clone(), 0),
                bytes(format, members.clone(), 1_700_000_000),
                "{}",
                format.as_str()
            );
        }
        assert_eq!(
            bytes(Format::Zip, members.clone(), 0),
            bytes(Format::Zip, members, 1_700_000_000),
            "zip always stamps 1980-01-01"
        );
    }

    #[test]
    fn tar_headers_are_flattened_and_modes_normalized() {
        let dir = tempfile::tempdir().unwrap();
        let tar = bytes(Format::Tar, tree(dir.path(), false), 42);
        let mut archive = tar::Archive::new(&tar[..]);
        let mut seen = Vec::new();
        for entry in archive.entries().unwrap() {
            let e = entry.unwrap();
            let h = e.header();
            assert_eq!(h.uid().unwrap(), 0);
            assert_eq!(h.gid().unwrap(), 0);
            assert_eq!(h.username().unwrap(), Some(""));
            assert_eq!(h.groupname().unwrap(), Some(""));
            assert_eq!(h.mtime().unwrap(), 42);
            let path = e.path().unwrap().to_string_lossy().into_owned();
            seen.push((path, h.mode().unwrap(), h.entry_type()));
        }
        let long = format!("deep/{}.txt", "y".repeat(110));
        assert_eq!(
            seen,
            [
                ("a.txt".to_string(), 0o644, tar::EntryType::Regular),
                ("bin/tool".to_string(), 0o755, tar::EntryType::Regular),
                (long, 0o644, tar::EntryType::Regular),
                ("link".to_string(), 0o777, tar::EntryType::Symlink),
            ],
            "sorted by name, 0o600 → 0o644, 0o750 → 0o755"
        );
    }

    /// What `write` makes, `unpack` reads back — every format, symlinks
    /// included, modes as normalized.
    #[test]
    fn every_format_round_trips_through_unpack() {
        let dir = tempfile::tempdir().unwrap();
        let members = tree(dir.path(), false);
        let long = format!("deep/{}.txt", "y".repeat(110));
        for format in Format::WRITABLE {
            let out = tempfile::tempdir().unwrap();
            let archive = out.path().join("a");
            std::fs::write(&archive, bytes(format, members.clone(), 0)).unwrap();
            let dest = out.path().join("x");
            let n = unpack(&archive, format, UnpackOptions::default(), &dest, &never)
                .unwrap_or_else(|e| panic!("{}: {e:#}", format.as_str()));
            assert!(n >= 4, "{}: {n} entries", format.as_str());
            let read = |rel: &str| std::fs::read_to_string(dest.join(rel)).unwrap();
            let mode = |rel: &str| {
                std::fs::metadata(dest.join(rel))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o7777
            };
            assert_eq!(read("a.txt"), "aye");
            assert_eq!(read(&long), "long");
            assert_eq!(mode("a.txt"), 0o644, "{}", format.as_str());
            assert_eq!(mode("bin/tool"), 0o755, "{}", format.as_str());
            let link = dest.join("link");
            assert!(
                std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{}: a symlink stays one",
                format.as_str()
            );
            assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("a.txt"));
        }
    }

    /// A zip as other tools read it: deflated files, the DOS epoch, unix modes.
    #[test]
    fn zip_entries_are_deflated_and_dated_1980() {
        let dir = tempfile::tempdir().unwrap();
        let zip = bytes(Format::Zip, tree(dir.path(), false), 0);
        let mut archive = zip::ZipArchive::new(Cursor::new(zip)).unwrap();
        let mut f = archive.by_name("bin/tool").unwrap();
        assert_eq!(f.compression(), zip::CompressionMethod::Deflated);
        assert_eq!(f.last_modified(), Some(zip::DateTime::default()));
        assert_eq!(f.unix_mode().map(|m| m & 0o7777), Some(0o755));
        let mut body = String::new();
        f.read_to_string(&mut body).unwrap();
        assert_eq!(body, "#!/bin/sh\n");
        drop(f);
        assert!(archive.by_name("link").unwrap().is_symlink());
    }

    #[test]
    fn a_directory_or_missing_member_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        for (source, want) in [("d", "directory"), ("missing", "stat")] {
            let members = vec![Member {
                name: PathBuf::from("x"),
                source: dir.path().join(source),
            }];
            for format in Format::WRITABLE {
                let e = write(
                    Cursor::new(Vec::new()),
                    format,
                    members.clone(),
                    WriteOptions::default(),
                    &never,
                )
                .unwrap_err();
                let msg = format!("{e:#}");
                assert!(msg.contains(want), "{}: {msg}", format.as_str());
            }
        }
    }

    #[test]
    fn a_read_only_format_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let members = tree(dir.path(), false);
        for format in Format::ALL.into_iter().filter(|f| !f.is_writable()) {
            let r = write(
                Cursor::new(Vec::new()),
                format,
                members.clone(),
                WriteOptions::default(),
                &never,
            );
            let msg = format!("{:#}", r.expect_err(format.as_str()));
            assert!(msg.contains("read only"), "{msg}");
        }
    }

    /// The bytes themselves, pinned per format over a fixed tree at a fixed
    /// mtime. A change here changes every archive `archive.create` makes from
    /// the same files: bump `CREATE_FORMAT_VERSION` in `crates/plugin-archive`
    /// with it. The `tar` coreutil writes through here too, so a tar change
    /// also sits behind `COREUTILS_VERSION` (`tar_create_bytes_are_pinned` in
    /// `crates/coreutils` pins its bytes).
    #[test]
    fn the_bytes_are_pinned_per_format() {
        let dir = tempfile::tempdir().unwrap();
        let members = tree(dir.path(), false);
        let got: Vec<(&str, String)> = Format::WRITABLE
            .into_iter()
            .map(|format| {
                let b = bytes(format, members.clone(), 1_700_000_000);
                (
                    format.as_str(),
                    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(&b)),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("tar", "a2239b751f02fc2b".to_string()),
                ("tar.gz", "37f527b4ef80a6fc".to_string()),
                ("tar.zst", "9734f9470e872b50".to_string()),
                ("zip", "0cc696aba54c2c0f".to_string()),
            ]
        );
    }

    #[test]
    fn a_name_that_could_leave_the_destination_is_refused_in_every_format() {
        use std::os::unix::ffi::OsStrExt as _;

        let dir = tempfile::tempdir().unwrap();
        let src = put(dir.path(), "f", "x", 0o644).source;
        let names: [&[u8]; 9] = [
            b"",
            b"/abs",
            b"..",
            b"../up",
            b"a/../../up",
            b"a/./b",
            b"a//b",
            b"dir/",
            b"nul\0byte",
        ];
        for name in names {
            for format in Format::WRITABLE {
                let members = vec![Member {
                    name: PathBuf::from(std::ffi::OsStr::from_bytes(name)),
                    source: src.clone(),
                }];
                let e = write(
                    Cursor::new(Vec::new()),
                    format,
                    members,
                    WriteOptions::default(),
                    &never,
                )
                .unwrap_err();
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("entry name"),
                    "{name:?} {}: {msg}",
                    format.as_str()
                );
            }
        }
        // The near misses are names, not escapes.
        for ok in ["..a", "a..", ".hidden/x", "a/...", "a b/c"] {
            let members = vec![Member {
                name: PathBuf::from(ok),
                source: src.clone(),
            }];
            for format in Format::WRITABLE {
                let r = write(
                    Cursor::new(Vec::new()),
                    format,
                    members.clone(),
                    WriteOptions::default(),
                    &never,
                );
                assert!(r.is_ok(), "{ok} {}: {:#}", format.as_str(), r.unwrap_err());
            }
        }
    }

    /// A tar carries a name's bytes as they are; a zip entry name is UTF-8,
    /// so a zip refuses one that is not rather than mangle it.
    #[test]
    fn a_non_utf8_name_is_carried_by_tar_and_refused_by_zip() {
        use std::os::unix::ffi::OsStrExt as _;

        let dir = tempfile::tempdir().unwrap();
        let src = put(dir.path(), "f", "body", 0o644).source;
        let raw: &[u8] = b"caf\xe9/x.txt";
        let members = vec![Member {
            name: PathBuf::from(std::ffi::OsStr::from_bytes(raw)),
            source: src,
        }];
        for format in [Format::Tar, Format::TarGz, Format::TarZst] {
            let b = bytes(format, members.clone(), 0);
            let tar = match format {
                Format::TarGz => {
                    let mut out = Vec::new();
                    flate2::read::GzDecoder::new(&b[..])
                        .read_to_end(&mut out)
                        .unwrap();
                    out
                }
                Format::TarZst => zstd::decode_all(&b[..]).unwrap(),
                _ => b,
            };
            let mut archive = tar::Archive::new(&tar[..]);
            let mut entries = archive.entries().unwrap();
            let mut e = entries.next().unwrap().unwrap();
            assert_eq!(&*e.path_bytes(), raw, "{}", format.as_str());
            let mut body = String::new();
            e.read_to_string(&mut body).unwrap();
            assert_eq!(body, "body");
        }
        let e = write(
            Cursor::new(Vec::new()),
            Format::Zip,
            members,
            WriteOptions::default(),
            &never,
        )
        .unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("not valid UTF-8"), "{msg}");
    }

    /// Zip64 from 16 MiB below `u32::MAX`, so a deflated entry that grew
    /// still fits its header; below that, the plain 32-bit header.
    #[test]
    fn zip64_is_chosen_with_headroom_below_u32_max() {
        let max = u64::from(u32::MAX);
        assert_eq!(ZIP64_FROM, max - (16 << 20));
        for (len, want) in [
            (0, false),
            (1 << 20, false),
            (ZIP64_FROM - 1, false),
            (ZIP64_FROM, true),
            (max - 1, true),
            (max, true),
            (max + 1, true),
            (u64::MAX, true),
        ] {
            assert_eq!(needs_zip64(len), want, "{len}");
        }
    }

    /// A file read for the length its header was written with: one that grew
    /// or shrank since the stat fails instead of corrupting the entry.
    #[test]
    fn a_file_whose_length_changed_fails_the_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = put(dir.path(), "f", "0123456789", 0o644).source;
        let read = |len: u64| {
            let mut out = Vec::new();
            ExactLen::open(&path, len)
                .unwrap()
                .read_to_end(&mut out)
                .map(|_| out)
        };
        assert_eq!(read(10).unwrap(), b"0123456789");
        let grew = read(4).unwrap_err().to_string();
        assert!(grew.contains("grew past the 4 bytes"), "{grew}");
        let shrank = read(25).unwrap_err().to_string();
        assert!(shrank.contains("15 bytes short of the 25"), "{shrank}");
        let empty = read(0).unwrap_err().to_string();
        assert!(empty.contains("grew past the 0 bytes"), "{empty}");
    }

    /// Cancellation lands inside a member's bytes, not only between entries:
    /// a closure that flips after a few polls stops a multi-MB member partway,
    /// and the member after it is never opened (its source does not exist,
    /// so reaching it would fail with `stat` instead).
    #[test]
    fn a_write_cancelled_mid_member_stops_there_in_every_format() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big");
        std::fs::write(&big, vec![7u8; 8 << 20]).unwrap();
        let members = vec![
            Member {
                name: PathBuf::from("big"),
                source: big,
            },
            Member {
                name: PathBuf::from("later"),
                source: dir.path().join("missing"),
            },
        ];
        for format in Format::WRITABLE {
            let polls = std::cell::Cell::new(0u32);
            let cancelled = || {
                polls.set(polls.get() + 1);
                polls.get() > 3
            };
            let e = write(
                Cursor::new(Vec::new()),
                format,
                members.clone(),
                WriteOptions::default(),
                &cancelled,
            )
            .unwrap_err();
            let msg = format!("{e:#}");
            assert!(
                msg.contains("stopped mid-entry"),
                "{}: {msg}",
                format.as_str()
            );
            assert!(msg.contains("as big"), "{}: {msg}", format.as_str());
            assert!(!msg.contains("later"), "{}: {msg}", format.as_str());
            assert_eq!(
                polls.get(),
                4,
                "{}: no poll after the stop",
                format.as_str()
            );
        }
    }

    #[test]
    fn a_cancelled_write_stops() {
        let dir = tempfile::tempdir().unwrap();
        let members = tree(dir.path(), false);
        for format in Format::WRITABLE {
            let e = write(
                Cursor::new(Vec::new()),
                format,
                members.clone(),
                WriteOptions::default(),
                &|| true,
            )
            .unwrap_err();
            assert!(
                format!("{e:#}").contains("cancelled"),
                "{}",
                format.as_str()
            );
        }
    }
}
