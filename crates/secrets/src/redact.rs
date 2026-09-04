//! The redacting tee: keep live credential values out of `log.txt`, the TUI and
//! the event stream.
//!
//! The broker is the only component that knows every live value, so it owns
//! this. A multi-pattern Aho–Corasick automaton over the small set of live
//! secrets replaces matches with `«redacted:NAME»` before bytes reach anything
//! durable or visible.
//!
//! # It is best-effort, and that has to be said out loud
//!
//! A value the tool *derives* before printing escapes it — a signature computed
//! from a key, a token the tool re-encodes some other way. This is a backstop
//! for accidents, not a containment boundary, which is exactly why the
//! log-artifact leak is also fixed at its source rather than papered over here.
//!
//! # Three implementation facts decide whether it works at all
//!
//! - **The tail buffer is per-stream.** stdout and stderr interleave in one
//!   loop; a single shared buffer would corrupt a match spanning the other
//!   stream's chunk. Hence [`RedactStream`], one per stream, over a shared
//!   [`Redactor`].
//! - **Holding bytes back fights the flush guarantee.** The tee flushes each
//!   chunk immediately so an interactive consumer sees output as it arrives.
//!   Withholding bytes means a prompt with no trailing newline that happens to
//!   share a prefix with a live secret is held indefinitely — so the held tail
//!   is only ever the longest suffix that could still begin a match, and
//!   [`RedactStream::flush`] exists for the caller's idle deadline and for EOF.
//! - **Nothing live must cost nothing.** With no patterns registered, `push`
//!   returns its input borrowed: one branch per chunk, no automaton, no copy.

use aho_corasick::{AhoCorasick, MatchKind};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Values shorter than this are not redacted.
///
/// A three-character secret cannot be masked without shredding unrelated
/// output, and a redactor that mangles a build log is worse than one that
/// misses. The broker warns rather than failing: the credential still works,
/// and the author can see the warning and pick a longer one — or accept that a
/// short value was never going to be maskable.
pub const MIN_PATTERN_LEN: usize = 8;

/// One credential's value, as every encoding it might realistically be printed
/// in.
///
/// Raw, base64 (both alphabets, padded and not) and percent-encoded covers the
/// accidents that actually happen: a token echoed directly, a token inside a
/// `Authorization: Basic` header, a token pasted into a URL.
fn encodings(value: &str) -> Vec<Vec<u8>> {
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};

    let raw = value.as_bytes();
    let mut out = vec![
        raw.to_vec(),
        STANDARD.encode(raw).into_bytes(),
        STANDARD_NO_PAD.encode(raw).into_bytes(),
        URL_SAFE.encode(raw).into_bytes(),
        URL_SAFE_NO_PAD.encode(raw).into_bytes(),
        percent_encode(raw),
    ];
    out.retain(|p| p.len() >= MIN_PATTERN_LEN);
    out.sort();
    out.dedup();
    out
}

/// Whether the value's *raw* form is long enough to mask.
///
/// This, and not "did every encoding get dropped", is what decides the warning.
/// A six-byte value base64-encodes to eight, so filtering per-encoding left a
/// redactor that masked the base64 form, left the raw form verbatim, and
/// reported nothing — the value appeared in the log in exactly the form it is
/// usually printed in, with no warning that it would.
fn raw_is_maskable(value: &str) -> bool {
    value.len() >= MIN_PATTERN_LEN
}

/// Percent-encode everything outside the RFC 3986 unreserved set.
///
/// Hand-rolled rather than pulling a crate in for ten lines on a path that only
/// ever needs one direction and one character class.
fn percent_encode(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b);
        } else {
            out.extend_from_slice(format!("%{b:02X}").as_bytes());
        }
    }
    out
}

