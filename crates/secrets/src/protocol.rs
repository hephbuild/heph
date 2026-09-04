//! The four helper wire protocols.
//!
//! "Run a helper and read a credential" sounds like one thing and is four.
//! Implementing only the Bazel-derived spec would have covered none of the
//! laptop paths this feature exists for, so the protocol is an explicit closed
//! field on the descriptor rather than something guessed from output.
//!
//! | protocol | stdin | stdout | expiry | speakers |
//! |---|---|---|---|---|
//! | `credential_helper` | `{"uri": …}` | `{"headers": {…}, "expires": …}` | native, RFC 3339 | Bazel `--credential_helper` helpers |
//! | `credential_process` | — | `{"Version":1,"AccessKeyId":…}` | `Expiration`, optional | `aws configure export-credentials`, aws-vault, Granted |
//! | `docker_credential` | bare URL, not JSON | `{"ServerURL","Username","Secret"}` | none | `docker-credential-osxkeychain`, `-ecr-login`, `-gcr` |
//! | `raw` | — | the value, verbatim | none | `gh auth token`, `gcloud auth print-access-token`, `op read` |
//!
//! All four are invoked once per mint, are aborted on a non-zero exit, and have
//! their stderr captured for diagnostics and redacted like anything else. This
//! module is only the encoding and decoding: spawning lives in
//! [`crate::provider`], so every shape below is unit-testable without a
//! subprocess.

use crate::descriptor::Protocol;
use crate::expiry::Expiry;
use crate::value::{Credential, SecretValue};
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

/// What to write to the helper's stdin, if anything.
///
/// The three encodings are genuinely different, which is the reason the
/// protocol cannot be inferred from the response: by the time a response exists
/// the request has already been sent in some encoding.
pub fn stdin_for(protocol: Protocol, uri: Option<&str>) -> Option<Vec<u8>> {
    match protocol {
        Protocol::CredentialHelper => {
            let uri = uri.unwrap_or_default();
            Some(format!("{{\"uri\":{}}}", json_string(uri)).into_bytes())
        }
        // A bare URL, not JSON — the single most commonly mis-implemented
        // detail of this protocol, and the reason it shares only a name with
        // the Bazel spec above it.
        Protocol::DockerCredential => Some(uri.unwrap_or_default().as_bytes().to_vec()),
        Protocol::CredentialProcess | Protocol::Raw => None,
    }
}

fn json_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// Parse a helper's stdout into a credential.
///
/// `now` and `declared_ttl` feed the expiry precedence in [`crate::expiry`];
/// the protocol's own field wins when it has one.
pub fn parse_response(
    protocol: Protocol,
    stdout: &[u8],
    now: SystemTime,
    declared_ttl: Option<Duration>,
) -> anyhow::Result<Credential> {
    match protocol {
        Protocol::Raw => parse_raw(stdout, now, declared_ttl),
        Protocol::CredentialHelper => parse_engflow(stdout, now, declared_ttl),
        Protocol::CredentialProcess => parse_credential_process(stdout, now, declared_ttl),
        Protocol::DockerCredential => parse_docker_credential(stdout, now, declared_ttl),
    }
}

/// stdout *is* the value, minus a trailing newline.
///
/// A concession rather than a protocol. The sharp edge is worth stating where
/// the code is: a helper that prints a warning or an update notice to stdout
/// has just made it part of your credential. Helpers on this protocol must be
/// silent, or wrapped until they are — heph cannot tell the difference, and a
/// heuristic that stripped "warning-looking" lines would eventually strip a
/// credential.
fn parse_raw(
    stdout: &[u8],
    now: SystemTime,
    declared_ttl: Option<Duration>,
) -> anyhow::Result<Credential> {
    let text = std::str::from_utf8(stdout)
        .map_err(|e| anyhow::anyhow!("raw helper output is not valid UTF-8: {e}"))?;
    let value = text.trim_end_matches(['\n', '\r']);
    if value.is_empty() {
        anyhow::bail!(
            "raw helper printed nothing to stdout. On this protocol stdout *is* the \
             credential, so an empty stdout is a failure even at exit 0 — check the helper \
             writes the value to stdout rather than stderr."
        );
    }
    // Where the value happens to be a JWT its own `exp` beats a declared ttl.
    let expiry = Expiry::resolve(now, None, Some(value), declared_ttl);
    Ok(Credential::single(value, expiry))
}

