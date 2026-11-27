//! Archives, in-process: [`unpack`] an untrusted one into a fresh directory
//! (`tar`, `tar.gz`, `tar.xz`, `tar.zst`, `zip`, and a bare `gz` holding one
//! file), [`write`] one reproducibly in any [`Format::WRITABLE`] format
//! ([`Format::infer`] tells a format from a file name).
//!
//! Both run with the same code on every supported platform: no host `tar`,
//! `unzip` or `gzip`, whose flags, defaults, symlink handling and header bytes
//! differ between GNU, BSD and busybox. `docs/ARCHIVE.md` is the user-facing
//! description of both.
//!
//! [`Format`] and [`Limits`] serialize, so a driver can carry them in its def.
//! They are all that is left with the default `codec` feature off: a crate that
//! only names a format (the `gh` plugin, inferring one from an asset's name)
//! does not compile or link the codecs.

#[cfg(feature = "codec")]
mod unpack;
#[cfg(feature = "codec")]
mod write;

#[cfg(feature = "codec")]
pub use unpack::{UnpackOptions, gz_holds_tar, unpack};
#[cfg(feature = "codec")]
pub use write::{GZIP_LEVEL, Member, WriteOptions, ZIP_DEFLATE_LEVEL, ZSTD_LEVEL, write};

use anyhow::Context as _;

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

/// The archive formats [`unpack`] reads; [`write`] writes the
/// [`Format::WRITABLE`] ones. A gzip or xz stream may be several members
/// back to back, as `gzip -d` and `xz -d` read them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Format {
    #[serde(rename = "tar")]
    Tar,
    #[serde(rename = "tar.gz")]
    TarGz,
    #[serde(rename = "tar.xz")]
    TarXz,
    #[serde(rename = "tar.zst")]
    TarZst,
    #[serde(rename = "zip")]
    Zip,
    /// One gzip-compressed file, not a tar (`tool-linux.gz`): it unpacks to
    /// one file, named after the archive with its `.gz` removed, as `gzip -d`
    /// names it. gzip records no mode, so the file is `0o644` unless the
    /// caller asks for `0o755`.
    #[serde(rename = "gz")]
    Gz,
}

impl Format {
    /// Every format, in the order the spellings are listed in errors and in
    /// the schema.
    pub const ALL: [Self; 6] = [
        Self::Tar,
        Self::TarGz,
        Self::TarXz,
        Self::TarZst,
        Self::Zip,
        Self::Gz,
    ];

    /// The formats [`write`] writes. The others are read only: `gz` holds
    /// one file, not a tree, and `tar.xz` has no writer here whose bytes are
    /// pinned.
    pub const WRITABLE: [Self; 4] = [Self::Tar, Self::TarGz, Self::TarZst, Self::Zip];

    /// Whether [`write`] writes this format.
    pub fn is_writable(self) -> bool {
        Self::WRITABLE.contains(&self)
    }

    /// Whether an `executable` option means something for this format: only
    /// a bare `gz`, whose one file has no recorded mode. An archive format's
    /// entries carry their own modes.
    pub fn takes_executable(self) -> bool {
        matches!(self, Self::Gz)
    }

