//! Unpacking an untrusted archive into a fresh directory.
//!
//! The whole API is [`unpack`], the [`Format`]s it reads (`tar`, `tar.gz`,
//! `tar.zst`, `zip`; [`Format::infer`] tells one from a file name) and the
//! [`Limits`] it enforces. Format and limits serialize, so a driver can carry
//! them in its def.
//!
//! It runs in-process, with the same code on every supported platform: no host
//! `tar` or `unzip`, whose flags, defaults and symlink handling differ between
//! GNU, BSD and busybox. One difference is the filesystem's, not the code's:
//! two entries whose names differ only by case (or Unicode normalization) both
//! extract on Linux, and on macOS's case-insensitive APFS the second fails,
//! saying so.
//!
//! The threat model: the archive is untrusted input (it came off the network,
//! or out of a target nobody audited), and its author wants to write outside
//! `dest`, leave something behind that reaches outside it later, or exhaust
//! the disk. So the rules are about what it may not do, each enforced per
//! entry, before anything is written for it:
//!
//! - **Nothing lands outside `dest`.** An absolute path or a `..` component is
//!   refused (zip-slip), and so is writing *through* a symlink the archive
//!   created earlier: a link is the other way an entry reaches outside.
//! - **A link stays inside `dest`.** A symlink with an absolute target is
//!   refused at its entry; once everything is written, every symlink is resolved
//!   component by component through the extracted tree (following the links it
//!   meets, as the OS would) and refused if that ever climbs above `dest`. A
//!   hard link names an entry already extracted, and is materialized as a copy.
//! - **Only files, directories and links.** A device or fifo entry is refused.
//! - **Nothing about the host leaks in or out.** Modes are normalized (`0o755`
//!   when any execute bit is set, else `0o644`; directories `0o755`); owners and
//!   mtimes are never applied. Two machines unpacking one archive get the same
//!   tree.
//! - **A path appears once.** A second entry for a path already written is an
//!   error rather than last-wins: which copy an archive "means" is not something
//!   to guess. A directory entry for a directory that already exists is fine —
//!   archivers routinely emit those. A zip whose central directory names one
//!   path twice is refused outright (the zip crate keys entries by name, so it
//!   would otherwise yield only the last).
//! - **The output is bounded.** At most [`Limits::max_bytes`] bytes written
//!   and [`Limits::max_entries`] entries, counted while copying, so a
//!   decompression bomb fails instead of filling the disk.
//! - **A cancelled run stops mid-entry.** The decompressed stream and each
//!   entry's bytes are read through [`StopRead`], which polls the token on
//!   every read.
//!
//! `strip_components` drops that many leading components from every entry, as
//! `tar --strip-components` does — a `.` counts, as in GNU tar and bsdtar, so
//! `./tool-1.0/bin/tool` with 1 is `tool-1.0/bin/tool`; an entry with no more
//! components than that (the top-level directory itself, usually) is skipped.
//! A hard link's target is stripped too; a symlink's is not (it is relative to
//! the link). An archive from which nothing remains is an error, not an empty
//! output.

use anyhow::{Context as _, bail};
use hcore::hartifactcontent::StopRead;
use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};

/// How much one archive may unpack to. Exceeding either fails the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Limits {
    /// Total bytes written for files (hard-link copies included).
    pub max_bytes: u64,
    /// Entries read from the archive, whatever their kind.
    pub max_entries: u64,
}

impl Limits {
    /// 4 GiB: above any release asset in practice, below a disk-filling bomb.
    pub const DEFAULT_MAX_BYTES: u64 = 4 << 30;
    /// A million entries: a large SDK has tens of thousands.
    pub const DEFAULT_MAX_ENTRIES: u64 = 1_000_000;
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: Self::DEFAULT_MAX_BYTES,
            max_entries: Self::DEFAULT_MAX_ENTRIES,
        }
    }
}

/// The archive formats [`unpack`] reads. A `tar.gz` may be several gzip
/// members back to back, as `gzip -d` reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Format {
    #[serde(rename = "tar")]
    Tar,
    #[serde(rename = "tar.gz")]
    TarGz,
    #[serde(rename = "tar.zst")]
    TarZst,
    #[serde(rename = "zip")]
    Zip,
}

impl Format {
    /// Every format, in the order the spellings are listed in errors and in
    /// the schema.
    pub const ALL: [Self; 4] = [Self::Tar, Self::TarGz, Self::TarZst, Self::Zip];

    /// The spelling a BUILD file uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tar => "tar",
            Self::TarGz => "tar.gz",
            Self::TarZst => "tar.zst",
            Self::Zip => "zip",
        }
    }

    /// File-name suffixes that imply this format, lowercase.
    fn suffixes(self) -> &'static [&'static str] {
        match self {
            Self::Tar => &[".tar"],
            Self::TarGz => &[".tar.gz", ".tgz"],
            Self::TarZst => &[".tar.zst", ".tzst"],
            Self::Zip => &[".zip"],
        }
    }

    /// Parse a BUILD-file spelling.
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        Self::ALL
            .into_iter()
            .find(|f| f.as_str() == raw)
            .with_context(|| {
                format!(
                    "unknown archive format {raw:?} (one of: {})",
                    Self::spellings().join(", ")
                )
            })
    }

    /// The format a file name's suffix implies, matched case-insensitively
    /// (`TOOL.TGZ` is a `tar.gz`). `None` when no suffix matches — the suffix
    /// must end the name, so `x.tar.gz.sha256` is not an archive.
    pub fn infer(file_name: &str) -> Option<Self> {
        let lower = file_name.to_ascii_lowercase();
        Self::ALL.into_iter().find(|f| {
            f.suffixes()
                .iter()
                .any(|s| lower.len() > s.len() && lower.ends_with(s))
        })
    }

    /// Every spelling, for messages.
    pub fn spellings() -> Vec<&'static str> {
        Self::ALL.iter().map(|f| f.as_str()).collect()
    }
}

/// Mode of an extracted regular file: executable when the archive says any
/// execute bit is set, nothing else carried over (no setuid, no group-write).
fn file_mode(archived: u32) -> u32 {
    if archived & 0o111 != 0 { 0o755 } else { 0o644 }
}

const DIR_MODE: u32 = 0o755;

/// How many symlinks one resolution may follow before it is refused, as the
/// kernel's own `ELOOP` limit.
const MAX_LINK_HOPS: usize = 40;

/// The longest symlink target accepted, in bytes: macOS's limit (Linux takes
/// up to 4095), so one archive unpacks the same on both.
const MAX_LINK_TARGET: usize = 1023;

