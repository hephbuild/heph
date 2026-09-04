//! The `oidc` provider: present an assertion, trade it for a credential.
//!
//! Two halves, configured independently, and that separation is what lets one
//! descriptor work in CI and on a laptop:
//!
//! - **The assertion.** In CI the workload identity is ambient — GitHub Actions
//!   hands a job an endpoint and a bearer token and will mint an ID token for
//!   any audience. Nothing is configured; it is either there or it is not.
//! - **The exchange.** An ordered pipeline of standard grants, each one taking
//!   what the last produced. See [`crate::descriptor::Exchange`].
//!
//! # Nothing here is interactive, ever
//!
//! A build that opens a browser at target 400 of 900 is an ambush for a human
//! and a silent hang for an agent. So this provider only ever *presents* an
//! identity it can already obtain non-interactively; establishing one in the
//! first place is a separate, explicit command. An expired or absent session
//! fails the build at once, saying what to run.

use crate::descriptor::{Acquire, Endpoint, Exchange, Identity, ProviderKind};
use crate::expiry::Expiry;
use crate::provider::{MintCtx, SecretProvider};
use crate::value::{Credential, SecretValue};
use anyhow::Context as _;
use std::collections::BTreeMap;
use std::time::Duration;

/// How long any single HTTP call to an IdP or cloud may take.
///
/// The same reasoning as the helper deadline: a build with nobody watching must
/// not hang on an unreachable endpoint, and a request that has not answered in
/// this long is not going to.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Where an ambient workload identity comes from, and how to ask it for a token.
///
/// A closed set only in the sense that each entry is a handful of lines; adding
/// a CI system is data, not design. What they have in common is the shape: an
/// endpoint, a bearer token to call it with, and an audience parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbientIdentity {
    /// Which CI system announced itself, for diagnostics.
    pub source: &'static str,
    url: String,
    bearer: String,
    audience_param: &'static str,
}

impl AmbientIdentity {
    /// Detect a workload identity from the environment.
    ///
    /// `None` is not an error here: a laptop legitimately has none, and the
    /// caller turns that into a message naming what to do rather than a bare
    /// failure.
    pub fn detect(env: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        // GitHub Actions. The two variables appear together and *only* when the
        // job declares `permissions: id-token: write` — without it they are
        // simply absent, which is why the diagnostic below names that rather
        // than reporting an authorization error.
        if let (Some(url), Some(bearer)) = (
            env("ACTIONS_ID_TOKEN_REQUEST_URL").filter(|v| !v.is_empty()),
            env("ACTIONS_ID_TOKEN_REQUEST_TOKEN").filter(|v| !v.is_empty()),
        ) {
            return Some(Self {
                source: "github actions",
                url,
                bearer,
                audience_param: "audience",
            });
        }
        None
    }

    /// Ask for an ID token with the given audience.
    async fn id_token(
        &self,
        client: &reqwest::Client,
        audience: Option<&str>,
    ) -> anyhow::Result<String> {
        let mut url = reqwest::Url::parse(&self.url)
            .with_context(|| format!("{} gave an unparseable token URL", self.source))?;
        if let Some(aud) = audience {
            url.query_pairs_mut().append_pair(self.audience_param, aud);
        }

        #[derive(serde::Deserialize)]
        struct TokenResponse {
            value: String,
        }

        let resp = client
            .get(url)
            .bearer_auth(&self.bearer)
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("request an ID token from {}", self.source))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "{} refused to mint an ID token ({status}): {body}",
                self.source
            );
        }
        let parsed: TokenResponse = serde_json::from_str(&body)
            .with_context(|| format!("parse the ID token {} returned", self.source))?;
        Ok(parsed.value)
    }
}

/// Acquire a workload identity token and run it through the exchange pipeline.
#[derive(Debug)]
pub struct OidcProvider {
    client: reqwest::Client,
}

impl Default for OidcProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OidcProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .unwrap_or_default(),
        }
    }
}

