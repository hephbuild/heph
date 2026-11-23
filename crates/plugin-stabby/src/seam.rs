//! Small helpers shared by both sides of the spawn-at-the-seam wrappers
//! (host adapters here; guest wrappers in `plugin-sdk::serve`). Always
//! compiled — no transport or host deps — so the guest SDK can use them with
//! `default-features = false`.

use stabby::vec::Vec as SVec;

/// Encode `m` straight into the `SVec` that crosses the seam.
///
/// The bytes are exactly `m.encode_to_vec()`'s — this runs the same two
/// passes (`compute_size`, then `write_to` into a buffer of that capacity) —
/// but into the seam's own vector, so a message is not encoded into a `Vec`
/// and then copied into a second allocation.
///
/// Panics, as `encode_to_vec` does, on a message over protobuf's 2 GiB limit.
#[expect(
    clippy::panic,
    reason = "same contract as buffa's panicking `encode_to_vec`"
)]
pub fn encode_svec<M: buffa::Message>(m: &M) -> SVec<u8> {
    let mut cache = buffa::SizeCache::new();
    let Ok(size) = buffa::checked_encode_size(m.compute_size(&mut cache)) else {
        panic!("seam message encoded size exceeds the 2 GiB protobuf limit");
    };
    let size = size as usize;
    let mut buf = SVec::with_capacity(size);
    m.write_to(&mut cache, &mut SVecSink(&mut buf));
    debug_assert_eq!(
        buf.len(),
        size,
        "write_to produced a different byte count than compute_size declared"
    );
    buf
}

/// A [`buffa::EncodeSink`] appending to an `SVec` (which is not a `BufMut`).
struct SVecSink<'a>(&'a mut SVec<u8>);

impl buffa::EncodeSink for SVecSink<'_> {
    #[inline]
    fn put_u8(&mut self, value: u8) {
        self.0.push(value);
    }

    #[inline]
    fn put_slice(&mut self, src: &[u8]) {
        self.0.copy_extend(src);
    }

    #[inline]
    fn put_u32_le(&mut self, value: u32) {
        self.0.copy_extend(&value.to_le_bytes());
    }

    #[inline]
    fn put_u64_le(&mut self, value: u64) {
        self.0.copy_extend(&value.to_le_bytes());
    }
}

/// Best-effort text of a panic payload (`panic!` `&str`/`String`, else a stub).
///
/// Used when mapping a seam task's `JoinError::is_panic` payload into an error
/// body — the wrapper futures must never `resume_unwind` (their polls run
/// inside `extern "C"` shims, where an unwind aborts the process).
pub fn panic_text(p: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = p.downcast_ref::<&'static str>() {
        s
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string panic payload>"
    }
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::encode_svec;
    use buffa::Message;
    use plugin_abi::pb;
    use plugin_abi::pb::frame::Body;

    fn same_bytes<M: Message>(m: &M) {
        assert_eq!(encode_svec(m).as_slice(), m.encode_to_vec().as_slice());
    }

    #[test]
    fn encode_svec_matches_encode_to_vec() {
        same_bytes(&pb::Frame::default());
        // Nested messages (sizes recorded in, then consumed from, the cache),
        // a bytes field and a string: every sink method a frame reaches.
        same_bytes(&pb::Frame {
            id: Some(u64::MAX),
            body: Some(Body::from(pb::StreamItem {
                item: Some([0, 1, 2, 255].repeat(300)),
                ..Default::default()
            })),
            ..Default::default()
        });
        same_bytes(&pb::Frame {
            id: Some(0),
            body: Some(Body::from(pb::Error {
                kind: Some(pb::error::Kind::Other.into()),
                message: Some("é — a non-ASCII message".to_owned()),
                ..Default::default()
            })),
            ..Default::default()
        });
    }
}
