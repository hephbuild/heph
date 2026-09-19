//! Scrubbing credential material out of a target's output, at the tee.
//!
//! # Why here and not at render
//!
//! Redacting where output is *displayed* misses everything that matters. The
//! captured log is packed into the cache as an artifact, lifted into the failure
//! event, and printed in the JSON output and the CI report — so by the time a
//! renderer sees it, the token is already on disk and on its way to a shared
//! remote. The tee is the one point every one of those paths passes through, and
//! it is upstream of all of them.
//!
//! # Why it holds bytes back
//!
//! A read boundary lands wherever the kernel put it, so a token can arrive split
//! across two chunks with nothing wrong with either half. Scanning per chunk would
//! let exactly that one through, and it would do so *intermittently*, which is
//! the worst possible failure shape for a secret. So the last `len(longest
//! secret) - 1` bytes of every chunk are held back and rescanned with the next
//! one.
//!
//! # What it does not do
//!
//! Redaction is **best-effort by construction**, and that is documented rather
//! than hidden. It is a substring replacement over a byte stream, so a short
//! secret cannot be scrubbed without corrupting output that merely happens to
//! contain those bytes — see [`REDACT_MIN_LEN`]. It also cannot see a secret the
//! target transformed: base64, a URL-encoding, or a token split across two lines
//! by the tool that printed it all pass through.

use hplugin::driver::REDACT_MIN_LEN;
use std::sync::Arc;

/// What a redacted run of bytes is replaced with.
///
/// Fixed-width and obviously not a value, so a log that has been scrubbed reads
/// as scrubbed rather than as a build that printed something odd.
const MARKER: &[u8] = b"[redacted]";

/// The secrets to scrub, shared by every stream of one run.
///
/// Built once per target and `None` when there is nothing to scrub — which is
/// every target that declares no credentials, so the hot path pays a null check.
pub struct Needles {
    /// Longest first, so a secret that contains another is replaced whole rather
    /// than leaving a scrubbed fragment inside a longer one.
    ///
    /// Each carries a prebuilt substring searcher. The naive
    /// `windows(n).position(…)` this replaced is `O(hay × needle)`, and a needle
    /// here is a whole session token — around a kilobyte — so a target printing a
    /// few megabytes of ordinary build output paid on the order of a thousand
    /// byte-comparisons per output byte, on the tee, in the middle of the run.
    needles: Vec<(Vec<u8>, memchr::memmem::Finder<'static>)>,
}

impl Needles {
    /// `None` when nothing needs scrubbing.
    pub fn new<I, S>(values: I) -> Option<Arc<Self>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut raw: Vec<Vec<u8>> = values
            .into_iter()
            // The floor, applied here as well as at the host so a driver that is
            // handed a short value by an older host still refuses to corrupt
            // ordinary output with it.
            .filter(|v| v.as_ref().len() >= REDACT_MIN_LEN)
            .map(|v| v.as_ref().as_bytes().to_vec())
            .collect();
        if raw.is_empty() {
            return None;
        }
        raw.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        raw.dedup();
        let needles = raw
            .into_iter()
            .map(|n| {
                let finder = memchr::memmem::Finder::new(&n).into_owned();
                (n, finder)
            })
            .collect();
        Some(Arc::new(Self { needles }))
    }
}

/// Per-stream redaction state.
///
/// One per stream, never shared: the held-back tail is a property of *this*
/// byte sequence, and interleaving two streams through one buffer would splice
/// them together.
pub struct Redactor {
    needles: Arc<Needles>,
    /// Bytes seen but not yet emitted.
    buf: Vec<u8>,
}

impl Redactor {
    pub fn new(needles: Arc<Needles>) -> Self {
        Self {
            needles,
            buf: Vec::new(),
        }
    }