#[async_trait::async_trait]
impl SecretProvider for OidcProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Oidc
    }

    async fn mint(
        &self,
        ctx: &MintCtx<'_>,
        identity: &Identity,
        acquire: &Acquire,
    ) -> anyhow::Result<Credential> {
        let Some(ambient) = AmbientIdentity::detect(ctx.env) else {
            anyhow::bail!(
                "secret {}: no ambient workload identity on this machine.\n  In GitHub Actions \
                 this means the job is missing `permissions: id-token: write` — without it the \
                 request variables are simply absent, which is why this is not an authorization \
                 error.\n  On a laptop, give the descriptor an `acquire` entry that uses a \
                 vendor CLI you are already signed into.",
                ctx.addr
            );
        };

        let token = ambient
            .id_token(&self.client, identity.audience.as_deref())
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "{}",
                    ctx.redactor
                        .redact_str(&format!("secret {}: {e:#}", ctx.addr))
                )
            })?;

        // The assertion on its own is not a credential. Every step consumes what
        // the last produced, so a two-hop federation is a list rather than a
        // special case.
        let mut current = Credential::single(
            token,
            Expiry::resolve(ctx.now, None, None, acquire.ttl_duration()?),
        );
        for (i, step) in acquire.exchange.iter().enumerate() {
            current = self
                .run_exchange(ctx, identity, acquire, step, &current)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "{}",
                        ctx.redactor
                            .redact_str(&format!("secret {}: exchange[{i}]: {e:#}", ctx.addr))
                    )
                })?;
        }
        Ok(current)
    }
}

impl OidcProvider {
    /// Resolve an [`Endpoint`] to a concrete token endpoint.
    ///
    /// The discovery form fetches `{issuer}/.well-known/openid-configuration`
    /// and reads `token_endpoint` — and checks `grant_types_supported` while it
    /// is there, so asking a server for a grant it does not implement fails by
    /// name rather than as a bare `400 unsupported_grant_type` from a URL
    /// nobody recognises.
    async fn token_endpoint(
        &self,
        at: Endpoint<'_>,
        grant: Option<&str>,
    ) -> anyhow::Result<String> {
        // Match on the variant rather than on `discovery_url()` returning
        // `None`, so the literal case is expressed by the type rather than by a
        // claim about another function's behaviour.
        let discovery = match at {
            Endpoint::Literal(url) => return Ok(url.to_string()),
            Endpoint::Discover(issuer) => {
                format!(
                    "{}{}",
                    issuer.trim_end_matches('/'),
                    crate::descriptor::OIDC_DISCOVERY_PATH
                )
            }
        };

        #[derive(serde::Deserialize)]
        struct Metadata {
            token_endpoint: Option<String>,
            #[serde(default)]
            grant_types_supported: Vec<String>,
        }

        let resp = self
            .client
            .get(&discovery)
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("fetch {discovery}"))?;
        if !resp.status().is_success() {
            anyhow::bail!(
                "{discovery} returned {} — the issuer publishes no discovery document, so give \
                 the exchange an explicit `endpoint` instead",
                resp.status()
            );
        }
        let meta: Metadata = resp
            .json()
            .await
            .with_context(|| format!("parse the metadata at {discovery}"))?;

        if let Some(g) = grant
            && !meta.grant_types_supported.is_empty()
            && !meta.grant_types_supported.iter().any(|s| s == g)
        {
            anyhow::bail!(
                "the server at {discovery} does not support the `{g}` grant. It advertises: \
                 {}. This is the check discovery exists for: the alternative is a bare 400 from \
                 an endpoint nobody recognises.",
                meta.grant_types_supported.join(", ")
            );
        }

