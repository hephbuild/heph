//! How cache blobs are encoded at rest, locally and on the remote.
//!
//! A revision written by this version stores its artifacts zstd-compressed
//! (level [`ZSTD_LEVEL`]) under a [`MANIFEST_V2`](crate::engine::local_cache::MANIFEST_V2)
//! manifest. Every blob a v2 manifest names carries a suffix saying how it is
//! stored: [`ZSTD_SUFFIX`] when compressed, [`RAW_SUFFIX`] when verbatim (see
//! [`entry_name`]). The remote stores the local bytes verbatim, under the same
//! names and the same manifest name, so a transfer in either direction neither
//! encodes nor decodes.
//!
//! Why new names rather than a new value in the existing `encoding` field: a
//! heph that predates compression reads `manifest-v1.borsh`, ignores
//! `encoding`, and would hand zstd bytes to its tar reader. It never looks up
//! `manifest-v2.borsh`, so it misses a v2 revision and rebuilds it, writing its
//! own v1 manifest and blobs under their bare names. A v2 name always ends in
//! one of [`V2_SUFFIXES`], and a target whose artifacts would make a bare name
//! equal a suffixed one is refused ([`suffix_collision`]), so the two sets of
//! names cannot collide. Both binaries then keep working on one cache directory
//! or one bucket.
//!
//! Why zstd at level 1: on Go package archives, Rust rlibs and native
//! binaries it compresses as well as gzip -6 (2.5–4.6x), about 3.5x faster than
//! gzip -1, and decodes at about 1 GB/s. The numbers are in `docs/CACHE_COMPRESSION.md`.
//!
//! The format is the zstd *seekable format* (zstd's
//! `contrib/seekable_format`): the decoded bytes are cut into frames of
//! [`SEEKABLE_FRAME_SIZE`], each compressed independently, followed by a seek
//! table in a skippable frame that lists every frame's stored and decoded
//! size. The whole blob is still one valid zstd stream, so a plain streaming
//! decoder reads it front to back ([`decode_reader`]); the seek table lets
//! [`SeekableDecoder`] serve a read at any decoded offset by decoding only the
//! frame that holds it. That is what gives a compressed blob a seekable reader:
//! a FUSE slot serves it lazily and the tar index is built from headers alone,
//! skipping the frames that hold nothing but file data.
//!
//! What a compressed blob still gives up: it has no plain file on disk, so
//! [`CacheArtifact`](crate::engine::local_cache::CacheArtifact) reports no file
//! path for it and a plugin streams instead of opening the file, and it cannot
//! be adopted into the cache by rename. A container output that is already
//! compressed (an OCI image of gzip layers) is stored verbatim instead, which
//! keeps those paths for exactly the artifacts that would gain nothing.

use crate::engine::local_cache::{
    EntryWriter, LocalCache, ManifestArtifactContentType, ManifestArtifactEncoding,
};
use anyhow::Context;
use hcore::hartifactcontent::{ReadSeek, sniff};
use hmodel::htaddr::Addr;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use tracing::debug;

/// zstd level for every blob heph compresses. Level 1 is the cheapest level
/// that already matches gzip -6's ratio on build outputs; higher levels cost
/// write CPU on every build for a few percent of disk.
pub(crate) const ZSTD_LEVEL: i32 = 1;

/// Decoded bytes per seekable frame.
///
/// The trade-off: each frame starts with an empty window, so smaller frames
/// compress worse; and a read at a random offset decodes up to a whole frame,
/// so larger frames make a FUSE read or a tar-header seek cost more. At 1 MiB
/// the blob is 0.5–1.1% larger than one frame on Go archives, rlibs and native
/// binaries, a random read decodes at most 1 MiB (well under a millisecond at
/// the 1.5 GB/s it decodes), and the seek table costs 8 bytes per MiB. It is
/// also the most a writer or a reader holds in memory per blob.
pub(crate) const SEEKABLE_FRAME_SIZE: usize = 1 << 20;

/// Magic of the skippable frame that carries the seek table.
const SKIPPABLE_MAGIC: u32 = 0x184D_2A5E;
/// Magic closing the seek table footer.
const SEEKABLE_MAGIC: u32 = 0x8F92_EAB1;
/// Skippable frame header: magic + frame size, both `u32` LE.
const SKIPPABLE_HEADER_SIZE: usize = 8;
/// Seek table footer: frame count `u32`, descriptor `u8`, magic `u32`.
const FOOTER_SIZE: usize = 9;
/// Descriptor bit: each entry carries a 4-byte decoded-data checksum.
const DESCRIPTOR_CHECKSUM: u8 = 0x80;
/// Descriptor bits the format reserves; a table with any set is refused.
const DESCRIPTOR_RESERVED: u8 = 0x7C;
/// A seek table entry that this writer emits: stored size, decoded size. The
/// per-entry checksum is left out because every frame carries zstd's own.
const ENTRY_SIZE: usize = 8;
/// Largest decoded frame a reader accepts. heph writes [`SEEKABLE_FRAME_SIZE`];
/// the cap only bounds what a corrupt table can make the reader allocate.
const MAX_FRAME_DECODED: u64 = 64 << 20;

