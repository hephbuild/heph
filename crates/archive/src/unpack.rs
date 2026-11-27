//! Unpacking an untrusted archive into a fresh directory: [`unpack`], and the
//! [`Limits`] it enforces.
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
//! - **The output and the decoding are bounded.** At most
//!   [`Limits::max_bytes`] bytes decoded for the archive's entries, whether
//!   written or read past, and [`Limits::max_entries`] entries, counted while
//!   copying, so a decompression bomb fails instead of filling the disk or
//!   decoding forever.
//! - **A decoder's memory is bounded.** An xz stream whose dictionary and
//!   decoder overhead need more than [`XZ_MEM_LIMIT_KIB`] is refused before
//!   it is allocated.
//! - **A cancelled run stops mid-entry.** The decompressed stream and each
//!   entry's bytes are read through [`StopRead`], which polls the token on
//!   every read.
//!
//! What integrity check is verified depends on the format. A `tar.xz` and a
//! bare `gz` are decoded to the end of the stream, so a corrupted payload
//! fails on the xz block check, index and footer, or on the gzip CRC32 and
//! length; bytes an xz stream decodes to after tar's end marker count against
//! [`Limits::max_bytes`]. A `tar.gz` and a `tar.zst` stop at tar's end
//! marker, as they always have: their gzip trailer or zstd checksum is not
//! verified, and bytes after the marker are not read. A `zip` entry's CRC32
//! is checked by the zip reader as the entry is read.
//!
//! `strip_components` drops that many leading components from every entry, as
//! `tar --strip-components` does — a `.` counts, as in GNU tar and bsdtar, so
//! `./tool-1.0/bin/tool` with 1 is `tool-1.0/bin/tool`; an entry with no more
//! components than that (the top-level directory itself, usually) is skipped.
//! A hard link's target is stripped too; a symlink's is not (it is relative to
//! the link). An archive from which nothing remains is an error, not an empty
//! output.
//!
//! `include` ([`UnpackOptions::include`]) picks entries by path: each pattern
//! is a glob in `group`'s `include` syntax ([`Globs`]), matched against the
//! entry's path below `dest` (after `strip_components`, `/`-separated:
//! `./bin/tool` and `bin/./tool` are both `bin/tool`). Set, only the files,
//! symlinks and hard links whose own path matches are extracted; directory
//! entries are skipped, and the parents an extracted entry needs are
//! created. An entry left out is still checked by the rules above and counts
//! towards [`Limits::max_entries`]. In a tar its bytes are decoded to reach
//! the next entry, and count towards [`Limits::max_bytes`] as written ones
//! do; a zip entry left out is never read, and costs nothing. A hard link
//! whose target is left out is an error; so is a symlink that does not
//! resolve to an extracted path (its target, or a link on the way, was left
//! out, or the archive never held it), and a pattern that matches nothing (a
//! typo guard: the error lists the first entry paths it was matched
//! against).
//!
//! `executable` ([`UnpackOptions::executable`]) only ever adds the bit: every
//! extracted regular file, hard-link copies included ([`Executable::All`]),
//! or those whose path matches ([`Executable::Matching`], each pattern of
//! which must match one), is `0o755`; every other file keeps its normalized
//! mode. A hard-link copy is its target's file in the archive, so it takes
//! the mode its target was written with, a raised bit included, and `0o755`
//! when its own path matches. Symlinks are untouched.
//!
//! `support` ([`UnpackOptions::support`]) names the entries a caller keeps
//! apart from the rest (`archive.extract`'s `support_files`): they are
//! patterns in `include`'s syntax, an entry matching one is extracted
//! whether or not `include` matches it, and each must match a file or link
//! the archive holds. What is extracted is the same tree either way; which
//! of its files are support files the caller answers with
//! [`Globs::is_match`] over the same paths.
//!
//! A bare `gz` is one entry: a file named after the archive (see
//! [`gz_file_name`]), held to the same rules as a tar's file entry, `include`
//! and `executable` included.

use crate::{DIR_MODE, Format, Limits, file_mode};
use anyhow::{Context as _, bail};
use hcore::hartifactcontent::StopRead;
use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};

/// How many symlinks one resolution may follow before it is refused, as the
/// kernel's own `ELOOP` limit.
const MAX_LINK_HOPS: usize = 40;

/// The longest symlink target accepted, in bytes: macOS's limit (Linux takes
/// up to 4095), so one archive unpacks the same on both.
const MAX_LINK_TARGET: usize = 1023;

/// The most memory an xz stream's decoder may use, in KiB: 256 MiB. An xz
/// block header names its dictionary size, up to 4 GiB, and the decoder
/// allocates it, so an archive could otherwise exhaust the host's memory
/// before writing a byte. lzma-rust2 counts ~100 KiB of decoder overhead on
/// top of the dictionary, so a dictionary of 256 MiB or more is refused, and
/// `xz -9`'s 64 MiB is accepted.
///
/// Not keyed, like [`Limits`]: it decides only whether a run fails, and a
/// failure is never cached, so changing it needs no `EXTRACT_FORMAT_VERSION`
/// bump (`crates/plugin-archive/src/extract.rs`).
const XZ_MEM_LIMIT_KIB: u32 = 256 << 10;

