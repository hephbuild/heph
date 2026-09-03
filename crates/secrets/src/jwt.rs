//! A best-effort JWT claim reader.
//!
//! # It reads claims; it does not verify them
//!
//! There is no signature check here, because heph has no business holding the
//! issuer's keys for this and does not need to. That is fine for *scheduling* —
//! a lie about `exp` costs at worst an extra mint, or a failure that was coming
//! anyway — and it draws one hard line, which the two entry points below make
//! structural rather than a comment somebody has to obey:
//!
//! - **[`expiry_of`] may be pointed at anything.** An expiry is a hint, and a
//!   wrong hint is not a security event.
//! - **[`subject_of_trusted`] may only be given a token heph obtained itself**,
//!   from a known issuer over TLS — the ambient CI token, or the session from
//!   `heph auth login`. Never helper output. A `sub` reaches the cache key
//!   under `cache.subject_scoped`, so a helper free to claim any subject is a
//!   helper free to have its artifacts served as somebody else's.
//!
//! # Why it exists even though its reach is small
//!
//! The tokens most `raw` helpers return are *opaque*, not JWTs: `gh auth token`
//! yields `ghs_…`, `gcloud auth print-access-token` yields `ya29.…`, AWS
//! session tokens are opaque blobs. So this earns nothing on the most common
//! paths. It is not optional anyway: heph must read the `exp` of the ID token
//! it holds itself, to avoid presenting a stale assertion to an exchange and
//! getting back an error that explains nothing, and `cache.subject_scoped`
//! needs the `sub` of that same token *before* the first `hashin`. The code
//! exists regardless; pointing it at helper output too is a few lines.
//!
//! # Deliberately dull detection
//!
//! Three base64url segments, the middle one decoding to JSON carrying the claim
//! wanted. Anything that is not obviously that returns `None` without an error.
//! That is what "best effort" has to mean: it never fails a build, it only ever
//! improves a number.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Decode the claims segment of a candidate JWT.
///
/// Returns `None` for anything that is not three base64url segments whose
/// middle decodes to a JSON object. No error: the caller's fallback is a
/// declared `ttl`, not a failure.
fn claims(token: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let token = token.trim();
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    // Exactly three segments. A fourth means JWE (five segments) or a mangled
    // value; either way this is not something to guess at.
    if parts.next().is_some() {
        return None;
    }
    if payload.is_empty() {
        return None;
    }
    let raw = URL_SAFE_NO_PAD.decode(payload).ok()?;
    match serde_json::from_slice(&raw).ok()? {
        serde_json::Value::Object(m) => Some(m),
        _ => None,
    }
}

/// Read the `exp` claim as an absolute time.
///
/// Safe to point at any value from any source: an expiry only schedules a
/// re-mint.
pub fn expiry_of(token: &str) -> Option<SystemTime> {
    // Integer seconds only. RFC 7519 permits a non-integer NumericDate, but no
    // IdP emits one, and accepting floats here would mean a lossy cast on a
    // path whose whole contract is "fall through rather than guess". A zero or
    // negative `exp` falls through for the same reason: it is nonsense, and the
    // declared `ttl` is at least a number a human wrote.
    let exp = claims(token)?.get("exp")?.as_u64().filter(|&e| e > 0)?;
    UNIX_EPOCH.checked_add(Duration::from_secs(exp))
}

/// Read the `sub` claim, for a token heph obtained itself.
///
/// The name carries the contract because the type cannot: there is no way to
/// make a `&str` remember where it came from, so the only defence is that every
/// call site has to type the word `trusted` and justify it. The callers are the
/// ambient CI ID token and the `heph auth login` session, and no others.
///
/// Also returns the `iss`, because a subject is only meaningful paired with the
/// issuer that asserted it — two IdPs can both say `alice`.
pub fn subject_of_trusted(token: &str) -> Option<(String, String)> {
    let c = claims(token)?;
    let sub = c.get("sub")?.as_str()?.to_string();
    let iss = c.get("iss").and_then(|v| v.as_str()).unwrap_or_default();
    if sub.is_empty() {
        return None;
    }
    Some((iss.to_string(), sub))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(payload: &str) -> String {
        format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#),
            URL_SAFE_NO_PAD.encode(payload.as_bytes()),
            URL_SAFE_NO_PAD.encode(b"not-a-real-signature"),
        )
    }

    #[test]
    fn reads_exp_without_verifying_anything() {
        let t = make(r#"{"exp":1893456000,"sub":"alice"}"#);
        let got = expiry_of(&t).expect("exp");
        assert_eq!(
            got.duration_since(UNIX_EPOCH)
                .expect("since epoch")
                .as_secs(),
            1_893_456_000
        );
    }

    /// The whole point of "best effort": an opaque token is the common case,
    /// and it must fall through silently rather than erroring.
    #[test]
    fn opaque_tokens_fall_through_silently() {
        for opaque in [
            "ghs_16C7e42F292c6912E7710c838347Ae178B4a",
            "ya29.a0AfH6SMBx-example-opaque-google-access-token",
            "",
            "not.a.jwt.at.all.five.segments",
            "only.two",
            "...",
        ] {
            assert!(expiry_of(opaque).is_none(), "{opaque:?} parsed as a JWT");
            assert!(subject_of_trusted(opaque).is_none(), "{opaque:?}");
        }
    }

    #[test]
    fn a_payload_that_is_not_an_object_is_not_a_jwt() {
        assert!(expiry_of(&make("[1,2,3]")).is_none());
        assert!(expiry_of(&make("\"hello\"")).is_none());
        assert!(expiry_of(&make("garbage{")).is_none());
    }

    #[test]
    fn absent_or_nonsense_exp_falls_through_to_the_declared_ttl() {
        assert!(expiry_of(&make(r#"{"sub":"alice"}"#)).is_none());
        assert!(expiry_of(&make(r#"{"exp":0}"#)).is_none());
        assert!(expiry_of(&make(r#"{"exp":-5}"#)).is_none());
        assert!(expiry_of(&make(r#"{"exp":"soon"}"#)).is_none());
        assert!(expiry_of(&make(r#"{"exp":1893456000.5}"#)).is_none());
    }

    #[test]
    fn subject_comes_back_paired_with_its_issuer() {
        let t = make(r#"{"iss":"https://org.okta.com","sub":"alice@org.example"}"#);
        let (iss, sub) = subject_of_trusted(&t).expect("sub");
        assert_eq!(iss, "https://org.okta.com");
        assert_eq!(sub, "alice@org.example");
    }

    #[test]
    fn an_empty_subject_is_no_subject() {
        assert!(subject_of_trusted(&make(r#"{"sub":""}"#)).is_none());
        assert!(subject_of_trusted(&make(r#"{"exp":1893456000}"#)).is_none());
    }
}