/// Suffix of the entry name of a blob a v2 manifest names stored zstd-compressed.
pub(crate) const ZSTD_SUFFIX: &str = ".zst";

/// Suffix of the entry name of a blob a v2 manifest names stored verbatim (a
/// container whose payload is already compressed, or a `tmp` revision's blob).
///
/// A verbatim blob needs a suffix as much as a compressed one: under its bare
/// name it would share its local key and its remote object with an older
/// heph's blob of the same artifact (raw locally, gzip on the remote), and
/// either binary would then read the other's bytes under its own manifest.
pub(crate) const RAW_SUFFIX: &str = ".raw";

/// Every suffix [`entry_name`] appends. They have one length, so two distinct
/// bases never map to one entry name (`a + s1 == b + s2` with `|s1| == |s2|`
/// forces `a == b`): within a v2 revision a collision is unrepresentable. The
/// one collision left is across versions, with a v1 blob whose *base* ends in
/// a suffix; [`suffix_collision`] is what refuses it.
pub(crate) const V2_SUFFIXES: [&str; 2] = [ZSTD_SUFFIX, RAW_SUFFIX];

/// Below this size a container is compressed without scanning its content.
/// Same reasoning as the remote's former gate: the scan saves seconds on a
/// 500 MB image and nothing measurable on a 200 KB blob.
const MIN_SNIFF_BYTES: u64 = 1024 * 1024;

/// The entry name a blob of `base` name is stored under with `encoding`. Every
/// name a v2 manifest holds comes from here, so it ends in one of
/// [`V2_SUFFIXES`] and never equals a v1 name of the same artifact, which is
/// the bare base.
pub(crate) fn entry_name(base: &str, encoding: &ManifestArtifactEncoding) -> String {
    match encoding {
        ManifestArtifactEncoding::Zstd => format!("{base}{ZSTD_SUFFIX}"),
        ManifestArtifactEncoding::None => format!("{base}{RAW_SUFFIX}"),
        // Never written (`open_writer` refuses it); named apart from both
        // so that nothing could ever confuse it with a blob that was.
        ManifestArtifactEncoding::Gzip => format!("{base}.gz"),
    }
}

/// A pair of bases among one target's artifacts that an older heph and this one
/// would store under one key: `b` and `b<suffix>`. This version stores `b` as
/// `b<suffix>`, which is the name an older heph gives the artifact whose base is
/// `b<suffix>` — a raw blob locally, a gzip object on the remote. Refusing the
/// pair keeps v1 and v2 entry names of one revision disjoint.
pub(crate) fn suffix_collision(
    bases: &std::collections::HashSet<String>,
) -> Option<(&str, String)> {
    bases.iter().find_map(|base| {
        V2_SUFFIXES.iter().find_map(|suffix| {
            let suffixed = format!("{base}{suffix}");
            bases
                .contains(&suffixed)
                .then_some((base.as_str(), suffixed))
        })
    })
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
        ManifestArtifactEncoding::Zstd => Ok(Box::new(
            ZstdEntryWriter::new(inner)
                .with_context(|| format!("open zstd writer for {addr} {name}"))?,
        )),
        ManifestArtifactEncoding::Gzip => {
            anyhow::bail!("heph does not write gzip cache blobs ({addr} {name})")
        }
    }
}

/// Wrap a stored-bytes reader so it yields decoded bytes.
///
/// A seekable blob is a plain zstd stream, so this streams it with the
/// ordinary decoder. One check is added: a stream of several frames that is
/// cut exactly between two frames decodes cleanly to short data, so the
/// stored bytes must end with the seek table's magic.
pub(crate) fn decode_reader(
    reader: Box<dyn io::Read>,
    encoding: &ManifestArtifactEncoding,
) -> anyhow::Result<Box<dyn io::Read>> {
    match encoding {
        ManifestArtifactEncoding::None => Ok(reader),
        ManifestArtifactEncoding::Zstd => Ok(Box::new(
            zstd::stream::read::Decoder::new(SeekTableTail::new(reader))
                .context("open zstd decoder")?,
        )),
        // Only ever written by the remote of an older heph, under a manifest
        // name this version does not read.
        ManifestArtifactEncoding::Gzip => anyhow::bail!("gzip cache blobs are not supported"),
    }
}