/// The set of live values, compiled once per run.
///
/// Cheap to clone (it is an `Arc` inside), because every stream of every target
/// holds one.
#[derive(Clone)]
pub struct Redactor {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    ac: AhoCorasick,
    /// Replacement text per pattern, parallel to the automaton's pattern ids.
    replacements: Vec<Vec<u8>>,
    /// Every pattern, sorted, for the "could this tail begin a match" test.
    sorted: Vec<Vec<u8>>,
    /// Which bytes any pattern can start with. Prunes the tail scan to nearly
    /// nothing on ordinary output.
    first_bytes: [bool; 256],
    max_len: usize,
}

/// A value to mask, and the name it is masked as.
pub struct Entry<'a> {
    /// The secret name as the target declared it — `«redacted:NAME»` is what a
    /// reader sees, and a name is what makes the log still explain itself.
    pub name: &'a str,
    pub value: &'a str,
}

impl Redactor {
    /// A redactor that masks nothing, for the overwhelmingly common target that
    /// declares no secrets.
    pub fn inert() -> Self {
        Self { inner: None }
    }

    /// Compile the automaton. Returns the names of any values too short to mask
    /// so the caller can warn about them by name.
    pub fn new(entries: &[Entry<'_>]) -> (Self, Vec<String>) {
        let mut patterns: Vec<Vec<u8>> = Vec::new();
        let mut replacements: Vec<Vec<u8>> = Vec::new();
        let mut too_short: Vec<String> = Vec::new();
        let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();

        // Sorted by name so that two secrets sharing one value mask under a
        // stable name. Registration order is mint-completion order, which under
        // concurrent mints is arbitrary — and a build whose log says
        // `«redacted:a»` on one run and `«redacted:b»` on the next is a
        // diagnosability bug, not a cosmetic one.
        let mut entries: Vec<&Entry<'_>> = entries.iter().collect();
        entries.sort_by_key(|e| e.name);

        for e in entries {
            if !raw_is_maskable(e.value) {
                too_short.push(e.name.to_string());
            }
            let encs = encodings(e.value);
            if encs.is_empty() {
                continue;
            }
            let replacement = format!("«redacted:{}»", e.name).into_bytes();
            for p in encs {
                // A value shared by two secrets masks as the first one's name.
                // Deduping matters more than the name here: two identical
                // patterns would make the automaton report overlapping matches
                // at the same offset.
                if seen.insert(p.clone()) {
                    patterns.push(p);
                    replacements.push(replacement.clone());
                }
            }
        }

        too_short.dedup();

        if patterns.is_empty() {
            return (Self::inert(), too_short);
        }

        // LeftmostLongest: with both a raw value and its base64 live, the longer
        // match is the right one, and leftmost-first would depend on insertion
        // order — which must not decide what a log looks like.
        let ac = match AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&patterns)
        {
            Ok(ac) => ac,
            Err(e) => {
                // Building cannot fail for a non-empty, deduped pattern set, but
                // a redactor that refuses to construct must never take the build
                // down with it: masking nothing is a worse log, not a broken one.
                tracing::error!(error = %e, "failed to build the redaction automaton; \
                    credential values will not be masked in this run's output");
                return (Self::inert(), too_short);
            }
        };

        let mut first_bytes = [false; 256];
        let mut max_len = 0usize;
        for p in &patterns {
            if let Some(&b) = p.first()
                && let Some(slot) = first_bytes.get_mut(b as usize)
            {
                *slot = true;
            }
            max_len = max_len.max(p.len());
        }
        let mut sorted = patterns;
        sorted.sort();

        let redactor = Self {
            inner: Some(Arc::new(Inner {
                ac,
                replacements,
                sorted,
                first_bytes,
                max_len,
            })),
        };
        (redactor, too_short)
    }

    /// Whether anything is masked at all.
    pub fn is_inert(&self) -> bool {
        self.inner.is_none()
    }

    /// Mask a complete, self-contained buffer.
    ///
    /// For anything already whole — an error message, a captured helper stderr,
    /// a diagnostic — where there is no next chunk for a match to span.
    pub fn redact(&self, buf: &[u8]) -> Vec<u8> {
        match &self.inner {
            None => buf.to_vec(),
            Some(inner) => inner.replace(buf),
        }
    }