    /// The spelling a BUILD file uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tar => "tar",
            Self::TarGz => "tar.gz",
            Self::TarXz => "tar.xz",
            Self::TarZst => "tar.zst",
            Self::Zip => "zip",
            Self::Gz => "gz",
        }
    }

    /// File-name suffixes that imply this format, lowercase.
    fn suffixes(self) -> &'static [&'static str] {
        match self {
            Self::Tar => &[".tar"],
            Self::TarGz => &[".tar.gz", ".tgz"],
            Self::TarXz => &[".tar.xz", ".txz"],
            Self::TarZst => &[".tar.zst", ".tzst"],
            Self::Zip => &[".zip"],
            Self::Gz => &[".gz"],
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
    /// (`TOOL.TGZ` is a `tar.gz`). The longest matching suffix wins, so
    /// `x.tar.gz` is a `tar.gz` and `x.gz` a bare `gz`. `None` when no suffix
    /// matches — the suffix must end the name, so `x.tar.gz.sha256` is not an
    /// archive — or when the name is all suffix (`.tar.gz`).
    ///
    /// `archive.extract` keys an inferred format by these rules: a change to
    /// what a name infers bumps its `INFERENCE_VERSION`
    /// (`crates/plugin-archive/src/extract.rs`).
    pub fn infer(file_name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .filter_map(|f| f.strip_suffix(file_name).map(|stem| (f, stem)))
            // The shortest stem is the longest suffix.
            .min_by_key(|(_, stem)| stem.len())
            // A name that is only its suffix (`.tar.gz`) names nothing: not
            // even the shorter suffix inside it.
            .filter(|(_, stem)| !stem.is_empty())
            .map(|(f, _)| f)
    }

    /// `file_name` without the longest of this format's suffixes it ends
    /// with, matched case-insensitively; `None` when it ends with none. The
    /// stem may be empty (`.gz`).
    fn strip_suffix(self, file_name: &str) -> Option<&str> {
        self.suffixes()
            .iter()
            .filter_map(|s| strip_suffix_ignore_case(file_name, s))
            .min_by_key(|stem| stem.len())
    }

    /// How many leading bytes [`Format::sniff`] looks at: through tar's
    /// `ustar` magic, the last signature it knows.
    pub const SNIFF_LEN: usize = TAR_MAGIC_AT + TAR_MAGIC.len();

    /// The format an archive's leading bytes announce, for a file whose name
    /// says nothing: gzip `1f 8b`, xz `fd 37 7a 58 5a 00`, zstd `28 b5 2f fd`,
    /// zip `50 4b 03 04` (a local file header) or `50 4b 05 06` (an empty
    /// zip's end record), tar `ustar` at offset 257. `head` is the first
    /// [`Format::SNIFF_LEN`] bytes or the whole file when it is shorter. A
    /// compressed stream is taken for a compressed tar: a bare `gz` is told
    /// apart only by its name (and then `gz_holds_tar`), or by `format`.
    ///
    /// `archive.extract` keys an inferred format by these rules: a change to
    /// what bytes sniff as bumps its `INFERENCE_VERSION`
    /// (`crates/plugin-archive/src/extract.rs`).
    pub fn sniff(head: &[u8]) -> Option<Self> {
        if head.starts_with(&[0x1f, 0x8b]) {
            Some(Self::TarGz)
        } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
            Some(Self::TarXz)
        } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            Some(Self::TarZst)
        } else if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
            Some(Self::Zip)
        } else if is_tar_head(head) {
            Some(Self::Tar)
        } else {
            None
        }
    }

    /// Every spelling, for messages.
    pub fn spellings() -> Vec<&'static str> {
        Self::ALL.iter().map(|f| f.as_str()).collect()
    }
}

/// Where a POSIX (and GNU) tar header carries its magic, and the magic's
/// first five bytes: `ustar\0` for POSIX, `ustar ` for GNU, so the five
/// match both.
const TAR_MAGIC_AT: usize = 257;
const TAR_MAGIC: &[u8] = b"ustar";

/// [`Format::sniff`]'s tar rule: `head` carries the `ustar` magic at offset
/// 257. `gz_holds_tar` applies it to a gzip stream's inflated head.
fn is_tar_head(head: &[u8]) -> bool {
    head.get(TAR_MAGIC_AT..Format::SNIFF_LEN) == Some(TAR_MAGIC)
}

/// `name` without `suffix`, the suffix matched ASCII case-insensitively;
/// `None` when `name` does not end with it.
fn strip_suffix_ignore_case<'a>(name: &'a str, suffix: &str) -> Option<&'a str> {
    let at = name.len().checked_sub(suffix.len())?;
    let (stem, tail) = name.split_at_checked(at)?;
    tail.eq_ignore_ascii_case(suffix).then_some(stem)
}

/// The mode a regular file gets, in an archive [`write`] makes and in a tree
/// [`unpack`] extracts: executable when any execute bit is set, nothing else
/// carried over (no setuid, no group-write, no umask).
#[cfg(feature = "codec")]
fn file_mode(mode: u32) -> u32 {
    if mode & 0o111 != 0 { 0o755 } else { 0o644 }
}

#[cfg(feature = "codec")]
const DIR_MODE: u32 = 0o755;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_reads_each_signature_and_nothing_else() {
        let mut tar = vec![0u8; 512];
        tar[257..262].copy_from_slice(b"ustar");
        for (head, want) in [
            (&[0x1f, 0x8b, 0x08][..], Some(Format::TarGz)),
            (&[0x28, 0xb5, 0x2f, 0xfd, 0][..], Some(Format::TarZst)),
            (b"\xfd7zXZ\x00\x00\x04", Some(Format::TarXz)),
            (b"\xfd7zXZ\x01", None),
            (b"\xfd7zX", None),
            (b"PK\x03\x04rest", Some(Format::Zip)),
            (b"PK\x05\x06", Some(Format::Zip)),
            (&tar[..], Some(Format::Tar)),
            (&tar[..Format::SNIFF_LEN], Some(Format::Tar)),
            // Near misses: one byte short of each signature.
            (&[0x1f][..], None),
            (&[0x28, 0xb5, 0x2f][..], None),
            (b"PK\x03", None),
            (&tar[..Format::SNIFF_LEN - 1], None),
            (b"", None),
            (b"#!/bin/sh\n", None),
        ] {
            assert_eq!(Format::sniff(head), want, "{head:?}");
        }
    }
}
