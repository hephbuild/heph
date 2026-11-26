//! Archives, in-process: [`unpack`] an untrusted one into a fresh directory,
//! [`write`] one reproducibly, in any of the [`Format`]s (`tar`, `tar.gz`,
//! `tar.zst`, `zip`; [`Format::infer`] tells one from a file name).
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
pub use unpack::unpack;
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

/// The archive formats [`unpack`] reads and [`write`] writes. A `tar.gz` may
/// be several gzip members back to back, as `gzip -d` reads them.
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
    ///
    /// `archive.extract` keys an inferred format by these rules: a change to
    /// what a name infers bumps its `INFERENCE_VERSION`
    /// (`crates/plugin-archive/src/extract.rs`).
    pub fn infer(file_name: &str) -> Option<Self> {
        let lower = file_name.to_ascii_lowercase();
        Self::ALL.into_iter().find(|f| {
            f.suffixes()
                .iter()
                .any(|s| lower.len() > s.len() && lower.ends_with(s))
        })
    }

    /// How many leading bytes [`Format::sniff`] looks at: through tar's
    /// `ustar` magic, the last signature it knows.
    pub const SNIFF_LEN: usize = TAR_MAGIC_AT + TAR_MAGIC.len();

    /// The format an archive's leading bytes announce, for a file whose name
    /// says nothing: gzip `1f 8b`, zstd `28 b5 2f fd`, zip `50 4b 03 04` (a
    /// local file header) or `50 4b 05 06` (an empty zip's end record), tar
    /// `ustar` at offset 257. `head` is the first [`Format::SNIFF_LEN`] bytes
    /// or the whole file when it is shorter. A gzip or zstd stream is taken
    /// for a compressed tar: those are the only compressed formats there are.
    ///
    /// `archive.extract` keys an inferred format by these rules: a change to
    /// what bytes sniff as bumps its `INFERENCE_VERSION`
    /// (`crates/plugin-archive/src/extract.rs`).
    pub fn sniff(head: &[u8]) -> Option<Self> {
        if head.starts_with(&[0x1f, 0x8b]) {
            Some(Self::TarGz)
        } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            Some(Self::TarZst)
        } else if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
            Some(Self::Zip)
        } else if head.get(TAR_MAGIC_AT..Self::SNIFF_LEN) == Some(TAR_MAGIC) {
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