/// Glob patterns over an archive's entry paths, in the syntax of `group`'s
/// `include` (wax: `*` within a component, `**` across them, `{a,b}`,
/// `[…]`), each matched against an entry's path below the output, after
/// `strip_components`, `/`-separated (`bin/tool`). No pattern is empty or
/// holds a NUL; the list may be empty.
///
/// Matching is wax's, on the path's bytes: case-sensitive, `*` matches a
/// leading `.` (dotfiles), `**/x` matches a top-level `x` too, and a name
/// is compared as written, so an NFD name (a zip made on macOS) needs an NFD
/// pattern, or a `*`. The `matching_is_pinned` test holds those answers.
///
/// `archive.extract` keys a target by its pattern strings, not by what they
/// match: a wax upgrade or a matcher change here that changes what a pattern
/// matches bumps its `GLOB_VERSION` (`crates/plugin-archive/src/extract.rs`).
#[derive(Clone, Debug, Default)]
pub struct Globs(Vec<(String, wax::Glob<'static>)>);

impl Globs {
    /// Compile `patterns`, refusing an empty one, a NUL, one that could
    /// never match an entry path (see [`never_matches`]), and anything wax
    /// does not compile, naming the pattern.
    pub fn new(patterns: &[String]) -> anyhow::Result<Self> {
        patterns
            .iter()
            .map(|p| {
                if p.is_empty() {
                    bail!("an empty pattern matches no path");
                }
                if p.contains('\0') {
                    bail!("pattern {p:?} must not contain a NUL byte");
                }
                never_matches(p)?;
                let glob = wax::Glob::new(p)
                    .map(wax::Glob::into_owned)
                    .with_context(|| format!("invalid glob pattern {p:?}"))?;
                Ok((p.clone(), glob))
            })
            .collect::<anyhow::Result<_>>()
            .map(Self)
    }

    /// Whether there are no patterns.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `path` (below the output, `/`-separated, as [`unpack`]
    /// matches it) matches one of the patterns.
    pub fn is_match(&self, path: &str) -> bool {
        use wax::Program as _;
        self.0.iter().any(|(_, glob)| glob.is_match(path))
    }
}

/// Refuse a pattern no entry path can match: an entry's path below the
/// output is relative, has no `.`, `..` or empty component, and names a file
/// or a link, never a directory with a trailing `/`. The check is on the
/// pattern's text, component by component.
fn never_matches(p: &str) -> anyhow::Result<()> {
    if let Some(rest) = p.strip_prefix('/') {
        bail!(
            "{p:?} matches no path: entry paths are relative to the output; write {:?}",
            rest.trim_start_matches('/')
        );
    }
    if let Some(rest) = p.strip_prefix("./") {
        bail!("{p:?} matches no path: entry paths have no leading `./`; write {rest:?}");
    }
    if p.ends_with('/') {
        bail!(
            "{p:?} matches no file; to take a directory's files write {:?}",
            format!("{}/**", p.trim_end_matches('/'))
        );
    }
    for c in p.split('/') {
        match c {
            "" => bail!(
                "{p:?} matches no path: it has an empty component; write {:?}",
                p.split('/')
                    .filter(|c| !c.is_empty())
                    .collect::<Vec<_>>()
                    .join("/")
            ),
            "." => bail!(
                "{p:?} matches no path: entry paths have no `.` component; write {:?}",
                p.split('/')
                    .filter(|c| *c != ".")
                    .collect::<Vec<_>>()
                    .join("/")
            ),
            ".." => bail!(
                "{p:?} matches no path: entry paths have no `..` component, they never \
                 leave the output"
            ),
            _ => {}
        }
    }
    Ok(())
}

/// Which extracted regular files [`unpack`] makes executable. The bit is
/// only ever added: a file that is `0o755` in the archive stays so.
#[derive(Clone, Debug, Default)]
pub enum Executable {
    /// The archive's normalized modes (`0o755` when any execute bit is set,
    /// else `0o644`; a bare `gz`'s file, which records none, `0o644`).
    #[default]
    Archive,
    /// Every extracted regular file, hard-link copies included, is `0o755`:
    /// a zip built on Windows records no Unix mode.
    All,
    /// A regular file whose path matches one of the patterns is `0o755`.
    /// Each pattern must match one, or the run fails naming it.
    Matching(Globs),
}

/// How [`unpack`] lays the archive out, besides the archive and the
/// destination.
#[derive(Clone, Debug, Default)]
pub struct UnpackOptions {
    /// Leading path components dropped from every entry, as `tar
    /// --strip-components` (see the module docs).
    pub strip_components: u32,
    /// Only the files, symlinks and hard links whose path matches one of
    /// these are extracted, and directory entries are skipped; each pattern
    /// must match one (see the module docs). Empty: every entry.
    pub include: Globs,
    /// The files and links the caller keeps apart (see the module docs):
    /// each one matching is extracted even where `include` does not match
    /// it, and each pattern must match one. Empty: none.
    pub support: Globs,
    /// Which regular files are made executable.
    pub executable: Executable,
    /// How much the archive may unpack to.
    pub limits: Limits,
}

/// Unpack `archive` (in `format`) into `dest`, which must not exist yet.
/// `cancelled` is polled between entries and on every read of the archive
/// stream and of each entry's bytes. Returns the number of entries written
/// (files, directories and links).
pub fn unpack(
    archive: &Path,
    format: Format,
    opts: UnpackOptions,
    dest: &Path,
    cancelled: &dyn Fn() -> bool,
) -> anyhow::Result<usize> {
    let UnpackOptions {
        strip_components,
        include,
        support,
        executable,
        limits,
    } = opts;
    // Before `dest` exists, so a name it cannot take leaves nothing behind.
    // Only a `gz` reads it.
    let gz_name = match format {
        Format::Gz => gz_file_name(archive)?,
        _ => String::new(),
    };
    if std::fs::symlink_metadata(dest).is_ok() {
        bail!("{} already exists", dest.display());
    }
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    set_mode(dest, DIR_MODE)?;

    let file =
        std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut tree = Tree::new(
        dest,
        strip_components,
        include,
        support,
        executable,
        limits,
        cancelled,
    );
    match format {
        Format::Tar => unpack_tar(StopRead::new(&mut reader, cancelled), &mut tree)?,
        // Multi-member: a gzip stream may be several members back to back
        // (`cat a.gz b.gz`, pigz), and `gzip -d` reads them all. Read only
        // to tar's end marker, so the last member's trailer is not verified.
        Format::TarGz => {
            let mut gz = flate2::read::MultiGzDecoder::new(reader);
            unpack_tar(StopRead::new(&mut gz, cancelled), &mut tree)?;
        }
        // Multi-stream, as `xz -d` reads `cat a.xz b.xz`.
        Format::TarXz => {
            let mut xz = lzma_rust2::XzReader::new_mem_limit(reader, true, XZ_MEM_LIMIT_KIB);
            unpack_tar(StopRead::new(&mut xz, cancelled), &mut tree)
                .and_then(|()| drain(&mut xz, &mut tree))
                .map_err(xz_context)?;
        }
        Format::Gz => {
            // Read to its end by the copy: each member's CRC32 and length are
            // verified, and anything after the last member is an error.
            let mut gz = flate2::read::MultiGzDecoder::new(reader);
            tree.entry()?;
            // gzip records no mode; `executable` may raise it. `Tree::file`
            // reads through `StopRead` itself.
            tree.file(&gz_name, &mut gz, 0o644)
                .with_context(|| format!("gz file {gz_name:?}"))?;
        }
        // Read only to tar's end marker: the frame checksum is not verified.
        Format::TarZst => {
            let mut zst =
                zstd::stream::read::Decoder::with_buffer(reader).context("open zstd stream")?;
            unpack_tar(StopRead::new(&mut zst, cancelled), &mut tree)?;
        }
        Format::Zip => unpack_zip(reader, &mut tree)?,
    }
    tree.finish()
}

/// Read an xz stream past tar's end marker to its end: the block check, the
/// index and the footer are verified only there, so stopping at the marker
/// would extract a corrupted payload without an error. What it decodes to
/// counts against [`Limits::max_bytes`], so trailing padding cannot be a
/// bomb.
fn drain(xz: &mut dyn Read, tree: &mut Tree<'_>) -> anyhow::Result<()> {
    tree.skip(xz).context("read the xz stream to its end")
}

/// Name the memory cap on the error lzma-rust2 gives for a dictionary over
/// it, so the message does not rest on that crate's wording alone.
fn xz_context(e: anyhow::Error) -> anyhow::Error {
    let over_cap = e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::OutOfMemory)
    });
    if over_cap {
        e.context(format!(
            "the xz stream's dictionary needs more decoder memory than the {} MiB cap; \
             recompress it with a smaller one (`xz -9` uses 64 MiB)",
            XZ_MEM_LIMIT_KIB >> 10
        ))
    } else {
        e
    }
}

/// The name a bare `gz`'s file takes: the archive's own name with a trailing
/// `.gz` removed (any case), as `gzip -d` names it, or the whole name when
/// it has none or is nothing else (`.gz`). Never the name the gzip header
/// records: that is the archive author's to choose, the archive's file name
/// is the build's.
///
/// `archive.extract` keys a tree by these rules: a change to the naming rule
/// or to a gz's mode rule bumps its `EXTRACT_FORMAT_VERSION`
/// (`crates/plugin-archive/src/extract.rs`).
fn gz_file_name(archive: &Path) -> anyhow::Result<String> {
    let name = archive
        .file_name()
        .with_context(|| format!("{}: the archive has no file name", archive.display()))?
        .to_str()
        .with_context(|| {
            format!(
                "{}: the archive's file name is not UTF-8",
                archive.display()
            )
        })?;
    let stem = Format::Gz
        .strip_suffix(name)
        .filter(|stem| !stem.is_empty())
        .unwrap_or(name);
    if stem == "." || stem == ".." {
        bail!(
            "{}: without its `.gz`, the archive's name is {stem:?}, which names no file; \
             rename the archive",
            archive.display()
        );
    }
    Ok(stem.to_string())
}

/// Whether the gzip file at `path` inflates to a tar: its first
/// [`Format::SNIFF_LEN`] inflated bytes carry tar's `ustar` magic, by the
/// rule [`Format::sniff`] applies to raw bytes. A file too short to hold it,
/// or one that does not inflate, is not a tar; only failing to open or read
/// the file is an error.
///
/// `archive.extract` uses it to tell a `.gz`-named tarball from a bare `gz`
/// when `format` is omitted, which its `INFERENCE_VERSION` keys.
pub fn gz_holds_tar(path: &Path) -> anyhow::Result<bool> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut head = Vec::with_capacity(Format::SNIFF_LEN);
    // A decode error only means the head is not a tar's; an IO error on the
    // file itself surfaces again when it is unpacked.
    let inflated = flate2::read::MultiGzDecoder::new(std::io::BufReader::new(file))
        .take(Format::SNIFF_LEN as u64)
        .read_to_end(&mut head);
    Ok(inflated.is_ok() && crate::is_tar_head(&head))
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
        }
        // Whatever the entry left unread (all of a file left out or
        // stripped, a directory's or a link's stray data) is decoded by tar
        // to reach the next header; read it here instead, so it is counted
        // against `max_bytes`.
        .and_then(|()| tree.skip(&mut entry).context("read past the entry"));
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
    /// Entry bytes decoded, written or read past, against
    /// [`Limits::max_bytes`].
    bytes: u64,
    /// Every symlink written, checked once the tree is complete.
    links: Vec<Vec<String>>,
    /// The first [`STRIPPED_SHOWN`] entry paths `strip_components` removed
    /// whole, so an archive it empties says what it held.
    stripped: Vec<String>,
    /// `include`'s patterns; `None` when every entry is extracted.
    include: Option<Selector>,
    /// `support`'s patterns, extracted whatever `include` says; `None`
    /// when there are none.
    support: Option<Selector>,
    /// The first [`STRIPPED_SHOWN`] paths `include` and `support` were
    /// matched against,
    /// and how many there were, so a pattern that matches none says what it
    /// could have matched.
    candidates: Vec<String>,
    candidate_count: u64,
    exec: Exec,
}

/// How many entry paths an error lists.
const STRIPPED_SHOWN: usize = 5;

/// A pattern list being matched, remembering which patterns matched a path.
struct Selector {
    globs: Globs,
    hit: Vec<bool>,
}

impl Selector {
    fn new(globs: Globs) -> Self {
        let hit = vec![false; globs.0.len()];
        Self { globs, hit }
    }

