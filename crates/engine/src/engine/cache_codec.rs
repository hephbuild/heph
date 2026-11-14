//! How cache blobs are encoded at rest, locally and on the remote.
//!
//! A revision written by this version stores its artifacts zstd-compressed
//! (level [`ZSTD_LEVEL`]) under a [`MANIFEST_V2`](crate::engine::local_cache::MANIFEST_V2)
//! manifest, and a compressed blob's entry name carries a [`ZSTD_SUFFIX`]. The
//! remote stores the local bytes verbatim, under the same names and the same
//! manifest name, so a transfer in either direction neither encodes nor
//! decodes.
//!
//! Why two new names rather than a new value in the existing `encoding` field:
//! a heph that predates compression reads `manifest-v1.borsh`, ignores
//! `encoding`, and would hand zstd bytes to its tar reader. It never looks up
//! `manifest-v2.borsh`, so it misses a v2 revision and rebuilds it, writing
//! its own v1 manifest and raw blobs under names that cannot collide with the
//! `.zst` ones. Both binaries then keep working on one cache directory or one
//! bucket.
//!
//! Why zstd at level 1: on Go package archives, Rust rlibs and native
//! binaries it compresses as well as gzip -6 (2.5–4.6x), about 3.5x faster than
//! gzip -1, and decodes at about 1 GB/s. The numbers are in `docs/CACHE_COMPRESSION.md`.
//!
//! What a compressed blob gives up: it has no O(1) pread and no plain file on
//! disk, so [`CacheArtifact`](crate::engine::local_cache::CacheArtifact)
//! reports no seekable reader and no file path for it. Their consumers already
//! fall back: a FUSE slot with such an input runs on the OS unpack path, a
//! plugin streams instead of opening the file, and the tar index is built by
//! walking. A container output that is already compressed (an OCI image of
//! gzip layers) is stored verbatim instead, which keeps those paths and the
//! zero-copy rename for exactly the artifacts that would gain nothing.

use crate::engine::local_cache::{
    EntryWriter, LocalCache, ManifestArtifactContentType, ManifestArtifactEncoding,
};
use anyhow::Context;
use hcore::hartifactcontent::sniff;
use hmodel::htaddr::Addr;
use std::io;
use std::path::Path;
use tracing::debug;

/// zstd level for every blob heph compresses. Level 1 is the cheapest level
/// that already matches gzip -6's ratio on build outputs; higher levels cost
/// write CPU on every build for a few percent of disk.
pub(crate) const ZSTD_LEVEL: i32 = 1;

/// Suffix of a zstd-compressed blob's entry name. Keeps compressed and raw
/// blobs of one revision under distinct keys (see the module docs).
pub(crate) const ZSTD_SUFFIX: &str = ".zst";

/// Below this size a container is compressed without scanning its content.
/// Same reasoning as the remote's former gate: the scan saves seconds on a
/// 500 MB image and nothing measurable on a 200 KB blob.
const MIN_SNIFF_BYTES: u64 = 1024 * 1024;

/// The entry name a blob of `base` name is stored under with `encoding`.
pub(crate) fn entry_name(base: &str, encoding: &ManifestArtifactEncoding) -> String {
    match encoding {
        ManifestArtifactEncoding::Zstd => format!("{base}{ZSTD_SUFFIX}"),
        ManifestArtifactEncoding::None | ManifestArtifactEncoding::Gzip => base.to_string(),
    }
}

/// Open a writer for `(addr, hashin, name)` that stores what it is given
/// encoded as `encoding`. The bytes written to it are the decoded bytes; only
/// what reaches the backend is compressed, so a size counted above it is the
/// decoded size and the backend's spill decision sees the compressed one.
pub(crate) fn open_writer(
    cache: &dyn LocalCache,
    addr: &Addr,
    hashin: &str,
    name: &str,
    encoding: &ManifestArtifactEncoding,
) -> anyhow::Result<Box<dyn EntryWriter>> {
    let inner = cache
        .writer(addr, hashin, name)
        .with_context(|| format!("open cache writer for {addr} {name}"))?;
    match encoding {
        ManifestArtifactEncoding::None => Ok(inner),
        ManifestArtifactEncoding::Zstd => Ok(Box::new(ZstdEntryWriter::new(inner)?)),
        ManifestArtifactEncoding::Gzip => {
            anyhow::bail!("heph does not write gzip cache blobs ({addr} {name})")
        }
    }
}

