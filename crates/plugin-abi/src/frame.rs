//! Length-prefixed `Frame` framing over any byte stream.
//!
//! Wire format: a 4-byte little-endian length prefix followed by the
//! prost-encoded [`crate::pb::Frame`]. Used by the proto transport directly;
//! the shm/wasm transports carry the same `Frame` bodies by other means.

use crate::pb;
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Max accepted frame size (16 MiB). Bulk artifact bytes stream as chunks, so
/// a single frame should never approach this; it guards against a corrupt
/// length prefix allocating unbounded memory.
const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

/// Append one length-prefixed frame to `buf` (no I/O). Lets the writer coalesce
/// many frames into a single `write` syscall — the dominant transport cost under
/// the high callback fan-out is per-frame syscalls, not bytes.
pub fn encode_frame_into(buf: &mut Vec<u8>, f: &pb::Frame) -> anyhow::Result<()> {
    let len = u32::try_from(f.encoded_len()).map_err(|_e| anyhow::anyhow!("frame too large"))?;
    buf.extend_from_slice(&len.to_le_bytes());
    f.encode(buf)?;
    Ok(())
}

/// Encode and write one frame (length prefix + payload) in a single write, then
/// flush. (Bulk paths use [`encode_frame_into`] + one write for many frames.)
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, f: &pb::Frame) -> anyhow::Result<()> {
    let mut buf = Vec::with_capacity(f.encoded_len() + 4);
    encode_frame_into(&mut buf, f)?;
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

/// Read one frame. Returns `Ok(None)` on a clean EOF at a frame boundary
/// (peer closed the connection), `Err` on a partial/corrupt frame.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> anyhow::Result<Option<pb::Frame>> {
    let len = match r.read_u32_le().await {
        Ok(len) => len,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if len > MAX_FRAME_LEN {
        anyhow::bail!("frame length {len} exceeds max {MAX_FRAME_LEN}");
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    let frame = pb::Frame::decode(&buf[..])?;
    Ok(Some(frame))
}