#[derive(serde::Deserialize)]
struct EngflowResponse {
    #[serde(default)]
    headers: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    expires: Option<String>,
}

/// The Bazel `--credential_helper` / EngFlow spec.
///
/// The only protocol carrying expiry natively, which is why the broker's TTL
/// cache prefers it. It also returns *headers* rather than a credential, so a
/// shape needing a bare token parses one back out of `Authorization` — a small
/// impedance mismatch, and the reason this is not simply the default.
fn parse_engflow(
    stdout: &[u8],
    now: SystemTime,
    declared_ttl: Option<Duration>,
) -> anyhow::Result<Credential> {
    let r: EngflowResponse = serde_json::from_slice(stdout)
        .map_err(|e| anyhow::anyhow!("credential_helper helper: parse response: {e}"))?;
    if r.headers.is_empty() {
        anyhow::bail!("credential_helper helper returned no `headers`");
    }

    let expires = r.expires.as_deref().and_then(parse_rfc3339);
    if r.expires.is_some() && expires.is_none() {
        // Do not fail: an unparseable expiry falls back to the declared ttl
        // like any other missing one. But say so, because the fallback is
        // usually longer than the truth and that is the dangerous direction.
        tracing::warn!(
            expires = ?r.expires,
            "credential_helper helper returned an `expires` that is not RFC 3339; falling back to the \
             declared ttl"
        );
    }

    let mut fields = BTreeMap::new();
    for (name, values) in &r.headers {
        if let Some(first) = values.first() {
            fields.insert(format!("header.{name}"), SecretValue::new(first.clone()));
        }
    }
    // A shape wanting a bare token gets one from `Authorization`, stripping the
    // scheme. Case-insensitive: the header name is, per RFC 9110.
    let auth = r
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .and_then(|(_, v)| v.first());
    if let Some(auth) = auth {
        let token = auth
            .split_once(' ')
            .map(|(_scheme, rest)| rest)
            .unwrap_or(auth.as_str());
        fields.insert(
            Credential::PRIMARY.to_string(),
            SecretValue::new(token.to_string()),
        );
    }

    if fields.is_empty() {
        let names = r.headers.keys().cloned().collect::<Vec<_>>().join(", ");
        anyhow::bail!(
            "credential_helper helper returned headers with no values: [{names}]. A credential with no \
             fields would be registered as unmaskable and fail far downstream, so it is rejected \
             here."
        );
    }

    let bearer = fields
        .get(Credential::PRIMARY)
        .map(|v| v.expose().to_string());
    let expiry = Expiry::resolve(now, expires, bearer.as_deref(), declared_ttl);
    Ok(Credential { fields, expiry })
}

/// Deliberately **not** `deny_unknown_fields`.
///
/// This is AWS's schema, emitted independently by `aws configure
/// export-credentials`, aws-vault, saml2aws and Granted. A field added by any
/// of them — or by AWS — must be ignored, not turned into a parse error that
/// breaks every mint. The opposite call is right for `Identity`, which is
/// heph's own format where an unknown field means a version skew worth
/// reporting; the two differ because one schema is ours and one is not.
#[derive(serde::Deserialize)]
struct CredentialProcessResponse {
    #[serde(rename = "Version")]
    version: u32,
    #[serde(rename = "AccessKeyId")]
    access_key_id: String,
    #[serde(rename = "SecretAccessKey")]
    secret_access_key: String,
    #[serde(rename = "SessionToken", default)]
    session_token: Option<String>,
    #[serde(rename = "Expiration", default)]
    expiration: Option<String>,
}