/// Wrap a stored-bytes reader so it yields decoded bytes.
pub(crate) fn decode_reader(
    reader: Box<dyn io::Read>,
    encoding: &ManifestArtifactEncoding,
) -> anyhow::Result<Box<dyn io::Read>> {
    match encoding {
        ManifestArtifactEncoding::None => Ok(reader),
        ManifestArtifactEncoding::Zstd => Ok(Box::new(
            zstd::stream::read::Decoder::new(reader).context("open zstd decoder")?,
        )),
        // Only ever written by the remote of an older heph, under a manifest
        // name this version does not read.
        ManifestArtifactEncoding::Gzip => anyhow::bail!("gzip cache blobs are not supported"),
    }
}

/// How to store a container file (`TarPath`/`CpioPath`) of `size` bytes at
/// `path`: verbatim when its payload is already compressed, zstd otherwise.
///
/// Best-effort like the scan it wraps: a scan that fails answers "compress",
/// which costs CPU and never correctness.
pub(crate) fn container_encoding(
    path: &Path,
    size: u64,
    content_type: &ManifestArtifactContentType,
) -> ManifestArtifactEncoding {
    if size >= MIN_SNIFF_BYTES
        && matches!(content_type, ManifestArtifactContentType::Tar)
        && is_precompressed(path, size)
    {
        ManifestArtifactEncoding::None
    } else {
        ManifestArtifactEncoding::Zstd
    }
}

fn is_precompressed(path: &Path, size: u64) -> bool {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            debug!(error = ?e, path = %path.display(), "content scan: open failed; compressing");
            return false;
        }
    };
    match sniff::scan_tar(io::BufReader::new(file), size) {
        Ok(v) => {
            if v.precompressed {
                debug!(
                    path = %path.display(),
                    format = v.dominant.unwrap_or("unknown"),
                    size,
                    "storing container verbatim: payload is already compressed"
                );
            }
            v.precompressed
        }
        Err(e) => {
            debug!(error = ?e, path = %path.display(), "content scan failed; compressing");
            false
        }
    }
}

/// An [`EntryWriter`] that zstd-compresses into another one. Commit finishes
/// the frame, then commits the inner writer; dropping it uncommitted drops the
/// inner writer uncommitted, which discards the entry.
struct ZstdEntryWriter {
    enc: zstd::stream::write::Encoder<'static, Box<dyn EntryWriter>>,
}

impl ZstdEntryWriter {
    fn new(inner: Box<dyn EntryWriter>) -> anyhow::Result<Self> {
        let mut enc =
            zstd::stream::write::Encoder::new(inner, ZSTD_LEVEL).context("open zstd encoder")?;
        // A cache serves the same bytes for weeks; a 4-byte checksum per frame
        // turns on-disk corruption into a read error instead of a bad tar.
        enc.include_checksum(true)
            .context("enable zstd frame checksum")?;
        Ok(Self { enc })
    }
}

impl io::Write for ZstdEntryWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.enc.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.enc.flush()
    }
}

impl EntryWriter for ZstdEntryWriter {
    fn commit(self: Box<Self>) -> anyhow::Result<()> {
        self.enc.finish().context("finish zstd frame")?.commit()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::engine::local_cache_fs::LocalCacheFS;
    use std::io::{Read, Write};

    fn addr() -> Addr {
        hmodel::htaddr::parse_addr("//p:t").expect("addr")
    }

    #[test]
    fn zstd_writer_round_trips_and_stores_fewer_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = LocalCacheFS::new(dir.path().to_path_buf()).expect("cache");
        let a = addr();
        let data = vec![0u8; 64 * 1024];
        let mut w =
            open_writer(&cache, &a, "h", "b.zst", &ManifestArtifactEncoding::Zstd).expect("writer");
        w.write_all(&data).expect("write");
        w.commit().expect("commit");

        let stored = cache.reader(&a, "h", "b.zst").expect("reader");
        assert!(stored.size < 1024, "stored {} bytes", stored.size);
        let mut out = Vec::new();
        decode_reader(stored.reader, &ManifestArtifactEncoding::Zstd)
            .expect("decoder")
            .read_to_end(&mut out)
            .expect("decode");
        assert_eq!(out, data);
    }

