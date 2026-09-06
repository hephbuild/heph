//! Where an expiry actually comes from, and what to do a moment before it.
//!
//! A credential's real lifetime and its declared one are different things, and
//! only one of them is load-bearing. Four sources, in precedence order:
//!
//! 1. **The protocol's own field** — `expires` from `credential_helper`, `Expiration`
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
//!
//! # Two margins, not one
//!
//! An expiry is consulted for two different questions, and answering both with
//! one number was a bug:
//!
//! - [`REFRESH_MARGIN`] — *is this token still valid?* Clock skew only. The
//!   host's clock and the issuer's need not agree.
//! - [`MIN_HANDOUT_LIFETIME`] — *is it worth giving to a target that is about
//!   to start?* A credential one second inside the skew margin passes the first
//!   test and fails every target that runs for longer than a second.
//!
//! So a handout re-mints a still-valid credential that has too little life
//! left, rather than passing the problem on. The exception is a credential
//! whose *whole* lifetime is shorter than the headroom: refreshing that buys
//! nothing, and the broker warns rather than minting once per target.
//!
//! None of this helps a credential expiring **inside** one long-running target.
//! Nothing outside that process can replace a value it has already read; that
//! needs a process credential the tool re-reads — `credential_process`,
//! `GOAUTH=command`, a git `credential.helper`.

use std::time::{Duration, SystemTime};

/// How long a conservative default lasts when nothing declares anything.
///
/// Short on purpose. An unknown lifetime that guesses long is the failure mode
/// this module exists to avoid, and the cost of guessing short is one extra
/// mint per five minutes on a path that had no information to begin with.
pub const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);

/// Treat a credential as expired this long *before* its stated expiry.
///
/// **Clock skew, and nothing else.** `exp` is absolute and the host's clock may
/// not agree with the issuer's — the same skew that shows up as a login failure
/// shows up here as a scheduling one.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// The minimum usable life a credential must have left to be handed to a target
/// that is about to start.
///
/// **A separate concern from [`REFRESH_MARGIN`], and conflating the two was a
/// bug.** The margin answers "is this token still valid?"; this answers "is it
/// worth giving to something that is about to run for a while?" A credential
/// one second inside the margin passes the first test and fails every target
/// that takes longer than a second — which is most of them.
///
/// So a handout re-mints when less than this is left, even though the value is
/// still perfectly valid. The cost is an occasional early mint. The failure it
/// prevents is a target dying partway through with an authentication error,
/// which costs whatever the target had already done.
///
/// Five minutes rather than something adaptive: heph cannot know how long a
/// target will run, and a number nobody can predict from is worse than a
/// conservative constant. A target that outlives even this wants a *process*
/// credential rather than a value — see the module docs on mid-target expiry.
pub const MIN_HANDOUT_LIFETIME: Duration = Duration::from_secs(5 * 60);

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
    /// When this value was minted.
    ///
    /// Kept so the broker can tell "this credential has aged" from "this
    /// credential was born short-lived". Re-minting helps in the first case and
    /// is pure waste in the second, and without an issue time the two are
    /// indistinguishable.
    pub issued_at: SystemTime,
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
                issued_at: now,
            };
        }
        if let Some(at) = value.and_then(crate::jwt::expiry_of) {
            return Expiry {
                at,
                source: ExpirySource::JwtClaim,
                issued_at: now,
            };
        }
        if let Some(ttl) = declared_ttl {
            return Expiry {
                at: now.checked_add(ttl).unwrap_or(now),
                source: ExpirySource::DeclaredTtl,
                issued_at: now,
            };
        }
        Expiry {
            at: now.checked_add(DEFAULT_TTL).unwrap_or(now),
            source: ExpirySource::Default,
            issued_at: now,
        }
    }

    /// Usable life remaining, with the skew margin already taken off.
    pub fn usable_for(&self, now: SystemTime) -> Duration {
        self.at
            .checked_sub(REFRESH_MARGIN)
            .and_then(|deadline| deadline.duration_since(now).ok())
            .unwrap_or(Duration::ZERO)
    }

    /// The whole usable life this credential had when it was minted.
    ///
    /// What distinguishes an aged credential from one that was always short.
    pub fn usable_lifetime(&self) -> Duration {
        self.usable_for(self.issued_at)
    }

    /// Enough life left to give to a target that is about to start.
    ///
    /// False well before [`Self::stale_at`] turns true, which is the point: a
    /// still-valid credential with a second of usable life left is not a
    /// credential a target can do anything with.
    pub fn has_handout_headroom(&self, now: SystemTime) -> bool {
        self.usable_for(now) >= MIN_HANDOUT_LIFETIME
    }

    /// Whether re-minting would actually buy more life than is left.
    ///
    /// False for a credential whose *whole* lifetime is shorter than the
    /// headroom a handout wants — a 60-second token cannot be refreshed into a
    /// five-minute one, and re-minting it on every handout would be a mint per
    /// target for no gain. Callers warn instead.
    pub fn refresh_would_help(&self, now: SystemTime) -> bool {
        self.usable_lifetime() > self.usable_for(now)
            && self.usable_lifetime() >= MIN_HANDOUT_LIFETIME
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

    /// The two margins answer different questions, and collapsing them was a
    /// bug: a credential can be perfectly valid and still useless to hand to
    /// something about to run.
    #[test]
    fn validity_and_handout_headroom_are_different_questions() {
        // A one-hour credential, minted at t=0.
        let e = Expiry {
            at: t(3600),
            source: ExpirySource::DeclaredTtl,
            issued_at: t(0),
        };
        assert_eq!(e.usable_lifetime(), Duration::from_secs(3600 - 60));

        // Halfway: valid, and plenty of headroom.
        assert!(!e.stale_at(t(1800)));
        assert!(e.has_handout_headroom(t(1800)));

        // 30 seconds of usable life: still valid, no headroom at all. This is
        // the state the old rule handed to a target.
        let nearly = t(3600 - 60 - 30);
        assert!(!e.stale_at(nearly));
        assert!(!e.has_handout_headroom(nearly));
        assert!(e.refresh_would_help(nearly));

        // Exactly at the headroom boundary still counts.
        assert!(e.has_handout_headroom(t(3600 - 60) - MIN_HANDOUT_LIFETIME));
    }

    /// A credential born shorter than the headroom cannot be refreshed into a
    /// longer one, so re-minting it every handout would be a mint per target
    /// for nothing.
    #[test]
    fn refreshing_a_born_short_credential_would_not_help() {
        // 90s total: 30s usable after the margin, always under the headroom.
        let e = Expiry {
            at: t(90),
            source: ExpirySource::DeclaredTtl,
            issued_at: t(0),
        };
        assert_eq!(e.usable_lifetime(), Duration::from_secs(30));
        assert!(!e.has_handout_headroom(t(0)));
        assert!(
            !e.refresh_would_help(t(0)),
            "a 30s-usable credential cannot be refreshed into a 5m one"
        );
        assert!(!e.refresh_would_help(t(20)));
    }

    /// An expiry inside the margin has no usable life rather than a negative
    /// one — the subtraction must not wrap or panic.
    #[test]
    fn an_already_expired_credential_has_no_usable_life() {
        let e = Expiry {
            at: t(100),
            source: ExpirySource::Protocol,
            issued_at: t(0),
        };
        assert_eq!(e.usable_for(t(1_000)), Duration::ZERO);
        assert!(!e.has_handout_headroom(t(1_000)));
        assert!(e.stale_at(t(1_000)));
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