/// Wrap a stored-bytes seekable reader so it yields decoded bytes with random
/// access. `None` for an encoding that has no seekable decoder.
pub(crate) fn decode_seekable(
    reader: Box<dyn ReadSeek + Send>,
    encoding: &ManifestArtifactEncoding,
) -> anyhow::Result<Option<Box<dyn ReadSeek + Send>>> {
    match encoding {
        ManifestArtifactEncoding::None => Ok(Some(reader)),
        ManifestArtifactEncoding::Zstd => Ok(Some(Box::new(SeekableDecoder::open(reader)?))),
        ManifestArtifactEncoding::Gzip => Ok(None),
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

/// An [`EntryWriter`] that writes the zstd seekable format into another one:
/// one frame per [`SEEKABLE_FRAME_SIZE`] decoded bytes, then the seek table.
/// Commit writes the last frame and the table, then commits the inner writer;
/// dropping it uncommitted drops the inner writer uncommitted, which discards
/// the entry.
struct ZstdEntryWriter {
    inner: Box<dyn EntryWriter>,
    cctx: zstd::bulk::Compressor<'static>,
    /// Decoded bytes of the frame being filled, at most [`SEEKABLE_FRAME_SIZE`].
    /// Grows with what is written, so a small blob costs a small buffer.
    pending: Vec<u8>,
    /// Scratch for one compressed frame.
    frame: Vec<u8>,
    /// `(stored, decoded)` size of every frame written so far.
    table: Vec<(u32, u32)>,
}

impl ZstdEntryWriter {
    fn new(inner: Box<dyn EntryWriter>) -> anyhow::Result<Self> {
        let mut cctx = zstd::bulk::Compressor::new(ZSTD_LEVEL).context("open zstd compressor")?;
        // A cache serves the same bytes for weeks; a 4-byte checksum per frame
        // turns on-disk corruption into a read error instead of a bad tar.
        cctx.set_parameter(zstd::stream::raw::CParameter::ChecksumFlag(true))
            .context("enable zstd frame checksum")?;
        Ok(Self {
            inner,
            cctx,
            pending: Vec::new(),
            frame: Vec::new(),
            table: Vec::new(),
        })
    }

    /// Compress `data` as one frame and write it to the inner writer.
    fn write_frame(&mut self, data: &[u8]) -> io::Result<()> {
        let decoded = u32::try_from(data.len()).map_err(io::Error::other)?;
        self.frame.clear();
        self.frame
            .reserve(zstd::zstd_safe::compress_bound(data.len()));
        let stored = self.cctx.compress_to_buffer(data, &mut self.frame)?;
        let stored = u32::try_from(stored).map_err(io::Error::other)?;
        self.inner.write_all(&self.frame)?;
        self.table.push((stored, decoded));
        Ok(())
    }

    /// Write the buffered bytes as a frame, keeping the buffer's allocation.
    fn write_pending(&mut self) -> io::Result<()> {
        let pending = std::mem::take(&mut self.pending);
        let res = self.write_frame(&pending);
        self.pending = pending;
        self.pending.clear();
        res
    }

    /// The seek table, as the skippable frame that ends the blob.
    fn seek_table(&self) -> anyhow::Result<Vec<u8>> {
        let frames = u32::try_from(self.table.len()).context("too many zstd frames")?;
        let body = self.table.len() * ENTRY_SIZE + FOOTER_SIZE;
        let mut out = Vec::with_capacity(SKIPPABLE_HEADER_SIZE + body);
        out.extend_from_slice(&SKIPPABLE_MAGIC.to_le_bytes());
        out.extend_from_slice(
            &u32::try_from(body)
                .context("zstd seek table too large")?
                .to_le_bytes(),
        );
        for (stored, decoded) in &self.table {
            out.extend_from_slice(&stored.to_le_bytes());
            out.extend_from_slice(&decoded.to_le_bytes());
        }
        out.extend_from_slice(&frames.to_le_bytes());
        out.push(0); // descriptor: no per-entry checksums
        out.extend_from_slice(&SEEKABLE_MAGIC.to_le_bytes());
        Ok(out)
    }
}

impl io::Write for ZstdEntryWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // A full frame is written before more is accepted, so a failed frame
        // write never reports bytes it took as consumed.
        if self.pending.len() == SEEKABLE_FRAME_SIZE {
            self.write_pending()?;
        }
        if self.pending.is_empty() && buf.len() >= SEEKABLE_FRAME_SIZE {
            // A whole frame straight from the caller's buffer, without a copy.
            let (frame, _) = buf.split_at(SEEKABLE_FRAME_SIZE);
            self.write_frame(frame)?;
            return Ok(SEEKABLE_FRAME_SIZE);
        }
        let take = buf.len().min(SEEKABLE_FRAME_SIZE - self.pending.len());
        let (taken, _) = buf.split_at(take);
        self.pending.extend_from_slice(taken);
        Ok(take)
    }

    /// Flushes the inner writer only: ending a frame early would change the
    /// blob's framing, and nothing reads an entry before it is committed.
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl EntryWriter for ZstdEntryWriter {
    fn commit(mut self: Box<Self>) -> anyhow::Result<()> {
        if !self.pending.is_empty() {
            self.write_pending().context("write last zstd frame")?;
        }
        let table = self.seek_table()?;
        self.inner
            .write_all(&table)
            .context("write zstd seek table")?;
        self.inner.commit()
    }
}

/// Little-endian `u32` at `at` in `buf`.
fn u32_at(buf: &[u8], at: usize) -> anyhow::Result<u32> {
    let bytes = buf.get(at..at + 4).context("zstd seek table cut short")?;
    Ok(u32::from_le_bytes(
        bytes.try_into().context("zstd seek table field")?,
    ))
}

/// Random access to the decoded bytes of a seekable-format blob.
///
/// Opening reads the seek table (one read at the end of the blob) and checks
/// that its frames account for every stored byte, so a truncated or padded
/// blob fails here rather than reading short. A read decodes the frame that
/// holds the position and keeps it, so sequential reads and seeks within one
/// frame decode it once.
pub(crate) struct SeekableDecoder {
    src: Box<dyn ReadSeek + Send>,
    /// Frame `i` is `stored[i]..stored[i + 1]` of the blob and
    /// `decoded[i]..decoded[i + 1]` of the decoded bytes. Both start at 0 and
    /// hold one more element than there are frames.
    stored: Vec<u64>,
    decoded: Vec<u64>,
    dctx: zstd::bulk::Decompressor<'static>,
    /// The frame whose decoded bytes are in `frame`.
    current: Option<usize>,
    frame: Vec<u8>,
    /// One frame's stored bytes.
    scratch: Vec<u8>,
    pos: u64,
}

impl SeekableDecoder {
    pub(crate) fn open(mut src: Box<dyn ReadSeek + Send>) -> anyhow::Result<Self> {
        let len = src.seek(SeekFrom::End(0)).context("size zstd blob")?;
        anyhow::ensure!(
            len >= (SKIPPABLE_HEADER_SIZE + FOOTER_SIZE) as u64,
            "zstd blob of {len} bytes is too short to hold a seek table"
        );
        src.seek(SeekFrom::Start(len - FOOTER_SIZE as u64))
            .context("seek to zstd seek table footer")?;
        let mut footer = [0u8; FOOTER_SIZE];
        src.read_exact(&mut footer)
            .context("read zstd seek table footer")?;
        let frames = u32_at(&footer, 0)?;
        let descriptor = footer.get(4).copied().unwrap_or_default();
        anyhow::ensure!(
            u32_at(&footer, 5)? == SEEKABLE_MAGIC,
            "zstd blob does not end with a seek table: truncated, or not in the seekable format"
        );
        anyhow::ensure!(
            descriptor & DESCRIPTOR_RESERVED == 0,
            "zstd seek table descriptor {descriptor:#x} sets reserved bits"
        );
        let entry: u64 = if descriptor & DESCRIPTOR_CHECKSUM != 0 {
            ENTRY_SIZE as u64 + 4
        } else {
            ENTRY_SIZE as u64
        };
        let body = u64::from(frames) * entry + FOOTER_SIZE as u64;
        let table_len = SKIPPABLE_HEADER_SIZE as u64 + body;
        anyhow::ensure!(
            table_len <= len,
            "zstd seek table of {frames} frames is longer than the {len}-byte blob"
        );
        let table_start = len - table_len;
        src.seek(SeekFrom::Start(table_start))
            .context("seek to zstd seek table")?;
        let mut table = vec![0u8; usize::try_from(table_len - FOOTER_SIZE as u64)?];
        src.read_exact(&mut table).context("read zstd seek table")?;
        anyhow::ensure!(
            u32_at(&table, 0)? == SKIPPABLE_MAGIC && u64::from(u32_at(&table, 4)?) == body,
            "zstd seek table has a malformed frame header"
        );

        let cap = usize::try_from(frames)? + 1;
        let mut stored = Vec::with_capacity(cap);
        let mut decoded = Vec::with_capacity(cap);
        let (mut c_end, mut d_end) = (0u64, 0u64);
        stored.push(c_end);
        decoded.push(d_end);
        let entry = usize::try_from(entry)?;
        for i in 0..usize::try_from(frames)? {
            let at = SKIPPABLE_HEADER_SIZE + i * entry;
            let c = u64::from(u32_at(&table, at)?);
            let d = u64::from(u32_at(&table, at + 4)?);
            anyhow::ensure!(
                d <= MAX_FRAME_DECODED,
                "zstd frame {i} claims {d} decoded bytes"
            );
            c_end += c;
            d_end += d;
            stored.push(c_end);
            decoded.push(d_end);
        }
        anyhow::ensure!(
            c_end == table_start,
            "zstd seek table frames cover {c_end} bytes, but {table_start} precede the table"
        );
        Ok(Self {
            src,
            stored,
            decoded,
            dctx: zstd::bulk::Decompressor::new().context("open zstd decompressor")?,
            current: None,
            frame: Vec::new(),
            scratch: Vec::new(),
            pos: 0,
        })
    }

    fn decoded_len(&self) -> u64 {
        self.decoded.last().copied().unwrap_or_default()
    }

    /// Decode frame `i` into `frame`, unless it is already there.
    fn load(&mut self, i: usize) -> io::Result<()> {
        if self.current == Some(i) {
            return Ok(());
        }
        let bad = |msg: String| io::Error::new(io::ErrorKind::InvalidData, msg);
        let (Some(&c0), Some(&c1), Some(&d0), Some(&d1)) = (
            self.stored.get(i),
            self.stored.get(i + 1),
            self.decoded.get(i),
            self.decoded.get(i + 1),
        ) else {
            return Err(bad(format!("zstd frame {i} is not in the seek table")));
        };
        self.current = None;
        self.src.seek(SeekFrom::Start(c0))?;
        self.scratch.clear();
        let c_len = c1 - c0;
        let read = (&mut self.src).take(c_len).read_to_end(&mut self.scratch)?;
        if read as u64 != c_len {
            return Err(bad(format!(
                "zstd frame {i}: read {read} of its {c_len} stored bytes"
            )));
        }
        let d_len = usize::try_from(d1 - d0).map_err(io::Error::other)?;
        self.frame.clear();
        self.frame.reserve(d_len);
        let n = self
            .dctx
            .decompress_to_buffer(&self.scratch, &mut self.frame)
            .map_err(|e| io::Error::new(e.kind(), format!("decode zstd frame {i}: {e}")))?;
        if n != d_len {
            return Err(bad(format!(
                "zstd frame {i} decoded to {n} bytes, the seek table says {d_len}"
            )));
        }
        self.current = Some(i);
        Ok(())
    }
}

impl Read for SeekableDecoder {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.decoded_len() {
            return Ok(0);
        }
        // The last frame starting at or before `pos`; `decoded[0]` is 0 and
        // `pos` is under the end, so it exists and is not past the last one.
        // Empty frames start where the next one does, so they are skipped.
        let i = self
            .decoded
            .partition_point(|&start| start <= self.pos)
            .saturating_sub(1);
        self.load(i)?;
        let start = self.decoded.get(i).copied().unwrap_or_default();
        let off = usize::try_from(self.pos - start).map_err(io::Error::other)?;
        let avail = self.frame.get(off..).unwrap_or_default();
        let n = avail.len().min(buf.len());
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("zstd frame {i} holds no byte at offset {}", self.pos),
            ));
        }
        let (dst, _) = buf.split_at_mut(n);
        let (src, _) = avail.split_at(n);
        dst.copy_from_slice(src);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SeekableDecoder {
    /// Like a file: seeking past the end is allowed and reads nothing there;
    /// seeking before the start is an error.
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.decoded_len().checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        }
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid seek {pos:?} from {} in a zstd blob", self.pos),
            )
        })?;
        self.pos = new;
        Ok(new)
    }
}

