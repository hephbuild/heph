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

use crate::descriptor::{Acquire, Endpoint, Exchange, Identity, ProviderKind, SignIn, Source};
use crate::expiry::Expiry;
use crate::provider::{MintCtx, SecretProvider};
use crate::session;
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
    /// The ID token a stored session last produced, per
    /// `(issuer, client_id, audience)`.
    ///
    /// In memory only, for the life of the request — the assertion is a
    /// credential and nothing but the refresh token is ever written down. It
    /// exists because every refresh may *rotate* the stored token: without it a
    /// run with ten descriptors would burn ten rotations, and any two of them
    /// racing is how the next build finds a refresh token the IdP has already
    /// invalidated.
    ///
    /// The audience is in the key because it is in the token: two descriptors
    /// asking for different audiences must not share one assertion.
    assertions: tokio::sync::Mutex<BTreeMap<String, CachedAssertion>>,
}

/// One issuer's assertion and when it stops being usable.
struct CachedAssertion {
    token: String,
    expiry: Expiry,
}

/// The token is a live credential; only its clock is printable.
impl std::fmt::Debug for CachedAssertion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedAssertion")
            .field("expiry", &self.expiry)
            .finish_non_exhaustive()
    }
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
            assertions: tokio::sync::Mutex::default(),
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
        // CI first: an ambient workload identity is scoped to the job, so it is
        // strictly better than a session when both exist.
        let token = match AmbientIdentity::detect(ctx.env) {
            Some(ambient) => ambient
                .id_token(&self.client, identity.audience.as_deref())
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "{}",
                        ctx.redactor
                            .redact_str(&format!("secret {}: {e:#}", ctx.addr))
                    )
                })?,
            // Redacted on the same terms as the ambient arm: nothing in this
            // path carries a credential today, and the asymmetry is what a
            // future edit trips over.
            None => self
                .session_assertion(ctx, sign_in(acquire)?, identity.audience.as_deref())
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "{}",
                        ctx.redactor
                            .redact_str(&format!("secret {}: {e:#}", ctx.addr))
                    )
                })?,
        };

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
    /// Present the identity `heph auth login` established.
    ///
    /// The laptop half of the same descriptor CI runs: one refresh-token grant,
    /// producing an ID token that goes through the very same exchange pipeline.
    /// Nothing here is interactive — when there is no session, or the grant is
    /// gone, it fails immediately naming the command that fixes it.
    async fn session_assertion(
        &self,
        ctx: &MintCtx<'_>,
        sign_in: &SignIn,
        audience: Option<&str>,
    ) -> anyhow::Result<String> {
        let home = ctx.auth_home.ok_or_else(|| {
            anyhow::anyhow!(
                "no ambient workload identity, and no `$HOME` in which to keep a session"
            )
        })?;
        let key = session::key_of(&[
            &sign_in.issuer,
            &sign_in.client_id,
            audience.unwrap_or_default(),
        ]);

        // One integration at a time, for the whole request. The lock is held
        // across the refresh deliberately: it is what stops ten descriptors
        // minting at once from rotating one stored token ten times.
        let mut cache = self.assertions.lock().await;
        // `stale_at`, deliberately not `has_handout_headroom`. That predicate
        // asks "is this worth giving to a target that will then run for a
        // while", and conflating the two is the bug `crate::expiry` was split
        // to fix: this assertion never leaves `mint` — it is consumed by
        // `run_exchange` microseconds later — so a five-minute ID token, which
        // is Keycloak's default, would otherwise never hit the cache at all.
        if let Some(hit) = cache.get(&key)
            && !hit.expiry.stale_at(ctx.now)
        {
            return Ok(hit.token.clone());
        }

        let fresh = self.refresh_session(ctx, sign_in, home, audience).await?;
        let token = fresh.token.clone();
        cache.insert(key, fresh);
        Ok(token)
    }

    /// Trade the stored refresh token for an ID token.
    ///
    /// The locking, the re-read and the write-back of a rotated token all live
    /// in [`session::refresh_locked`] — the same call `heph auth login` and
    /// `heph auth status` make, so what a build does and what the CLI reports
    /// can never diverge.
    async fn refresh_session(
        &self,
        ctx: &MintCtx<'_>,
        sign_in: &SignIn,
        home: &std::path::Path,
        audience: Option<&str>,
    ) -> anyhow::Result<CachedAssertion> {
        let tokens =
            session::refresh_locked(&self.client, sign_in, home, audience, ctx.now, ctx.ctoken)
                .await
                .map_err(|e| {
                    // The ordinary end of a session, not an outage. Saying so is
                    // the difference between "log in again" and "the IdP is down".
                    if session::is_invalid_grant(&e) {
                        return anyhow::anyhow!(
                            "the session for {} is no longer valid — run `heph auth login`",
                            sign_in.issuer
                        );
                    }
                    e
                })?;

        let assertion = tokens.assertion()?.to_string();

        // Asking for an audience is not the same as getting one. Most IdPs
        // ignore an `audience` parameter on a refresh and hand back an ID token
        // whose `aud` is the client id — which the exchange then presents, and
        // the cloud rejects with an error naming itself and never the audience.
        // Better to fail here, where the two values can be shown side by side.
        if let Some(want) = audience {
            let got = crate::jwt::audiences_of(&assertion);
            if !got.is_empty() && !got.iter().any(|a| a == want) {
                anyhow::bail!(
                    "the descriptor asks for audience `{want}`, but {} issued an ID token for \
                     [{}] — this IdP does not honour an audience on the refresh grant. Either \
                     drop `audience` from the identity and let the exchange step set it, or use \
                     an `acquire` entry whose provider can mint the audience directly.",
                    sign_in.issuer,
                    got.join(", ")
                );
            }
        }

        // The ID token's own `exp` when it has one, so the cache above expires
        // with the token rather than on a guess.
        Ok(CachedAssertion {
            expiry: Expiry::resolve(ctx.now, None, Some(&assertion), None),
            token: assertion,
        })
    }

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