/// Unpack `archive` (in `format`) into `dest`, which must not exist yet.
/// `cancelled` is polled between entries and on every read of the archive
/// stream and of each entry's bytes. Returns the number of entries written
/// (files, directories and links).
pub fn unpack(
    archive: &Path,
    format: Format,
    strip_components: u32,
    limits: Limits,
    dest: &Path,
    cancelled: &dyn Fn() -> bool,
) -> anyhow::Result<usize> {
    if std::fs::symlink_metadata(dest).is_ok() {
        bail!("{} already exists", dest.display());
    }
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    set_mode(dest, DIR_MODE)?;

    let file =
        std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut tree = Tree::new(dest, strip_components, limits, cancelled);
    match format {
        Format::Tar => unpack_tar(StopRead::new(&mut reader, cancelled), &mut tree)?,
        // Multi-member: a gzip stream may be several members back to back
        // (`cat a.gz b.gz`, pigz), and `gzip -d` reads them all.
        Format::TarGz => {
            let mut gz = flate2::read::MultiGzDecoder::new(reader);
            unpack_tar(StopRead::new(&mut gz, cancelled), &mut tree)?;
        }
        Format::TarZst => {
            let mut zst =
                zstd::stream::read::Decoder::with_buffer(reader).context("open zstd stream")?;
            unpack_tar(StopRead::new(&mut zst, cancelled), &mut tree)?;
        }
        Format::Zip => unpack_zip(reader, &mut tree)?,
    }
    tree.finish()
}

fn unpack_tar(reader: impl Read, tree: &mut Tree<'_>) -> anyhow::Result<()> {
    use tar::EntryType;

    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().context("read tar entries")? {
        tree.entry()?;
        let mut entry = entry.context("read tar entry")?;
        let path = utf8(&entry.path_bytes(), "entry path")?.to_string();
        let kind = entry.header().entry_type();
        let mode = entry.header().mode().unwrap_or(0o644);
        let result = match kind {
            EntryType::Regular | EntryType::Continuous => tree.file(&path, &mut entry, mode),
            EntryType::Directory => tree.dir(&path),
            EntryType::Symlink => {
                let target = link_name(&entry)?;
                tree.symlink(&path, &target)
            }
            EntryType::Link => {
                let target = link_name(&entry)?;
                tree.hardlink(&path, &target)
            }
            // Metadata records (pax/GNU long names are folded into the entry
            // they describe by `tar`; a pax *global* header is yielded alone).
            EntryType::XGlobalHeader | EntryType::XHeader => Ok(()),
            EntryType::Char | EntryType::Block | EntryType::Fifo => {
                Err(anyhow::anyhow!("device and fifo entries are refused"))
            }
            other => Err(anyhow::anyhow!("unsupported tar entry type {other:?}")),
        };
        result.with_context(|| format!("tar entry {path:?}"))?;
    }
    Ok(())
}

fn link_name<R: Read>(entry: &tar::Entry<'_, R>) -> anyhow::Result<String> {
    let raw = entry
        .link_name_bytes()
        .context("link entry has no target")?;
    Ok(utf8(&raw, "link target")?.to_string())
}

fn unpack_zip<R: Read + std::io::Seek>(reader: R, tree: &mut Tree<'_>) -> anyhow::Result<()> {
    /// `S_IFMT` and the two file types besides a link (which the zip crate
    /// reports itself) an entry may be.
    const S_IFMT: u32 = 0o170_000;
    const S_IFREG: u32 = 0o100_000;
    const S_IFDIR: u32 = 0o040_000;

    let mut archive = zip::ZipArchive::new(reader).context("read zip central directory")?;
    // The crate keys entries by name, so a repeated name would surface as its
    // last copy only; refused like a repeated tar path.
    if archive.has_duplicate_names() {
        bail!("the zip's central directory names a path twice");
    }
    for i in 0..archive.len() {
        tree.entry()?;
        let mut file = archive
            .by_index(i)
            .with_context(|| format!("read zip entry #{i}"))?;
        let path = utf8(file.name_raw(), "entry path")?.to_string();
        let mode = file.unix_mode();
        let file_type = mode.map_or(0, |m| m & S_IFMT);
        let result = if file.is_dir() {
            tree.dir(&path)
        } else if file.is_symlink() {
            let mut target = Vec::new();
            (&mut file)
                .take(MAX_LINK_TARGET as u64 + 1)
                .read_to_end(&mut target)
                .context("read symlink target")?;
            utf8(&target, "link target").and_then(|t| tree.symlink(&path, t))
        } else if file_type != 0 && file_type != S_IFREG && file_type != S_IFDIR {
            Err(anyhow::anyhow!(
                "device and fifo entries are refused (unix mode {:o})",
                mode.unwrap_or(0)
            ))
        } else {
            tree.file(&path, &mut file, mode.unwrap_or(0o644))
        };
        result.with_context(|| format!("zip entry {path:?}"))?;
    }
    Ok(())
}

/// Paths and link targets must be UTF-8: a name only some filesystems can
/// spell (APFS refuses invalid UTF-8) would unpack differently per platform.
fn utf8<'a>(raw: &'a [u8], what: &str) -> anyhow::Result<&'a str> {
    std::str::from_utf8(raw).with_context(|| format!("{what} is not valid UTF-8: {raw:?}"))
}

fn set_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {}", path.display()))
}

/// Why `path` already exists although this entry has not been written: an
/// earlier entry the filesystem folds to the same name (case, or Unicode
/// normalization, on macOS), or a plain duplicate. Only runs on that error, so
/// listing the parent costs nothing on a good archive.
fn duplicate(path: &Path) -> anyhow::Error {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return anyhow::anyhow!("duplicate path: already extracted");
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return anyhow::anyhow!("duplicate path: already extracted");
    };
    let lower = name.to_string_lossy().to_lowercase();
    let mut folded = None;
    for e in entries.flatten() {
        let n = e.file_name();
        if n == name {
            return anyhow::anyhow!("duplicate path: already extracted");
        }
        if folded.is_none() && n.to_string_lossy().to_lowercase() == lower {
            folded = Some(n);
        }
    }
    // The name exists to the filesystem but no entry is spelled that way.
    let earlier = folded.map_or_else(
        || "an earlier entry".to_string(),
        |n| format!("the earlier entry {:?}", n.to_string_lossy()),
    );
    anyhow::anyhow!(
        "collides with {earlier} on this case-insensitive filesystem (names that differ only \
         by case or Unicode normalization are one file here)"
    )
}

/// The extracted tree under construction.
struct Tree<'a> {
    root: PathBuf,
    strip: usize,
    limits: Limits,
    cancelled: &'a dyn Fn() -> bool,
    written: usize,
    /// Archive entries seen, against [`Limits::max_entries`].
    entries: u64,
    /// File bytes written, against [`Limits::max_bytes`].
    bytes: u64,
    /// Every symlink written, checked once the tree is complete.
    links: Vec<Vec<String>>,
    /// The first [`STRIPPED_SHOWN`] entry paths `strip_components` removed
    /// whole, so an archive it empties says what it held.
    stripped: Vec<String>,
}