    /// Whether `path` matches a pattern, marking every pattern that does.
    fn select(&mut self, path: &str) -> bool {
        use wax::Program as _;
        let mut any = false;
        for ((_, glob), hit) in self.globs.0.iter().zip(&mut self.hit) {
            if glob.is_match(path) {
                *hit = true;
                any = true;
            }
        }
        any
    }

    /// Whether `path` matches a pattern, marking none.
    fn matches(&self, path: &str) -> bool {
        self.globs.is_match(path)
    }

    /// The first pattern that matched no path.
    fn unmatched(&self) -> Option<&str> {
        self.globs
            .0
            .iter()
            .zip(&self.hit)
            .find(|(_, hit)| !**hit)
            .map(|((p, _), _)| p.as_str())
    }
}

/// [`Executable`], with its patterns being matched.
enum Exec {
    Archive,
    All,
    Matching(Selector),
}

impl<'a> Tree<'a> {
    fn new(
        root: &Path,
        strip: u32,
        include: Globs,
        support: Globs,
        executable: Executable,
        limits: Limits,
        cancelled: &'a dyn Fn() -> bool,
    ) -> Self {
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
            include: (!include.is_empty()).then(|| Selector::new(include)),
            support: (!support.is_empty()).then(|| Selector::new(support)),
            candidates: Vec::new(),
            candidate_count: 0,
            exec: match executable {
                Executable::Archive => Exec::Archive,
                Executable::All => Exec::All,
                // No pattern: nothing to match, the archive's modes.
                Executable::Matching(g) if g.is_empty() => Exec::Archive,
                Executable::Matching(g) => Exec::Matching(Selector::new(g)),
            },
        }
    }

    /// Whether the file, symlink or hard link at `comps` is extracted:
    /// always without `include`, else when its path matches a pattern of
    /// `include` or of `support`. Every pattern that matches is marked.
    fn included(&mut self, comps: &[String]) -> bool {
        if self.include.is_none() && self.support.is_none() {
            return true;
        }
        let path = comps.join("/");
        // Both, not `||`: a pattern of each that matches is marked.
        let included = self.include.as_mut().is_none_or(|sel| sel.select(&path));
        let support = self.support.as_mut().is_some_and(|sel| sel.select(&path));
        self.candidate_count += 1;
        if self.candidates.len() < STRIPPED_SHOWN {
            self.candidates.push(path);
        }
        included || support
    }

    /// Whether `path` is extracted, marking no pattern: [`Self::included`]
    /// without its bookkeeping.
    fn would_include(&self, path: &str) -> bool {
        self.include.as_ref().is_none_or(|sel| sel.matches(path))
            || self.support.as_ref().is_some_and(|sel| sel.matches(path))
    }

    /// The paths a pattern was matched against, for an error saying it
    /// matched none of them.
    fn candidates_shown(&self) -> String {
        format!(
            "{:?}{}",
            self.candidates,
            if self.candidate_count > self.candidates.len() as u64 {
                " and more"
            } else {
                ""
            }
        )
    }

    /// The mode of the regular file at `comps`, whose normalized mode in the
    /// archive is `archived`: raised to `0o755` by `executable`, never
    /// lowered.
    fn file_mode_of(&mut self, comps: &[String], archived: u32) -> u32 {
        match &mut self.exec {
            Exec::Archive => archived,
            Exec::All => 0o755,
            Exec::Matching(sel) => {
                if sel.select(&comps.join("/")) {
                    0o755
                } else {
                    archived
                }
            }
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

    /// Decode what is left of `data` into nothing, counting it against
    /// [`Limits::max_bytes`] and polling cancellation on every read: a tar
    /// entry left out, or removed whole by `strip_components`, is decoded to
    /// reach the next one, and without the bound an excluded entry of
    /// terabytes of zeros would decode forever.
    fn skip(&mut self, data: &mut dyn Read) -> anyhow::Result<()> {
        let left = self.bytes_left();
        let n = std::io::copy(
            &mut StopRead::new(data, self.cancelled).take(left.saturating_add(1)),
            &mut std::io::sink(),
        )?;
        if n > left {
            return Err(self.too_many_bytes());
        }
        self.bytes += n;
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
        // With `include`, only what an extracted entry needs is created.
        if self.include.is_some() {
            return Ok(());
        }
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
        // Left out: not written. A tar's bytes are read past and counted by
        // `unpack_tar`; a zip's are never read.
        if !self.included(&comps) {
            return Ok(());
        }
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
        let mode = self.file_mode_of(&comps, file_mode(mode));
        set_mode(&path, mode)?;
        self.written += 1;
        Ok(())
    }

    fn symlink(&mut self, raw: &str, target: &str) -> anyhow::Result<()> {
        let Some(comps) = self.entry_components(raw)? else {
            return Ok(());
        };
        if !self.included(&comps) {
            return Ok(());
        }
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
        if !self.included(&comps) {
            return Ok(());
        }
        let target_comps = self
            .components(target)
            .with_context(|| format!("hard link target {target:?}"))?
            .with_context(|| {
                format!("hard link target {target:?} is removed by strip_components")
            })?;
        let target_path = target_comps.join("/");
        if !self.would_include(&target_path) {
            bail!(
                "hard link target {target_path:?} is not included: the link is extracted but \
                 the file it is a copy of is not; include that too"
            );
        }
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
        // The link is its target's file in the archive, so it takes the mode
        // the target was written with, a bit a pattern raised included.
        let archived = {
            use std::os::unix::fs::PermissionsExt as _;
            file_mode(meta.permissions().mode())
        };
        let mode = self.file_mode_of(&comps, archived);
        set_mode(&dest, mode)?;
        self.written += 1;
        Ok(())
    }

    fn finish(self) -> anyhow::Result<usize> {
        // First: a typo in a pattern is likelier than an empty archive, and
        // this error says what the pattern was matched against.
        for (option, sel) in [("include", &self.include), ("support_files", &self.support)] {
            if let Some(p) = sel.as_ref().and_then(Selector::unmatched) {
                bail!(
                    "`{option}` pattern {p:?} matches no file or link in the archive; the paths \
                     it was matched against (below the output, after strip_components) begin {}",
                    self.candidates_shown()
                );
            }
        }
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
        if let Exec::Matching(sel) = &self.exec
            && let Some(p) = sel.unmatched()
        {
            bail!("`executable` pattern {p:?} matches no extracted regular file");
        }
        // Under `include` a link must resolve: a dangling one is likelier a
        // target the patterns left out than one the archive meant.
        let must_resolve = self.include.is_some();
        for link in &self.links {
            if (self.cancelled)() {
                bail!("cancelled");
            }
            stays_inside(&self.root, link, must_resolve)
                .with_context(|| format!("symlink {:?}", link.join("/")))?;
        }
        Ok(self.written)
    }
}

/// Resolve the symlink at `link` (components below `root`) the way the OS
/// would — component by component, following every link met on the way — and
/// fail if the walk ever climbs above `root`. A component that does not exist
/// ends the physical walk: nothing below it can be a link, so the rest is
/// lexical — unless `must_resolve`, when it is an error (the link dangles).
fn stays_inside(root: &Path, link: &[String], must_resolve: bool) -> anyhow::Result<()> {
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
    let target = read(&cur)?;
    let mut pending = split(&target)?;
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
                let meta = std::fs::symlink_metadata(&p);
                if must_resolve && meta.is_err() {
                    bail!(
                        "points at {target:?}, which `include` left out (or the archive does \
                         not hold); include it too"
                    );
                }
                let is_link = meta.is_ok_and(|m| m.file_type().is_symlink());
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

    fn xz(bytes: &[u8]) -> Vec<u8> {
        let opts = lzma_rust2::XzOptions::with_preset(1);
        let mut w = lzma_rust2::XzWriter::new(Vec::new(), opts).unwrap();
        w.write_all(bytes).unwrap();
        w.finish().unwrap()
    }

    /// Unpack `bytes`, written to a file called `name`, into `out/`: a bare
    /// `gz` takes its file's name from the archive's.
    fn run_named(
        name: &str,
        bytes: &[u8],
        format: Format,
        strip: u32,
        executable: bool,
    ) -> (tempfile::TempDir, anyhow::Result<usize>) {
        let opts = UnpackOptions {
            strip_components: strip,
            executable: if executable {
                Executable::All
            } else {
                Executable::Archive
            },
            ..UnpackOptions::default()
        };
        run_opts(name, bytes, format, opts)
    }

    /// Unpack `bytes`, written to a file called `name`, into `out/` with
    /// `opts`.
    fn run_opts(
        name: &str,
        bytes: &[u8],
        format: Format,
        opts: UnpackOptions,
    ) -> (tempfile::TempDir, anyhow::Result<usize>) {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join(name);
        std::fs::write(&archive, bytes).unwrap();
        let out = dir.path().join("out");
        let r = unpack(&archive, format, opts, &out, &never);
        (dir, r)
    }

    fn globs(patterns: &[&str]) -> Globs {
        let owned: Vec<String> = patterns.iter().map(|p| (*p).to_string()).collect();
        Globs::new(&owned).unwrap()
    }

    /// `include` = `patterns`, and nothing else set.
    fn including(patterns: &[&str]) -> UnpackOptions {
        UnpackOptions {
            include: globs(patterns),
            ..UnpackOptions::default()
        }
    }

    /// The files under `out/`, sorted, workspace-style (`a/b`).
    fn listing(dir: &tempfile::TempDir) -> Vec<String> {
        let out = dir.path().join("out");
        let mut seen = Vec::new();
        let mut todo = vec![out.clone()];
        while let Some(d) = todo.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    todo.push(p);
                } else {
                    let rel = p.strip_prefix(&out).unwrap();
                    seen.push(rel.to_string_lossy().into_owned());
                }
            }
        }
        seen.sort();
        seen
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
        let opts = UnpackOptions {
            strip_components: strip,
            limits,
            ..UnpackOptions::default()
        };
        let r = unpack(&archive, format, opts, &out, cancelled);
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
    fn tar_xz_round_trips_with_modes() {
        let (dir, r) = run(&xz(&sample_tar()), Format::TarXz, 1);
        assert_eq!(r.unwrap(), 3);
        assert_eq!(read(&dir, "bin/tool"), "#!/bin/sh\necho hi\n");
        assert_eq!(mode(&dir, "bin/tool"), 0o755);
        assert_eq!(mode(&dir, "README"), 0o644);
    }

    /// `xz -d` reads `cat a.xz b.xz` to the end; so does the unpacker.
    #[test]
    fn a_multi_stream_xz_is_read_to_the_end() {
        let t = sample_tar();
        let mut bytes = xz(&t[..1024]);
        bytes.extend(xz(&t[1024..]));
        let (dir, r) = run(&bytes, Format::TarXz, 0);
        assert_eq!(r.unwrap(), 4);
        assert_eq!(read(&dir, "tool-1.0/README"), "readme\n");
    }

    #[test]
    fn a_truncated_or_corrupt_xz_is_an_error() {
        let x = xz(&sample_tar());
        let (_dir, r) = run(&x[..x.len() / 2], Format::TarXz, 0);
        err(r);
        // A flipped byte in the compressed data.
        let mut bad = x.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0xff;
        let (_dir, r) = run(&bad, Format::TarXz, 0);
        err(r);
        // Not xz at all.
        let (_dir, r) = run(&gz(&sample_tar()), Format::TarXz, 0);
        err(r);
    }

    /// `len` bytes no compressor shrinks, the same on every run.
    fn incompressible(len: usize) -> Vec<u8> {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect()
    }

    /// Where an xz stream's one block check (CRC64) sits: just before the
    /// index, whose size the footer records.
    fn xz_check_field(x: &[u8]) -> std::ops::Range<usize> {
        let footer = x.len() - 12;
        let backward = u32::from_le_bytes(x[footer + 4..footer + 8].try_into().unwrap());
        let index = footer - (backward as usize + 1) * 4;
        index - 8..index
    }

    /// What only the xz check, index and footer catch: tar stops reading at
    /// its end marker, so these are verified because the unpacker reads the
    /// stream past it, to its end.
    #[test]
    fn xz_integrity_is_verified_past_the_tar_end_marker() {
        // LZMA2 stores an incompressible payload as is, so a flipped payload
        // byte decodes cleanly to a wrong byte: only the check sees it.
        let payload = incompressible(200_000);
        let x = xz(&tar_bytes(&[E::File("r", &payload, 0o644)]));
        let (dir, r) = run(&x, Format::TarXz, 0);
        r.unwrap();
        assert_eq!(std::fs::read(dir.path().join("out/r")).unwrap(), payload);
        let at = x
            .windows(32)
            .position(|w| w == &payload[100_000..100_032])
            .expect("the payload is stored uncompressed");
        let mut flipped = x.clone();
        flipped[at] ^= 0x01;
        let (_dir, r) = run(&flipped, Format::TarXz, 0);
        err(r);

        // The CRC64 itself.
        let mut crc = x.clone();
        crc[xz_check_field(&x).start] ^= 0x01;
        let (_dir, r) = run(&crc, Format::TarXz, 0);
        err(r);

        // A tar padded to a 10240-byte record, as GNU tar writes it, cut
        // through the footer, or through the index.
        let mut padded = sample_tar();
        padded.resize(10240, 0);
        let x = xz(&padded);
        let (_dir, r) = run(&x, Format::TarXz, 0);
        r.unwrap();
        for cut in [12, 24] {
            let (_dir, r) = run(&x[..x.len() - cut], Format::TarXz, 0);
            err(r);
        }
    }

    /// What an xz decodes to after tar's end marker is read, so it counts
    /// against `max_bytes`: zeros after the marker cannot be a bomb.
    #[test]
    fn bytes_after_the_tar_end_marker_count_against_max_bytes() {
        let mut t = tar_bytes(&[E::File("a", b"0123456789", 0o644)]);
        t.resize(t.len() + (8 << 20), 0);
        let x = xz(&t);
        let limits = Limits {
            max_bytes: 64 << 10,
            ..Limits::default()
        };
        let (_dir, r) = run_with(&x, Format::TarXz, 0, limits, &never);
        let msg = err(r);
        assert!(msg.contains("`max_bytes`"), "{msg}");
        // The same stream within the default limit unpacks.
        let (dir, r) = run(&x, Format::TarXz, 0);
        r.unwrap();
        assert_eq!(read(&dir, "a"), "0123456789");
    }

    /// An xz bomb: a small stream that decodes to far more than `max_bytes`.
    #[test]
    fn an_xz_bomb_is_bounded() {
        let big = vec![0u8; 32 << 20];
        let x = xz(&tar_bytes(&[E::File("bomb", &big, 0o644)]));
        assert!(x.len() < 64 << 10, "a bomb: {} bytes", x.len());
        let limits = Limits {
            max_bytes: 1 << 20,
            ..Limits::default()
        };
        let (dir, r) = run_with(&x, Format::TarXz, 0, limits, &never);
        let msg = err(r);
        assert!(msg.contains("`max_bytes`"), "{msg}");
        let written = std::fs::metadata(dir.path().join("out/bomb"))
            .unwrap()
            .len();
        // The copy reads one byte past the budget to tell "at it" from "over".
        assert!(written <= (1 << 20) + 1, "stopped at the budget: {written}");
    }

    /// `x` with its first block header's LZMA2 dictionary property set to
    /// `prop`, and the header's CRC32 fixed up to match.
    fn with_xz_dictionary(mut x: Vec<u8>, prop: u8) -> Vec<u8> {
        // The block header follows the 12-byte stream header; its first byte
        // is its size / 4 - 1, and its last four bytes a CRC32 of the rest.
        let start = 12;
        let len = (usize::from(x[start]) + 1) * 4;
        let header = start..start + len - 4;
        // The LZMA2 filter flags: id 0x21, one property byte, the dictionary.
        let at = x[header.clone()]
            .windows(2)
            .position(|w| w == [0x21, 0x01])
            .map(|i| start + i + 2)
            .expect("an LZMA2 filter in the block header");
        x[at] = prop;
        let mut crc = flate2::Crc::new();
        crc.update(&x[header.clone()]);
        x[header.end..header.end + 4].copy_from_slice(&crc.sum().to_le_bytes());
        x
    }

    /// An xz block header names its dictionary size, and the decoder would
    /// allocate it: one over the cap is refused before anything is allocated
    /// or written, naming the cap. A property byte `p` below 40 is a
    /// dictionary of `(2 | (p & 1)) << (p / 2 + 11)` bytes.
    #[test]
    fn an_xz_dictionary_over_the_memory_limit_is_refused() {
        // 40: the largest dictionary, 4 GiB - 1. 32: 256 MiB, which with the
        // decoder's overhead is over the 256 MiB cap.
        for prop in [40, 32] {
            let x = with_xz_dictionary(xz(&sample_tar()), prop);
            let (dir, r) = run(&x, Format::TarXz, 0);
            let msg = err(r);
            assert!(msg.contains("256 MiB cap"), "{prop}: {msg}");
            assert!(listing(&dir).is_empty(), "{prop}: nothing written");
        }
        // 28: 64 MiB, what `xz -9` uses; 31: 192 MiB, the largest under it.
        for prop in [28, 31] {
            let x = with_xz_dictionary(xz(&sample_tar()), prop);
            let (dir, r) = run(&x, Format::TarXz, 0);
            assert_eq!(r.unwrap(), 4, "{prop}");
            assert_eq!(read(&dir, "tool-1.0/README"), "readme\n");
        }
    }

    /// A bare `gz` is one file, named after the archive minus `.gz`, as
    /// `gzip -d` names it; whatever name the gzip header records is ignored.
    #[test]
    fn a_bare_gz_unpacks_to_one_file_named_after_the_archive() {
        let body = b"\x7fELF not really";
        let mut named = Vec::new();
        flate2::GzBuilder::new()
            .filename("evil/../../name")
            .write(&mut named, flate2::Compression::default())
            .write_all(body)
            .unwrap();
        for (archive, want) in [
            ("tool-linux-amd64.gz", "tool-linux-amd64"),
            ("TOOL.GZ", "TOOL"),
            ("tool.tar.gz", "tool.tar"),
            // `format = "gz"` on a name without the suffix keeps the name.
            ("tool", "tool"),
            ("tool.gzip", "tool.gzip"),
        ] {
            let (dir, r) = run_named(archive, &named, Format::Gz, 0, false);
            assert_eq!(r.unwrap(), 1, "{archive}");
            assert_eq!(listing(&dir), [want], "{archive}");
            assert_eq!(
                std::fs::read(dir.path().join("out").join(want)).unwrap(),
                body
            );
            assert_eq!(mode(&dir, want), 0o644, "gzip records no mode");
        }
    }

    #[test]
    fn a_bare_gz_is_executable_only_when_asked() {
        let (dir, r) = run_named("tool.gz", &gz(b"bin"), Format::Gz, 0, true);
        r.unwrap();
        assert_eq!(mode(&dir, "tool"), 0o755);
        let (dir, r) = run_named("tool.gz", &gz(b"bin"), Format::Gz, 0, false);
        r.unwrap();
        assert_eq!(mode(&dir, "tool"), 0o644);
    }

    /// `include` and `executable` patterns match a gz's one file by the
    /// name it takes, and a pattern for another name fails the run.
    #[test]
    fn a_bare_gz_takes_include_and_executable_patterns() {
        let opts = UnpackOptions {
            include: globs(&["tool"]),
            executable: Executable::Matching(globs(&["t*"])),
            ..UnpackOptions::default()
        };
        let (dir, r) = run_opts("tool.gz", &gz(b"bin"), Format::Gz, opts);
        assert_eq!(r.unwrap(), 1);
        assert_eq!(listing(&dir), ["tool"]);
        assert_eq!(mode(&dir, "tool"), 0o755);

        let (_dir, r) = run_opts("tool.gz", &gz(b"bin"), Format::Gz, including(&["tool.gz"]));
        let msg = err(r);
        assert!(
            msg.contains(r#"`include` pattern "tool.gz""#) && msg.contains(r#"["tool"]"#),
            "{msg}"
        );
    }

    /// `include` keeps only the matching files and links, under their own
    /// paths, with the parents they need and no other directory.
    #[test]
    fn include_keeps_only_the_matches() {
        let bytes = tar_bytes(&[
            E::Dir("bin/", 0o755),
            E::File("bin/tool", b"tool", 0o755),
            E::File("bin/helper", b"helper", 0o755),
            E::File("README.md", b"readme", 0o644),
            E::Symlink("bin/alias", "tool"),
            E::Dir("share/", 0o755),
            E::File("share/doc", b"doc", 0o644),
        ]);
        let (dir, r) = run_opts(
            "a",
            &bytes,
            Format::Tar,
            including(&["bin/tool", "**/alias"]),
        );
        assert_eq!(r.unwrap(), 2);
        assert_eq!(listing(&dir), ["bin/alias", "bin/tool"]);
        assert_eq!(read(&dir, "bin/alias"), "tool");
        assert!(!dir.path().join("out/share").exists());
        // A string pattern without metacharacters is a literal path.
        let (dir, r) = run_opts("a", &bytes, Format::Tar, including(&["README.md"]));
        assert_eq!(r.unwrap(), 1);
        assert_eq!(listing(&dir), ["README.md"]);
        // `*` stays within a component.
        let (dir, r) = run_opts("a", &bytes, Format::Tar, including(&["*"]));
        r.unwrap();
        assert_eq!(listing(&dir), ["README.md"]);
    }

    /// Under `include` a directory entry is never written: an empty
    /// directory the archive lists is not created, nor a matching one.
    #[test]
    fn include_skips_directory_entries() {
        let bytes = tar_bytes(&[
            E::Dir("empty/", 0o755),
            E::Dir("bin/", 0o700),
            E::File("bin/tool", b"t", 0o755),
        ]);
        let (dir, r) = run_opts("a", &bytes, Format::Tar, including(&["bin/tool"]));
        assert_eq!(r.unwrap(), 1, "the file only");
        assert!(!dir.path().join("out/empty").exists());
        assert_eq!(mode(&dir, "bin"), 0o755, "a parent made as needed");
        // A pattern that matches only a directory matches nothing.
        let (_dir, r) = run_opts("a", &bytes, Format::Tar, including(&["empty"]));
        let msg = err(r);
        assert!(msg.contains(r#"`include` pattern "empty""#), "{msg}");
    }

    /// A pattern is matched against the path after `strip_components`.
    #[test]
    fn include_matches_the_stripped_path() {
        let (dir, r) = run_opts(
            "a",
            &sample_tar(),
            Format::Tar,
            UnpackOptions {
                strip_components: 1,
                include: globs(&["bin/tool"]),
                ..UnpackOptions::default()
            },
        );
        assert_eq!(r.unwrap(), 1);
        assert_eq!(listing(&dir), ["bin/tool"]);
        // The unstripped path matches nothing once it is stripped.
        let (_dir, r) = run_opts(
            "a",
            &sample_tar(),
            Format::Tar,
            UnpackOptions {
                strip_components: 1,
                include: globs(&["tool-1.0/bin/tool"]),
                ..UnpackOptions::default()
            },
        );
        let msg = err(r);
        assert!(msg.contains(r#"["bin/tool", "README"]"#), "{msg}");
    }

    /// A pattern that matches nothing fails the run, naming it and listing
    /// the first paths it was matched against; one pattern matching is not
    /// enough.
    #[test]
    fn an_unmatched_include_pattern_fails_naming_itself() {
        let (_dir, r) = run_opts(
            "a",
            &sample_tar(),
            Format::Tar,
            including(&["tool-1.0/bin/tool", "tool-1.0/bin/tol"]),
        );
        let msg = err(r);
        assert!(
            msg.contains(r#"`include` pattern "tool-1.0/bin/tol" matches no file"#)
                && msg.contains(r#"["tool-1.0/bin/tool", "tool-1.0/README"]"#)
                && !msg.contains("and more"),
            "{msg}"
        );
        let many: Vec<String> = (0..7).map(|i| format!("f{i}")).collect();
        let entries: Vec<E<'_>> = many.iter().map(|p| E::File(p, b"x", 0o644)).collect();
        let (_dir, r) = run_opts("a", &tar_bytes(&entries), Format::Tar, including(&["g"]));
        let msg = err(r);
        assert!(
            msg.contains(r#"["f0", "f1", "f2", "f3", "f4"] and more"#),
            "{msg}"
        );
    }

    fn supporting(include: &[&str], support: &[&str]) -> UnpackOptions {
        UnpackOptions {
            include: globs(include),
            support: globs(support),
            ..UnpackOptions::default()
        }
    }

    fn sdk_tar() -> Vec<u8> {
        tar_bytes(&[
            E::File("bin/tool", b"bin", 0o755),
            E::File("include/a.h", b"a", 0o644),
            E::File("include/sub/b.h", b"b", 0o644),
            E::File("README", b"r", 0o644),
        ])
    }

    /// A `support` match is extracted whether or not `include` matches it,
    /// and the tree is the same one `include` naming both would write.
    #[test]
    fn support_patterns_are_extracted_beside_the_included_files() {
        let (dir, r) = run_opts(
            "t",
            &sdk_tar(),
            Format::Tar,
            supporting(&["bin/tool"], &["include/**"]),
        );
        assert_eq!(r.unwrap(), 3);
        assert_eq!(
            listing(&dir),
            ["bin/tool", "include/a.h", "include/sub/b.h"]
        );
        // Without `include`, everything, as before.
        let (dir, r) = run_opts(
            "t",
            &sdk_tar(),
            Format::Tar,
            supporting(&[], &["include/**"]),
        );
        assert_eq!(r.unwrap(), 4);
        assert_eq!(
            listing(&dir),
            ["README", "bin/tool", "include/a.h", "include/sub/b.h"]
        );
        // `Globs::is_match` answers the caller's split over the same paths.
        let support = globs(&["include/**"]);
        assert!(support.is_match("include/sub/b.h") && !support.is_match("bin/tool"));
    }

    /// A `support` pattern that matches nothing fails like an `include` one,
    /// naming itself as the option it came from, with `include` set or not.
    #[test]
    fn an_unmatched_support_pattern_fails_naming_itself() {
        for include in [&["bin/tool"][..], &[]] {
            let (_dir, r) = run_opts(
                "t",
                &sdk_tar(),
                Format::Tar,
                supporting(include, &["include/**", "lib/**"]),
            );
            let msg = err(r);
            assert!(
                msg.contains(r#"`support_files` pattern "lib/**" matches no file"#)
                    && msg.contains(r#"["bin/tool", "include/a.h""#),
                "{msg}"
            );
        }
    }

    /// A hard link `include` takes, whose target only `support` takes, is a
    /// copy of an extracted file, so it is written.
    #[test]
    fn a_hard_link_to_a_support_file_is_written() {
        let bytes = tar_bytes(&[E::File("lib/a", b"same", 0o644), E::Hardlink("b", "lib/a")]);
        let (dir, r) = run_opts("t", &bytes, Format::Tar, supporting(&["b"], &["lib/**"]));
        assert_eq!(r.unwrap(), 2);
        assert_eq!(read(&dir, "b"), "same");
    }

    /// An included hard link copies its target, so the target must be
    /// included too; one only the target's pattern matches is not written.
    #[test]
    fn an_included_hard_link_to_an_excluded_target_fails() {
        let bytes = tar_bytes(&[E::File("a", b"same", 0o755), E::Hardlink("b", "a")]);
        let (dir, r) = run_opts("t", &bytes, Format::Tar, including(&["b"]));
        let msg = err(r);
        assert!(
            msg.contains(r#"hard link target "a" is not included"#),
            "{msg}"
        );
        assert!(!dir.path().join("out/a").exists());
        // Both included: the copy is written.
        let (dir, r) = run_opts("t", &bytes, Format::Tar, including(&["a", "b"]));
        assert_eq!(r.unwrap(), 2);
        assert_eq!(read(&dir, "b"), "same");
        // Only the target: the link is left out.
        let (dir, r) = run_opts("t", &bytes, Format::Tar, including(&["a"]));
        r.unwrap();
        assert_eq!(listing(&dir), ["a"]);
    }

    /// An included symlink is held to the in-tree check, whether or not
    /// what it points at was included.
    #[test]
    fn an_included_symlink_is_still_checked() {
        let bytes = tar_bytes(&[
            E::Dir("d/", 0o755),
            E::Symlink("d/up", ".."),
            E::Symlink("b", "d/up/.."),
        ]);
        let (_dir, r) = run_opts("t", &bytes, Format::Tar, including(&["b", "d/up"]));
        let msg = err(r);
        assert!(msg.contains("resolves outside the output"), "{msg}");
        let bytes = tar_bytes(&[E::Symlink("l", "../outside")]);
        let (_dir, r) = run_opts("t", &bytes, Format::Tar, including(&["l"]));
        assert!(err(r).contains("escapes the output"));
        // A left-out entry's path is checked all the same.
        let bytes = tar_bytes(&[E::File("../x", b"x", 0o644), E::File("y", b"y", 0o644)]);
        let (_dir, r) = run_opts("t", &bytes, Format::Tar, including(&["y"]));
        assert!(err(r).contains("`..`"));
    }

    /// A tar entry left out is decoded to reach the next one, so its bytes
    /// count against `max_bytes` as written ones do: an excluded bomb fails
    /// instead of decoding forever. Its entry counts against `max_entries`.
    #[test]
    fn excluded_bytes_count_against_max_bytes() {
        let big = vec![0u8; 64 << 20];
        let bytes = gz(&tar_bytes(&[
            E::File("big", &big, 0o644),
            E::File("small", b"0123456789", 0o644),
        ]));
        let limits = Limits {
            max_bytes: 1 << 20,
            ..Limits::default()
        };
        let opts = UnpackOptions {
            include: globs(&["small"]),
            limits,
            ..UnpackOptions::default()
        };
        let (dir, r) = run_opts("t", &bytes, Format::TarGz, opts.clone());
        let msg = err(r);
        assert!(
            msg.contains("`max_bytes`") && msg.contains(r#"tar entry "big""#),
            "{msg}"
        );
        assert!(!dir.path().join("out/small").exists());
        // Under the limit, the excluded entry's bytes are read past.
        let opts = UnpackOptions {
            limits: Limits::default(),
            ..opts
        };
        let (dir, r) = run_opts("t", &bytes, Format::TarGz, opts.clone());
        assert_eq!(r.unwrap(), 1);
        assert_eq!(listing(&dir), ["small"]);
        let opts = UnpackOptions {
            limits: Limits {
                max_entries: 1,
                ..Limits::default()
            },
            ..opts
        };
        let (_dir, r) = run_opts("t", &bytes, Format::TarGz, opts);
        assert!(err(r).contains("`max_entries`"));
    }

    /// An entry `strip_components` removes whole is decoded past too, and
    /// counted.
    #[test]
    fn stripped_bytes_count_against_max_bytes() {
        let bytes = tar_bytes(&[
            E::File("top", &[0u8; 4096], 0o644),
            E::File("d/small", b"s", 0o644),
        ]);
        let limits = Limits {
            max_bytes: 1024,
            ..Limits::default()
        };
        let (_dir, r) = run_with(&bytes, Format::Tar, 1, limits, &never);
        let msg = err(r);
        assert!(
            msg.contains("`max_bytes`") && msg.contains(r#"tar entry "top""#),
            "{msg}"
        );
    }

    /// A zip entry left out is never read: it is not counted against
    /// `max_bytes`, and a corrupted one is not even noticed; a corrupted
    /// entry that is included still fails its CRC.
    #[test]
    fn an_excluded_zip_entry_is_not_read() {
        let big: Vec<u8> = (0..4096u32).flat_map(u32::to_le_bytes).collect();
        let small = b"small-entry-payload";
        let mut zip = zip_bytes(|w| {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.start_file("big", stored).unwrap();
            w.write_all(&big).unwrap();
            w.start_file("small", stored).unwrap();
            w.write_all(small).unwrap();
        });
        let flip = |zip: &mut [u8], needle: &[u8]| {
            let at = zip
                .windows(needle.len())
                .position(|w| w == needle)
                .expect("stored data is in the archive verbatim");
            zip[at + 1] ^= 0xff;
        };
        flip(&mut zip, &big[64..96]);
        let opts = UnpackOptions {
            include: globs(&["small"]),
            limits: Limits {
                max_bytes: 1024,
                ..Limits::default()
            },
            ..UnpackOptions::default()
        };
        let (dir, r) = run_opts("t.zip", &zip, Format::Zip, opts.clone());
        assert_eq!(r.unwrap(), 1, "the big entry is neither read nor counted");
        assert_eq!(read(&dir, "small"), "small-entry-payload");
        // Included, the corrupted entry fails its CRC.
        let (_dir, r) = run_opts("t.zip", &zip, Format::Zip, including(&["big"]));
        let msg = err(r);
        assert!(msg.to_lowercase().contains("checksum"), "{msg}");
        flip(&mut zip, &small[..]);
        let (_dir, r) = run_opts("t.zip", &zip, Format::Zip, opts);
        let msg = err(r);
        assert!(msg.to_lowercase().contains("checksum"), "{msg}");
    }

    /// A cancel that fires while an excluded entry is being read past stops
    /// the run there.
    #[test]
    fn cancellation_stops_mid_skip() {
        let big = vec![0u8; 16 << 20];
        let bytes = gz(&tar_bytes(&[
            E::File("big", &big, 0o644),
            E::File("small", b"s", 0o644),
        ]));
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("t");
        std::fs::write(&archive, &bytes).unwrap();
        // Entries: `big`'s header is polled once; then the skip's reads.
        let polls = std::cell::Cell::new(0u32);
        let cancelled = || {
            polls.set(polls.get() + 1);
            polls.get() > 8
        };
        let r = unpack(
            &archive,
            Format::TarGz,
            including(&["small"]),
            &dir.path().join("out"),
            &cancelled,
        );
        let msg = err(r);
        assert!(
            msg.contains("cancelled") && msg.contains(r#"tar entry "big": read past the entry"#),
            "{msg}"
        );
    }

    /// `executable = True`: every regular file is `0o755`, a hard link's
    /// copy included, whatever the archive recorded; symlinks are links.
    #[test]
    fn executable_all_marks_every_file() {
        let bytes = tar_bytes(&[
            E::File("a", b"a", 0o644),
            E::File("d/b", b"b", 0o600),
            E::Hardlink("c", "a"),
            E::Symlink("l", "a"),
        ]);
        let opts = UnpackOptions {
            executable: Executable::All,
            ..UnpackOptions::default()
        };
        let (dir, r) = run_opts("t", &bytes, Format::Tar, opts.clone());
        r.unwrap();
        for f in ["a", "d/b", "c"] {
            assert_eq!(mode(&dir, f), 0o755, "{f}");
        }
        assert!(
            std::fs::symlink_metadata(dir.path().join("out/l"))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        // A zip made on Windows: no Unix mode, only DOS attributes.
        let mut zip = zip_bytes(|w| {
            w.start_file("tool", zip_opts(0o644)).unwrap();
            w.write_all(b"#!/bin/sh\n").unwrap();
        });
        windows_built(&mut zip);
        let (dir, r) = run_opts("t.zip", &zip, Format::Zip, UnpackOptions::default());
        r.unwrap();
        assert_eq!(mode(&dir, "tool"), 0o644, "a Windows zip records no mode");
        let (dir, r) = run_opts("t.zip", &zip, Format::Zip, opts);
        r.unwrap();
        assert_eq!(mode(&dir, "tool"), 0o755);
    }

    /// Mark a one-entry zip as made on MS-DOS/Windows (version made by,
    /// offset 5 of its central record) with the archive attribute only.
    fn windows_built(zip: &mut [u8]) {
        patch_central(zip, |c| {
            c[5] = 0;
            c[38..42].copy_from_slice(&0x20u32.to_le_bytes());
        });
    }

    /// Executable patterns mark only the regular files they match; every
    /// other file keeps its mode. A hard link is its target's file in the
    /// archive: copied from a raised file it is executable too, and raised
    /// itself when its own path matches.
    #[test]
    fn executable_patterns_mark_only_the_matches() {
        let bytes = tar_bytes(&[
            E::File("bin/tool", b"t", 0o644),
            E::File("bin/other", b"o", 0o644),
            E::File("lib/run.sh", b"r", 0o755),
            E::Hardlink("bin/copy", "bin/tool"),
            E::Hardlink("bin/tool2", "bin/other"),
        ]);
        let opts = UnpackOptions {
            executable: Executable::Matching(globs(&["bin/tool", "bin/tool2"])),
            ..UnpackOptions::default()
        };
        let (dir, r) = run_opts("t", &bytes, Format::Tar, opts);
        r.unwrap();
        for (f, want) in [
            ("bin/tool", 0o755),
            ("bin/other", 0o644),
            ("lib/run.sh", 0o755),
            ("bin/copy", 0o755),
            ("bin/tool2", 0o755),
        ] {
            assert_eq!(mode(&dir, f), want, "{f}");
        }
    }

    /// An executable pattern must match a regular file that is extracted:
    /// one matching only a symlink, a left-out file or nothing fails.
    #[test]
    fn an_unmatched_executable_pattern_fails() {
        let bytes = tar_bytes(&[
            E::File("bin/tool", b"t", 0o644),
            E::File("README", b"r", 0o644),
            E::Symlink("bin/alias", "tool"),
        ]);
        for (include, pattern) in [
            (&[][..], "bin/tol"),
            (&[][..], "bin/alias"),
            (&["bin/*"][..], "README"),
        ] {
            let opts = UnpackOptions {
                include: globs(include),
                executable: Executable::Matching(globs(&["bin/tool", pattern])),
                ..UnpackOptions::default()
            };
            let (_dir, r) = run_opts("t", &bytes, Format::Tar, opts);
            let msg = err(r);
            assert!(
                msg.contains(&format!(
                    "`executable` pattern {pattern:?} matches no extracted regular file"
                )),
                "{pattern}: {msg}"
            );
        }
    }

    /// The bit is only ever added: a file the archive records as executable
    /// stays so when no pattern matches it.
    #[test]
    fn the_executable_bit_is_never_cleared() {
        let bytes = tar_bytes(&[
            E::File("a", b"a", 0o755),
            E::File("b", b"b", 0o700),
            E::File("c", b"c", 0o644),
        ]);
        for exec in [
            Executable::Archive,
            Executable::Matching(globs(&["c"])),
            Executable::All,
        ] {
            let opts = UnpackOptions {
                executable: exec.clone(),
                ..UnpackOptions::default()
            };
            let (dir, r) = run_opts("t", &bytes, Format::Tar, opts);
            r.unwrap();
            assert_eq!(mode(&dir, "a"), 0o755, "{exec:?}");
            assert_eq!(mode(&dir, "b"), 0o755, "{exec:?}");
        }
    }

    #[test]
    fn globs_refuse_empty_nul_and_invalid_patterns() {
        for (bad, want) in [
            ("", "empty pattern"),
            ("a\0b", "NUL"),
            ("a/**b", "invalid glob pattern"),
            ("/bin/tool", r#"relative to the output; write "bin/tool""#),
            ("//bin/tool", r#"write "bin/tool""#),
            ("./bin/tool", r#"no leading `./`; write "bin/tool""#),
            (
                "bin/",
                r#""bin/" matches no file; to take a directory's files write "bin/**""#,
            ),
            ("bin/./tool", r#"no `.` component; write "bin/tool""#),
            (".", "no `.` component"),
            ("bin/../tool", "no `..` component"),
            ("..", "no `..` component"),
            ("bin//tool", r#"empty component; write "bin/tool""#),
        ] {
            let msg = format!("{:#}", Globs::new(&[bad.to_string()]).unwrap_err());
            assert!(msg.contains(want), "{bad:?}: {msg}");
        }
        assert!(Globs::new(&[]).unwrap().is_empty());
        // Near misses that can match are accepted.
        for ok in [
            "bin/**",
            ".hidden",
            "..x",
            "a.",
            "bin/.tool",
            "{a,b}/x",
            "**",
        ] {
            Globs::new(&[ok.to_string()]).unwrap_or_else(|e| panic!("{ok:?}: {e:#}"));
        }
    }

    /// What a pattern matches is wax's answer, and `archive.extract` keys a
    /// target by the pattern's text alone; this table pins those answers.
    #[test]
    fn matching_is_pinned() {
        use wax::Program as _;
        let nfc = "\u{e9}/tool";
        let nfd = "e\u{301}/tool";
        for (pattern, path, want) in [
            ("*", "tool", true),
            ("*", "bin/tool", false),
            ("*", ".hidden", true),
            ("bin/*", "bin/.hidden", true),
            ("**/tool", "tool", true),
            ("**/tool", "a/b/tool", true),
            ("bin/**", "bin/a/b", true),
            ("bin/**", "bin", true),
            ("**", "a/b/c", true),
            ("**", "a", true),
            ("{a,b}/x", "a/x", true),
            ("{a,b}/x", "b/x", true),
            ("{a,b}/x", "c/x", false),
            ("[!x]", "y", true),
            ("[!x]", "x", false),
            ("[^x]", "y", false),
            ("[^x]", "x", true),
            ("[^x]", "^", true),
            ("?", "a", true),
            ("?", "ab", false),
            ("\\*", "*", true),
            ("\\*", "a", false),
            ("bin/*", "Bin/tool", false),
            ("(?i)bin/*", "Bin/tool", true),
            ("\u{e9}/*", nfc, true),
            ("\u{e9}/*", nfd, false),
            ("*/tool", nfd, true),
        ] {
            let glob = wax::Glob::new(pattern).unwrap();
            assert_eq!(
                glob.is_match(path),
                want,
                "{pattern:?} against {path:?} changed its answer: a pattern's meaning is in \
                 every `archive.extract` key that uses it, so bump GLOB_VERSION in \
                 crates/plugin-archive/src/extract.rs, then update this table"
            );
        }
    }

    /// The path a pattern sees: after `strip_components`, which counts a
    /// leading `.` as GNU tar does, and without any `.` component.
    #[test]
    fn a_pattern_sees_the_path_without_dot_components() {
        let bytes = tar_bytes(&[E::File("./tool-1.0/bin/tool", b"t", 0o755)]);
        let opts = UnpackOptions {
            strip_components: 1,
            include: globs(&["tool-1.0/bin/tool"]),
            ..UnpackOptions::default()
        };
        let (dir, r) = run_opts("t", &bytes, Format::Tar, opts);
        assert_eq!(r.unwrap(), 1, "the strip eats the `.`");
        assert_eq!(listing(&dir), ["tool-1.0/bin/tool"]);
        for raw in ["./bin/tool", "bin/./tool"] {
            let bytes = tar_bytes(&[E::File(raw, b"t", 0o755)]);
            let (dir, r) = run_opts("t", &bytes, Format::Tar, including(&["bin/tool"]));
            assert_eq!(r.unwrap(), 1, "{raw}");
            assert_eq!(listing(&dir), ["bin/tool"], "{raw}");
        }
    }

    /// Under `include`, a symlink must resolve to an extracted path: a
    /// target left out, or a link on the way left out, fails the run. Both
    /// included, or without `include`, it is written.
    #[test]
    fn under_include_a_dangling_symlink_fails() {
        let to_file = tar_bytes(&[
            E::File("libexec/tool", b"t", 0o755),
            E::Symlink("bin/tool", "../libexec/tool"),
        ]);
        let (_dir, r) = run_opts("t", &to_file, Format::Tar, including(&["bin/tool"]));
        let msg = err(r);
        assert!(
            msg.contains(
                r#"symlink "bin/tool": points at "../libexec/tool", which `include` left out"#
            ) && msg.contains("include it too"),
            "{msg}"
        );
        let (dir, r) = run_opts("t", &to_file, Format::Tar, including(&["**/tool"]));
        assert_eq!(r.unwrap(), 2);
        assert_eq!(read(&dir, "bin/tool"), "t");
        let (_dir, r) = run_opts("t", &to_file, Format::Tar, UnpackOptions::default());
        r.unwrap();

        // Through a link that was left out: `s/d -> ..`, `s/l -> d/x`.
        let through = tar_bytes(&[
            E::File("x", b"x", 0o644),
            E::Symlink("s/d", ".."),
            E::Symlink("s/l", "d/x"),
        ]);
        let (_dir, r) = run_opts("t", &through, Format::Tar, including(&["s/l", "x"]));
        let msg = err(r);
        assert!(
            msg.contains(r#"symlink "s/l": points at "d/x", which `include` left out"#),
            "{msg}"
        );
        let (dir, r) = run_opts("t", &through, Format::Tar, including(&["s/*", "x"]));
        assert_eq!(r.unwrap(), 3);
        assert_eq!(read(&dir, "s/l"), "x");

        // A link that dangles in the archive itself: fine without
        // `include`, refused with it.
        let dangling = tar_bytes(&[E::Symlink("l", "nowhere"), E::File("f", b"f", 0o644)]);
        let (dir, r) = run_opts("t", &dangling, Format::Tar, UnpackOptions::default());
        assert_eq!(r.unwrap(), 2);
        assert!(
            std::fs::symlink_metadata(dir.path().join("out/l"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let (_dir, r) = run_opts("t", &dangling, Format::Tar, including(&["*"]));
        assert!(err(r).contains(r#"points at "nowhere""#));
    }

    #[test]
    fn a_bare_gz_obeys_the_tree_rules() {
        // A multi-member stream is one file, read to the end.
        let mut two = gz(b"one ");
        two.extend(gz(b"two"));
        let (dir, r) = run_named("t.gz", &two, Format::Gz, 0, false);
        r.unwrap();
        assert_eq!(read(&dir, "t"), "one two");
        // `strip_components` removes its one component: nothing is left.
        let (_dir, r) = run_named("t.gz", &gz(b"x"), Format::Gz, 1, false);
        let msg = err(r);
        assert!(msg.contains("nothing to extract"), "{msg}");
        // Not gzip, empty, or truncated: errors, not an empty or short file.
        let g = gz(&vec![b'x'; 64 << 10]);
        for bad in [&b"plain text"[..], b"", &g[..g.len() / 2]] {
            let (_dir, r) = run_named("t.gz", bad, Format::Gz, 0, false);
            err(r);
        }
    }

    /// The name corners: what is left once `.gz` is removed must name a
    /// file, and the refusal comes before `out/` is created.
    #[test]
    fn a_bare_gz_name_must_leave_a_file_name() {
        for (archive, want) in [
            // All suffix: the whole name, as `gzip -d` cannot do better.
            (".gz", ".gz"),
            // Only the last `.gz` goes.
            ("a.gz.gz", "a.gz"),
            ("a.GZ.gz", "a.GZ"),
        ] {
            let (dir, r) = run_named(archive, &gz(b"x"), Format::Gz, 0, false);
            assert_eq!(r.unwrap(), 1, "{archive}");
            assert_eq!(listing(&dir), [want], "{archive}");
        }
        for archive in ["..gz", "...gz"] {
            let (dir, r) = run_named(archive, &gz(b"x"), Format::Gz, 0, false);
            let msg = err(r);
            assert!(msg.contains("names no file"), "{archive}: {msg}");
            assert!(!dir.path().join("out").exists(), "{archive}: no `out/`");
        }
    }

    /// An empty file gzipped is one empty file, not "nothing to extract".
    #[test]
    fn a_bare_gz_of_nothing_is_one_empty_file() {
        let (dir, r) = run_named("e.gz", &gz(b""), Format::Gz, 0, false);
        assert_eq!(r.unwrap(), 1);
        assert_eq!(listing(&dir), ["e"]);
        assert_eq!(read(&dir, "e"), "");
        assert_eq!(mode(&dir, "e"), 0o644);
    }

    /// A bare `gz` is read to its end, so its trailer (CRC32, then the
    /// length) is verified, and anything after the last member is refused.
    /// That is stricter than `gzip -d`, which warns about trailing garbage
    /// and ignores trailing zeros.
    #[test]
    fn a_bare_gz_trailer_is_verified_and_nothing_may_follow_it() {
        let g = gz(&incompressible(4096));
        let (_dir, r) = run_named("t.gz", &g, Format::Gz, 0, false);
        r.unwrap();
        // The trailer cut through the length, and through the CRC32.
        for cut in [4, 8] {
            let (_dir, r) = run_named("t.gz", &g[..g.len() - cut], Format::Gz, 0, false);
            err(r);
        }
        // A flipped CRC32 byte.
        let mut crc = g.clone();
        let at = crc.len() - 8;
        crc[at] ^= 0x01;
        let (_dir, r) = run_named("t.gz", &crc, Format::Gz, 0, false);
        err(r);
        // Trailing garbage, and trailing zeros.
        for tail in [&b"garbage!"[..], &[0u8; 512][..]] {
            let mut trailing = g.clone();
            trailing.extend_from_slice(tail);
            let (_dir, r) = run_named("t.gz", &trailing, Format::Gz, 0, false);
            err(r);
        }
    }

    /// A `.gz` named file that inflates to a tar is a `tar.gz`; one that
    /// inflates to anything else, is too short, or is not gzip at all is not.
    #[test]
    fn gz_holds_tar_reads_the_inflated_head() {
        let dir = tempfile::tempdir().unwrap();
        let mut short_tar = sample_tar();
        short_tar.truncate(Format::SNIFF_LEN);
        for (body, want) in [
            (gz(&sample_tar()), true),
            (gz(&short_tar), true),
            (gz(b"\x7fELF a binary"), false),
            (gz(&short_tar[..Format::SNIFF_LEN - 1]), false),
            (gz(b""), false),
            (b"not gzip".to_vec(), false),
            (Vec::new(), false),
        ] {
            let p = dir.path().join("t.gz");
            std::fs::write(&p, &body).unwrap();
            assert_eq!(gz_holds_tar(&p).unwrap(), want, "{body:?}");
        }
        gz_holds_tar(&dir.path().join("missing.gz")).unwrap_err();
    }

    #[test]
    fn a_bare_gz_is_bounded_and_cancellable() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("big.gz");
        std::fs::write(&archive, gz(&vec![b'x'; 4 << 20])).unwrap();
        let opts = UnpackOptions {
            limits: Limits {
                max_bytes: 1 << 20,
                ..Limits::default()
            },
            ..UnpackOptions::default()
        };
        let out = dir.path().join("a");
        let r = unpack(&archive, Format::Gz, opts, &out, &never);
        assert!(err(r).contains("max_bytes"));
        let out = dir.path().join("b");
        let started = || out.join("big").exists();
        let r = unpack(
            &archive,
            Format::Gz,
            UnpackOptions::default(),
            &out,
            &started,
        );
        assert!(err(r).contains("cancelled"));
        assert!(std::fs::metadata(out.join("big")).unwrap().len() < 4 << 20);
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
        let r = unpack(
            &archive,
            Format::Tar,
            UnpackOptions::default(),
            &out,
            &never,
        );
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
            UnpackOptions::default(),
            &out,
            &started,
        );
        let msg = err(r);
        assert!(msg.contains("cancelled"), "{msg}");
        let written = std::fs::metadata(out.join("big")).unwrap().len();
        assert!(written < big.len() as u64, "stopped early: {written}");
    }

    /// The same for an xz stream: its decoder is read through `StopRead` too.
    #[test]
    fn cancellation_stops_mid_entry_in_an_xz() {
        let big = vec![b'x'; 4 << 20];
        let bytes = xz(&tar_bytes(&[E::File("big", &big, 0o644)]));
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        std::fs::write(&archive, &bytes).unwrap();
        let out = dir.path().join("out");
        let started = || out.join("big").exists();
        let r = unpack(
            &archive,
            Format::TarXz,
            UnpackOptions::default(),
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
            ("x.tar.xz", Some(Format::TarXz)),
            ("x.TXZ", Some(Format::TarXz)),
            // The longest suffix wins: a `.tar.gz` is never a bare `gz`.
            ("x.gz", Some(Format::Gz)),
            ("X.GZ", Some(Format::Gz)),
            ("x.tar.gz", Some(Format::TarGz)),
            ("x.tgz.gz", Some(Format::Gz)),
            ("x.tar.xz.gz", Some(Format::Gz)),
            ("x.xz", None),
            ("x.tar.xz.sha256", None),
            ("a.gz.gz", Some(Format::Gz)),
            ("x.TaR.Xz", Some(Format::TarXz)),
            ("é.gz", Some(Format::Gz)),
            ("é", None),
            (".gz", None),
            (".tgz", None),
            ("xgz", None),
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