    /// Mask a complete UTF-8 string, lossily where the replacement is not.
    pub fn redact_str(&self, s: &str) -> String {
        if self.is_inert() {
            return s.to_string();
        }
        String::from_utf8_lossy(&self.redact(s.as_bytes())).into_owned()
    }

    /// A per-stream view. One per stream, never shared between two.
    pub fn stream(&self) -> RedactStream {
        RedactStream {
            redactor: self.clone(),
            carry: Vec::new(),
        }
    }
}

/// Counts, never patterns. A derived `Debug` here would print every live
/// credential, in a type whose entire job is stopping exactly that.
impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            None => f.write_str("Redactor(inert)"),
            Some(i) => write!(
                f,
                "Redactor({} patterns, max {} bytes)",
                i.sorted.len(),
                i.max_len
            ),
        }
    }
}

/// Likewise: the carry buffer holds the partial tail of a live value.
impl std::fmt::Debug for RedactStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RedactStream({:?}, {} bytes held)",
            self.redactor,
            self.carry.len()
        )
    }
}

/// An inert redactor: the right default, because a target with no secrets is
/// overwhelmingly the common case and must pay nothing.
impl Default for Redactor {
    fn default() -> Self {
        Redactor::inert()
    }
}

impl Inner {
    fn replace(&self, buf: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(buf.len());
        let mut last = 0usize;
        for m in self.ac.find_iter(buf) {
            let pre = buf.get(last..m.start());
            let rep = self.replacements.get(m.pattern().as_usize());
            // `replacements` is pushed in lockstep with `patterns`, and
            // `AhoCorasick::build` assigns ids in slice order, so both are
            // always `Some`. If that ever stops holding, dropping the segment
            // while still advancing `last` would silently truncate output
            // rather than fail — which is the worse of the two outcomes.
            debug_assert!(
                pre.is_some() && rep.is_some(),
                "replacement table is out of step with the automaton"
            );
            if let (Some(pre), Some(rep)) = (pre, rep) {
                out.extend_from_slice(pre);
                out.extend_from_slice(rep);
            }
            last = m.end();
        }
        if let Some(rest) = buf.get(last..) {
            out.extend_from_slice(rest);
        }
        out
    }

    /// How far into `buf` it is safe to emit; the rest is carried.
    ///
    /// Two separate hazards, and covering only the first was a live leak:
    ///
    /// 1. **A match may still be starting.** A trailing suffix that is a proper
    ///    prefix of some pattern could complete once the next chunk arrives, so
    ///    it is held. That is [`Inner::prefix_hold`].
    /// 2. **A match may still be *growing*.** This is the subtle one. Holding a
    ///    suffix back shortens what the automaton sees, and a shorter view can
    ///    turn a long match into a short one: with `tokenvalue-aaaa` and
    ///    `tokenvalue-aaaa-extended` both live, holding a single trailing byte
    ///    left the automaton looking at `…-extende`, where only the *short*
    ///    pattern fits. It matched that, masked it under the wrong name, and
    ///    emitted `-extende` — eight bytes of the longer credential — in clear.
    ///
    /// So any match that overlaps the held region is carried **whole**. The
    /// result is bounded by `2 * max_len`: at most one pattern's length, plus at
    /// most a pattern-length prefix behind it.
    fn emit_boundary(&self, buf: &[u8]) -> usize {
        let mut cut = buf.len().saturating_sub(self.prefix_hold(buf));
        // `find_iter` yields matches in increasing start order, so the first one
        // that reaches past the cut is also the earliest — nothing later can
        // pull the boundary further back.
        for m in self.ac.find_iter(buf) {
            if m.end() > cut {
                cut = cut.min(m.start());
                break;
            }
        }
        cut
    }