/// How many stripped entry paths the "nothing to extract" error lists.
const STRIPPED_SHOWN: usize = 5;

impl<'a> Tree<'a> {
    fn new(root: &Path, strip: u32, limits: Limits, cancelled: &'a dyn Fn() -> bool) -> Self {
        Self {
            root: root.to_path_buf(),
            strip: strip as usize,
            limits,
            cancelled,
            written: 0,
            entries: 0,
            bytes: 0,
            links: Vec::new(),
            stripped: Vec::new(),
        }
    }

    /// [`Self::components`] of an entry's own path, remembering the first
    /// few that `strip_components` removed whole.
    fn entry_components(&mut self, raw: &str) -> anyhow::Result<Option<Vec<String>>> {
        let comps = self.components(raw)?;
        // With no strip, only `.` entries come out empty: nothing to report.
        if comps.is_none() && self.strip > 0 && self.stripped.len() < STRIPPED_SHOWN {
            self.stripped.push(raw.to_string());
        }
        Ok(comps)
    }

    /// The archive yields another entry: check cancellation and the count.
    fn entry(&mut self) -> anyhow::Result<()> {
        if (self.cancelled)() {
            bail!("cancelled");
        }
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            bail!(
                "the archive has more than {} entries; raise `max_entries` if it is meant to",
                self.limits.max_entries
            );
        }
        Ok(())
    }

    /// The bytes left under [`Limits::max_bytes`].
    fn bytes_left(&self) -> u64 {
        self.limits.max_bytes.saturating_sub(self.bytes)
    }

    fn too_many_bytes(&self) -> anyhow::Error {
        anyhow::anyhow!(
            "the archive unpacks to more than {} bytes; raise `max_bytes` if it is meant to",
            self.limits.max_bytes
        )
    }

    /// An archive path's components below the root, after `strip_components`,
    /// or `None` when nothing remains. Absolute paths and `..` are refused,
    /// wherever they appear. Empty components are dropped; `.` counts towards
    /// `strip_components` (as in GNU tar and bsdtar) and is then dropped.
    fn components(&self, raw: &str) -> anyhow::Result<Option<Vec<String>>> {
        if raw.starts_with('/') {
            bail!("absolute path refused");
        }
        let mut comps = Vec::new();
        for c in raw.split('/') {
            match c {
                "" => {}
                ".." => bail!("`..` path component refused"),
                c if c.contains('\0') => bail!("NUL in path refused"),
                c => comps.push(c),
            }
        }
        let kept: Vec<String> = comps
            .into_iter()
            .skip(self.strip)
            .filter(|c| *c != ".")
            .map(str::to_string)
            .collect();
        Ok((!kept.is_empty()).then_some(kept))
    }

    fn path_of(&self, comps: &[String]) -> PathBuf {
        let mut p = self.root.clone();
        p.extend(comps);
        p
    }

    /// Create every parent directory of `comps`, refusing to pass through a
    /// symlink or a file.
    fn parents(&self, comps: &[String]) -> anyhow::Result<()> {
        let mut path = self.root.clone();
        for c in comps.iter().take(comps.len().saturating_sub(1)) {
            path.push(c);
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.file_type().is_symlink() => {
                    bail!("would be written through the symlink {:?}", c)
                }
                Ok(m) if m.is_dir() => {}
                Ok(_) => bail!("a parent, {c:?}, is a file"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&path)
                        .with_context(|| format!("create {}", path.display()))?;
                    set_mode(&path, DIR_MODE)?;
                }
                Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
            }
        }
        Ok(())
    }

    fn dir(&mut self, raw: &str) -> anyhow::Result<()> {
        let Some(comps) = self.entry_components(raw)? else {
            return Ok(());
        };
        self.parents(&comps)?;
        let path = self.path_of(&comps);
        match std::fs::symlink_metadata(&path) {
            // Archivers list a directory again after its children made it.
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(duplicate(&path)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&path).with_context(|| format!("create {}", path.display()))?;
                set_mode(&path, DIR_MODE)?;
            }
            Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
        }
        self.written += 1;
        Ok(())
    }

    fn file(&mut self, raw: &str, data: &mut dyn Read, mode: u32) -> anyhow::Result<()> {
        let Some(comps) = self.entry_components(raw)? else {
            return Ok(());
        };
        self.parents(&comps)?;
        let path = self.path_of(&comps);
        // `create_new` is O_EXCL: it refuses an existing path, a symlink
        // included, rather than writing through it.
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => duplicate(&path),
                _ => anyhow::Error::new(e).context(format!("create {}", path.display())),
            })?;
        // One byte past the budget tells "exactly at it" from "over it"
        // without reading the rest of a bomb.
        let left = self.bytes_left();
        let mut guarded = StopRead::new(data, self.cancelled).take(left.saturating_add(1));
        let n = std::io::copy(&mut guarded, &mut out)
            .with_context(|| format!("write {}", path.display()))?;
        if n > left {
            return Err(self.too_many_bytes());
        }
        self.bytes += n;
        drop(out);
        set_mode(&path, file_mode(mode))?;
        self.written += 1;
        Ok(())
    }

    fn symlink(&mut self, raw: &str, target: &str) -> anyhow::Result<()> {
        let Some(comps) = self.entry_components(raw)? else {
            return Ok(());
        };
        if target.is_empty() {
            bail!("symlink with an empty target");
        }
        if target.len() > MAX_LINK_TARGET {
            bail!("symlink target is longer than {MAX_LINK_TARGET} bytes");
        }
        if target.starts_with('/') {
            bail!("symlink to the absolute path {target:?} refused");
        }
        if target.contains('\0') {
            bail!("NUL in symlink target refused");
        }
        // Lexically first, so the common escape names itself here rather than
        // at the end; `finish` repeats it through the links it meets.
        let mut depth = comps.len().saturating_sub(1);
        for c in target.split('/') {
            match c {
                "" | "." => {}
                ".." => {
                    depth = depth
                        .checked_sub(1)
                        .with_context(|| format!("symlink to {target:?} escapes the output"))?;
                }
                _ => depth += 1,
            }
        }
        self.parents(&comps)?;
        let path = self.path_of(&comps);
        std::os::unix::fs::symlink(target, &path).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => duplicate(&path),
            _ => anyhow::Error::new(e).context(format!("symlink {}", path.display())),
        })?;
        self.links.push(comps);
        self.written += 1;
        Ok(())
    }

    /// A hard link names an entry already extracted; it is written as a copy,
    /// so the output holds no inode shared with anything.
    fn hardlink(&mut self, raw: &str, target: &str) -> anyhow::Result<()> {
        let Some(comps) = self.entry_components(raw)? else {
            return Ok(());
        };
        let target_comps = self
            .components(target)
            .with_context(|| format!("hard link target {target:?}"))?
            .with_context(|| {
                format!("hard link target {target:?} is removed by strip_components")
            })?;
        // The target is reached without following any link, and must be a
        // file this archive already wrote.
        let mut path = self.root.clone();
        for c in &target_comps {
            path.push(c);
            let m = std::fs::symlink_metadata(&path).with_context(|| {
                format!("hard link target {target:?} was not extracted before it")
            })?;
            if m.file_type().is_symlink() {
                bail!("hard link target {target:?} passes through a symlink");
            }
        }
        let meta = std::fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        if !meta.is_file() {
            bail!("hard link target {target:?} is not a file");
        }
        if meta.len() > self.bytes_left() {
            return Err(self.too_many_bytes());
        }
        self.parents(&comps)?;
        let dest = self.path_of(&comps);
        if std::fs::symlink_metadata(&dest).is_ok() {
            return Err(duplicate(&dest));
        }
        if (self.cancelled)() {
            bail!("cancelled");
        }
        let n = std::fs::copy(&path, &dest)
            .with_context(|| format!("copy {} to {}", path.display(), dest.display()))?;
        self.bytes += n;
        self.written += 1;
        Ok(())
    }

    fn finish(self) -> anyhow::Result<usize> {
        if self.written == 0 {
            if self.stripped.is_empty() {
                bail!("nothing to extract: the archive is empty");
            }
            bail!(
                "nothing to extract: strip_components = {} removed every entry; the archive \
                 holds {:?}{}",
                self.strip,
                self.stripped,
                if self.entries > self.stripped.len() as u64 {
                    " and more"
                } else {
                    ""
                }
            );
        }
        for link in &self.links {
            if (self.cancelled)() {
                bail!("cancelled");
            }
            stays_inside(&self.root, link)
                .with_context(|| format!("symlink {:?}", link.join("/")))?;
        }
        Ok(self.written)
    }
}

