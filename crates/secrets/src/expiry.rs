//! Where an expiry actually comes from, and what to do a moment before it.
//!
//! A credential's real lifetime and its declared one are different things, and
//! only one of them is load-bearing. Four sources, in precedence order:
//!
//! 1. **The protocol's own field** — `expires` from `engflow`, `Expiration`
//!    from `credential_process`. Authoritative when present.
//! 2. **A parsed `exp` claim**, when the value turns out to be a JWT.
//! 3. **The descriptor's `ttl`** — a declaration, not an observation.
//! 4. **A conservative default**, when nothing says anything.
//!
//! Two of the four helper protocols carry no expiry at all, so for `raw` and
//! `docker_credential` the fallback is a hand-written `ttl` — and **the
//! dangerous direction is a `ttl` longer than the truth**. Too short merely
//! re-mints more than it needs to; too long means holding a dead credential and
//! discovering it mid-target, which is the expensive failure.

use std::time::{Duration, SystemTime};

/// How long a conservative default lasts when nothing declares anything.
///
/// Short on purpose. An unknown lifetime that guesses long is the failure mode
/// this module exists to avoid, and the cost of guessing short is one extra
/// mint per five minutes on a path that had no information to begin with.
pub const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);

/// Re-mint this long *before* the stated expiry.
///
/// `exp` is absolute and the host's clock may not agree with the issuer's —
/// the same skew that shows up as a login failure shows up here as a scheduling
/// one. A margin costs a slightly earlier mint and buys not handing a target a
/// credential that expires while it is being written to disk.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// Which of the four sources decided, so a diagnostic can say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpirySource {
    /// The protocol reported it.
    Protocol,
    /// Parsed from an `exp` claim.
    JwtClaim,
    /// The descriptor's declared `ttl`.
    DeclaredTtl,
    /// Nothing said anything; [`DEFAULT_TTL`] applied.
    Default,
}

impl ExpirySource {
    pub fn as_str(self) -> &'static str {
        match self {
            ExpirySource::Protocol => "protocol",
            ExpirySource::JwtClaim => "jwt exp claim",
            ExpirySource::DeclaredTtl => "declared ttl",
            ExpirySource::Default => "default",
        }
    }
}

/// When a minted value stops being usable, and how that was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expiry {
    pub at: SystemTime,
    pub source: ExpirySource,
}

impl Expiry {
    /// Apply the precedence order.
    ///
    /// `value` is only inspected when the protocol reported nothing — the JWT
    /// reader is a fallback, not a cross-check, because a protocol that states
    /// an expiry knows better than an unverified claim inside its own payload.
    pub fn resolve(
        now: SystemTime,
        from_protocol: Option<SystemTime>,
        value: Option<&str>,
        declared_ttl: Option<Duration>,
    ) -> Expiry {
        if let Some(at) = from_protocol {
            return Expiry {
                at,
                source: ExpirySource::Protocol,
            };
        }
        if let Some(at) = value.and_then(crate::jwt::expiry_of) {
            return Expiry {
                at,
                source: ExpirySource::JwtClaim,
            };
        }
        if let Some(ttl) = declared_ttl {
            return Expiry {
                at: now.checked_add(ttl).unwrap_or(now),
                source: ExpirySource::DeclaredTtl,
            };
        }
        Expiry {
            at: now.checked_add(DEFAULT_TTL).unwrap_or(now),
            source: ExpirySource::Default,
        }
    }

    /// Whether this value should be re-minted rather than reused.
    ///
    /// Deliberately not `at <= now`: see [`REFRESH_MARGIN`].
    pub fn stale_at(&self, now: SystemTime) -> bool {
        match self.at.checked_sub(REFRESH_MARGIN) {
            // An expiry inside the margin of the epoch is already gone.
            None => true,
            Some(deadline) => now >= deadline,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn jwt_expiring_at(secs: u64) -> String {
        format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
            URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{secs}}}"#).as_bytes()),
            URL_SAFE_NO_PAD.encode(b"sig"),
        )
    }

    #[test]
    fn protocol_wins_over_everything() {
        let jwt = jwt_expiring_at(9_000);
        let e = Expiry::resolve(
            t(0),
            Some(t(1_000)),
            Some(&jwt),
            Some(Duration::from_secs(7_000)),
        );
        assert_eq!(e.at, t(1_000));
        assert_eq!(e.source, ExpirySource::Protocol);
    }

    #[test]
    fn a_jwt_claim_beats_a_declared_ttl() {
        let jwt = jwt_expiring_at(9_000);
        let e = Expiry::resolve(t(0), None, Some(&jwt), Some(Duration::from_secs(7_000)));
        assert_eq!(e.at, t(9_000));
        assert_eq!(e.source, ExpirySource::JwtClaim);
    }

    /// The common `raw`-helper path: an opaque token and a hand-written ttl.
    #[test]
    fn an_opaque_value_falls_through_to_the_declared_ttl() {
        let e = Expiry::resolve(
            t(100),
            None,
            Some("ghs_opaque"),
            Some(Duration::from_secs(60)),
        );
        assert_eq!(e.at, t(160));
        assert_eq!(e.source, ExpirySource::DeclaredTtl);
    }

    #[test]
    fn nothing_at_all_gets_the_conservative_default() {
        let e = Expiry::resolve(t(100), None, None, None);
        assert_eq!(e.source, ExpirySource::Default);
        assert_eq!(e.at, t(100) + DEFAULT_TTL);
        assert!(
            DEFAULT_TTL < Duration::from_secs(3600),
            "default must be short"
        );
    }

    /// Re-minting happens at a margin *before* the stated expiry, because the
    /// host clock and the issuer's need not agree.
    #[test]
    fn staleness_leaves_a_margin_before_the_stated_expiry() {
        let e = Expiry::resolve(t(0), Some(t(1_000)), None, None);
        assert!(!e.stale_at(t(1_000 - REFRESH_MARGIN.as_secs() - 1)));
        assert!(e.stale_at(t(1_000 - REFRESH_MARGIN.as_secs())));
        assert!(e.stale_at(t(1_000)));
        assert!(e.stale_at(t(2_000)));
    }
}