        meta.token_endpoint
            .ok_or_else(|| anyhow::anyhow!("the metadata at {discovery} names no `token_endpoint`"))
    }

    async fn run_exchange(
        &self,
        ctx: &MintCtx<'_>,
        identity: &Identity,
        acquire: &Acquire,
        step: &Exchange,
        input: &Credential,
    ) -> anyhow::Result<Credential> {
        let subject = input.resolve_pointer("$.")?.expose().to_string();
        let ttl = acquire.ttl_duration()?;

        match step {
            Exchange::TokenExchange {
                audience,
                resource,
                scope,
                requested_token_type,
                ..
            } => {
                let at = step
                    .endpoint()?
                    .context("token_exchange has no destination")?;
                let url = self.token_endpoint(at, step.grant_type()).await?;
                let mut form: Vec<(&str, String)> = vec![
                    (
                        "grant_type",
                        "urn:ietf:params:oauth:grant-type:token-exchange".to_string(),
                    ),
                    (
                        "subject_token_type",
                        "urn:ietf:params:oauth:token-type:jwt".to_string(),
                    ),
                    ("subject_token", subject),
                ];
                // The descriptor's audience is the identity's; a step may
                // override it for a hop that addresses something else.
                if let Some(a) = audience.as_deref().or(identity.audience.as_deref()) {
                    form.push(("audience", a.to_string()));
                }
                if let Some(r) = resource {
                    form.push(("resource", r.clone()));
                }
                let scopes = if scope.is_empty() {
                    &identity.scope
                } else {
                    scope
                };
                if !scopes.is_empty() {
                    form.push(("scope", scopes.join(" ")));
                }
                form.push((
                    "requested_token_type",
                    requested_token_type
                        .clone()
                        .unwrap_or_else(|| "urn:ietf:params:oauth:token-type:access_token".into()),
                ));
                self.oauth_post(&url, &form, ctx, ttl).await
            }

            Exchange::JwtBearer { scope, .. } => {
                let at = step.endpoint()?.context("jwt_bearer has no destination")?;
                let url = self.token_endpoint(at, step.grant_type()).await?;
                let mut form: Vec<(&str, String)> = vec![
                    (
                        "grant_type",
                        "urn:ietf:params:oauth:grant-type:jwt-bearer".to_string(),
                    ),
                    ("assertion", subject),
                ];
                let scopes = if scope.is_empty() {
                    &identity.scope
                } else {
                    scope
                };
                if !scopes.is_empty() {
                    form.push(("scope", scopes.join(" ")));
                }
                self.oauth_post(&url, &form, ctx, ttl).await
            }

            Exchange::ClientCredentials { scope, .. } => {
                let at = step
                    .endpoint()?
                    .context("client_credentials has no destination")?;
                let url = self.token_endpoint(at, step.grant_type()).await?;
                let mut form: Vec<(&str, String)> =
                    vec![("grant_type", "client_credentials".to_string())];
                let scopes = if scope.is_empty() {
                    &identity.scope
                } else {
                    scope
                };
                if !scopes.is_empty() {
                    form.push(("scope", scopes.join(" ")));
                }
                self.oauth_post(&url, &form, ctx, ttl).await
            }

            Exchange::AwsSts { endpoint } => {
                self.assume_role_with_web_identity(identity, endpoint.as_deref(), &subject, ctx.now)
                    .await
            }

            Exchange::Http {
                url,
                method,
                headers,
                body,
                fields,
            } => {
                self.vendor_call(
                    url, method, headers, body, fields, identity, &subject, ctx.now,
                )
                .await
            }
        }
    }

    /// POST an OAuth form and read the standard token response.
    async fn oauth_post(
        &self,
        url: &str,
        form: &[(&str, String)],
        ctx: &MintCtx<'_>,
        ttl: Option<Duration>,
    ) -> anyhow::Result<Credential> {
        #[derive(serde::Deserialize)]
        struct TokenResponse {
            access_token: Option<String>,
            id_token: Option<String>,
            expires_in: Option<u64>,
        }

        let resp = self
            .client
            .post(url)
            .form(form)
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("post to {url}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            // The body is an OAuth error object, which names the problem far
            // better than the status does — and it is redacted like anything
            // else, because a failed exchange can echo what it was given.
            anyhow::bail!(
                "{url} returned {status}: {}",
                ctx.redactor.redact_str(&text)
            );
        }
        let parsed: TokenResponse = serde_json::from_str(&text)
            .with_context(|| format!("parse the response from {url}"))?;
        let value = parsed
            .access_token
            .or(parsed.id_token)
            .context("the response carried neither an access_token nor an id_token")?;

        // `expires_in` is the protocol speaking, so it outranks a declared ttl.
        let from_protocol = parsed
            .expires_in
            .and_then(|s| ctx.now.checked_add(Duration::from_secs(s)));
        Ok(Credential::single(
            value.clone(),
            Expiry::resolve(ctx.now, from_protocol, Some(&value), ttl),
        ))
    }

    /// `sts:AssumeRoleWithWebIdentity`.
    ///
    /// A query API returning XML rather than an OAuth grant, which is why it
    /// keeps a variant of its own: expressing it as a raw HTTP call would put
    /// XML parsing in a BUILD file.
    async fn assume_role_with_web_identity(
        &self,
        identity: &Identity,
        endpoint: Option<&str>,
        token: &str,
        now: std::time::SystemTime,
    ) -> anyhow::Result<Credential> {
        let role = identity.role.as_deref().context(
            "aws_sts needs a `role` on the descriptor — it is the identity being assumed, so it \
             belongs in the hashed half rather than in the exchange",
        )?;
        let url = endpoint.unwrap_or("https://sts.amazonaws.com/");
        let session = format!("heph-{}", now_secs(now));

        let resp = self
            .client
            .post(url)
            .form(&[
                ("Action", "AssumeRoleWithWebIdentity"),
                ("Version", "2011-06-15"),
                ("RoleArn", role),
                ("RoleSessionName", &session),
                ("WebIdentityToken", token),
            ])
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("post to {url}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("{url} returned {status}: {}", first_lines(&body, 6));
        }

        let mut fields = BTreeMap::new();
        for (tag, key) in [
            ("AccessKeyId", "AccessKeyId"),
            ("SecretAccessKey", "SecretAccessKey"),
            ("SessionToken", "SessionToken"),
        ] {
            let v = xml_text(&body, tag).with_context(|| {
                format!(
                    "the STS response carried no <{tag}>: {}",
                    first_lines(&body, 4)
                )
            })?;
            fields.insert(key.to_string(), SecretValue::new(v));
        }
        let expires =
            xml_text(&body, "Expiration").and_then(|s| crate::protocol::parse_rfc3339(&s));

        Ok(Credential {
            fields,
            expiry: Expiry::resolve(now, expires, None, None),
        })
    }

    /// A vendor REST call: a URL, headers, a body, and pointers naming what to
    /// keep.
    ///
    /// The escape hatch that keeps heph out of the business of knowing vendors.
    /// A GitHub App installation token and a Cloudflare R2 temporary credential
    /// are each one POST and a couple of pointers.
    #[expect(clippy::too_many_arguments, reason = "one call, all of it declared")]
    async fn vendor_call(
        &self,
        url: &str,
        method: &Option<String>,
        headers: &BTreeMap<String, String>,
        body: &Option<String>,
        fields: &BTreeMap<String, String>,
        identity: &Identity,
        token: &str,
        now: std::time::SystemTime,
    ) -> anyhow::Result<Credential> {
        let url = interpolate(url, identity, token);
        let method = method.as_deref().unwrap_or("POST").to_uppercase();
        let m = reqwest::Method::from_bytes(method.as_bytes())
            .with_context(|| format!("`{method}` is not an HTTP method"))?;

        let mut req = self.client.request(m, &url).timeout(HTTP_TIMEOUT);
        // The previous step's value is presented as a bearer unless the
        // descriptor said otherwise, because that is what every vendor endpoint
        // this replaces expects.
        if !headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("authorization"))
        {
            req = req.bearer_auth(token);
        }
        for (k, v) in headers {
            req = req.header(k.as_str(), interpolate(v, identity, token));
        }
        if let Some(b) = body {
            req = req
                .header("content-type", "application/json")
                .body(interpolate(b, identity, token));
        }

        let resp = req.send().await.with_context(|| format!("call {url}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("{url} returned {status}: {}", first_lines(&text, 6));
        }
        let json: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("parse the response from {url}"))?;

        let mut out = BTreeMap::new();
        // Default to the whole-token convention when the descriptor named no
        // fields, so the common single-value case needs no boilerplate.
        let wanted: BTreeMap<String, String> = if fields.is_empty() {
            BTreeMap::from([(Credential::PRIMARY.to_string(), "/token".to_string())])
        } else {
            fields.clone()
        };
        for (name, pointer) in &wanted {
            let v = json
                .pointer(pointer)
                .and_then(|v| v.as_str())
                .with_context(|| {
                    format!(
                        "the response from {url} has nothing at {pointer:?} for field {name:?}; it \
                     carried: {}",
                        top_level_keys(&json)
                    )
                })?;
            out.insert(name.clone(), SecretValue::new(v.to_string()));
        }

        let expires = json
            .pointer("/expires_at")
            .or_else(|| json.pointer("/expires"))
            .and_then(|v| v.as_str())
            .and_then(crate::protocol::parse_rfc3339);
        let primary = out.get(Credential::PRIMARY).map(|v| v.expose().to_string());
        Ok(Credential {
            fields: out,
            expiry: Expiry::resolve(now, expires, primary.as_deref(), None),
        })
    }
}