    /// How many trailing bytes of `buf` could still begin a match.
    ///
    /// The longest suffix that is a proper prefix of some pattern. Pruned hard
    /// by [`Inner::first_bytes`]: on ordinary output no byte position survives
    /// the check and this costs a scan of at most `max_len` bytes.
    fn prefix_hold(&self, buf: &[u8]) -> usize {
        let window = self.max_len.saturating_sub(1).min(buf.len());
        for len in (1..=window).rev() {
            let Some(cand) = buf.get(buf.len().saturating_sub(len)..) else {
                continue;
            };
            let Some(&first) = cand.first() else { continue };
            if !self
                .first_bytes
                .get(first as usize)
                .copied()
                .unwrap_or(false)
            {
                continue;
            }
            // First pattern >= `cand`. A prefix sorts immediately before its
            // extensions, so an extension of `cand` is at `idx` — *unless*
            // `cand` is itself a pattern, in which case `sorted[idx] == cand`
            // and the extension is one slot further.
            //
            // Checking only `idx` was a live leak: with `tokenvalue` and
            // `tokenvalue-extended` both registered, a chunk ending exactly at
            // `tokenvalue` held nothing back, so `-extended` — nine bytes of a
            // real credential — was emitted verbatim, and which mask name
            // appeared depended on where the pipe happened to split.
            let idx = self.sorted.partition_point(|p| p.as_slice() < cand);
            let extends = |i: usize| {
                self.sorted
                    .get(i)
                    .is_some_and(|p| p.len() > cand.len() && p.starts_with(cand))
            };
            if extends(idx) || extends(idx.saturating_add(1)) {
                return len;
            }
        }
        0
    }
}

/// One stream's redaction state.
///
/// Holds back only what could still begin a match, so an interactive prompt is
/// not withheld waiting for a newline that never comes.
pub struct RedactStream {
    redactor: Redactor,
    carry: Vec<u8>,
}