/// Resolve the symlink at `link` (components below `root`) the way the OS
/// would — component by component, following every link met on the way — and
/// fail if the walk ever climbs above `root`. A component that does not exist
/// ends the physical walk: nothing below it can be a link, so the rest is
/// lexical.
fn stays_inside(root: &Path, link: &[String]) -> anyhow::Result<()> {
    let split = |t: &Path| -> anyhow::Result<VecDeque<String>> {
        let t = t
            .to_str()
            .with_context(|| format!("link target {t:?} is not UTF-8"))?;
        if t.starts_with('/') {
            bail!("links to the absolute path {t:?}");
        }
        Ok(t.split('/').map(str::to_string).collect())
    };
    let read = |cur: &[String]| -> anyhow::Result<PathBuf> {
        let mut p = root.to_path_buf();
        p.extend(cur);
        std::fs::read_link(&p).with_context(|| format!("read link {}", p.display()))
    };

    let mut cur: Vec<String> = link.to_vec();
    let mut pending = split(&read(&cur)?)?;
    cur.pop();
    let mut hops = 1;
    while let Some(c) = pending.pop_front() {
        match c.as_str() {
            "" | "." => {}
            ".." => {
                if cur.pop().is_none() {
                    bail!("resolves outside the output");
                }
            }
            _ => {
                cur.push(c);
                let mut p = root.to_path_buf();
                p.extend(&cur);
                let is_link =
                    std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink());
                if is_link {
                    hops += 1;
                    if hops > MAX_LINK_HOPS {
                        bail!("too many levels of symlinks");
                    }
                    let next = split(&read(&cur)?)?;
                    cur.pop();
                    for n in next.into_iter().rev() {
                        pending.push_front(n);
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    fn never() -> bool {
        false
    }

    /// One tar entry to build. `kind` decides the header; `raw` writes the
    /// path (and link target) straight into the header bytes, bypassing the
    /// builder's own refusal of `..` and absolute paths — an attacker's
    /// archive is not built with a careful builder.
    enum E<'a> {
        File(&'a str, &'a [u8], u32),
        Dir(&'a str, u32),
        Symlink(&'a str, &'a str),
        Hardlink(&'a str, &'a str),
        Fifo(&'a str),
        Char(&'a str),
    }

    fn header(path: &str, kind: tar::EntryType, mode: u32, size: u64) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(kind);
        h.set_mode(mode);
        h.set_size(size);
        h.set_mtime(1_700_000_000);
        let name = &mut h.as_old_mut().name;
        name.fill(0);
        name[..path.len()].copy_from_slice(path.as_bytes());
        h
    }

    fn set_link(h: &mut tar::Header, target: &str) {
        let link = &mut h.as_old_mut().linkname;
        link.fill(0);
        link[..target.len()].copy_from_slice(target.as_bytes());
    }

    fn tar_bytes(entries: &[E<'_>]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for e in entries {
            let (mut h, data): (tar::Header, &[u8]) = match e {
                E::File(p, d, m) => (header(p, tar::EntryType::Regular, *m, d.len() as u64), d),
                E::Dir(p, m) => (header(p, tar::EntryType::Directory, *m, 0), b""),
                E::Symlink(p, t) => {
                    let mut h = header(p, tar::EntryType::Symlink, 0o777, 0);
                    set_link(&mut h, t);
                    (h, b"")
                }
                E::Hardlink(p, t) => {
                    let mut h = header(p, tar::EntryType::Link, 0o644, 0);
                    set_link(&mut h, t);
                    (h, b"")
                }
                E::Fifo(p) => (header(p, tar::EntryType::Fifo, 0o644, 0), b""),
                E::Char(p) => (header(p, tar::EntryType::Char, 0o644, 0), b""),
            };
            h.set_cksum();
            b.append(&h, data).unwrap();
        }
        b.into_inner().unwrap()
    }

    fn gz(bytes: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    fn zst(bytes: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(bytes, 3).unwrap()
    }

    fn zip_bytes(build: impl FnOnce(&mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>)) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        build(&mut w);
        w.finish().unwrap().into_inner()
    }

    fn zip_opts(mode: u32) -> zip::write::SimpleFileOptions {
        zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(mode)
    }

    /// Unpack `bytes` as `format` into a fresh `out/` of a fresh tempdir.
    fn run(bytes: &[u8], format: Format, strip: u32) -> (tempfile::TempDir, anyhow::Result<usize>) {
        run_with(bytes, format, strip, Limits::default(), &never)
    }

    fn run_with(
        bytes: &[u8],
        format: Format,
        strip: u32,
        limits: Limits,
        cancelled: &dyn Fn() -> bool,
    ) -> (tempfile::TempDir, anyhow::Result<usize>) {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        std::fs::write(&archive, bytes).unwrap();
        let out = dir.path().join("out");
        let r = unpack(&archive, format, strip, limits, &out, cancelled);
        (dir, r)
    }

    fn read(dir: &tempfile::TempDir, rel: &str) -> String {
        std::fs::read_to_string(dir.path().join("out").join(rel)).unwrap()
    }

    fn mode(dir: &tempfile::TempDir, rel: &str) -> u32 {
        std::fs::symlink_metadata(dir.path().join("out").join(rel))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    fn err(r: anyhow::Result<usize>) -> String {
        format!("{:#}", r.expect_err("must fail"))
    }

    fn sample_tar() -> Vec<u8> {
        tar_bytes(&[
            E::Dir("tool-1.0/", 0o755),
            E::Dir("tool-1.0/bin/", 0o755),
            E::File("tool-1.0/bin/tool", b"#!/bin/sh\necho hi\n", 0o755),
            E::File("tool-1.0/README", b"readme\n", 0o644),
        ])
    }

    #[test]
    fn tar_round_trips() {
        let (dir, r) = run(&sample_tar(), Format::Tar, 0);
        assert_eq!(r.unwrap(), 4);
        assert_eq!(read(&dir, "tool-1.0/bin/tool"), "#!/bin/sh\necho hi\n");
        assert_eq!(read(&dir, "tool-1.0/README"), "readme\n");
    }

    #[test]
    fn tar_gz_round_trips() {
        let (dir, r) = run(&gz(&sample_tar()), Format::TarGz, 0);
        r.unwrap();
        assert_eq!(read(&dir, "tool-1.0/README"), "readme\n");
    }

    #[test]
    fn tar_zst_round_trips() {
        let (dir, r) = run(&zst(&sample_tar()), Format::TarZst, 0);
        r.unwrap();
        assert_eq!(read(&dir, "tool-1.0/bin/tool"), "#!/bin/sh\necho hi\n");
    }

    #[test]
    fn zip_round_trips_with_modes_and_an_in_tree_symlink() {
        let bytes = zip_bytes(|w| {
            w.add_directory("tool/", zip_opts(0o755)).unwrap();
            w.start_file("tool/run", zip_opts(0o700)).unwrap();
            w.write_all(b"run\n").unwrap();
            w.start_file("tool/data", zip_opts(0o600)).unwrap();
            w.write_all(b"data\n").unwrap();
            w.add_symlink("tool/alias", "run", zip_opts(0o777)).unwrap();
        });
        let (dir, r) = run(&bytes, Format::Zip, 1);
        r.unwrap();
        assert_eq!(read(&dir, "run"), "run\n");
        assert_eq!(
            read(&dir, "alias"),
            "run\n",
            "the link resolves in the tree"
        );
        assert_eq!(mode(&dir, "run"), 0o755);
        assert_eq!(mode(&dir, "data"), 0o644);
    }

    #[test]
    fn a_wrong_format_is_an_error_not_an_empty_tree() {
        let (_dir, r) = run(&sample_tar(), Format::TarGz, 0);
        let msg = err(r);
        assert!(msg.contains("tar"), "{msg}");
    }

    #[test]
    fn strip_components_drops_the_top_level_directory() {
        let (dir, r) = run(&sample_tar(), Format::Tar, 1);
        // `tool-1.0/` itself is skipped; its three descendants remain.
        assert_eq!(r.unwrap(), 3);
        assert_eq!(read(&dir, "bin/tool"), "#!/bin/sh\necho hi\n");
        assert_eq!(read(&dir, "README"), "readme\n");
        assert!(!dir.path().join("out/tool-1.0").exists());
    }

    #[test]
    fn strip_components_that_removes_everything_is_an_error() {
        let (_dir, r) = run(&sample_tar(), Format::Tar, 3);
        let msg = err(r);
        assert!(msg.contains("strip_components = 3"), "{msg}");
        // The entries are listed, so the right count can be read off them.
        assert!(
            msg.contains(
                r#"["tool-1.0/", "tool-1.0/bin/", "tool-1.0/bin/tool", "tool-1.0/README"]"#
            ),
            "{msg}"
        );
        assert!(!msg.contains("and more"), "{msg}");

        // Past five entries, the first five and a note that there are more.
        let many: Vec<String> = (0..7).map(|i| format!("top/f{i}")).collect();
        let entries: Vec<E<'_>> = many.iter().map(|p| E::File(p, b"x", 0o644)).collect();
        let (_dir, r) = run(&tar_bytes(&entries), Format::Tar, 2);
        let msg = err(r);
        assert!(
            msg.contains(r#"["top/f0", "top/f1", "top/f2", "top/f3", "top/f4"] and more"#),
            "{msg}"
        );
        assert!(!msg.contains("top/f5"), "{msg}");
    }

    #[test]
    fn an_empty_archive_is_an_error() {
        let (_dir, r) = run(&tar_bytes(&[]), Format::Tar, 0);
        assert!(err(r).contains("nothing to extract"));
    }

    #[test]
    fn modes_are_normalized() {
        let bytes = tar_bytes(&[
            E::Dir("d/", 0o700),
            E::File("d/owner-exec", b"x", 0o700),
            E::File("d/setuid", b"x", 0o4755),
            E::File("d/private", b"x", 0o600),
            E::File("d/group-write", b"x", 0o664),
        ]);
        let (dir, r) = run(&bytes, Format::Tar, 0);
        r.unwrap();
        assert_eq!(mode(&dir, "d"), 0o755);
        assert_eq!(mode(&dir, "d/owner-exec"), 0o755);
        assert_eq!(mode(&dir, "d/setuid"), 0o755);
        assert_eq!(mode(&dir, "d/private"), 0o644);
        assert_eq!(mode(&dir, "d/group-write"), 0o644);
    }

    #[test]
    fn mtimes_are_not_applied() {
        let (dir, r) = run(&sample_tar(), Format::Tar, 0);
        r.unwrap();
        let mtime = std::fs::metadata(dir.path().join("out/tool-1.0/README"))
            .unwrap()
            .modified()
            .unwrap();
        let archived = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_ne!(mtime, archived);
    }

    #[test]
    fn parent_dir_components_are_refused() {
        for path in ["../x", "a/../../x", "a/../x"] {
            let (dir, r) = run(&tar_bytes(&[E::File(path, b"x", 0o644)]), Format::Tar, 0);
            let msg = err(r);
            assert!(msg.contains("`..`"), "{path}: {msg}");
            assert!(!dir.path().join("x").exists(), "{path} escaped");
        }
    }

    #[test]
    fn absolute_paths_are_refused() {
        let (_dir, r) = run(&tar_bytes(&[E::File("/abs", b"x", 0o644)]), Format::Tar, 0);
        assert!(err(r).contains("absolute path"));
    }

    #[test]
    fn zip_slip_is_refused() {
        let bytes = zip_bytes(|w| {
            w.start_file("../evil", zip_opts(0o644)).unwrap();
            w.write_all(b"x").unwrap();
        });
        let (dir, r) = run(&bytes, Format::Zip, 0);
        assert!(err(r).contains("`..`"));
        assert!(!dir.path().join("evil").exists());
    }

    #[test]
    fn a_symlink_escaping_the_output_is_refused() {
        for target in ["../outside", "a/../../outside", "../../etc/passwd"] {
            let bytes = tar_bytes(&[E::Symlink("link", target)]);
            let (_dir, r) = run(&bytes, Format::Tar, 0);
            let msg = err(r);
            assert!(msg.contains("escapes the output"), "{target}: {msg}");
        }
        let (_dir, r) = run(
            &tar_bytes(&[E::Symlink("link", "/etc/passwd")]),
            Format::Tar,
            0,
        );
        assert!(err(r).contains("absolute path"));
    }

    /// Lexically `b -> a/..` stays at the root; physically `a` is a link to
    /// `.`'s parent chain, so `b` resolves above the output.
    #[test]
    fn a_symlink_escaping_through_another_link_is_refused() {
        let bytes = tar_bytes(&[
            E::Dir("d/", 0o755),
            E::Symlink("d/up", ".."),
            E::Symlink("b", "d/up/.."),
        ]);
        let (_dir, r) = run(&bytes, Format::Tar, 0);
        let msg = err(r);
        assert!(msg.contains("resolves outside the output"), "{msg}");
    }

    #[test]
    fn an_in_tree_symlink_is_kept() {
        let bytes = tar_bytes(&[
            E::File("lib/libfoo.so.1", b"so", 0o755),
            E::Symlink("lib/libfoo.so", "libfoo.so.1"),
            E::Symlink("bin/foo", "../lib/libfoo.so"),
        ]);
        let (dir, r) = run(&bytes, Format::Tar, 0);
        r.unwrap();
        let link = std::fs::read_link(dir.path().join("out/bin/foo")).unwrap();
        assert_eq!(link, PathBuf::from("../lib/libfoo.so"));
        assert_eq!(read(&dir, "bin/foo"), "so");
    }

    #[test]
    fn writing_through_a_symlink_is_refused() {
        let bytes = tar_bytes(&[
            E::Dir("real/", 0o755),
            E::Symlink("alias", "real"),
            E::File("alias/x", b"x", 0o644),
        ]);
        let (dir, r) = run(&bytes, Format::Tar, 0);
        assert!(err(r).contains("through the symlink"));
        assert!(!dir.path().join("out/real/x").exists());
    }

    #[test]
    fn device_and_fifo_entries_are_refused() {
        for e in [E::Fifo("f"), E::Char("c")] {
            let (_dir, r) = run(&tar_bytes(&[e]), Format::Tar, 0);
            assert!(err(r).contains("device and fifo"));
        }
    }

    #[test]
    fn a_hardlink_becomes_a_copy_and_must_name_an_extracted_file() {
        let bytes = tar_bytes(&[
            E::File("top/a", b"same", 0o755),
            E::Hardlink("top/b", "top/a"),
        ]);
        let (dir, r) = run(&bytes, Format::Tar, 1);
        r.unwrap();
        assert_eq!(read(&dir, "b"), "same");
        assert_eq!(mode(&dir, "b"), 0o755);

        let (_dir, r) = run(&tar_bytes(&[E::Hardlink("b", "missing")]), Format::Tar, 0);
        assert!(err(r).contains("was not extracted before it"));
        let (_dir, r) = run(&tar_bytes(&[E::Hardlink("b", "../x")]), Format::Tar, 0);
        assert!(err(r).contains("`..`"));
    }

    #[test]
    fn a_duplicate_path_is_an_error() {
        let bytes = tar_bytes(&[E::File("x", b"one", 0o644), E::File("x", b"two", 0o644)]);
        let (dir, r) = run(&bytes, Format::Tar, 0);
        assert!(err(r).contains("duplicate path"));
        assert_eq!(read(&dir, "x"), "one", "the first copy is not overwritten");

        // A directory listed again after its children made it is fine.
        let bytes = tar_bytes(&[E::File("d/x", b"x", 0o644), E::Dir("d/", 0o755)]);
        let (_dir, r) = run(&bytes, Format::Tar, 0);
        r.unwrap();
    }

    #[test]
    fn an_existing_destination_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar");
        std::fs::write(&archive, sample_tar()).unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let r = unpack(&archive, Format::Tar, 0, Limits::default(), &out, &never);
        assert!(err(r).contains("already exists"));
    }

    #[test]
    fn cancellation_stops_between_entries() {
        let (_dir, r) = run_with(&sample_tar(), Format::Tar, 0, Limits::default(), &|| true);
        assert!(err(r).contains("cancelled"));
    }

    /// The flag flips once the big entry's file exists, i.e. mid-copy: the
    /// copy stops within a chunk instead of running to the end of the entry.
    #[test]
    fn cancellation_stops_mid_entry() {
        let big = vec![b'x'; 4 << 20];
        let bytes = gz(&tar_bytes(&[E::File("big", &big, 0o644)]));
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        std::fs::write(&archive, &bytes).unwrap();
        let out = dir.path().join("out");
        let started = || out.join("big").exists();
        let r = unpack(
            &archive,
            Format::TarGz,
            0,
            Limits::default(),
            &out,
            &started,
        );
        let msg = err(r);
        assert!(msg.contains("cancelled"), "{msg}");
        let written = std::fs::metadata(out.join("big")).unwrap().len();
        assert!(written < big.len() as u64, "stopped early: {written}");
    }

    #[test]
    fn a_multi_member_gzip_is_read_to_the_end() {
        let t = sample_tar();
        let mut bytes = gz(&t[..1024]);
        bytes.extend(gz(&t[1024..]));
        let (dir, r) = run(&bytes, Format::TarGz, 0);
        assert_eq!(r.unwrap(), 4);
        assert_eq!(read(&dir, "tool-1.0/README"), "readme\n");
    }

    #[test]
    fn truncated_or_corrupt_streams_are_errors() {
        let t = sample_tar();
        let g = gz(&t);
        let (_dir, r) = run(&g[..g.len() / 2], Format::TarGz, 0);
        err(r);
        let mut z = zst(&t);
        let (_dir, r) = run(&z[..z.len() / 2], Format::TarZst, 0);
        err(r);
        let mid = z.len() / 2;
        z[mid] ^= 0xff;
        z[mid + 1] ^= 0xff;
        let (_dir, r) = run(&z, Format::TarZst, 0);
        err(r);
        let (_dir, r) = run(&t[..t.len() - 1500], Format::Tar, 0);
        err(r);
    }

    #[test]
    fn the_byte_limit_is_enforced_while_copying() {
        let limits = Limits {
            max_bytes: 10,
            ..Limits::default()
        };
        let exact = tar_bytes(&[E::File("a", b"0123456789", 0o644)]);
        let (_dir, r) = run_with(&exact, Format::Tar, 0, limits, &never);
        r.expect("exactly at the limit passes");

        let over = tar_bytes(&[
            E::File("a", b"01234", 0o644),
            E::File("b", b"567890", 0o644),
        ]);
        let (dir, r) = run_with(&over, Format::Tar, 0, limits, &never);
        let msg = err(r);
        assert!(
            msg.contains("`max_bytes`") && msg.contains("10 bytes"),
            "{msg}"
        );
        let b = std::fs::metadata(dir.path().join("out/b")).unwrap().len();
        assert!(b <= 6, "copy stopped at the budget: {b}");

        // A hard link's copy counts too.
        let linked = tar_bytes(&[E::File("a", b"012345", 0o644), E::Hardlink("b", "a")]);
        let (_dir, r) = run_with(&linked, Format::Tar, 0, limits, &never);
        assert!(err(r).contains("`max_bytes`"));

        let zip = zip_bytes(|w| {
            w.start_file("a", zip_opts(0o644)).unwrap();
            w.write_all(&[0u8; 64]).unwrap();
        });
        let (_dir, r) = run_with(&zip, Format::Zip, 0, limits, &never);
        assert!(err(r).contains("`max_bytes`"));
    }

    #[test]
    fn the_entry_limit_is_enforced() {
        let limits = Limits {
            max_entries: 3,
            ..Limits::default()
        };
        let (_dir, r) = run_with(&sample_tar(), Format::Tar, 0, limits, &never);
        let msg = err(r);
        assert!(
            msg.contains("`max_entries`") && msg.contains("3 entries"),
            "{msg}"
        );
        let four = Limits {
            max_entries: 4,
            ..Limits::default()
        };
        let (_dir, r) = run_with(&sample_tar(), Format::Tar, 0, four, &never);
        r.unwrap();

        let zip = zip_bytes(|w| {
            for n in ["a", "b", "c", "d"] {
                w.start_file(n, zip_opts(0o644)).unwrap();
            }
        });
        let (_dir, r) = run_with(&zip, Format::Zip, 0, limits, &never);
        assert!(err(r).contains("`max_entries`"));
    }

    #[test]
    fn strip_components_counts_a_leading_dot() {
        let bytes = tar_bytes(&[
            E::Dir("./", 0o755),
            E::Dir("./tool-1.0/", 0o755),
            E::File("./tool-1.0/bin/tool", b"t", 0o755),
        ]);
        let (dir, r) = run(&bytes, Format::Tar, 1);
        // `./` alone is skipped; `./tool-1.0/` is `tool-1.0`.
        assert_eq!(r.unwrap(), 2);
        assert_eq!(read(&dir, "tool-1.0/bin/tool"), "t");

        let (dir, r) = run(&bytes, Format::Tar, 2);
        r.unwrap();
        assert_eq!(read(&dir, "bin/tool"), "t");

        // Unstripped, a bare `./` is the root itself: skipped, not an error.
        let (dir, r) = run(&bytes, Format::Tar, 0);
        assert_eq!(r.unwrap(), 2);
        assert_eq!(read(&dir, "tool-1.0/bin/tool"), "t");
    }

    /// Link targets are relative to the link, so they are never stripped; a
    /// target that climbed to the stripped top now climbs out.
    #[test]
    fn strip_components_leaves_symlink_targets_alone() {
        let bytes = tar_bytes(&[E::File("top/a", b"a", 0o644), E::Symlink("top/l", "a")]);
        let (dir, r) = run(&bytes, Format::Tar, 1);
        r.unwrap();
        assert_eq!(
            std::fs::read_link(dir.path().join("out/l")).unwrap(),
            PathBuf::from("a")
        );
        assert_eq!(read(&dir, "l"), "a");

        let bytes = tar_bytes(&[
            E::File("top/y", b"y", 0o644),
            E::Symlink("top/x", "../../top/y"),
        ]);
        let (_dir, r) = run(&bytes, Format::Tar, 1);
        assert!(err(r).contains("escapes the output"));
    }

    #[test]
    fn hard_links_through_a_symlink_or_to_a_directory_are_refused() {
        let bytes = tar_bytes(&[
            E::File("real/f", b"f", 0o644),
            E::Symlink("alias", "real"),
            E::Hardlink("h", "alias/f"),
        ]);
        let (_dir, r) = run(&bytes, Format::Tar, 0);
        assert!(err(r).contains("passes through a symlink"));

        let bytes = tar_bytes(&[E::Dir("d/", 0o755), E::Hardlink("h", "d")]);
        let (_dir, r) = run(&bytes, Format::Tar, 0);
        assert!(err(r).contains("is not a file"));
    }

    #[test]
    fn a_symlink_loop_is_refused() {
        let bytes = tar_bytes(&[E::Symlink("a", "b"), E::Symlink("b", "a")]);
        let (_dir, r) = run(&bytes, Format::Tar, 0);
        assert!(err(r).contains("too many levels"));
    }

    #[test]
    fn bad_symlink_targets_are_refused() {
        // Tar: an empty link name, and one over the cap (a GNU long link).
        let (_dir, r) = run(&tar_bytes(&[E::Symlink("l", "")]), Format::Tar, 0);
        let msg = err(r);
        assert!(
            msg.contains("no target") || msg.contains("empty target"),
            "{msg}"
        );
        let long = "a/".repeat(600);
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(&mut h, "l", &long).unwrap();
        let (_dir, r) = run(&b.into_inner().unwrap(), Format::Tar, 0);
        assert!(err(r).contains("longer than 1023 bytes"));

        // Zip: a link's target is its contents.
        for (target, want) in [
            ("", "empty target"),
            ("a\0b", "NUL"),
            (long.as_str(), "longer than 1023 bytes"),
        ] {
            let bytes = zip_bytes(|w| w.add_symlink("l", target, zip_opts(0o777)).unwrap());
            let (_dir, r) = run(&bytes, Format::Zip, 0);
            let msg = err(r);
            assert!(msg.contains(want), "{target:?}: {msg}");
        }
    }

    /// `a -> b` (dangling, in the tree) then a file or directory `a`: the
    /// second entry is refused rather than written through the link.
    #[test]
    fn an_entry_on_an_existing_symlink_is_refused() {
        let bytes = tar_bytes(&[E::Symlink("a", "b"), E::File("a", b"x", 0o644)]);
        let (dir, r) = run(&bytes, Format::Tar, 0);
        assert!(err(r).contains("duplicate path"));
        assert!(!dir.path().join("out/b").exists(), "not written through");

        let bytes = tar_bytes(&[E::Symlink("a", "b"), E::Dir("a/", 0o755)]);
        let (dir, r) = run(&bytes, Format::Tar, 0);
        assert!(err(r).contains("duplicate path"));
        assert!(!dir.path().join("out/b").exists());
    }

    /// GNU long names and pax `path` records are folded into their entry.
    #[test]
    fn tar_paths_over_100_bytes_extract() {
        let gnu = format!("{}/gnu", "d".repeat(150));
        let pax = format!("{}/pax", "p".repeat(150));
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        h.set_size(3);
        b.append_data(&mut h, &gnu, &b"gnu"[..]).unwrap();
        b.append_pax_extensions([("path", pax.as_bytes())]).unwrap();
        let mut h = tar::Header::new_ustar();
        h.set_path("short").unwrap();
        h.set_mode(0o644);
        h.set_size(3);
        h.set_cksum();
        b.append(&h, &b"pax"[..]).unwrap();
        let (dir, r) = run(&b.into_inner().unwrap(), Format::Tar, 0);
        r.unwrap();
        assert_eq!(read(&dir, &gnu), "gnu");
        assert_eq!(read(&dir, &pax), "pax");
        assert!(!dir.path().join("out/short").exists());
    }

    /// Offsets into a zip's (single-entry) central-directory record.
    fn patch_central(bytes: &mut [u8], f: impl FnOnce(&mut [u8])) {
        let at = bytes
            .windows(4)
            .position(|w| w == b"PK\x01\x02")
            .expect("central directory");
        f(&mut bytes[at..]);
    }

    #[test]
    fn zip_device_entries_are_refused() {
        let mut bytes = zip_bytes(|w| {
            w.start_file("dev", zip_opts(0o644)).unwrap();
        });
        // External attributes (offset 38): S_IFCHR | 0644 in the high half.
        patch_central(&mut bytes, |c| {
            c[38..42].copy_from_slice(&((0o020_644u32) << 16).to_le_bytes());
        });
        let (_dir, r) = run(&bytes, Format::Zip, 0);
        assert!(err(r).contains("device and fifo"));
    }

    #[test]
    fn zip_without_a_unix_mode_is_0644() {
        let mut bytes = zip_bytes(|w| {
            w.start_file("f", zip_opts(0o755)).unwrap();
            w.write_all(b"f").unwrap();
        });
        // No external attributes at all: the crate reports no unix mode.
        patch_central(&mut bytes, |c| c[38..42].fill(0));
        let (dir, r) = run(&bytes, Format::Zip, 0);
        r.unwrap();
        assert_eq!(mode(&dir, "f"), 0o644);
    }

    #[test]
    fn zip_encrypted_entries_are_refused() {
        let mut bytes = zip_bytes(|w| {
            w.start_file("f", zip_opts(0o644)).unwrap();
            w.write_all(b"secret").unwrap();
        });
        // General-purpose flag bit 0 ("encrypted"), in the local header
        // (offset 6) and the central record (offset 8).
        bytes[6] |= 1;
        patch_central(&mut bytes, |c| c[8] |= 1);
        let (dir, r) = run(&bytes, Format::Zip, 0);
        let msg = err(r);
        assert!(msg.contains("zip entry"), "{msg}");
        assert!(!dir.path().join("out/f").exists());
    }

    #[test]
    fn zip_absolute_paths_and_escaping_links_are_refused() {
        let bytes = zip_bytes(|w| {
            w.start_file("/abs", zip_opts(0o644)).unwrap();
            w.write_all(b"x").unwrap();
        });
        let (_dir, r) = run(&bytes, Format::Zip, 0);
        assert!(err(r).contains("absolute path"));

        let bytes = zip_bytes(|w| w.add_symlink("l", "../outside", zip_opts(0o777)).unwrap());
        let (_dir, r) = run(&bytes, Format::Zip, 0);
        assert!(err(r).contains("escapes the output"));
    }

    #[test]
    fn a_zip_naming_a_path_twice_is_refused() {
        let mut bytes = zip_bytes(|w| {
            w.start_file("a", zip_opts(0o644)).unwrap();
            w.write_all(b"one").unwrap();
            w.start_file("b", zip_opts(0o644)).unwrap();
            w.write_all(b"two").unwrap();
        });
        // Rename `b` to `a` in its local header and central record (the
        // writer itself refuses a repeated name).
        for (sig, name_at, len_at) in [(b"PK\x03\x04", 30, 26), (b"PK\x01\x02", 46, 28)] {
            let mut at = 0;
            while let Some(p) = bytes[at..].windows(4).position(|w| w == &sig[..]) {
                let h = at + p;
                let len = u16::from_le_bytes([bytes[h + len_at], bytes[h + len_at + 1]]) as usize;
                if len == 1 && bytes[h + name_at] == b'b' {
                    bytes[h + name_at] = b'a';
                }
                at = h + 4;
            }
        }
        let (_dir, r) = run(&bytes, Format::Zip, 0);
        assert!(err(r).contains("names a path twice"));
    }

    /// The one platform difference, and it is the filesystem's: names that
    /// differ only by case are two files on Linux and one on macOS, where the
    /// second entry fails saying why rather than "duplicate path".
    #[test]
    fn names_differing_only_by_case() {
        let bytes = tar_bytes(&[
            E::File("README", b"upper", 0o644),
            E::File("readme", b"lower", 0o644),
        ]);
        let (dir, r) = run(&bytes, Format::Tar, 0);
        #[cfg(target_os = "linux")]
        {
            assert_eq!(r.unwrap(), 2);
            assert_eq!(read(&dir, "README"), "upper");
            assert_eq!(read(&dir, "readme"), "lower");
        }
        #[cfg(target_os = "macos")]
        {
            let msg = err(r);
            assert!(
                msg.contains("collides with the earlier entry \"README\"")
                    && msg.contains("case-insensitive filesystem"),
                "{msg}"
            );
            assert_eq!(read(&dir, "README"), "upper", "the first is kept");
        }
    }

    #[test]
    fn format_inference() {
        let cases = [
            ("x.tar.gz", Some(Format::TarGz)),
            ("x.tgz", Some(Format::TarGz)),
            ("X.TGZ", Some(Format::TarGz)),
            ("x.Tar.Gz", Some(Format::TarGz)),
            ("x.tar.zst", Some(Format::TarZst)),
            ("x.tzst", Some(Format::TarZst)),
            ("x.tar", Some(Format::Tar)),
            ("x.zip", Some(Format::Zip)),
            ("x.tar.gz.sha256", None),
            ("x.gz", None),
            ("x.tar.xz", None),
            ("x", None),
            (".tar.gz", None),
            ("", None),
        ];
        for (name, want) in cases {
            assert_eq!(Format::infer(name), want, "{name:?}");
        }
    }

    #[test]
    fn format_spellings_round_trip() {
        for f in Format::ALL {
            assert_eq!(Format::parse(f.as_str()).unwrap(), f);
        }
        let msg = format!("{:#}", Format::parse("rar").unwrap_err());
        assert!(msg.contains("tar.gz"), "{msg}");
    }
}