/// Substitute `{identity-field}` and `{token}` into a URL, header or body.
///
/// A deliberately tiny substitution rather than a template language: this text
/// sits in a BUILD file next to a cache key, and every extra capability is
/// another thing that can differ between two machines.
fn interpolate(s: &str, id: &Identity, token: &str) -> String {
    let mut out = s.replace("{token}", token);
    for (key, value) in [
        ("account", id.account.as_deref()),
        ("region", id.region.as_deref()),
        ("bucket", id.bucket.as_deref()),
        ("registry", id.registry.as_deref()),
        ("machine", id.machine.as_deref()),
        ("role", id.role.as_deref()),
    ] {
        if let Some(v) = value {
            out = out.replace(&format!("{{{key}}}"), v);
        }
    }
    for (key, value) in &id.params {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    out
}

fn now_secs(now: std::time::SystemTime) -> u64 {
    now.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// The text of the first `<tag>…</tag>`.
///
/// Hand-rolled rather than an XML dependency: this reads three known tags out of
/// one AWS response shape, and a parser would be a whole ecosystem for that.
fn xml_text(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)?.checked_add(open.len())?;
    let end = body.get(start..)?.find(&close)?.checked_add(start)?;
    Some(body.get(start..end)?.to_string())
}

fn first_lines(s: &str, n: usize) -> String {
    s.lines().take(n).collect::<Vec<_>>().join("\n")
}

fn top_level_keys(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) => m.keys().cloned().collect::<Vec<_>>().join(", "),
        other => format!("a {}", type_name(other)),
    }
}