/// The `sign_in` on the entry this mint is running.
///
/// Reached from the [`Acquire`] rather than from anywhere ambient, which is the
/// whole point of the move: two secrets federating to two clouds sign in to two
/// Okta applications, and the one that applies is the one on the route that was
/// selected.
fn sign_in(acquire: &Acquire) -> anyhow::Result<&SignIn> {
    let Source::Oidc { sign_in } = &acquire.source else {
        // Unreachable through the registry, which dispatches on `Source::kind`.
        anyhow::bail!("the oidc provider was given a {:?} source", acquire.source);
    };
    sign_in.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "no ambient workload identity on this machine, and this route declares no \
             `sign_in`.\n  In GitHub Actions a missing identity means the job has no \
             `permissions: id-token: write` — without it the request variables are simply \
             absent, which is why this is not an authorization error.\n  On a laptop, either \
             give this route a `sign_in = {{\"issuer\": …, \"client_id\": …}}` and run `heph \
             auth login`, or add an `acquire` entry using a CLI you are already signed into."
        )
    })
}

/// Substitute `{identity-field}` and `{token}` into a URL, header or body.
///
/// A deliberately tiny substitution rather than a template language: this text
/// sits in a BUILD file next to a cache key, and every extra capability is
/// another thing that can differ between two machines.
fn interpolate(s: &str, id: &Identity, token: &str) -> String {
    let mut out = s.replace("{token}", token);
    // The named fields, then the open map. `{bucket}` and `{account}` used to
    // appear in the first list *and* work through the second; they were named
    // fields whose entire contribution was this substitution, which is why they
    // are now simply `params` entries like any other vendor vocabulary.
    for (key, value) in [
        ("role", id.role.as_deref()),
        ("registry", id.registry.as_deref()),
        ("machine", id.machine.as_deref()),
        ("profile", id.profile.as_deref()),
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
            registry: Some("ghcr.io".into()),
            params: BTreeMap::from([
                ("account".to_string(), "4711".to_string()),
                ("install".to_string(), "42".to_string()),
            ]),
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

    fn ctx_with<'a>(
        env: &'a (dyn Fn(&str) -> Option<String> + Send + Sync),
        token: &'a hcore::hasync::StdCancellationToken,
        redactor: &'a crate::redact::Redactor,
        auth_home: Option<&'a std::path::Path>,
    ) -> MintCtx<'a> {
        MintCtx {
            addr: "//infra/creds:ecr",
            now: std::time::SystemTime::UNIX_EPOCH,
            env,
            ctoken: token,
            request_id: "req",
            runner: None,
            cwd: std::path::Path::new("."),
            redactor,
            auth_home,
        }
    }

    fn oidc_acquire() -> Acquire {
        oidc_acquire_with(None)
    }

    fn oidc_acquire_with(sign_in: Option<SignIn>) -> Acquire {
        Acquire {
            when_env: None,
            source: Source::Oidc { sign_in },
            exchange: vec![Exchange::AwsSts { endpoint: None }],
            ttl: None,
        }
    }

    /// No ambient identity *and* no configured IdP: the message has to name
    /// both ways out rather than reporting an authorization failure that never
    /// happened.
    #[tokio::test]
    async fn with_no_identity_and_no_auth_block_both_routes_are_named() {
        let token = hcore::hasync::StdCancellationToken::new();
        let redactor = crate::redact::Redactor::inert();
        let env = env_of(&[]);
        let err = OidcProvider::new()
            .mint(
                &ctx_with(&env, &token, &redactor, None),
                &Identity::default(),
                &oidc_acquire(),
            )
            .await
            .expect_err("no identity");
        let msg = format!("{err:#}");
        assert!(msg.contains("id-token: write"), "{msg}");
        assert!(msg.contains("heph auth login"), "{msg}");
    }

    /// A configured workspace on a machine nobody has signed in on. It must
    /// fail *before* touching the network — a build that hangs on an
    /// unreachable IdP to discover it was never logged in is the worst of both.
    #[tokio::test]
    async fn a_configured_workspace_with_no_session_says_to_log_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sign_in = sign_in_cfg("https://unreachable.invalid");
        let token = hcore::hasync::StdCancellationToken::new();
        let redactor = crate::redact::Redactor::inert();
        let env = env_of(&[]);
        let err = OidcProvider::new()
            .mint(
                &ctx_with(&env, &token, &redactor, Some(dir.path())),
                &Identity::default(),
                &oidc_acquire_with(Some(sign_in)),
            )
            .await
            .expect_err("no session");
        let msg = format!("{err:#}");
        assert!(msg.contains("not signed in"), "{msg}");
        assert!(msg.contains("heph auth login"), "{msg}");
    }

    /// An expired refresh token is the ordinary end of a session, so it reads
    /// as one — and, like the case above, without a network round trip.
    #[tokio::test]
    async fn an_expired_session_says_to_log_in_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = sign_in_cfg("https://unreachable.invalid");
        crate::session::Session {
            issuer: config.issuer.clone(),
            client_id: config.client_id.clone(),
            refresh_token: "rt".into(),
            subject: Some("alice".into()),
            expires_at: Some(std::time::SystemTime::UNIX_EPOCH),
            updated_at: None,
        }
        .store(dir.path())
        .expect("store");

        let token = hcore::hasync::StdCancellationToken::new();
        let redactor = crate::redact::Redactor::inert();
        let env = env_of(&[]);
        let err = OidcProvider::new()
            .mint(
                &ctx_with(&env, &token, &redactor, Some(dir.path())),
                &Identity::default(),
                &oidc_acquire_with(Some(config)),
            )
            .await
            .expect_err("expired");
        let msg = format!("{err:#}");
        assert!(msg.contains("expired"), "{msg}");
        assert!(msg.contains("heph auth login"), "{msg}");
    }

    /// CI wins when both exist: an ambient identity is scoped to the job, so
    /// falling back to a developer's personal session there would run the build
    /// as the wrong principal.
    #[tokio::test]
    async fn an_ambient_identity_is_preferred_over_a_stored_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = sign_in_cfg("https://unreachable.invalid");
        crate::session::Session {
            issuer: config.issuer.clone(),
            client_id: config.client_id.clone(),
            refresh_token: "rt".into(),
            subject: Some("alice".into()),
            expires_at: None,
            updated_at: None,
        }
        .store(dir.path())
        .expect("store");

        let token = hcore::hasync::StdCancellationToken::new();
        let redactor = crate::redact::Redactor::inert();
        // An unroutable endpoint: reaching it at all is the assertion, and what
        // it reports is that the *ambient* route was taken.
        let env = env_of(&[
            ("ACTIONS_ID_TOKEN_REQUEST_URL", "http://127.0.0.1:1/token"),
            ("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "bearer"),
        ]);
        let err = OidcProvider::new()
            .mint(
                &ctx_with(&env, &token, &redactor, Some(dir.path())),
                &Identity::default(),
                &oidc_acquire(),
            )
            .await
            .expect_err("unroutable");
        let msg = format!("{err:#}");
        assert!(msg.contains("github actions"), "{msg}");
        assert!(!msg.contains("heph auth login"), "{msg}");
    }

    fn sign_in_cfg(issuer: &str) -> SignIn {
        SignIn {
            issuer: issuer.to_string(),
            client_id: "client".into(),
            scopes: vec!["openid".into(), "offline_access".into()],
            redirect_ports: Vec::new(),
        }
    }
}
