//! Deciding whether an artifact's bytes are worth putting through a compressor.
//!
//! Every cache artifact is a tar, so the artifact blob's own first bytes are
//! always a tar header — sniffing them answers "it's a tar" and nothing else.
//! The signal lives one level in, in the tar's *members*: an OCI image layer, a
//! `.tar.gz`, a jar, a PNG. Compressing those a second time is not a wash, it is
//! a loss — it burns CPU on the upload and again on every download, and a
//! deflate pass over incompressible input reliably makes it slightly larger.
//!
//! The scan is deliberately cheap enough to run on the byte-moving path.
//! `tar::entries_with_seek` reads the 512-byte header of each member and seeks
//! past its data, so a 500 MB image tar with a handful of members costs a few
//! seeks and well under a kilobyte read — no trial compression, no decode.
//! Weighing members by size is also what makes it robust to layout: a
//! `docker save` tar that opens with `manifest.json` and a config blob before
//! the layers is still judged on where its bytes actually are.
//!
//! This is a *hint about the bytes*, consulted by transports that compress. It
//! never feeds a hash: the same artifact compressed or not is the same artifact,
//! so the verdict must not move a cache key.

use anyhow::Context;
use std::io::{Read, Seek};

/// A magic number identifying a format whose payload is already compressed.
struct Signature {
    /// Byte offset of `magic` within the member.
    offset: usize,
    magic: &'static [u8],
    /// Short name, for the "why did it skip compression?" log line.
    label: &'static str,
}

/// Table constructor. A call expression keeps each entry on one line under
/// rustfmt, which a struct literal does not — and the table is only readable as
/// a table.
const fn sig(offset: usize, magic: &'static [u8], label: &'static str) -> Signature {
    Signature {
        offset,
        magic,
        label,
    }
}

/// Formats that actually turn up in a build cache and that a second compressor
/// pass cannot help. Deliberately a short, conservative list: a format missing
/// here costs a wasted gzip (the status quo), whereas a wrong entry costs a
/// missed compression on every upload of that artifact forever.
///
/// Not listed, on purpose: ELF/Mach-O binaries, wasm, and PDF — all of which
/// compress perfectly well despite often being called "binary".
const SIGNATURES: &[Signature] = &[
    // Stream codecs. Any `.tar.gz`/`.tgz` member and every OCI layer blob.
    sig(0, b"\x1f\x8b", "gzip"),
    sig(0, b"\x28\xb5\x2f\xfd", "zstd"),
    sig(0, b"\xfd7zXZ\x00", "xz"),
    sig(0, b"BZh", "bzip2"),
    sig(0, b"\x04\x22\x4d\x18", "lz4"),
    // Archive containers that deflate their members. jar, whl, apk, egg, nupkg.
    sig(0, b"PK\x03\x04", "zip"),
    sig(0, b"7z\xbc\xaf\x27\x1c", "7z"),
    sig(0, b"Rar!\x1a\x07", "rar"),
    // Media. Common as test fixtures, web assets, and embedded resources.
    sig(0, b"\x89PNG\r\n\x1a\n", "png"),
    sig(0, b"\xff\xd8\xff", "jpeg"),
    sig(0, b"GIF8", "gif"),
    sig(0, b"OggS", "ogg"),
    sig(0, b"fLaC", "flac"),
    // `RIFF....WEBP` and `....ftyp` (mp4/mov/heif) — the discriminating bytes
    // sit past the start, which is why the peek window is 12 bytes.
    sig(8, b"WEBP", "webp"),
    sig(4, b"ftyp", "mp4"),
    // Web fonts: both are compressed wrappers around sfnt.
    sig(0, b"wOFF", "woff"),
    sig(0, b"wOF2", "woff2"),
];

/// Bytes read from the head of each sniffed member. Covers the furthest
/// signature (`WEBP` at offset 8).
const PEEK_LEN: usize = 12;

/// A tar block. Every member carries at least one as its header, and that
/// framing is payload no compressor has to work for.
const BLOCK_BYTES: u64 = 512;

/// Members below this are not sniffed at all.
///
/// A blob is called precompressed only when *most* of it is (see
/// [`PRECOMPRESSED_PERCENT`]), so members too small to move that ratio cannot
/// change the verdict — and skipping them bounds the read count on a tar of many
/// tiny files. Skipping can only under-count incompressible bytes, i.e. it errs
/// toward compressing, which is the safe direction.
const MIN_MEMBER_BYTES: u64 = 4 * 1024;