fn type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| {
            owned
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    /// Both variables appear together and *only* when the job declares
    /// `permissions: id-token: write`; without it they are simply absent.
    #[test]
    fn github_actions_is_detected_only_when_both_variables_are_present() {
        let full = env_of(&[
            ("ACTIONS_ID_TOKEN_REQUEST_URL", "https://x/token"),
            ("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "bearer"),
        ]);
        let got = AmbientIdentity::detect(&full).expect("detected");
        assert_eq!(got.source, "github actions");

        // The half-configured shapes, which are what a missing `permissions:`
        // block actually looks like.
        for partial in [
            vec![("ACTIONS_ID_TOKEN_REQUEST_URL", "https://x/token")],
            vec![("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "bearer")],
            vec![
                ("ACTIONS_ID_TOKEN_REQUEST_URL", ""),
                ("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "bearer"),
            ],
            vec![],
        ] {
            assert!(
                AmbientIdentity::detect(&env_of(&partial)).is_none(),
                "{partial:?} was treated as an identity"
            );
        }
    }

    /// The substitution is deliberately tiny; what matters is that it reaches
    /// the open `params` map, since that is where vendor-shaped identity lives.
    #[test]
    fn interpolation_covers_named_fields_the_token_and_params() {
        let id = Identity {
            account: Some("4711".into()),
            registry: Some("ghcr.io".into()),
            params: BTreeMap::from([("install".to_string(), "42".to_string())]),
            ..Identity::default()
        };
        assert_eq!(
            interpolate(
                "https://api.example/{account}/{registry}/installations/{install}?t={token}",
                &id,
                "TOK"
            ),
            "https://api.example/4711/ghcr.io/installations/42?t=TOK"
        );
        // An unknown placeholder is left alone rather than blanked: a URL with a
        // literal brace is a far better diagnostic than one silently missing a
        // path segment.
        assert_eq!(interpolate("{nope}", &id, "TOK"), "{nope}");
    }

    #[test]
    fn xml_text_reads_the_fields_sts_actually_returns() {
        let body = r#"<AssumeRoleWithWebIdentityResponse><Credentials>
            <AccessKeyId>ASIAEXAMPLE</AccessKeyId>
            <SecretAccessKey>s3cret</SecretAccessKey>
            <SessionToken>tok</SessionToken>
            <Expiration>2026-01-01T00:00:00Z</Expiration>
        </Credentials></AssumeRoleWithWebIdentityResponse>"#;
        assert_eq!(
            xml_text(body, "AccessKeyId").as_deref(),
            Some("ASIAEXAMPLE")
        );
        assert_eq!(xml_text(body, "SessionToken").as_deref(), Some("tok"));
        assert!(xml_text(body, "Missing").is_none());
    }

    /// A laptop has no ambient identity, and the message has to say what to do
    /// rather than reporting an authorization failure that did not happen.
    #[tokio::test]
    async fn no_ambient_identity_names_the_two_ways_out() {
        let token = hcore::hasync::StdCancellationToken::new();
        let redactor = crate::redact::Redactor::inert();
        let env = env_of(&[]);
        let ctx = MintCtx {
            addr: "//infra/creds:ecr",
            now: std::time::SystemTime::UNIX_EPOCH,
            env: &env,
            ctoken: &token,
            request_id: "req",
            runner: None,
            cwd: std::path::Path::new("."),
            redactor: &redactor,
        };
        let err = OidcProvider::new()
            .mint(
                &ctx,
                &Identity::default(),
                &Acquire {
                    when_env: None,
                    source: crate::descriptor::Source::Oidc {},
                    exchange: vec![Exchange::AwsSts { endpoint: None }],
                    ttl: None,
                },
            )
            .await
            .expect_err("no ambient identity");
        let msg = format!("{err:#}");
        assert!(msg.contains("id-token: write"), "{msg}");
        assert!(msg.contains("acquire"), "{msg}");
    }
}
