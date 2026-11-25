//! The gzip encoder for layer blobs.
//!
//! A compressed layer's bytes are its digest, so they must be a function of the
//! tar and of nothing else. Two things would break that, and this module
//! exists to close both:
//!
//! - **The deflate backend.** `flate2` picks its backend by cargo feature, and
//!   features unify across whatever is built together. This workspace's lock
//!   already carries both `miniz_oxide` and `zlib-rs` under `flate2`, so the
//!   same source could produce different bytes depending on whether this
//!   cdylib was built alone or alongside the host. `miniz_oxide` is called
//!   directly here. Its output depends only on its version (its one optional
//!   feature, `simd`, touches adler32, which a raw deflate stream never
//!   computes), and `layer.rs`'s pinned-digest test fails if a version bump
//!   moves it.
//! - **The gzip header.** It has room for an mtime, a file name and the
//!   writing OS. The header here is fixed: mtime 0, no name, OS 255
//!   ("unknown"), as `gzip -n` and every reproducible-build tool write it.

use miniz_oxide::deflate::core::{
    CompressorOxide, TDEFLFlush, TDEFLStatus, compress, create_comp_flags_from_zip_params,
};
use std::io::{self, Write};

/// zlib's default, and what `docker push` and buildx use: most of the size
/// win for a fraction of level 9's time.
const LEVEL: i32 = 6;

/// Magic, CM = deflate, FLG = none, MTIME = 0, XFL = 0, OS = unknown.
const HEADER: [u8; 10] = [0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];

/// The output chunk size. Every chunk is handed straight to the inner writer.
const CHUNK: usize = 64 * 1024;

/// A streaming gzip writer. Call [`finish`](Self::finish): dropping it leaves
/// a truncated stream with no trailer.
pub(crate) struct GzipWriter<W: Write> {
    inner: W,
    deflate: Box<CompressorOxide>,
    crc: crc32fast::Hasher,
    /// Uncompressed length modulo 2^32, which is what the trailer records.
    isize: u32,
    out: Box<[u8]>,
}

impl<W: Write> GzipWriter<W> {
    pub(crate) fn new(mut inner: W) -> io::Result<Self> {
        inner.write_all(&HEADER)?;
        // Negative window bits: a raw deflate stream, framed by this module
        // rather than by zlib's wrapper.
        let flags = create_comp_flags_from_zip_params(LEVEL, -15, 0);
        Ok(Self {
            inner,
            deflate: Box::new(CompressorOxide::new(flags)),
            crc: crc32fast::Hasher::new(),
            isize: 0,
            out: vec![0; CHUNK].into_boxed_slice(),
        })
    }

    /// Feed `input` through the compressor, writing out everything it emits.
    /// Returns once the input is consumed or, under `Finish`, once the stream
    /// is closed.
    fn pump(&mut self, mut input: &[u8], flush: TDEFLFlush) -> io::Result<()> {
        loop {
            let (status, consumed, produced) =
                compress(&mut self.deflate, input, &mut self.out, flush);
            let chunk = self.out.get(..produced).ok_or_else(|| {
                io::Error::other("deflate reported more output than it had room for")
            })?;
            self.inner.write_all(chunk)?;
            input = input
                .get(consumed..)
                .ok_or_else(|| io::Error::other("deflate consumed more input than it was given"))?;
            match status {
                TDEFLStatus::Done => return Ok(()),
                TDEFLStatus::Okay if flush == TDEFLFlush::None && input.is_empty() => {
                    return Ok(());
                }
                TDEFLStatus::Okay => {}
                other => return Err(io::Error::other(format!("deflate failed: {other:?}"))),
            }
        }
    }

    /// Close the deflate stream, write the trailer, and hand back the writer.
    pub(crate) fn finish(mut self) -> io::Result<W> {
        self.pump(&[], TDEFLFlush::Finish)?;
        let crc = std::mem::take(&mut self.crc).finalize();
        self.inner.write_all(&crc.to_le_bytes())?;
        self.inner.write_all(&self.isize.to_le_bytes())?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for GzipWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.crc.update(buf);
        self.isize = self.isize.wrapping_add(buf.len() as u32);
        self.pump(buf, TDEFLFlush::None)?;
        Ok(buf.len())
    }

    /// Flushes the inner writer only. A deflate sync flush would put a marker
    /// into the stream, and so make the bytes depend on when the caller flushed.
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    fn gzip(chunks: &[&[u8]]) -> Vec<u8> {
        let mut w = GzipWriter::new(Vec::new()).expect("new");
        for c in chunks {
            w.write_all(c).expect("write");
        }
        w.finish().expect("finish")
    }

    /// A stream any gzip reader accepts, holding exactly what went in: empty,
    /// small, and larger than one output chunk of incompressible bytes.
    #[test]
    fn round_trips_through_a_standard_decoder() {
        let mut big = Vec::with_capacity(3 * CHUNK);
        let mut x: u32 = 0x9e37_79b9;
        while big.len() < 3 * CHUNK {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            big.extend_from_slice(&x.to_le_bytes());
        }
        for input in [&b""[..], b"hello\n", &big] {
            let gz = gzip(&[input]);
            let mut back = Vec::new();
            flate2::read::GzDecoder::new(gz.as_slice())
                .read_to_end(&mut back)
                .expect("decode");
            assert_eq!(back, input, "{} bytes in", input.len());
        }
    }

    /// The bytes may not depend on how the writer was fed: `tar::Builder`
    /// writes in header- and body-sized pieces, and a `BufWriter` in front of
    /// it would change those.
    #[test]
    fn the_bytes_do_not_depend_on_the_write_sizes() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let whole = gzip(&[&data]);
        let pieces: Vec<&[u8]> = data.chunks(512).collect();
        assert_eq!(whole, gzip(&pieces));
    }

    /// The header carries nothing about the host or the clock.
    #[test]
    fn the_header_is_fixed() {
        assert_eq!(gzip(&[b"x"]).get(..10), Some(&HEADER[..]));
    }
}