/// Percentage of a blob that must be already-compressed payload before the blob
/// as a whole is treated as not worth compressing.
///
/// Measured against the *whole blob*, not the sum of member sizes, so tar
/// headers and padding count against the verdict: an archive that is mostly
/// framing (thousands of tiny files) can never reach this bar, which is right —
/// that framing compresses away to nothing.
///
/// Integer percent rather than a float fraction so both this comparison and the
/// early-exit bound derived from it are exact.
const PRECOMPRESSED_PERCENT: u64 = 90;

/// What a scan concluded about a tar blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Whether the blob is not worth compressing.
    pub precompressed: bool,
    /// Total size of members whose magic identifies an already-compressed
    /// format. A *lower bound* rather than a census: the scan stops as soon as
    /// the answer is settled (see [`scan_tar`]), so a blob that is not
    /// precompressed reports only what it saw before giving up.
    pub incompressible_bytes: u64,
    /// The format holding the most of those bytes — the human-readable half of
    /// "why was this stored verbatim?". `None` when nothing matched.
    pub dominant: Option<&'static str>,
}

/// Match `head` against the signature table. A `head` too short for a given
/// signature's window simply does not match it.
fn match_signature(head: &[u8]) -> Option<&'static str> {
    SIGNATURES
        .iter()
        .find(|s| {
            head.get(s.offset..s.offset + s.magic.len())
                .is_some_and(|window| window == s.magic)
        })
        .map(|s| s.label)
}