/// The AWS `credential_process` schema.
///
/// The one protocol heph both reads and *writes*: the same shape it accepts
/// from `aws-vault` is what it renders into the sandbox so a long-running
/// target can refresh mid-flight.
fn parse_credential_process(
    stdout: &[u8],
    now: SystemTime,
    declared_ttl: Option<Duration>,
) -> anyhow::Result<Credential> {
    let r: CredentialProcessResponse = serde_json::from_slice(stdout)
        .map_err(|e| anyhow::anyhow!("credential_process helper: parse response: {e}"))?;
    if r.version != 1 {
        anyhow::bail!(
            "credential_process helper returned Version {} — this heph understands only \
             Version 1",
            r.version
        );
    }

    // An absent `Expiration` means "treat as static", not "expired". Getting
    // this backwards would re-mint every single use of a long-lived profile.
    let expires = r.expiration.as_deref().and_then(parse_rfc3339);
    if r.expiration.is_some() && expires.is_none() {
        tracing::warn!(
            expiration = ?r.expiration,
            "credential_process helper returned an unparseable `Expiration`; falling back to \
             the declared ttl"
        );
    }

    let mut fields = BTreeMap::from([
        ("AccessKeyId".to_string(), SecretValue::new(r.access_key_id)),
        (
            "SecretAccessKey".to_string(),
            SecretValue::new(r.secret_access_key),
        ),
    ]);
    if let Some(t) = r.session_token {
        fields.insert("SessionToken".to_string(), SecretValue::new(t));
    }

    Ok(Credential {
        fields,
        expiry: Expiry::resolve(now, expires, None, declared_ttl),
    })
}

#[derive(serde::Deserialize)]
struct DockerCredentialResponse {
    #[serde(rename = "ServerURL", default)]
    server_url: Option<String>,
    #[serde(rename = "Username")]
    username: String,
    #[serde(rename = "Secret")]
    secret: String,
}

/// The Docker credential-helper protocol.
///
/// heph only ever calls `get`, never `store`/`erase`/`list`. A `Username` of
/// `<token>` is the convention for "`Secret` is an identity token, not a
/// password", and the shape must honour it — which is why the username is
/// carried through as a field rather than assumed.
fn parse_docker_credential(
    stdout: &[u8],
    now: SystemTime,
    declared_ttl: Option<Duration>,
) -> anyhow::Result<Credential> {
    let r: DockerCredentialResponse = serde_json::from_slice(stdout)
        .map_err(|e| anyhow::anyhow!("docker_credential helper: parse response: {e}"))?;
    if r.secret.is_empty() {
        anyhow::bail!("docker_credential helper returned an empty `Secret`");
    }

    let mut fields = BTreeMap::from([
        // The username is not a credential, but it travels with one and the
        // `docker_config` shape needs it to build the `auth` blob.
        ("Username".to_string(), SecretValue::new(r.username)),
        (
            Credential::PRIMARY.to_string(),
            SecretValue::new(r.secret.clone()),
        ),
        ("Secret".to_string(), SecretValue::new(r.secret.clone())),
    ]);
    if let Some(u) = r.server_url {
        fields.insert("ServerURL".to_string(), SecretValue::new(u));
    }

    Ok(Credential {
        fields,
        expiry: Expiry::resolve(now, None, Some(&r.secret), declared_ttl),
    })
}