    /// Dropping without commit must not leave a readable entry: the wrapper
    /// must not finish the frame (and commit the inner writer) on drop.
    #[test]
    fn zstd_writer_dropped_uncommitted_leaves_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = LocalCacheFS::new(dir.path().to_path_buf()).expect("cache");
        let a = addr();
        let mut w =
            open_writer(&cache, &a, "h", "b.zst", &ManifestArtifactEncoding::Zstd).expect("writer");
        w.write_all(b"partial").expect("write");
        drop(w);
        assert!(!cache.exists(&a, "h", "b.zst").expect("exists"));
    }

    #[test]
    fn corrupt_zstd_blob_is_a_read_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = LocalCacheFS::new(dir.path().to_path_buf()).expect("cache");
        let a = addr();
        let mut w =
            open_writer(&cache, &a, "h", "b.zst", &ManifestArtifactEncoding::Zstd).expect("writer");
        w.write_all(&[7u8; 4096]).expect("write");
        w.commit().expect("commit");
        let mut stored = Vec::new();
        cache
            .reader(&a, "h", "b.zst")
            .expect("reader")
            .reader
            .read_to_end(&mut stored)
            .expect("read");
        let last = stored.len() - 1;
        stored[last] ^= 0xff; // flips a checksum byte
        let mut w = cache.writer(&a, "h", "b.zst").expect("writer");
        w.write_all(&stored).expect("write");
        w.commit().expect("commit");

        let mut out = Vec::new();
        let res = decode_reader(
            cache.reader(&a, "h", "b.zst").expect("reader").reader,
            &ManifestArtifactEncoding::Zstd,
        )
        .expect("decoder")
        .read_to_end(&mut out);
        assert!(res.is_err(), "corruption decoded silently");
    }

    #[test]
    fn entry_name_suffixes_only_compressed_blobs() {
        assert_eq!(
            entry_name("out_a.tar", &ManifestArtifactEncoding::Zstd),
            "out_a.tar.zst"
        );
        assert_eq!(
            entry_name("out_a.tar", &ManifestArtifactEncoding::None),
            "out_a.tar"
        );
    }

    /// A tar file whose single member opens with `magic` and runs to `payload`
    /// bytes — the shape of a cached OCI layer (one big blob, little framing).
    /// Written to `dir/c.tar`. Shared with the cache-write and result tests,
    /// which need a container the scan classifies one way or the other.
    pub(crate) fn tar_of_one(
        dir: &Path,
        magic: &[u8],
        payload: usize,
    ) -> (std::path::PathBuf, u64) {
        let mut data = magic.to_vec();
        data.resize(payload, 0xA5);
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "member.bin", data.as_slice())
            .expect("append");
        let bytes = builder.into_inner().expect("tar");
        let path = dir.join("c.tar");
        std::fs::write(&path, &bytes).expect("write");
        (path, bytes.len() as u64)
    }

    /// The case the scan exists for: a big container of gzip payload is stored
    /// verbatim, so it keeps the zero-copy adopt, FUSE and direct-file paths.
    #[test]
    fn a_large_precompressed_container_is_stored_verbatim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (p, size) = tar_of_one(dir.path(), b"\x1f\x8b", 4 * 1024 * 1024);
        assert_eq!(
            container_encoding(&p, size, &ManifestArtifactContentType::Tar),
            ManifestArtifactEncoding::None
        );
    }

    #[test]
    fn a_large_plain_container_is_compressed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (p, size) = tar_of_one(dir.path(), b"\x7fELF", 4 * 1024 * 1024);
        assert_eq!(
            container_encoding(&p, size, &ManifestArtifactContentType::Tar),
            ManifestArtifactEncoding::Zstd
        );
    }

    /// The scan is gated on size: a small container is compressed even when it
    /// is all gzip payload. Guards the gate, which keeps a tar walk off the
    /// path of every small artifact.
    #[test]
    fn a_small_precompressed_container_is_not_scanned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (p, size) = tar_of_one(dir.path(), b"\x1f\x8b", 64 * 1024);
        assert!(size < MIN_SNIFF_BYTES, "fixture must be under the gate");
        assert_eq!(
            container_encoding(&p, size, &ManifestArtifactContentType::Tar),
            ManifestArtifactEncoding::Zstd
        );
    }

    /// A container that does not parse as a tar is compressed, not refused.
    #[test]
    fn an_unparseable_container_is_compressed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("x");
        std::fs::write(&p, vec![0x7f; 4 * 1024 * 1024]).expect("write");
        assert_eq!(
            container_encoding(&p, 4 * 1024 * 1024, &ManifestArtifactContentType::Tar),
            ManifestArtifactEncoding::Zstd
        );
    }

    #[test]
    fn small_and_cpio_containers_are_compressed_without_a_scan() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Not a tar at all: a scan would fail, but neither case scans.
        let p = dir.path().join("x");
        std::fs::write(&p, b"not a tar").expect("write");
        assert_eq!(
            container_encoding(&p, 9, &ManifestArtifactContentType::Tar),
            ManifestArtifactEncoding::Zstd
        );
        assert_eq!(
            container_encoding(&p, 10 << 20, &ManifestArtifactContentType::Cpio),
            ManifestArtifactEncoding::Zstd
        );
    }
}