/// Read up to `buf.len()` bytes, tolerating short reads. A member shorter than
/// the peek window fills only its prefix; the rest stays zeroed and simply fails
/// to match, which is the correct answer for a member that small anyway.
fn peek(mut r: impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while let Some(rest) = buf.get_mut(n..).filter(|r| !r.is_empty()) {
        match r.read(rest) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Scan a seekable tar of `blob_size` bytes and decide whether it is worth
/// compressing.
///
/// Header-only: each member's first [`PEEK_LEN`] bytes are read and the rest is
/// seeked past, so cost scales with the member *count*, not the archive size —
/// roughly 2µs per member.
///
/// **Stops as soon as the answer is settled.** Every byte that is not identified
/// as compressed payload — a member that failed to match, one below
/// [`MIN_MEMBER_BYTES`], and all the tar framing — is counted against the
/// verdict, and once that total passes `100 - PRECOMPRESSED_PERCENT` of the blob
/// the bar is unreachable and the walk gives up. This is an exact bound, not a
/// heuristic: those bytes are definitively in the blob and definitively not
/// known-compressed.
///
/// It matters because the pathological input for a per-member walk — an archive
/// of tens of thousands of small files — is also one that can never be judged
/// precompressed. Without the early exit that case pays the full walk to learn
/// nothing (~6% on top of its compression); with it, the walk ends a few percent
/// in. The cases that *do* pay off are unaffected: one big matching member
/// settles the answer on the first iteration.
pub fn scan_tar<R: Read + Seek>(reader: R, blob_size: u64) -> anyhow::Result<Verdict> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries_with_seek()
        .context("tar archive entries_with_seek")?;

    // Bytes that cannot count toward the verdict. Seeded with the framing the
    // archive must contain: the two 512-byte end-of-archive blocks, plus one
    // header block per member as we go.
    let mut rejected_bytes = 0u64;
    let reject_budget = blob_size / 100 * (100 - PRECOMPRESSED_PERCENT);

    let mut incompressible_bytes = 0u64;
    // Bounded by SIGNATURES.len(); a Vec of pairs beats a map at this size.
    let mut by_label: Vec<(&'static str, u64)> = Vec::new();

    for entry in entries {
        let mut entry = entry.context("read tar entry header")?;
        let header = entry.header();
        let size = header.size().context("read tar entry size")?;
        // The member's own header block is framing, never payload.
        rejected_bytes = rejected_bytes.saturating_add(BLOCK_BYTES);

        let matched = if header.entry_type().is_file() && size >= MIN_MEMBER_BYTES {
            let mut head = [0u8; PEEK_LEN];
            // Reading from the entry (rather than seeking by
            // `raw_file_position`) keeps the read clamped to this member and
            // lets the iterator do the seek to the next header itself.
            let n = peek(&mut entry, &mut head).context("read tar entry head")?;
            head.get(..n).and_then(match_signature)
        } else {
            None
        };

        match matched {
            Some(label) => {
                incompressible_bytes = incompressible_bytes.saturating_add(size);
                match by_label.iter_mut().find(|(l, _)| *l == label) {
                    Some((_, bytes)) => *bytes = bytes.saturating_add(size),
                    None => by_label.push((label, size)),
                }
            }
            None => {
                rejected_bytes = rejected_bytes.saturating_add(size);
                if rejected_bytes > reject_budget {
                    // Unreachable bar: stop walking. `incompressible_bytes` is
                    // left partial, which the `precompressed: false` verdict
                    // already tells the caller.
                    break;
                }
            }
        }
    }

    let dominant = by_label
        .iter()
        .max_by_key(|(_, bytes)| *bytes)
        .map(|(label, _)| *label);

    Ok(Verdict {
        precompressed: blob_size > 0
            && incompressible_bytes.saturating_mul(100)
                >= blob_size.saturating_mul(PRECOMPRESSED_PERCENT),
        incompressible_bytes,
        dominant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build a tar in memory from `(path, contents)` pairs.
    fn tar_of(members: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, data) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, data.as_slice())
                .expect("append");
        }
        builder.into_inner().expect("finish tar")
    }

    /// `len` bytes opening with `magic`. The tail is incompressible-ish filler,
    /// but nothing in the scan looks at it — only the size and the magic.
    fn blob(magic: &[u8], len: usize) -> Vec<u8> {
        let mut v = magic.to_vec();
        v.resize(len, 0xA5);
        v
    }

    fn scan(bytes: &[u8]) -> Verdict {
        scan_tar(Cursor::new(bytes.to_vec()), bytes.len() as u64).expect("scan")
    }

    #[test]
    fn plain_text_members_are_worth_compressing() {
        let tar = tar_of(&[
            ("a.txt", vec![b'a'; 64 * 1024]),
            ("b.txt", vec![b'b'; 64 * 1024]),
        ]);
        let v = scan(&tar);
        assert_eq!(v.incompressible_bytes, 0);
        assert_eq!(v.dominant, None);
        assert!(!v.precompressed);
    }

    #[test]
    fn gzip_members_dominate_the_verdict() {
        let tar = tar_of(&[("layer.tar.gz", blob(b"\x1f\x8b", 512 * 1024))]);
        let v = scan(&tar);
        assert_eq!(v.incompressible_bytes, 512 * 1024);
        assert_eq!(v.dominant, Some("gzip"));
        assert!(v.precompressed);
    }

    /// The `docker save` layout: small JSON first, layer blobs after. A scan
    /// that only looked at the head of the blob would call this compressible.
    #[test]
    fn json_before_the_layers_does_not_fool_the_scan() {
        let tar = tar_of(&[
            ("manifest.json", vec![b'{'; 8 * 1024]),
            ("config.json", vec![b'{'; 16 * 1024]),
            ("layer0/layer.tar.gz", blob(b"\x1f\x8b", 2 * 1024 * 1024)),
            ("layer1/layer.tar.gz", blob(b"\x1f\x8b", 2 * 1024 * 1024)),
        ]);
        let v = scan(&tar);
        assert_eq!(v.incompressible_bytes, 4 * 1024 * 1024);
        assert_eq!(v.dominant, Some("gzip"));
        assert!(v.precompressed);
    }

    /// The converse: a couple of images inside an otherwise text artifact must
    /// not suppress compression of the whole thing.
    #[test]
    fn a_minority_of_compressed_members_still_compresses() {
        let tar = tar_of(&[
            ("icon.png", blob(b"\x89PNG\r\n\x1a\n", 64 * 1024)),
            ("src.txt", vec![b's'; 4 * 1024 * 1024]),
        ]);
        let v = scan(&tar);
        assert_eq!(v.incompressible_bytes, 64 * 1024);
        assert_eq!(v.dominant, Some("png"));
        assert!(!v.precompressed);
    }

    /// Framing counts against the verdict: measuring against the sum of member
    /// sizes rather than the blob would call this precompressed, but the tar is
    /// mostly zeroed headers and padding, which compress away.
    #[test]
    fn an_archive_that_is_mostly_framing_is_worth_compressing() {
        let members: Vec<(String, Vec<u8>)> = (0..64)
            .map(|i| (format!("f{i}.gz"), blob(b"\x1f\x8b", 8 * 1024)))
            .collect();
        let refs: Vec<(&str, Vec<u8>)> = members
            .iter()
            .map(|(p, d)| (p.as_str(), d.clone()))
            .collect();
        let mut tar = tar_of(&refs);
        // Pad the archive out so member bytes are a minority of the blob, the
        // shape a tar of very many small files has.
        tar.resize(tar.len() * 3, 0);
        let v = scan(&tar);
        assert_eq!(v.incompressible_bytes, 64 * 8 * 1024);
        assert!(!v.precompressed);
    }

    #[test]
    fn members_below_the_floor_are_skipped() {
        let tar = tar_of(&[("tiny.gz", blob(b"\x1f\x8b", 1024))]);
        let v = scan(&tar);
        assert_eq!(v.incompressible_bytes, 0);
        assert_eq!(v.dominant, None);
    }

    #[test]
    fn offset_signatures_match() {
        let mut webp = b"RIFF\0\0\0\0WEBP".to_vec();
        webp.resize(64 * 1024, 0xA5);
        let mut mp4 = b"\0\0\0\x20ftypisom".to_vec();
        mp4.resize(64 * 1024, 0xA5);
        let tar = tar_of(&[("a.webp", webp), ("b.mp4", mp4)]);
        let v = scan(&tar);
        assert_eq!(v.incompressible_bytes, 128 * 1024);
    }

    #[test]
    fn every_signature_is_recognized() {
        for sig in SIGNATURES {
            let mut data = vec![0u8; sig.offset];
            data.extend_from_slice(sig.magic);
            data.resize(MIN_MEMBER_BYTES as usize * 2, 0xA5);
            let tar = tar_of(&[("m.bin", data)]);
            let v = scan(&tar);
            assert_eq!(
                v.dominant,
                Some(sig.label),
                "signature {} did not round-trip through a tar scan",
                sig.label
            );
        }
    }

    /// Binaries are the single most common large artifact in a build cache and
    /// they compress well — a signature table that caught them would be a
    /// serious regression.
    #[test]
    fn executables_are_not_treated_as_compressed() {
        let elf = blob(b"\x7fELF\x02\x01\x01\x00", 1024 * 1024);
        let macho = blob(b"\xcf\xfa\xed\xfe\x0c\x00\x00\x01", 1024 * 1024);
        let wasm = blob(b"\x00asm\x01\x00\x00\x00", 1024 * 1024);
        let tar = tar_of(&[("bin", elf), ("dylib", macho), ("mod.wasm", wasm)]);
        let v = scan(&tar);
        assert_eq!(v.incompressible_bytes, 0);
        assert!(!v.precompressed);
    }

    /// Counts `read` calls so a test can see how far the walk got.
    struct CountingReader {
        inner: Cursor<Vec<u8>>,
        reads: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl Read for CountingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reads.set(self.reads.get() + 1);
            self.inner.read(buf)
        }
    }

    impl Seek for CountingReader {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    /// The cost guard. An archive of many small members can never clear the bar,
    /// and walking all of it would be pure loss — the scan must give up once
    /// enough rejected bytes have accumulated, not walk to the end.
    #[test]
    fn the_scan_gives_up_once_the_bar_is_unreachable() {
        let members: Vec<(String, Vec<u8>)> = (0..2000)
            .map(|i| (format!("f{i}.txt"), vec![b'x'; 32 * 1024]))
            .collect();
        let refs: Vec<(&str, Vec<u8>)> = members
            .iter()
            .map(|(p, d)| (p.as_str(), d.clone()))
            .collect();
        let tar = tar_of(&refs);

        let reads = std::rc::Rc::new(std::cell::Cell::new(0));
        let v = scan_tar(
            CountingReader {
                inner: Cursor::new(tar.clone()),
                reads: std::rc::Rc::clone(&reads),
            },
            tar.len() as u64,
        )
        .expect("scan");

        assert!(!v.precompressed);
        // ~10% of the blob is all it takes to settle this; the walk must not
        // have touched anything close to all 2000 members.
        assert!(
            reads.get() < 2000 / 4,
            "scan read {} times for a 2000-member archive; the early exit is not firing",
            reads.get()
        );
    }

    /// The converse: the early exit must not fire on the case it exists to
    /// serve. One big matching member settles the answer immediately.
    #[test]
    fn a_precompressed_archive_is_still_recognized_with_the_early_exit() {
        let tar = tar_of(&[
            ("layer0.tar.gz", blob(b"\x1f\x8b", 4 * 1024 * 1024)),
            ("small.txt", vec![b'x'; 8 * 1024]),
            ("layer1.tar.gz", blob(b"\x1f\x8b", 4 * 1024 * 1024)),
        ]);
        let v = scan(&tar);
        assert!(v.precompressed);
        assert_eq!(v.incompressible_bytes, 8 * 1024 * 1024);
    }

    #[test]
    fn an_empty_archive_is_not_precompressed() {
        let tar = tar_of(&[]);
        let v = scan(&tar);
        assert_eq!(v.incompressible_bytes, 0);
        assert!(!v.precompressed);
        assert!(
            !scan_tar(Cursor::new(tar_of(&[])), 0)
                .expect("scan")
                .precompressed
        );
    }
}