/// Parse an RFC 3339 timestamp.
///
/// `chrono` rather than the ~70 hand-rolled lines this replaces. That version
/// was not merely redundant — `chrono` is already a direct dependency of the
/// root crate, `engine` and `telemetry` — it was also *wrong in the dangerous
/// direction*: it never checked the separators, never range-checked an offset,
/// and rolled an impossible date forward, so `2026-02-30T00:00:00Z` came back
/// as March 2nd. An expiry silently later than the truth is precisely the
/// failure [`crate::expiry`] names as the expensive one, and because the parse
/// *succeeded* the fallback warning never fired either.
///
/// `None` for anything unparseable, which falls through to the declared `ttl` —
/// the same best-effort contract the JWT reader has.
pub(crate) fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(SystemTime::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expiry::ExpirySource;
    use std::time::UNIX_EPOCH;

    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// The three stdin encodings really are different, which is why the
    /// protocol cannot be guessed from a response.
    #[test]
    fn each_protocol_encodes_its_request_differently() {
        assert_eq!(
            stdin_for(Protocol::CredentialHelper, Some("https://host/p")).as_deref(),
            Some(&br#"{"uri":"https://host/p"}"#[..])
        );
        // A bare URL, not JSON — the detail most often got wrong.
        assert_eq!(
            stdin_for(Protocol::DockerCredential, Some("ghcr.io")).as_deref(),
            Some(&b"ghcr.io"[..])
        );
        assert!(stdin_for(Protocol::Raw, Some("x")).is_none());
        assert!(stdin_for(Protocol::CredentialProcess, Some("x")).is_none());
    }

    #[test]
    fn raw_is_stdout_minus_the_trailing_newline() {
        let c = parse_response(Protocol::Raw, b"ghs_abc123\n", t(0), None).expect("raw");
        assert_eq!(c.resolve_pointer("$.").expect("v").expose(), "ghs_abc123");
    }

    /// A silent exit-0 helper that printed nothing is a failure, not an empty
    /// credential that fails a mile downstream as a 401.
    #[test]
    fn raw_rejects_empty_stdout_even_at_exit_zero() {
        let err = parse_response(Protocol::Raw, b"\n", t(0), None).expect_err("empty");
        assert!(err.to_string().contains("printed nothing"), "{err}");
    }

    #[test]
    fn credential_process_carries_its_own_expiration() {
        let body = br#"{"Version":1,"AccessKeyId":"ASIA1","SecretAccessKey":"s",
                        "SessionToken":"tok","Expiration":"2026-01-01T00:00:00Z"}"#;
        let c = parse_response(Protocol::CredentialProcess, body, t(0), None).expect("parse");
        assert_eq!(c.expiry.source, ExpirySource::Protocol);
        assert_eq!(c.get("AccessKeyId").expect("k").expose(), "ASIA1");
        assert_eq!(c.get("SessionToken").expect("t").expose(), "tok");
        assert_eq!(
            c.expiry
                .at
                .duration_since(UNIX_EPOCH)
                .expect("epoch")
                .as_secs(),
            1_767_225_600
        );
    }

    /// An absent `Expiration` means "treat as static", not "expired". Getting
    /// it backwards re-mints on every single use of a long-lived profile.
    #[test]
    fn credential_process_without_expiration_is_not_expired() {
        let body = br#"{"Version":1,"AccessKeyId":"AKIA","SecretAccessKey":"s"}"#;
        let c = parse_response(
            Protocol::CredentialProcess,
            body,
            t(1000),
            Some(Duration::from_secs(3600)),
        )
        .expect("parse");
        assert_eq!(c.expiry.source, ExpirySource::DeclaredTtl);
        assert!(!c.expiry.stale_at(t(1000)));
        assert!(c.get("SessionToken").is_none());
    }

    #[test]
    fn credential_process_rejects_a_version_it_does_not_understand() {
        let body = br#"{"Version":2,"AccessKeyId":"A","SecretAccessKey":"s"}"#;
        let err =
            parse_response(Protocol::CredentialProcess, body, t(0), None).expect_err("version");
        assert!(err.to_string().contains("Version 2"), "{err}");
    }

    /// The engflow response is headers, so a bare token has to be recovered
    /// from `Authorization` with the scheme stripped.
    #[test]
    fn engflow_recovers_a_bare_token_from_the_authorization_header() {
        let body = br#"{"headers":{"Authorization":["Bearer abc123"]},
                        "expires":"2026-01-01T00:00:00Z"}"#;
        let c = parse_response(Protocol::CredentialHelper, body, t(0), None).expect("parse");
        assert_eq!(c.resolve_pointer("$.").expect("v").expose(), "abc123");
        assert_eq!(
            c.get("header.Authorization").expect("h").expose(),
            "Bearer abc123"
        );
        assert_eq!(c.expiry.source, ExpirySource::Protocol);
    }

    #[test]
    fn engflow_header_names_are_matched_case_insensitively() {
        let body = br#"{"headers":{"authorization":["Bearer xyz"]}}"#;
        let c = parse_response(Protocol::CredentialHelper, body, t(0), None).expect("parse");
        assert_eq!(c.resolve_pointer("$.").expect("v").expose(), "xyz");
    }

    /// An unparseable expiry must not fail the mint — it falls back like any
    /// other missing one. It warns because the fallback is usually longer than
    /// the truth.
    #[test]
    fn an_unparseable_expiry_falls_back_rather_than_failing() {
        let body = br#"{"headers":{"Authorization":["Bearer a1b2c3"]},"expires":"next tuesday"}"#;
        let c = parse_response(
            Protocol::CredentialHelper,
            body,
            t(0),
            Some(Duration::from_secs(60)),
        )
        .expect("parse");
        assert_eq!(c.expiry.source, ExpirySource::DeclaredTtl);
    }

    /// `Username: <token>` is the convention for "the secret is an identity
    /// token, not a password", so the username must survive to the shape.
    #[test]
    fn docker_credential_keeps_the_username_convention() {
        let body = br#"{"ServerURL":"ghcr.io","Username":"<token>","Secret":"ghs_xyz"}"#;
        let c = parse_response(Protocol::DockerCredential, body, t(0), None).expect("parse");
        assert_eq!(c.get("Username").expect("u").expose(), "<token>");
        assert_eq!(c.resolve_pointer("$.").expect("v").expose(), "ghs_xyz");
        assert_eq!(c.get("ServerURL").expect("s").expose(), "ghcr.io");
    }

    #[test]
    fn docker_credential_rejects_an_empty_secret() {
        let body = br#"{"Username":"u","Secret":""}"#;
        let err = parse_response(Protocol::DockerCredential, body, t(0), None).expect_err("empty");
        assert!(err.to_string().contains("empty `Secret`"), "{err}");
    }

    #[test]
    fn rfc3339_handles_z_offsets_and_fractional_seconds() {
        let epoch = |s: &str| {
            parse_rfc3339(s).map(|t| t.duration_since(UNIX_EPOCH).expect("epoch").as_secs())
        };
        assert_eq!(epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch("2026-01-01T00:00:00Z"), Some(1_767_225_600));
        assert_eq!(epoch("2026-01-01T00:00:00.123456Z"), Some(1_767_225_600));
        // +01:00 means the instant is an hour earlier in UTC.
        assert_eq!(epoch("2026-01-01T01:00:00+01:00"), Some(1_767_225_600));
        assert_eq!(epoch("2025-12-31T23:00:00-01:00"), Some(1_767_225_600));
        // A leap day, and a century year that is a leap year.
        assert_eq!(epoch("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        assert_eq!(epoch("2000-02-29T00:00:00Z"), Some(951_782_400));
        // A half-hour offset, which a naive two-digit reader gets wrong.
        assert_eq!(epoch("2026-01-01T05:30:00+05:30"), Some(1_767_225_600));
        // RFC 3339 §5.6 permits a space in place of `T` by agreement, and it
        // names the same instant — so it is accepted, not rejected.
        assert_eq!(epoch("2026-01-01 00:00:00Z"), Some(1_767_225_600));
    }

    /// The test whose name used to assert the opposite of what the code did.
    /// Every row below was accepted by the hand-rolled parser, and each one is
    /// an expiry wrong in the direction that costs a build mid-target.
    #[test]
    fn rfc3339_rejects_nonsense_rather_than_guessing() {
        for bad in [
            "",
            "next tuesday",
            "2026-01-01",
            "2026-13-01T00:00:00Z",
            "2026-01-32T00:00:00Z",
            "2026-01-01T25:00:00Z",
            // No offset at all: silently reinterpreting a local timestamp as
            // UTC moves the expiry by up to 14 hours.
            "2026-01-01T00:00:00",
            // Trailing junk after a valid instant.
            "2026-01-01T00:00:00zzzz",
            // Separators were never checked.
            "2026x01x01T00:00:00Z",
            "2026:01:01T00:00:00Z",
            // An impossible day rolled forward three days.
            "2026-02-30T00:00:00Z",
            "2026-02-31T00:00:00Z",
            // Offsets were never range-checked.
            "2026-01-01T00:00:00+99:00",
            "2026-01-01T00:00:00-99:99",
        ] {
            assert!(parse_rfc3339(bad).is_none(), "{bad:?} parsed");
        }
    }
}