/// A stored-bytes reader that fails at the end of the stream unless the bytes
/// it passed on ended with the seek table's magic. See [`decode_reader`].
struct SeekTableTail<R> {
    inner: R,
    /// The last four bytes read, as a little-endian `u32`.
    tail: u32,
    seen: u64,
}

impl<R> SeekTableTail<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            tail: 0,
            seen: 0,
        }
    }
}

impl<R: Read> Read for SeekTableTail<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 && !buf.is_empty() {
            if self.seen < 4 || self.tail != SEEKABLE_MAGIC {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "zstd cache blob does not end with its seek table: it is truncated",
                ));
            }
            return Ok(0);
        }
        let filled = buf.get(..n).unwrap_or_default();
        for &b in filled.iter().skip(n.saturating_sub(4)) {
            self.tail = (self.tail >> 8) | (u32::from(b) << 24);
        }
        self.seen += n as u64;
        Ok(n)
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

    const F: usize = SEEKABLE_FRAME_SIZE;

    /// Deterministic bytes that compress somewhat (a quarter of each 16-byte
    /// run is noise), so frames are neither trivial nor incompressible.
    fn sample(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|i| {
                if i % 16 < 4 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                } else {
                    (i / 16) as u8
                }
            })
            .collect()
    }

    /// The stored bytes of `data` written through the zstd writer.
    fn encode(data: &[u8]) -> Vec<u8> {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = LocalCacheFS::new(dir.path().to_path_buf()).expect("cache");
        let a = addr();
        let mut w =
            open_writer(&cache, &a, "h", "b.zst", &ManifestArtifactEncoding::Zstd).expect("writer");
        // Uneven writes, so frames are cut from buffered and direct input alike.
        for chunk in data.chunks(F / 3 + 7) {
            w.write_all(chunk).expect("write");
        }
        w.commit().expect("commit");
        let mut stored = Vec::new();
        cache
            .reader(&a, "h", "b.zst")
            .expect("reader")
            .reader
            .read_to_end(&mut stored)
            .expect("read");
        stored
    }

    fn seekable(stored: Vec<u8>) -> anyhow::Result<SeekableDecoder> {
        SeekableDecoder::open(Box::new(io::Cursor::new(stored)))
    }

    fn stream(stored: Vec<u8>) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        decode_reader(
            Box::new(io::Cursor::new(stored)),
            &ManifestArtifactEncoding::Zstd,
        )
        .expect("decoder")
        .read_to_end(&mut out)?;
        Ok(out)
    }

    /// Frame count from the seek table footer.
    fn frame_count(stored: &[u8]) -> u32 {
        u32_at(stored, stored.len() - FOOTER_SIZE).expect("footer")
    }

    /// Every size around a frame boundary decodes back through the seekable
    /// decoder, the streaming path, and zstd's own decoder (format validity:
    /// the blob is a plain zstd stream), with one frame per started MiB.
    #[test]
    fn seekable_format_round_trips_at_frame_boundaries() {
        for len in [0, 1, F - 1, F, F + 1, 3 * F + 12345] {
            let data = sample(len, len as u64);
            let stored = encode(&data);
            assert_eq!(frame_count(&stored) as usize, len.div_ceil(F), "len {len}");

            let mut plain = Vec::new();
            zstd::stream::read::Decoder::new(stored.as_slice())
                .expect("zstd decoder")
                .read_to_end(&mut plain)
                .expect("plain zstd decode");
            assert!(plain == data, "plain zstd decode differs at len {len}");

            assert!(
                stream(stored.clone()).expect("stream") == data,
                "stream differs at len {len}"
            );

            let mut out = Vec::new();
            seekable(stored)
                .expect("open")
                .read_to_end(&mut out)
                .expect("seekable read");
            assert!(out == data, "seekable read differs at len {len}");
        }
    }

    /// Seeks of every kind land on the right bytes, including across frame
    /// boundaries and backwards into an already-decoded frame.
    #[test]
    fn seekable_decoder_reads_at_any_offset() {
        let len = 3 * F + 12345;
        let data = sample(len, 42);
        let mut d = seekable(encode(&data)).expect("open");
        let len64 = len as u64;

        let check = |d: &mut SeekableDecoder, at: u64, n: usize| {
            let mut buf = vec![0u8; n];
            let mut got = 0;
            while got < n {
                let k = d.read(&mut buf[got..]).expect("read");
                if k == 0 {
                    break;
                }
                got += k;
            }
            let at = at as usize;
            let want = &data[at.min(len)..(at + n).min(len)];
            assert_eq!(got, want.len(), "length at {at}");
            assert!(&buf[..got] == want, "bytes at {at}");
        };

        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut offsets: Vec<u64> = vec![0, F as u64 - 1, F as u64, F as u64 + 1, len64 - 1];
        for _ in 0..64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            offsets.push(x % len64);
        }
        for (k, &at) in offsets.iter().enumerate() {
            let n = 1 + (k * 4099) % (F + 17);
            assert_eq!(d.seek(SeekFrom::Start(at)).expect("seek"), at);
            check(&mut d, at, n);
        }

        // Relative to the current position, backwards into a decoded frame.
        d.seek(SeekFrom::Start(F as u64 + 100)).expect("seek");
        check(&mut d, F as u64 + 100, 50);
        let at = d.seek(SeekFrom::Current(-120)).expect("seek current");
        assert_eq!(at, F as u64 + 30);
        check(&mut d, at, 300);

        // Relative to the end.
        let at = d.seek(SeekFrom::End(-10)).expect("seek end");
        assert_eq!(at, len64 - 10);
        check(&mut d, at, 100);

        // Past the end: allowed, and reads nothing.
        assert_eq!(d.seek(SeekFrom::End(5)).expect("seek past end"), len64 + 5);
        assert_eq!(d.read(&mut [0u8; 8]).expect("read past end"), 0);
        // Before the start: an error.
        assert!(d.seek(SeekFrom::Current(-(len as i64) - 100)).is_err());
    }

    /// A flipped byte in a frame's checksum fails both read paths.
    #[test]
    fn corrupt_zstd_frame_is_a_read_error() {
        let data = sample(2 * F, 7);
        let mut stored = encode(&data);
        let first_frame =
            u32_at(&stored, stored.len() - FOOTER_SIZE - 2 * ENTRY_SIZE).expect("entry") as usize;
        stored[first_frame - 1] ^= 0xff; // last byte of frame 0: its checksum

        assert!(stream(stored.clone()).is_err(), "stream decoded corruption");
        let mut out = Vec::new();
        let res = seekable(stored).expect("open").read_to_end(&mut out);
        assert!(res.is_err(), "seekable decoded corruption");
    }

    /// A blob cut anywhere at its end is an error, never short data: inside
    /// the seek table, exactly at the last frame's end (the table gone whole,
    /// which a multi-frame stream would otherwise decode cleanly), and inside
    /// a frame.
    #[test]
    fn truncated_zstd_blob_is_an_error() {
        let data = sample(2 * F + 99, 11);
        let stored = encode(&data);
        let table = SKIPPABLE_HEADER_SIZE + 3 * ENTRY_SIZE + FOOTER_SIZE;
        for cut in [1, 5, table, table + 1, table + 1000] {
            let short = stored[..stored.len() - cut].to_vec();
            assert!(seekable(short.clone()).is_err(), "seekable open, cut {cut}");
            assert!(stream(short).is_err(), "stream, cut {cut}");
        }
    }

    /// The case the seekable format exists for: a compressed artifact on the
    /// sqlite + spill backend has a seekable reader, whether it stayed inline
    /// or spilled to a file, and the header-only index agrees with the walk.
    /// Each entry then reads back through a fresh reader at its data offset,
    /// as a FUSE layer reads it.
    #[test]
    fn a_compressed_artifact_has_a_seekable_reader() {
        use crate::engine::local_cache::CacheArtifact;
        use crate::engine::local_cache_spill::LocalCacheSpill;
        use crate::engine::local_cache_sqlite::{DEFAULT_MAX_CONCURRENT_PIPES, LocalCacheSQLite};
        use hcore::hartifactcontent::Content;
        use hcore::hartifactcontent::tar_index::TarIndex;
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let sqlite = Arc::new(
            LocalCacheSQLite::with_pipe_limit(
                dir.path().join("cache.db"),
                16 * 1024,
                DEFAULT_MAX_CONCURRENT_PIPES,
            )
            .expect("sqlite"),
        );
        let blobs = Arc::new(LocalCacheFS::new(dir.path().join("blobs")).expect("fs"));
        let cache: Arc<dyn LocalCache> =
            Arc::new(LocalCacheSpill::new(sqlite, Arc::clone(&blobs), 64 * 1024));
        let a = addr();

        // Members around and across frame boundaries.
        let files: Vec<(String, Vec<u8>)> = [10, 3 * F + 5, 0, F / 2, 1, 2 * F + 7]
            .iter()
            .enumerate()
            .map(|(i, &n)| (format!("d{}/f{i}.bin", i % 2), sample(n, i as u64 + 1)))
            .collect();
        let tar_of = |files: &[(String, Vec<u8>)]| {
            let mut b = tar::Builder::new(Vec::new());
            for (path, body) in files {
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, path, body.as_slice())
                    .expect("append");
            }
            b.into_inner().expect("tar")
        };

        for (name, files) in [("small.tar.zst", &files[..1]), ("big.tar.zst", &files[..])] {
            let tar = tar_of(files);
            let mut w = open_writer(
                cache.as_ref(),
                &a,
                "h",
                name,
                &ManifestArtifactEncoding::Zstd,
            )
            .expect("writer");
            w.write_all(&tar).expect("write");
            w.commit().expect("commit");
            let art = CacheArtifact {
                addr: a.clone(),
                hashin: "h".to_string(),
                name: name.to_string(),
                cache: Arc::clone(&cache),
                encoding: ManifestArtifactEncoding::Zstd,
                content_type: hcore::hartifactcontent::Type::Tar,
                hashout: "ho".to_string(),
                group: String::new(),
                r#type: crate::engine::local_cache::ManifestArtifactType::Output,
                size: tar.len() as u64,
            };
            let spilled = blobs.exists(&a, "h", name).expect("exists");
            assert_eq!(spilled, name == "big.tar.zst", "{name} spill placement");
            assert!(art.file_path().is_none(), "{name}: no plain file to offer");

            let index = TarIndex::build(
                art.seekable_reader()
                    .expect("seekable")
                    .expect("some reader"),
            )
            .expect("index");
            // The index is path-ordered, the walk is archive-ordered.
            let mut walked: Vec<_> = art
                .walk()
                .expect("walk")
                .map(|e| e.expect("entry").path)
                .collect();
            walked.sort();
            assert_eq!(art.entry_paths().expect("entry paths"), walked, "{name}");
            assert_eq!(index.entry_paths(), walked, "{name}");

            for (path, body) in files {
                let entry = &index.entries[std::path::Path::new(path)];
                let mut r = art.seekable_reader().expect("seekable").expect("reader");
                r.seek(SeekFrom::Start(entry.data_offset)).expect("seek");
                let mut got = vec![0u8; entry.size as usize];
                r.read_exact(&mut got).expect("read member");
                assert!(&got == body, "{name}: {path} bytes");
            }
        }
    }

    #[test]
    fn entry_name_suffixes_every_blob() {
        assert_eq!(
            entry_name("out_a.tar", &ManifestArtifactEncoding::Zstd),
            "out_a.tar.zst"
        );
        assert_eq!(
            entry_name("out_a.tar", &ManifestArtifactEncoding::None),
            "out_a.tar.raw"
        );
    }

    /// The names a v1 manifest (bare bases) and a v2 manifest (suffixed, in
    /// either encoding) give one target's artifacts never meet, and no two v2
    /// names meet each other — for every base set `suffix_collision` lets
    /// through, including bases that already end in a suffix.
    #[test]
    fn v1_and_v2_entry_names_are_disjoint() {
        use std::collections::HashSet;
        let pool = [
            "out_a.tar",
            "out_a",
            "out_a.zst",
            "out_a.raw",
            "out_a.raw.zst",
            "out_a.zst.raw",
            "log_a.tar",
            "out_.raw",
            "out_",
        ];
        let mut admitted = 0;
        // Every subset of the pool, as one target's bases.
        for mask in 0u32..(1 << pool.len()) {
            let bases: HashSet<String> = pool
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, b)| (*b).to_string())
                .collect();
            if suffix_collision(&bases).is_some() {
                continue;
            }
            admitted += 1;
            let mut v2 = HashSet::new();
            for base in &bases {
                for enc in [
                    ManifestArtifactEncoding::None,
                    ManifestArtifactEncoding::Zstd,
                ] {
                    let name = entry_name(base, &enc);
                    assert!(
                        !bases.contains(&name),
                        "{name} is also a v1 name: {bases:?}"
                    );
                    // One per (base, encoding): the same base in another
                    // encoding is a different name too.
                    assert!(v2.insert(name.clone()), "{name} named twice: {bases:?}");
                }
            }
        }
        assert!(admitted > 1, "the pool exercised nothing");
    }

    #[test]
    fn a_base_and_its_suffixed_twin_are_refused() {
        use std::collections::HashSet;
        for suffix in V2_SUFFIXES {
            let bases: HashSet<String> = ["out_a".to_string(), format!("out_a{suffix}")]
                .into_iter()
                .collect();
            let (base, suffixed) = suffix_collision(&bases).expect("collision");
            assert_eq!(
                (base, suffixed.as_str()),
                ("out_a", format!("out_a{suffix}").as_str())
            );
        }
        let fine: HashSet<String> = ["out_a.tar".to_string(), "out_b.tar.zst".to_string()]
            .into_iter()
            .collect();
        assert!(suffix_collision(&fine).is_none());
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