impl RedactStream {
    /// Feed a chunk; get back what may be written now.
    ///
    /// With nothing to mask this borrows the input and does no work at all.
    pub fn push<'a>(&mut self, chunk: &'a [u8]) -> Cow<'a, [u8]> {
        let Some(inner) = self.redactor.inner.as_ref() else {
            return Cow::Borrowed(chunk);
        };

        let buf: Cow<'_, [u8]> = if self.carry.is_empty() {
            Cow::Borrowed(chunk)
        } else {
            let mut joined = std::mem::take(&mut self.carry);
            joined.extend_from_slice(chunk);
            Cow::Owned(joined)
        };

        let cut = inner.emit_boundary(&buf);
        let (emit, tail) = buf.split_at(cut);
        self.carry.clear();
        self.carry.extend_from_slice(tail);

        // A target that declares a secret but whose output never contains one is
        // the common case, and it should not pay an allocation and a full memcpy
        // per chunk per stream. Probing first makes the miss path a scan.
        if inner.ac.find(emit).is_none() {
            return match &buf {
                // Nothing matched, nothing was carried in, and nothing is held
                // back — so the caller's own chunk is already the answer.
                Cow::Borrowed(b) if cut == b.len() => Cow::Borrowed(b),
                _ => Cow::Owned(emit.to_vec()),
            };
        }
        Cow::Owned(inner.replace(emit))
    }

    /// Release the held tail: at EOF, or when the caller's idle deadline fires.
    ///
    /// The tail is by construction a partial match that never completed, so it
    /// is emitted as-is — after one more pass, because a *shorter* pattern may
    /// have completed inside it.
    pub fn flush(&mut self) -> Vec<u8> {
        if self.carry.is_empty() {
            return Vec::new();
        }
        let carry = std::mem::take(&mut self.carry);
        self.redactor.redact(&carry)
    }

    /// How many bytes are currently held back. For the caller's idle timer, and
    /// for tests.
    pub fn held(&self) -> usize {
        self.carry.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "ghs_16C7e42F292c6912E7710c838347Ae178B4a";

    fn redactor() -> Redactor {
        let (r, short) = Redactor::new(&[Entry {
            name: "github",
            value: TOKEN,
        }]);
        assert!(short.is_empty());
        r
    }

    #[test]
    fn masks_a_value_and_names_it() {
        let out = redactor().redact(format!("token={TOKEN} done").as_bytes());
        let s = String::from_utf8(out).expect("utf8");
        assert_eq!(s, "token=«redacted:github» done");
        assert!(!s.contains(TOKEN));
    }

    /// The base64 case is the one that actually bites: a token inside a Basic
    /// auth header never appears raw.
    #[test]
    fn masks_base64_and_percent_encoded_forms() {
        use base64::Engine as _;
        let r = redactor();
        let b64 = base64::engine::general_purpose::STANDARD.encode(TOKEN);
        let out = String::from_utf8(r.redact(format!("Basic {b64}").as_bytes())).expect("utf8");
        assert!(!out.contains(&b64), "{out}");
        assert!(out.contains("«redacted:github»"));

        let pct = String::from_utf8(percent_encode(b"a b/c+d")).expect("utf8");
        assert_eq!(pct, "a%20b%2Fc%2Bd");
    }

    /// The property the whole design rests on for the hot path: a target with
    /// no secrets pays a single branch and no copy.
    #[test]
    fn inert_redactor_is_a_byte_for_byte_borrow() {
        let mut s = Redactor::inert().stream();
        let chunk = b"hello world, nothing to hide\n";
        let out = s.push(chunk);
        assert!(matches!(out, Cow::Borrowed(_)), "inert path allocated");
        assert_eq!(&*out, chunk);
        assert_eq!(s.held(), 0);
        assert!(s.flush().is_empty());
    }

    /// A value split across two chunks must still be caught — this is the
    /// failure a naive per-chunk replace has, and it is silent.
    #[test]
    fn catches_a_value_split_across_a_chunk_boundary() {
        let mut s = redactor().stream();
        let text = format!("prefix {TOKEN} suffix");
        let bytes = text.as_bytes();

        let mut got = Vec::new();
        // One byte at a time is the meanest possible split.
        for i in 0..bytes.len() {
            let Some(b) = bytes.get(i..=i) else { continue };
            got.extend_from_slice(&s.push(b));
        }
        got.extend_from_slice(&s.flush());

        let out = String::from_utf8(got).expect("utf8");
        assert_eq!(out, "prefix «redacted:github» suffix");
    }

    #[test]
    fn split_at_every_possible_offset_still_masks() {
        let text = format!("a{TOKEN}b");
        for cut in 0..text.len() {
            let mut s = redactor().stream();
            let (a, b) = text.split_at(cut);
            let mut got = Vec::new();
            got.extend_from_slice(&s.push(a.as_bytes()));
            got.extend_from_slice(&s.push(b.as_bytes()));
            got.extend_from_slice(&s.flush());
            let out = String::from_utf8(got).expect("utf8");
            assert!(!out.contains(TOKEN), "leaked at cut {cut}: {out}");
            assert_eq!(out, "a«redacted:github»b", "cut {cut}");
        }
    }

    /// Withholding must be bounded by what could still match. Ordinary output
    /// sharing no prefix with any secret is released immediately, or an
    /// interactive prompt hangs forever.
    #[test]
    fn output_that_cannot_begin_a_match_is_never_held() {
        let mut s = redactor().stream();
        let out = s.push(b"Enter your name: ");
        assert_eq!(&*out, b"Enter your name: ");
        assert_eq!(s.held(), 0, "a prompt with no newline was withheld");
    }

    #[test]
    fn only_a_real_partial_prefix_is_held() {
        let mut s = redactor().stream();
        // "ghs_16" is a prefix of the token, so it is held pending more bytes.
        let out = s.push(b"tok ghs_16");
        assert_eq!(&*out, b"tok ");
        assert_eq!(s.held(), 6);
        // ...and released on flush if it never completes.
        assert_eq!(s.flush(), b"ghs_16");
        assert_eq!(s.held(), 0);
    }

    /// A live secret whose value is a strict prefix of another's must not have
    /// its tail emitted when a chunk boundary falls exactly between them.
    ///
    /// The bug this pins: `hold_back` located the first pattern >= the candidate
    /// suffix and required it to be strictly longer. When the suffix *was* a
    /// pattern, that test found the suffix itself and concluded nothing could
    /// extend it — so `tokenvalue|-extended` held nothing back and emitted nine
    /// bytes of a real credential. Worse, the mask name depended on where the
    /// pipe split, so the same build produced different logs.
    #[test]
    fn a_value_that_prefixes_another_value_is_not_split_open() {
        const SHORT: &str = "tokenvalue-aaaa";
        const LONG: &str = "tokenvalue-aaaa-extended";
        let (r, short) = Redactor::new(&[
            Entry {
                name: "a",
                value: SHORT,
            },
            Entry {
                name: "b",
                value: LONG,
            },
        ]);
        assert!(short.is_empty());

        let text = format!("x {LONG} y");
        let whole = String::from_utf8(r.redact(text.as_bytes())).expect("utf8");
        assert_eq!(whole, "x «redacted:b» y");

        // Every split must agree with the unsplit answer, and none may leak.
        for cut in 0..text.len() {
            let mut st = r.stream();
            let (head, tail) = text.split_at(cut);
            let mut got = Vec::new();
            got.extend_from_slice(&st.push(head.as_bytes()));
            got.extend_from_slice(&st.push(tail.as_bytes()));
            got.extend_from_slice(&st.flush());
            let out = String::from_utf8(got).expect("utf8");
            assert!(!out.contains(SHORT), "leaked at cut {cut}: {out}");
            assert!(
                !out.contains("-extended"),
                "leaked a tail at cut {cut}: {out}"
            );
            assert_eq!(out, whole, "cut {cut} disagreed with the unsplit result");
        }
    }

    /// A value too short to mask must be *reported*, whatever its encodings do.
    ///
    /// The bug this pins: the length filter ran per encoding, so a six-byte
    /// value had its raw form dropped and its eight-byte base64 kept — leaving a
    /// redactor that masked a form nobody prints, left the raw value verbatim,
    /// and told the author nothing.
    #[test]
    fn a_value_whose_raw_form_is_too_short_is_reported_even_when_an_encoding_survives() {
        for value in ["ab", "abcdef", "abcdefg", "!!!"] {
            let (r, short) = Redactor::new(&[Entry { name: "s", value }]);
            assert_eq!(
                short,
                vec!["s".to_string()],
                "{value:?} ({} bytes) was not reported as unmaskable",
                value.len()
            );
            // It is still printed verbatim — the warning is the whole remedy,
            // which is exactly why it must not be skipped.
            let out = String::from_utf8(r.redact(format!("raw={value}").as_bytes())).expect("utf8");
            assert_eq!(out, format!("raw={value}"));
        }

        // At the threshold it is masked and not reported.
        let (r, short) = Redactor::new(&[Entry {
            name: "s",
            value: "abcdefgh",
        }]);
        assert!(short.is_empty());
        assert_eq!(
            String::from_utf8(r.redact(b"raw=abcdefgh")).expect("utf8"),
            "raw=«redacted:s»"
        );
    }

    /// Two secrets sharing one value must mask under a stable name. Registration
    /// order is mint-completion order, which under concurrent mints is
    /// arbitrary — a log that says `«redacted:a»` on one run and `«redacted:b»`
    /// on the next is a diagnosability bug.
    #[test]
    fn a_shared_value_masks_under_a_deterministic_name() {
        let shared = "sharedvalue123456";
        let forward = Redactor::new(&[
            Entry {
                name: "zeta",
                value: shared,
            },
            Entry {
                name: "alpha",
                value: shared,
            },
        ])
        .0;
        let reverse = Redactor::new(&[
            Entry {
                name: "alpha",
                value: shared,
            },
            Entry {
                name: "zeta",
                value: shared,
            },
        ])
        .0;
        assert_eq!(
            forward.redact_str(shared),
            reverse.redact_str(shared),
            "the mask name depended on registration order"
        );
        assert_eq!(forward.redact_str(shared), "«redacted:alpha»");
    }

    /// The carry is what stops the tee being starved, so its bound is a
    /// property worth asserting rather than assuming.
    #[test]
    fn the_held_tail_is_bounded_by_the_longest_pattern() {
        let r = redactor();
        let max = r.inner.as_ref().map(|i| i.max_len).expect("live");
        let mut s = r.stream();
        // Feed only bytes that can begin a match, for a long time.
        for _ in 0..50 {
            let head = TOKEN.as_bytes().get(..TOKEN.len().saturating_sub(1));
            let _ = s.push(head.unwrap_or_default());
            assert!(
                s.held() < max.saturating_mul(2),
                "held {} bytes, longest pattern is {max}",
                s.held()
            );
        }
    }

    #[test]
    fn an_empty_chunk_is_a_no_op() {
        let mut s = redactor().stream();
        assert!(s.push(b"").is_empty());
        assert_eq!(s.held(), 0);
        let mut inert = Redactor::inert().stream();
        assert!(inert.push(b"").is_empty());
    }

    #[test]
    fn the_same_value_twice_in_one_chunk_is_masked_twice() {
        let out = redactor().redact_str(&format!("{TOKEN} and {TOKEN}"));
        assert_eq!(out, "«redacted:github» and «redacted:github»");
    }

    /// A live redactor whose patterns never appear must not pay an allocation
    /// and a full copy per chunk — that is the common case for a target that
    /// declares a secret and simply does not print it.
    #[test]
    fn a_live_redactor_borrows_output_that_contains_no_match() {
        let mut s = redactor().stream();
        let out = s.push(b"ordinary build output with no credential in it\n");
        assert!(
            matches!(out, Cow::Borrowed(_)),
            "the no-match path allocated"
        );
    }

    #[test]
    fn short_values_are_reported_rather_than_masked() {
        let (r, short) = Redactor::new(&[
            Entry {
                name: "tiny",
                value: "abc",
            },
            Entry {
                name: "real",
                value: TOKEN,
            },
        ]);
        assert_eq!(short, vec!["tiny".to_string()]);
        let out = String::from_utf8(r.redact(b"abc is fine to print")).expect("utf8");
        assert_eq!(out, "abc is fine to print");
    }

    #[test]
    fn an_all_short_secret_set_yields_an_inert_redactor() {
        let (r, short) = Redactor::new(&[Entry {
            name: "tiny",
            value: "ab",
        }]);
        assert!(r.is_inert());
        assert_eq!(short, vec!["tiny".to_string()]);
    }

    #[test]
    fn two_secrets_are_both_masked_under_their_own_names() {
        let other = "AKIAIOSFODNN7EXAMPLEKEY";
        let (r, _) = Redactor::new(&[
            Entry {
                name: "github",
                value: TOKEN,
            },
            Entry {
                name: "aws",
                value: other,
            },
        ]);
        let out =
            String::from_utf8(r.redact(format!("{TOKEN} and {other}").as_bytes())).expect("utf8");
        assert_eq!(out, "«redacted:github» and «redacted:aws»");
    }

    /// Two streams must not share carry state, or a match spanning one stream's
    /// chunks is corrupted by the other's.
    #[test]
    fn streams_carry_independently() {
        let r = redactor();
        let mut a = r.stream();
        let mut b = r.stream();
        let (head, tail) = TOKEN.split_at(10);

        assert!(a.push(head.as_bytes()).is_empty());
        // b interleaves with unrelated output; a's pending tail is untouched.
        assert_eq!(&*b.push(b"unrelated\n"), b"unrelated\n");
        let out = String::from_utf8(a.push(tail.as_bytes()).into_owned()).expect("utf8");
        assert_eq!(out, "«redacted:github»");
    }
}