    /// Feed a chunk; returns the bytes that are now safe to emit.
    ///
    /// Every returned byte is past every possible secret boundary: replacement
    /// happens *before* the emit split, and the split leaves behind exactly the
    /// tail that could still be the beginning of a secret.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.buf.extend_from_slice(chunk);
        self.scrub();
        let keep = self.partial_tail();
        let emit_len = self.buf.len() - keep;
        let rest = self.buf.split_off(emit_len);
        std::mem::replace(&mut self.buf, rest)
    }

    /// Flush at end of stream. Nothing is held back after this.
    pub fn finish(&mut self) -> Vec<u8> {
        self.scrub();
        std::mem::take(&mut self.buf)
    }

    /// How many trailing bytes could still turn out to be the start of a secret.
    ///
    /// **This is what keeps an interactive session alive.** Holding back a flat
    /// `len(longest secret) - 1` bytes — the obvious implementation — blanks a
    /// `--shell` prompt and makes a streaming `terraform apply` look stalled,
    /// because a session token is around a kilobyte and a prompt is twenty bytes.
    /// Holding back only a genuine partial match means ordinary output keeps
    /// nothing back at all: the answer is `0` unless the chunk really did end
    /// mid-token.
    ///
    /// Cost is bounded by the longest needle and paid only on the tail: for each
    /// needle, the candidate split points are the positions of its first byte in
    /// the last `len(needle) - 1` bytes, which `memchr` finds without scanning
    /// byte by byte.
    fn partial_tail(&self) -> usize {
        let mut keep = 0usize;
        for (needle, _) in &self.needles.needles {
            let window = (needle.len().saturating_sub(1)).min(self.buf.len());
            if window <= keep {
                // Longest-first ordering means no shorter needle can beat a
                // longer partial match already found.
                continue;
            }
            let start = self.buf.len() - window;
            let tail = self.buf.get(start..).unwrap_or_default();
            let Some(first) = needle.first().copied() else {
                continue;
            };
            for pos in memchr::memchr_iter(first, tail) {
                let k = tail.len() - pos;
                if k <= keep {
                    break;
                }
                let candidate = tail.get(pos..).unwrap_or_default();
                if needle.get(..candidate.len()) == Some(candidate) {
                    keep = k;
                    break;
                }
            }
        }
        keep.min(self.buf.len())
    }

    /// Replace every occurrence of every needle in the pending buffer.
    ///
    /// Longest-first, so a secret that is a substring of another does not leave a
    /// `[redacted]` embedded in the longer one — which would both look wrong and
    /// reveal the longer secret's shape.
    fn scrub(&mut self) {
        for (needle, finder) in &self.needles.needles {
            if needle.is_empty() {
                continue;
            }
            let mut from = 0usize;
            while let Some(pos) = finder.find(self.buf.get(from..).unwrap_or_default()) {
                let at = from + pos;
                self.buf
                    .splice(at..at + needle.len(), MARKER.iter().copied());
                from = at + MARKER.len();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn needles(vs: &[&str]) -> Arc<Needles> {
        Needles::new(vs.iter().copied()).expect("some needles")
    }

    fn run(r: &mut Redactor, chunks: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for c in chunks {
            out.extend(r.push(c));
        }
        out.extend(r.finish());
        out
    }

    #[test]
    fn a_secret_in_one_chunk_is_replaced() {
        let mut r = Redactor::new(needles(&["s3cr3t-value"]));
        let out = run(&mut r, &[b"token=s3cr3t-value done\n"]);
        assert_eq!(out, b"token=[redacted] done\n");
    }

    /// The failure this exists to prevent: a read boundary lands wherever the
    /// kernel put it, so per-chunk scanning lets a split secret through — and
    /// does it intermittently.
    #[test]
    fn a_secret_split_across_a_read_boundary_is_still_caught() {
        let mut r = Redactor::new(needles(&["s3cr3t-value"]));
        let out = run(&mut r, &[b"token=s3cr", b"3t-value done\n"]);
        assert_eq!(out, b"token=[redacted] done\n");
    }

    #[test]
    fn a_secret_split_one_byte_at_a_time_is_still_caught() {
        let mut r = Redactor::new(needles(&["s3cr3t-value"]));
        let input = b"a s3cr3t-value b";
        let chunks: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(run(&mut r, &chunks), b"a [redacted] b");
    }

    #[test]
    fn every_occurrence_is_replaced() {
        let mut r = Redactor::new(needles(&["abcdefgh"]));
        assert_eq!(
            run(&mut r, &[b"abcdefgh abcdefgh"]),
            b"[redacted] [redacted]"
        );
    }

    #[test]
    fn ordinary_output_is_untouched_byte_for_byte() {
        let mut r = Redactor::new(needles(&["s3cr3t-value"]));
        let text = b"compiling 42 files in 3.1s\n" as &[u8];
        assert_eq!(run(&mut r, &[text]), text);
    }

    /// A secret that contains another must not end up with a `[redacted]`
    /// embedded in it — that both looks wrong and reveals the longer one's shape.
    #[test]
    fn the_longest_secret_wins() {
        let mut r = Redactor::new(needles(&["abcdefgh", "abcdefghijkl"]));
        assert_eq!(run(&mut r, &[b"x abcdefghijkl y"]), b"x [redacted] y");
    }

    /// The stated limit. A short secret cannot be scrubbed without corrupting
    /// output that merely happens to contain those bytes.
    #[test]
    fn a_secret_shorter_than_the_floor_is_not_scrubbed_and_that_is_documented() {
        assert!(Needles::new(["abc"]).is_none());
        assert_eq!(REDACT_MIN_LEN, 8);
    }

    #[test]
    fn no_secrets_means_no_redactor_at_all() {
        assert!(Needles::new(Vec::<String>::new()).is_none());
    }

    /// The property that keeps `--shell` usable and a streaming build legible:
    /// ordinary output is emitted immediately, not held back by the length of the
    /// longest secret.
    #[test]
    fn ordinary_output_is_not_held_back_at_all() {
        let mut r = Redactor::new(needles(&["a-very-long-session-token-value"]));
        // A shell prompt. A flat `len(longest) - 1` hold-back would show nothing
        // until ~30 more bytes arrived.
        assert_eq!(r.push(b"$ "), b"$ ");
        assert_eq!(r.push(b"echo hi\n"), b"echo hi\n");
    }

    /// …and a chunk that really does end mid-secret holds back exactly that much.
    #[test]
    fn only_a_genuine_partial_match_is_held_back() {
        let mut r = Redactor::new(needles(&["s3cr3t-value"]));
        // Ends with `s3cr`, which is a prefix of the needle.
        assert_eq!(r.push(b"tok=s3cr"), b"tok=");
        assert_eq!(r.push(b"3t-value!"), b"[redacted]!");
    }

    /// A tail that looks like a prefix but is not must not be held back.
    #[test]
    fn a_near_miss_tail_is_emitted() {
        let mut r = Redactor::new(needles(&["s3cr3t-value"]));
        // `s3cx` is not a prefix of the needle, but shares its first byte.
        assert_eq!(r.push(b"tok=s3cx"), b"tok=s3cx");
    }

    #[test]
    fn nothing_is_held_back_after_finish() {
        let mut r = Redactor::new(needles(&["s3cr3t-value"]));
        let mut out = r.push(b"tail");
        out.extend(r.finish());
        assert_eq!(out, b"tail");
    }
}
