//! `heph auth login`: giving a laptop an identity of the same kind CI has.
//!
//! CI hands a job an ambient workload identity; a laptop has none. Without an
//! answer here [`crate::oidc`] is a CI-only feature and every developer keeps
//! the long-lived tokens in `~/.zshrc` that this whole design exists to remove
//! — two systems instead of one, and the weaker one is the one humans use.
//!
//! # What is stored, and where
//!
//! **One refresh token**, in a mode-0600 file under `$HOME/.heph/auth`, keyed by
//! issuer and client id so two workspaces on one IdP share a session and two
//! IdPs never collide. That is the entire durable footprint: no cloud
//! credentials, no access tokens, nothing under `~/.aws` or `~/.docker`, and
//! nothing inside a workspace, a `.heph3`, or a sandbox.
//!
//! A file on **all three supported targets**, deliberately, rather than the OS
//! keychain where one exists. A keychain is stronger at rest on macOS and on a
//! Linux desktop — and it makes *where a credential lives* depend on whether
//! the machine happens to have a D-Bus session, which is how "works on my
//! desktop, prompts forever on the build box" is born. One code path and one
//! failure mode was judged worth more than the stronger locker on two of three
//! configurations. The trade is recorded rather than assumed: a token here is
//! readable by any process running as you.
//!
//! # No version field, deliberately
//!
//! [`Session`] carries no schema version, and does not need one. It is
//! per-user and per-machine rather than a shared team resource, it holds
//! nothing but a refresh token that is trivially reacquired, and any parse
//! failure already names the recovery: delete it and run `heph auth login`. A
//! version field would buy a migration for data nobody would miss.
//!
//! The one thing that reasoning cannot catch is **reusing an existing key with
//! a new type or a new meaning** — an old binary would read it as the old thing
//! and be confidently wrong. Add a new optional field instead; a version bump
//! is not the alternative, a different key is.
//!
//! # Why the interactive flow is not in the provider
//!
//! A build that opens a browser at target 400 of 900 is an ambush for a human
//! and a silent hang for an agent. So establishing a session is an explicit
//! command, and the provider only ever *presents* an identity that already
//! exists — failing at once, naming `heph auth login`, when one does not.

use anyhow::Context as _;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How long any single call to the IdP may take.
///
/// The same reasoning as [`crate::oidc::HTTP_TIMEOUT`]: a request that has not
/// answered in this long is not going to.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long one connection to the loopback socket may take to send its request
/// line.
///
/// A browser's speculative preconnect opens a socket and sends nothing.
/// Without this the login would wait out its whole [`LOGIN_TIMEOUT`] on it and
/// then blame the redirect registration.
pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// A cap on the request line, so a local peer sending bytes without a newline
/// cannot grow the buffer without limit. Far above any real authorization
/// callback, which is a code, a state, and a path.
const MAX_REQUEST_LINE: u64 = 16 * 1024;

/// How long to wait at the loopback socket for the browser to come back.
///
/// Long enough for a password, a push notification and a hardware key; short
/// enough that a login nobody completed does not hold a terminal forever.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);

/// What to say when a workspace configures no identity provider.
///
/// One message, used by both the CLI and the build path — the same condition
/// reported two different ways is how a user ends up believing they are two
/// different problems. `{path}` is substituted with the resolved config file.
/// The message, with the config file's path filled in.
pub fn no_auth_block(path: &str) -> String {
    NO_AUTH_BLOCK.replace("{path}", path)
}

const NO_AUTH_BLOCK: &str = "\
this workspace has no `auth:` block in {path}, and this machine has no ambient \
workload identity.\n\
  In GitHub Actions a missing identity means the job has no `permissions: id-token: write` — \
without it the request variables are simply absent, which is why this is not an authorization \
error.\n\
  On a laptop, add an `auth:` block naming the provider to sign in to, then run `heph auth \
login`:\n\
\n    auth:\n      issuer: https://org.okta.com/oauth2/default\n      clientId: <the registered \
public client>\n\
\n  Whoever administers your IdP supplies both, by registering heph as a public client. The \
block holds no secret — a CLI is a public client (RFC 8252 §8.5) and PKCE replaces the client \
secret — so it belongs in version control.\n\
  Or give the descriptor an `acquire` entry that uses a CLI you are already signed into.";

/// The HTTP client the login flow uses.
///
/// Built here rather than by the caller so the CLI needs no HTTP dependency of
/// its own, and so every call in the flow carries the same deadline.
pub fn http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("build an HTTP client")
}

/// Re-exported from [`crate::descriptor`], where the declaration lives.
pub use crate::descriptor::SignIn;

/// A stored session: one refresh token, and enough to know whose it is.
///
/// No derived `Debug`: this holds a live credential, and the invariant
/// [`crate::value`] states applies here too — a `Debug` that prints a token
/// turns the next `tracing::debug!(?session)` anyone writes into a leak.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub issuer: String,
    pub client_id: String,
    /// The only durable secret heph holds anywhere.
    pub refresh_token: String,
    /// Who signed in, from the ID token's `sub`.
    ///
    /// Read with [`crate::jwt::subject_of_trusted`], whose contract this
    /// satisfies: the token came to this process over TLS, from the issuer
    /// named above, in response to a request this process made. It is a label
    /// for `heph auth status`, never an authorization decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// When the refresh token itself stops working, if the IdP said so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<SystemTime>,
    /// When this session was established or last rotated, for `status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<SystemTime>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("subject", &self.subject)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// The per-user heph directory, `$HOME/.heph`.
    ///
    /// Deliberately *not* the workspace's `.heph3`: a session is a property of
    /// the person, not of a checkout. Putting it in the workspace would mean
    /// logging in once per worktree, and `heph clean` would sign you out.
    pub fn home() -> anyhow::Result<PathBuf> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .context("$HOME is not set, so there is nowhere to keep a session")?;
        Ok(PathBuf::from(home).join(".heph"))
    }

    /// Where a session rests: one file per `(issuer, client_id)`.
    ///
    /// Hashed rather than spelled out, because an issuer is a URL and a client
    /// id is opaque — neither is a filename.
    pub fn path(home: &Path, issuer: &str, client_id: &str) -> PathBuf {
        let digest = xxhash_rust::xxh3::xxh3_64(key_of(&[issuer, client_id]).as_bytes());
        home.join("auth").join(format!("{digest:016x}.json"))
    }

    /// Read the session for an issuer, if there is one.
    ///
    /// Absence is `Ok(None)`: a machine that has never logged in is a normal
    /// state, and the caller turns it into a message naming what to run.
    pub fn load(home: &Path, issuer: &str, client_id: &str) -> anyhow::Result<Option<Self>> {
        let path = Self::path(home, issuer, client_id);
        match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).with_context(|| {
                format!(
                    "parse {} — delete it and run `heph auth login` again",
                    path.display()
                )
            }),
        }
    }

    /// Write it atomically: directory 0700, file 0600.
    ///
    /// A temp file in the same directory, then `rename(2)`. The obvious
    /// truncate-and-write is a real hazard here and not a theoretical one: this
    /// is called immediately *after* the old refresh token has been spent, so a
    /// Ctrl-C, an ENOSPC or an OOM kill between the truncate and the write
    /// leaves the old token gone from disk and already dead at the IdP, with
    /// the new one never written. The user is locked out by an interruption.
    ///
    /// The mode is set on the *handle* rather than on the path, so there is no
    /// instant at which the token exists at a name with the umask's mode.
    pub fn store(&self, home: &Path) -> anyhow::Result<PathBuf> {
        use std::os::unix::fs::PermissionsExt as _;

        let path = Self::path(home, &self.issuer, &self.client_id);
        let dir = path.parent().context("session path has no parent")?;
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        set_mode(dir, 0o700)?;

        // Same directory, so the rename is within one filesystem and therefore
        // atomic.
        let mut tmp = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("create a temp file in {}", dir.display()))?;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .context("restrict the session file to its owner")?;
        tmp.write_all(&serde_json::to_vec_pretty(self).context("serialize the session")?)
            .context("write the session")?;
        // Durable before it is visible: a rename that lands ahead of the data
        // would publish an empty file across a crash.
        tmp.as_file().sync_all().context("flush the session")?;
        tmp.persist(&path)
            .map_err(|e| anyhow::Error::new(e.error))
            .with_context(|| format!("replace {}", path.display()))?;
        Ok(path)
    }

    /// Forget it locally. Returns whether there was one.
    ///
    /// Local only — no server-side revocation is implied, and `heph auth
    /// logout` says so rather than letting anyone believe the token is dead
    /// everywhere.
    pub fn forget(home: &Path, issuer: &str, client_id: &str) -> anyhow::Result<bool> {
        let path = Self::path(home, issuer, client_id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
        }
    }

    /// The lock serialising refresh across concurrent `heph` processes.
    ///
    /// Most IdPs rotate the refresh token on every use and invalidate the one
    /// presented. Two builds refreshing at once would each store a token the
    /// other's refresh had already killed, and the *next* build — the one that
    /// changed nothing — would be the one that fails. A separate file rather
    /// than the session itself, so the lock never competes with the read.
    pub fn lock_path(home: &Path, issuer: &str, client_id: &str) -> PathBuf {
        Self::path(home, issuer, client_id).with_extension("lock")
    }

    /// Whether the refresh token itself is past the life the IdP stated.
    pub fn expired_at(&self, now: SystemTime) -> bool {
        self.expires_at.is_some_and(|at| now >= at)
    }
}

/// Join fields so that no two distinct tuples can produce the same string.
///
/// Length-prefixed rather than delimited. A delimiter is only unambiguous if
/// no field can contain it, and these come from YAML — which has block scalars,
/// so the invariant would be assumed rather than true. Two identities colliding
/// here means one person's session file answering for another's.
pub(crate) fn key_of(fields: &[&str]) -> String {
    let mut out = String::new();
    for f in fields {
        out.push_str(&f.len().to_string());
        out.push(':');
        out.push_str(f);
    }
    out
}

fn set_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", path.display()))
}

/// A PKCE verifier and its S256 challenge (RFC 7636).
///
/// What replaces a client secret for a public client. The verifier stays in
/// this process until the token request; only its SHA-256 travels with the
/// authorization request, so a code intercepted in the redirect is useless to
/// whoever caught it.
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> anyhow::Result<Self> {
        // 32 bytes of OS randomness → 43 base64url characters, at the bottom of
        // RFC 7636's 43..=128 range and well past its entropy floor.
        let mut bytes = [0u8; 32];
        fill_random(&mut bytes)?;
        Ok(Self::from_verifier(URL_SAFE_NO_PAD.encode(bytes)))
    }

    fn from_verifier(verifier: String) -> Self {
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(verifier.as_bytes());
        Self {
            challenge: URL_SAFE_NO_PAD.encode(digest),
            verifier,
        }
    }
}

/// Opaque state binding the callback to the request that started it.
pub fn random_state() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    fill_random(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// `/dev/urandom` rather than a RNG crate: this is the only randomness the
/// feature needs and every supported target has it.
///
/// A failure here is an error, never a fallback. A predictable verifier defeats
/// the entire point of PKCE, so a machine that cannot produce randomness must
/// not be handed something that merely looks random.
fn fill_random(buf: &mut [u8]) -> anyhow::Result<()> {
    use std::io::Read as _;
    let mut f = std::fs::File::open("/dev/urandom").context(
        "open /dev/urandom — without a source of randomness the PKCE verifier would be \
         guessable, so this cannot fall back",
    )?;
    f.read_exact(buf).context("read /dev/urandom")
}

/// The endpoints an IdP publishes, from OIDC Discovery.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Metadata {
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    pub device_authorization_endpoint: Option<String>,
    #[serde(default)]
    pub grant_types_supported: Vec<String>,
    #[serde(default)]
    pub code_challenge_methods_supported: Vec<String>,
}

impl Metadata {
    /// Fetch `{issuer}/.well-known/openid-configuration`.
    pub async fn discover(client: &reqwest::Client, issuer: &str) -> anyhow::Result<Self> {
        let url = format!(
            "{}{}",
            issuer.trim_end_matches('/'),
            crate::descriptor::OIDC_DISCOVERY_PATH
        );
        let resp = client
            .get(&url)
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("fetch {url}"))?;
        if !resp.status().is_success() {
            anyhow::bail!(
                "{url} returned {} — check `auth.issuer` in `.hephconfig`; it is the base URL, \
                 not an endpoint",
                resp.status()
            );
        }
        resp.json()
            .await
            .with_context(|| format!("parse the metadata at {url}"))
    }

    pub fn authorization(&self) -> anyhow::Result<String> {
        self.authorization_endpoint
            .clone()
            .context("the issuer's metadata names no `authorization_endpoint`")
    }

    pub fn token(&self) -> anyhow::Result<String> {
        self.token_endpoint
            .clone()
            .context("the issuer's metadata names no `token_endpoint`")
    }

    pub fn device_authorization(&self) -> anyhow::Result<String> {
        self.device_authorization_endpoint.clone().context(
            "the issuer publishes no `device_authorization_endpoint`, so it does not support the \
             device flow — sign in on a machine with a browser, or enable the device grant on \
             the client",
        )
    }

    /// Whether the server is known *not* to support PKCE-S256.
    ///
    /// `code_challenge_methods_supported` is optional and plenty of working
    /// servers omit it, so its absence proves nothing; only an explicit list
    /// that excludes S256 is evidence. Warned about rather than refused, for
    /// the same reason.
    pub fn lacks_s256(&self) -> bool {
        !self.code_challenge_methods_supported.is_empty()
            && !self
                .code_challenge_methods_supported
                .iter()
                .any(|m| m == "S256")
    }
}

/// What a token endpoint returned. Every field here is a live credential.
#[derive(Clone, Default, serde::Deserialize)]
pub struct TokenSet {
    pub access_token: Option<String>,
    pub id_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
    /// Non-standard but widely emitted (Keycloak, Okta); when absent the
    /// session simply records no refresh expiry, which is the honest answer.
    pub refresh_expires_in: Option<u64>,
}

/// Counts and presence, never values.
impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &self.access_token.is_some())
            .field("id_token", &self.id_token.is_some())
            .field("refresh_token", &self.refresh_token.is_some())
            .field("expires_in", &self.expires_in)
            .field("refresh_expires_in", &self.refresh_expires_in)
            .finish()
    }
}

impl TokenSet {
    /// The assertion `oidc` presents downstream.
    ///
    /// An ID token, because that is what a federated exchange verifies — an
    /// opaque access token is not a JWT and no `token-exchange` endpoint will
    /// take one.
    pub fn assertion(&self) -> anyhow::Result<&str> {
        self.id_token
            .as_deref()
            .context("the IdP returned no `id_token` — add `openid` to `auth.scopes`")
    }
}

/// A token endpoint that said no.
///
/// Typed rather than a string because one OAuth error code changes what the
/// caller should do: `invalid_grant` on a refresh means the session is over and
/// a browser is required, which is a different outcome from "the IdP is down".
/// Everything else is reported as-is.
#[derive(Debug)]
pub struct TokenError {
    pub url: String,
    pub status: u16,
    /// The OAuth `error` code, when the body carried one.
    pub code: Option<String>,
    body: String,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} returned {}: {}", self.url, self.status, self.body)
    }
}

impl std::error::Error for TokenError {}

impl TokenError {
    /// RFC 6749 §5.2: the grant is expired, revoked, or already spent. For a
    /// refresh token that is the ordinary end of a session, not an outage.
    pub fn is_invalid_grant(&self) -> bool {
        self.code.as_deref() == Some("invalid_grant")
    }
}

/// Whether anything in an error chain was an `invalid_grant`.
pub fn is_invalid_grant(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<TokenError>()
            .is_some_and(TokenError::is_invalid_grant)
    })
}

/// POST a form to a token endpoint and read the response.
///
/// The error body is included because an OAuth error object names the problem
/// far better than the status does. It carries no credential: the request did
/// not succeed, so there is no token in it.
pub async fn token_post(
    client: &reqwest::Client,
    url: &str,
    form: &[(&str, String)],
) -> anyhow::Result<TokenSet> {
    let resp = client
        .post(url)
        .form(form)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("post to {url}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow::Error::new(TokenError {
            url: url.to_string(),
            status: status.as_u16(),
            code: serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("error")?.as_str().map(str::to_string)),
            body: first_lines(&text, 5),
        }));
    }
    serde_json::from_str(&text).with_context(|| format!("parse the response from {url}"))
}

/// Refresh the stored session, single-flighted across every heph on this
/// machine, and write back whatever the IdP rotated.
///
/// The one call the build path makes, and the same one `heph auth login` and
/// `heph auth status` make to find out whether a session still works. Nothing
/// here is interactive.
///
/// The lock is the point. Most IdPs **rotate** the refresh token on use and
/// invalidate the one presented, so two builds refreshing at once would each
/// store a token the other had already spent — and the failure would land on
/// the *next* build, the one that changed nothing. The session is re-read
/// *under* the lock for the same reason.
/// `audience` is forwarded when the caller wants a specific one. Whether the
/// IdP honours it is not knowable from here — the caller checks the `aud` claim
/// it actually got, because a token with the wrong audience fails far away,
/// with an error that names the cloud and never the audience.
pub async fn refresh_locked(
    client: &reqwest::Client,
    cfg: &SignIn,
    home: &Path,
    audience: Option<&str>,
    now: SystemTime,
    ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
) -> anyhow::Result<TokenSet> {
    use hlock::hlock::traits::Lock as _;

    let lock_path = Session::lock_path(home, &cfg.issuer, &cfg.client_id);
    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let _guard = hlock::hlock::flock::FLock::new(&lock_path)
        .lock(ctoken)
        .await
        .with_context(|| format!("wait for {}", lock_path.display()))?;

    let mut session = Session::load(home, &cfg.issuer, &cfg.client_id)?.ok_or_else(|| {
        anyhow::anyhow!("not signed in to {} — run `heph auth login`", cfg.issuer)
    })?;
    if session.expired_at(now) {
        anyhow::bail!(
            "the session for {} has expired — run `heph auth login`",
            cfg.issuer
        );
    }

    let meta = Metadata::discover(client, &cfg.issuer).await?;
    let tokens = refresh(
        client,
        &meta.token()?,
        &cfg.client_id,
        &session.refresh_token,
        audience,
    )
    .await?;

    merge_into_session(&mut session, &tokens, now);
    session.store(home)?;
    Ok(tokens)
}

fn first_lines(s: &str, n: usize) -> String {
    s.lines().take(n).collect::<Vec<_>>().join("\n")
}

/// Trade a refresh token for a fresh token set.
///
/// The one call the build path makes, and it never becomes interactive: when
/// the grant is gone the caller's error says to run `heph auth login`.
pub async fn refresh(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
    audience: Option<&str>,
) -> anyhow::Result<TokenSet> {
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("client_id", client_id.to_string()),
        ("refresh_token", refresh_token.to_string()),
    ];
    // Sent when asked for; most IdPs ignore it, which is why the caller
    // verifies the `aud` it actually got rather than trusting this.
    if let Some(aud) = audience {
        form.push(("audience", aud.to_string()));
    }
    token_post(client, token_endpoint, &form).await
}

/// Fold a token set into the stored session.
///
/// Rotation is the case that matters: an IdP that returns a new refresh token
/// has usually invalidated the old one, so *not* storing it turns the next
/// build into a mystery failure. One that returns none has kept the old one
/// valid, so it is retained rather than blanked.
pub fn merge_into_session(session: &mut Session, tokens: &TokenSet, now: SystemTime) {
    if let Some(rt) = &tokens.refresh_token {
        session.refresh_token.clone_from(rt);
    }
    if let Some(id) = &tokens.id_token
        && let Some((_iss, sub)) = crate::jwt::subject_of_trusted(id)
    {
        session.subject = Some(sub);
    }
    session.expires_at = refresh_expiry(now, tokens.refresh_expires_in);
    session.updated_at = Some(now);
}

/// When a refresh token expires, given the IdP's `refresh_expires_in`.
pub fn refresh_expiry(now: SystemTime, seconds: Option<u64>) -> Option<SystemTime> {
    seconds.and_then(|s| now.checked_add(Duration::from_secs(s)))
}

/// The loopback redirect for a port.
///
/// The literal IP, never `localhost`: RFC 8252 §8.3 requires it, and some IdPs
/// treat the two as different registrations — a difference that produces a
/// `redirect_uri_mismatch` whose cause is invisible.
pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}/callback")
}

/// The authorization URL a browser is sent to.
///
/// No `audience` parameter: the descriptor's `audience` is the identity's, it
/// is checked on the token that comes back, and a second one here would be a
/// value nothing verifies.
pub fn authorization_url(
    endpoint: &str,
    cfg: &SignIn,
    pkce: &Pkce,
    state: &str,
    redirect_uri: &str,
) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(endpoint)
        .with_context(|| format!("the issuer's authorization endpoint is not a URL: {endpoint}"))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", &cfg.client_id);
        q.append_pair("redirect_uri", redirect_uri);
        q.append_pair("scope", &cfg.scopes.join(" "));
        q.append_pair("state", state);
        q.append_pair("code_challenge", &pkce.challenge);
        q.append_pair("code_challenge_method", "S256");
    }
    Ok(url.to_string())
}

/// Pull `code` and `state` out of a callback request line.
///
/// Hand-parsed because the whole server is one accept and one response; adding
/// an HTTP stack to read a query string would be the larger risk.
pub fn parse_callback(request_line: &str) -> anyhow::Result<(String, String)> {
    let target = request_line
        .split_whitespace()
        .nth(1)
        .context("the callback request had no target")?;
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
        .context("the callback target is not a URL")?;

    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut description = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            "error_description" => description = Some(v.into_owned()),
            _ => {}
        }
    }

    // The IdP's own words: it knows why it refused far better than a status
    // code does.
    if let Some(e) = error {
        anyhow::bail!(
            "the identity provider refused the sign-in: {e}{}",
            description.map(|d| format!(" — {d}")).unwrap_or_default()
        );
    }
    Ok((
        code.context("the callback carried no `code`")?,
        state.context("the callback carried no `state`")?,
    ))
}

/// What the browser is left looking at.
///
/// A body, not a bare 200: the tab stays open, and a blank page is
/// indistinguishable from a broken one.
fn callback_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>heph</title>\
     <body style=\"font:14px system-ui;padding:3rem\">\
     <h1>Signed in</h1><p>You can close this tab and return to your terminal.</p>";

/// Bind the first available configured loopback port.
///
/// Tried in order, and every failure is reported together: "port 47113 is in
/// use" is actionable where "could not bind" is not.
async fn bind_loopback(ports: &[u16]) -> anyhow::Result<(tokio::net::TcpListener, u16)> {
    // An empty list means "any port", which is what an IdP following RFC 8252
    // §7.3 permits.
    let candidates: Vec<u16> = if ports.is_empty() {
        vec![0]
    } else {
        ports.to_vec()
    };
    let mut errs = Vec::new();
    for port in candidates {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => {
                let bound = l.local_addr().map(|a| a.port()).unwrap_or(port);
                return Ok((l, bound));
            }
            Err(e) => errs.push(format!("  127.0.0.1:{port}: {e}")),
        }
    }
    anyhow::bail!(
        "no configured loopback port could be bound:\n{}\nSet `auth.redirect_ports` in \
         `.hephconfig` to ports this machine can use — and register the matching redirect URIs \
         with the IdP.",
        errs.join("\n")
    )
}

/// Wait for the browser's callback on an already-bound listener.
async fn await_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> anyhow::Result<String> {
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};

    loop {
        let (stream, _) = listener.accept().await.context("accept the callback")?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();

        // Per-connection deadline, and a bounded read. Both matter: a browser's
        // speculative preconnect opens a socket and sends nothing, and without
        // the deadline that one connection holds the loop until the whole login
        // times out — reported as "no callback arrived", which is the wrong
        // diagnosis entirely. The cap stops a local peer that sends bytes and
        // no newline from growing `line` without limit.
        let read = {
            let mut limited = BufReader::new((&mut reader).take(MAX_REQUEST_LINE));
            tokio::time::timeout(CONNECTION_TIMEOUT, limited.read_line(&mut line)).await
        };
        if !matches!(read, Ok(Ok(n)) if n > 0) {
            continue;
        }

        // Browsers ask for /favicon.ico on the same origin; answering the first
        // connection and stopping would abandon the real callback.
        if !line.contains("/callback") {
            // A browser that has gone away is not a failure of the login.
            let _ignored = reader
                .get_mut()
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            continue;
        }

        // The state is checked *before* the page is chosen, and a mismatch
        // continues rather than aborting. Any page the user has open can hit
        // this port with an `<img src=…>` — no preflight on an image GET — and
        // ending the login there would let anything on the web kill it. The
        // PKCE verifier is what keeps that from being worse than a nuisance.
        let outcome = parse_callback(&line).and_then(|(code, state)| {
            anyhow::ensure!(
                state == expected_state,
                "the callback's `state` does not match the one this login sent — the response \
                 did not come from the sign-in started here"
            );
            Ok(code)
        });

        let body = match &outcome {
            Ok(_) => DONE_PAGE.to_string(),
            // The failure belongs on the page too: a terminal the user has
            // tabbed away from is not where they are looking.
            Err(e) => format!(
                "<!doctype html><meta charset=utf-8><title>heph</title>\
                 <body style=\"font:14px system-ui;padding:3rem\"><h1>Sign-in failed</h1>\
                 <pre>{}</pre>",
                html_escape(&format!("{e:#}"))
            ),
        };
        // A browser that hung up before reading the page changes nothing about
        // the outcome.
        let _ignored = reader
            .get_mut()
            .write_all(callback_response(&body).as_bytes())
            .await;
        let _ignored = reader.get_mut().shutdown().await;

        match outcome {
            Ok(code) => return Ok(code),
            // A stray request must not end the login; the real callback may
            // still be on its way.
            Err(e) if is_state_mismatch(&e) => {
                tracing::debug!("ignoring a callback whose state did not match");
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// The one error [`await_callback`] retries rather than reports.
fn is_state_mismatch(e: &anyhow::Error) -> bool {
    e.to_string().contains("`state` does not match")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The interactive PKCE loopback flow (RFC 8252).
///
/// `open_browser` is injected rather than called, so the whole flow is testable
/// against a real socket without a browser ever opening.
pub async fn login(
    client: &reqwest::Client,
    cfg: &SignIn,
    meta: &Metadata,
    now: SystemTime,
    // Given the redirect URI as well as the authorization URL: an unregistered
    // redirect is the usual failure, the user sees it in the browser in three
    // seconds, and without this the caller could only name it after the
    // three-minute timeout.
    open_browser: impl FnOnce(&str, &str),
) -> anyhow::Result<Session> {
    let pkce = Pkce::generate()?;
    let state = random_state()?;
    let (listener, port) = bind_loopback(&cfg.redirect_ports).await?;
    let redirect = redirect_uri(port);
    let url = authorization_url(&meta.authorization()?, cfg, &pkce, &state, &redirect)?;

    open_browser(&url, &redirect);

    let code = tokio::time::timeout(LOGIN_TIMEOUT, await_callback(listener, &state))
        .await
        // The elapsed error carries nothing but "it elapsed"; what is worth
        // saying is the redirect URI, which is the usual cause.
        .map_err(|_elapsed| {
            anyhow::anyhow!(
                "no callback arrived within {}s. If the browser showed an error, it is most \
                 often that `{redirect}` is not registered as a redirect URI for client `{}`.",
                LOGIN_TIMEOUT.as_secs(),
                cfg.client_id
            )
        })??;

    let tokens = token_post(
        client,
        &meta.token()?,
        &[
            ("grant_type", "authorization_code".to_string()),
            ("code", code),
            ("redirect_uri", redirect),
            ("client_id", cfg.client_id.clone()),
            // The verifier's first and only trip over the wire, proving this is
            // the same client that made the authorization request.
            ("code_verifier", pkce.verifier.clone()),
        ],
    )
    .await?;

    session_from(cfg, &tokens, now)
}

/// What the device flow is waiting on, so a caller can show it.
///
/// `device_code` is the bearer of the pending grant, so no derived `Debug`.
/// The `user_code` is meant to be read aloud and is safe to print.
#[derive(Clone, serde::Deserialize)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// The pre-filled form, where the IdP offers one — worth preferring,
    /// because a user code typed by hand is a user code typed wrong.
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: Option<u64>,
    pub interval: Option<u64>,
}

impl std::fmt::Debug for DeviceAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceAuthorization")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

/// Start the device flow (RFC 8628) — for a machine with no browser.
pub async fn device_start(
    client: &reqwest::Client,
    cfg: &SignIn,
    meta: &Metadata,
) -> anyhow::Result<DeviceAuthorization> {
    let url = meta.device_authorization()?;
    let resp = client
        .post(&url)
        .form(&[
            ("client_id", cfg.client_id.clone()),
            ("scope", cfg.scopes.join(" ")),
        ])
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("post to {url}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("{url} returned {status}: {}", first_lines(&text, 5));
    }
    serde_json::from_str(&text).with_context(|| format!("parse the response from {url}"))
}

/// Poll the token endpoint until the user finishes, or the code expires.
pub async fn device_poll(
    client: &reqwest::Client,
    cfg: &SignIn,
    meta: &Metadata,
    auth: &DeviceAuthorization,
    now: SystemTime,
) -> anyhow::Result<Session> {
    let token_endpoint = meta.token()?;
    // RFC 8628 §3.5: five seconds when the server does not say, and `slow_down`
    // means add five more. Ignoring either is how a client gets rate-limited
    // out of a flow that would otherwise have worked.
    let mut interval = Duration::from_secs(auth.interval.unwrap_or(5));
    let started = std::time::Instant::now();
    let budget = Duration::from_secs(auth.expires_in.unwrap_or(600));

    loop {
        tokio::time::sleep(interval).await;

        let resp = client
            .post(&token_endpoint)
            .form(&[
                (
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:device_code".to_string(),
                ),
                ("device_code", auth.device_code.clone()),
                ("client_id", cfg.client_id.clone()),
            ])
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("post to {token_endpoint}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if status.is_success() {
            let tokens: TokenSet = serde_json::from_str(&text)
                .with_context(|| format!("parse the response from {token_endpoint}"))?;
            return session_from(cfg, &tokens, now);
        }

        match device_error(&text) {
            Some(DeviceError::Pending) => {}
            Some(DeviceError::SlowDown) => interval += Duration::from_secs(5),
            Some(DeviceError::Expired) => {
                anyhow::bail!("the device code expired before the sign-in was approved")
            }
            Some(DeviceError::Denied) => anyhow::bail!("the sign-in was denied"),
            // Anything else is a real failure and the body says more than a
            // name this code does not recognise would.
            None => anyhow::bail!(
                "{token_endpoint} returned {status}: {}",
                first_lines(&text, 5)
            ),
        }

        if started.elapsed() >= budget {
            anyhow::bail!("the device code expired before the sign-in was approved");
        }
    }
}

/// The four RFC 8628 §3.5 responses that are part of the flow rather than the
/// end of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceError {
    Pending,
    SlowDown,
    Expired,
    Denied,
}

fn device_error(body: &str) -> Option<DeviceError> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    match v.get("error")?.as_str()? {
        "authorization_pending" => Some(DeviceError::Pending),
        "slow_down" => Some(DeviceError::SlowDown),
        "expired_token" => Some(DeviceError::Expired),
        "access_denied" => Some(DeviceError::Denied),
        _ => None,
    }
}

/// Turn a token set into the session that will be stored.
fn session_from(cfg: &SignIn, tokens: &TokenSet, now: SystemTime) -> anyhow::Result<Session> {
    let refresh_token = tokens.refresh_token.clone().context(
        "the IdP returned no refresh token, so nothing durable can be stored. Add \
         `offline_access` to `auth.scopes` in `.hephconfig`, and check the client is allowed the \
         refresh grant.",
    )?;
    let mut session = Session {
        issuer: cfg.issuer.clone(),
        client_id: cfg.client_id.clone(),
        refresh_token,
        subject: None,
        expires_at: None,
        updated_at: Some(now),
    };
    merge_into_session(&mut session, tokens, now);
    Ok(session)
}

/// Best-effort: put the URL in front of the user's browser.
///
/// The caller always prints the URL as well, so a machine where this does
/// nothing behaves identically, only with one more copy-paste. Different
/// commands per OS, same behaviour — the divergence is in the implementation,
/// not in what happens.
pub fn open_in_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    // Output discarded: `xdg-open` on a headless box writes a diagnostic that
    // would land in the middle of the instructions the user needs to read.
    let _ignored = std::process::Command::new(cmd)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SignIn {
        SignIn {
            issuer: "https://org.example/oauth2/default".into(),
            client_id: "0oa8f3k2mQvR1nZx5d7".into(),
            scopes: ["openid", "profile", "email", "offline_access"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            redirect_ports: vec![47113, 47114, 47115],
        }
    }

    fn jwt(payload: &str) -> String {
        format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#),
            URL_SAFE_NO_PAD.encode(payload.as_bytes()),
            URL_SAFE_NO_PAD.encode(b"sig"),
        )
    }

    /// PKCE is what replaces a client secret for a public client, so the
    /// challenge must genuinely be the S256 of the verifier — RFC 7636's own
    /// appendix-B vector.
    #[test]
    fn the_challenge_is_the_s256_of_the_verifier() {
        let p = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_string());
        assert_eq!(p.challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn a_generated_verifier_is_in_the_rfc_range_and_never_repeats() {
        let a = Pkce::generate().expect("pkce");
        let b = Pkce::generate().expect("pkce");
        assert!(
            (43..=128).contains(&a.verifier.len()),
            "{}",
            a.verifier.len()
        );
        assert_ne!(a.verifier, b.verifier, "the verifier is not random");
        assert_ne!(a.challenge, b.challenge);
    }

    /// RFC 8252 §8.3, and some IdPs treat the two spellings as different
    /// registrations.
    #[test]
    fn the_redirect_is_a_literal_loopback_ip() {
        let uri = redirect_uri(47113);
        assert_eq!(uri, "http://127.0.0.1:47113/callback");
        assert!(!uri.contains("localhost"));
    }

    #[test]
    fn the_authorization_url_carries_pkce_and_no_secret() {
        let pkce = Pkce::generate().expect("pkce");
        let url = authorization_url(
            "https://org.example/oauth2/default/v1/authorize",
            &cfg(),
            &pkce,
            "st4te",
            &redirect_uri(47113),
        )
        .expect("url");

        assert!(url.contains("response_type=code"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains(&pkce.challenge), "{url}");
        assert!(url.contains("state=st4te"), "{url}");
        assert!(url.contains("offline_access"), "{url}");
        // A CLI is a public client; a secret committed to a repo is not one.
        assert!(!url.contains("client_secret"), "{url}");
        // The verifier does not travel with the authorization request — that is
        // the entire mechanism.
        assert!(!url.contains(&pkce.verifier), "{url}");
    }

    #[test]
    fn the_callback_yields_the_code_and_state() {
        let (code, state) =
            parse_callback("GET /callback?code=abc123&state=st4te HTTP/1.1").expect("parsed");
        assert_eq!(code, "abc123");
        assert_eq!(state, "st4te");
    }

    /// The IdP's own words explain a refusal far better than a status does.
    #[test]
    fn a_refusal_is_reported_in_the_providers_words() {
        let err = parse_callback(
            "GET /callback?error=access_denied&error_description=User%20cancelled HTTP/1.1",
        )
        .expect_err("refused");
        let msg = err.to_string();
        assert!(msg.contains("access_denied"), "{msg}");
        assert!(msg.contains("User cancelled"), "{msg}");
    }

    #[test]
    fn a_malformed_callback_says_what_was_missing() {
        assert!(
            parse_callback("GET /callback?state=only HTTP/1.1")
                .expect_err("no code")
                .to_string()
                .contains("`code`")
        );
        parse_callback("nonsense").expect_err("not a request line");
    }

    /// One file per `(issuer, client_id)`: two workspaces on one IdP share a
    /// session, two IdPs never collide.
    #[test]
    fn sessions_are_keyed_by_issuer_and_client() {
        let home = Path::new("/nonexistent/heph-home");
        let a = Session::path(home, "https://a.example", "client1");
        assert_eq!(a, Session::path(home, "https://a.example", "client1"));
        assert_ne!(a, Session::path(home, "https://b.example", "client1"));
        assert_ne!(a, Session::path(home, "https://a.example", "client2"));
        // The delimiter has to actually separate: concatenated, these two pairs
        // would hash the same bytes.
        assert_ne!(
            Session::path(home, "https://a.example", "bc"),
            Session::path(home, "https://a.exampleb", "c")
        );
    }

    /// The mode is the only thing protecting the whole durable footprint, since
    /// the decision was one file on every platform rather than a keychain where
    /// one exists.
    #[test]
    fn a_stored_session_is_0600_and_round_trips() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let s = Session {
            issuer: "https://org.example".into(),
            client_id: "client1".into(),
            refresh_token: "rt_the_only_durable_secret".into(),
            subject: Some("alice@org.example".into()),
            expires_at: None,
            updated_at: None,
        };

        let path = s.store(dir.path()).expect("store");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "file mode {mode:o}");
        let dir_mode = std::fs::metadata(path.parent().expect("parent"))
            .expect("stat dir")
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "dir mode {dir_mode:o}");

        let back = Session::load(dir.path(), &s.issuer, &s.client_id)
            .expect("load")
            .expect("present");
        assert_eq!(back.refresh_token, s.refresh_token);
        assert_eq!(back.subject.as_deref(), Some("alice@org.example"));

        assert!(Session::forget(dir.path(), &s.issuer, &s.client_id).expect("forget"));
        assert!(
            Session::load(dir.path(), &s.issuer, &s.client_id)
                .expect("load")
                .is_none()
        );
        // Forgetting twice is not a failure: it is the state that was asked for.
        assert!(!Session::forget(dir.path(), &s.issuer, &s.client_id).expect("again"));
    }

    /// Overwriting must not widen the mode — a second login is the common case,
    /// not the first one.
    #[test]
    fn re_storing_keeps_the_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = Session {
            issuer: "https://org.example".into(),
            client_id: "c".into(),
            refresh_token: "one".into(),
            subject: None,
            expires_at: None,
            updated_at: None,
        };
        s.store(dir.path()).expect("first");
        s.refresh_token = "two".into();
        let path = s.store(dir.path()).expect("second");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        let back = Session::load(dir.path(), &s.issuer, &s.client_id)
            .expect("load")
            .expect("present");
        assert_eq!(back.refresh_token, "two");
    }

    #[test]
    fn a_missing_session_is_absence_rather_than_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            Session::load(dir.path(), "https://nope", "client")
                .expect("no error")
                .is_none()
        );
    }

    #[test]
    fn expiry_is_only_claimed_when_the_idp_stated_one() {
        let now = SystemTime::UNIX_EPOCH;
        let mut s = Session {
            issuer: "i".into(),
            client_id: "c".into(),
            refresh_token: "rt".into(),
            subject: None,
            expires_at: None,
            updated_at: None,
        };
        // No stated lifetime is not "expired": most IdPs rotate silently and
        // never say when the grant ends.
        assert!(!s.expired_at(now + Duration::from_secs(86_400 * 365)));

        s.expires_at = refresh_expiry(now, Some(60));
        assert!(!s.expired_at(now));
        assert!(s.expired_at(now + Duration::from_secs(61)));
    }

    /// Rotation is the case that matters: storing the old token after the IdP
    /// replaced it turns the *next* build — the one that changed nothing — into
    /// the failure.
    #[test]
    fn a_rotated_refresh_token_replaces_the_stored_one() {
        let now = SystemTime::UNIX_EPOCH;
        let mut s = Session {
            issuer: "https://org.example".into(),
            client_id: "c".into(),
            refresh_token: "old".into(),
            subject: None,
            expires_at: None,
            updated_at: None,
        };
        merge_into_session(
            &mut s,
            &TokenSet {
                refresh_token: Some("new".into()),
                id_token: Some(jwt(r#"{"sub":"alice","iss":"https://org.example"}"#)),
                refresh_expires_in: Some(3600),
                ..TokenSet::default()
            },
            now,
        );
        assert_eq!(s.refresh_token, "new");
        assert_eq!(s.subject.as_deref(), Some("alice"));
        assert_eq!(s.expires_at, Some(now + Duration::from_secs(3600)));
    }

    /// And an IdP that returns none has kept the old one valid, so blanking it
    /// would sign the user out for no reason.
    #[test]
    fn a_response_without_a_refresh_token_keeps_the_stored_one() {
        let mut s = Session {
            issuer: "i".into(),
            client_id: "c".into(),
            refresh_token: "keep".into(),
            subject: Some("alice".into()),
            expires_at: None,
            updated_at: None,
        };
        merge_into_session(
            &mut s,
            &TokenSet {
                access_token: Some("at".into()),
                ..TokenSet::default()
            },
            SystemTime::UNIX_EPOCH,
        );
        assert_eq!(s.refresh_token, "keep");
        assert_eq!(s.subject.as_deref(), Some("alice"));
    }

    /// A login that cannot produce a durable session is a failure at login
    /// time, not a surprise at build time three days later.
    #[test]
    fn a_login_without_a_refresh_token_fails_naming_offline_access() {
        let err = session_from(&cfg(), &TokenSet::default(), SystemTime::UNIX_EPOCH)
            .expect_err("no refresh token");
        assert!(format!("{err:#}").contains("offline_access"), "{err:#}");
    }

    /// An omitted `code_challenge_methods_supported` proves nothing — plenty of
    /// working servers omit it — so only an explicit list without S256 counts.
    #[test]
    fn s256_support_is_only_doubted_when_the_server_lists_methods() {
        assert!(!Metadata::default().lacks_s256());
        assert!(
            !Metadata {
                code_challenge_methods_supported: vec!["plain".into(), "S256".into()],
                ..Metadata::default()
            }
            .lacks_s256()
        );
        assert!(
            Metadata {
                code_challenge_methods_supported: vec!["plain".into()],
                ..Metadata::default()
            }
            .lacks_s256()
        );
    }

    #[test]
    fn a_missing_endpoint_is_named() {
        let m = Metadata::default();
        assert!(
            format!("{:#}", m.authorization().expect_err("no auth ep"))
                .contains("authorization_endpoint")
        );
        assert!(format!("{:#}", m.token().expect_err("no token ep")).contains("token_endpoint"));
        assert!(
            format!("{:#}", m.device_authorization().expect_err("no device ep"))
                .contains("device flow")
        );
    }

    #[test]
    fn device_errors_are_classified_and_unknown_ones_fall_through() {
        assert_eq!(
            device_error(r#"{"error":"authorization_pending"}"#),
            Some(DeviceError::Pending)
        );
        assert_eq!(
            device_error(r#"{"error":"slow_down"}"#),
            Some(DeviceError::SlowDown)
        );
        assert_eq!(device_error(r#"{"error":"invalid_client"}"#), None);
        assert_eq!(device_error("<html>502</html>"), None);
    }

    #[test]
    fn an_assertion_needs_an_id_token_and_says_so() {
        let err = TokenSet {
            access_token: Some("opaque".into()),
            ..TokenSet::default()
        }
        .assertion()
        .expect_err("no id token");
        assert!(format!("{err:#}").contains("openid"), "{err:#}");
    }

    /// The loopback half end to end, over a real socket, with the browser
    /// replaced by a plain GET: the only way the state check, the favicon skip
    /// and the response body are exercised together.
    #[tokio::test]
    async fn the_loopback_flow_ignores_favicon_and_returns_the_code() {
        let (listener, port) = bind_loopback(&[0]).await.expect("bind");
        let waiter = tokio::spawn(async move { await_callback(listener, "st4te").await });

        // What a browser does first, and what would otherwise be mistaken for
        // the callback.
        get(port, "/favicon.ico").await;
        let body = get(port, "/callback?code=the-code&state=st4te").await;

        let code = waiter.await.expect("join").expect("callback");
        assert_eq!(code, "the-code");
        assert!(body.contains("Signed in"), "{body}");
    }

    /// Without the state check, anything that can reach the loopback port can
    /// feed in a code of its choosing — and any page the user has open can
    /// reach it with an `<img src=…>`. So a mismatch is *ignored*: the stray
    /// request gets the failure page, and the real callback still lands.
    #[tokio::test]
    async fn a_callback_with_the_wrong_state_is_ignored_and_the_real_one_still_lands() {
        let (listener, port) = bind_loopback(&[0]).await.expect("bind");
        let waiter = tokio::spawn(async move { await_callback(listener, "expected").await });

        let stray = get(port, "/callback?code=attacker-code&state=wrong").await;
        assert!(stray.contains("Sign-in failed"), "{stray}");

        let real = get(port, "/callback?code=real-code&state=expected").await;
        assert!(real.contains("Signed in"), "{real}");
        assert_eq!(waiter.await.expect("join").expect("callback"), "real-code");
    }

    /// A socket that connects and says nothing must not hold the login. A
    /// browser's speculative preconnect does exactly this, and without the
    /// per-connection deadline it would wait out the whole login and then blame
    /// the redirect registration.
    #[tokio::test]
    async fn a_silent_connection_does_not_wedge_the_login() {
        let (listener, port) = bind_loopback(&[0]).await.expect("bind");
        let waiter = tokio::spawn(async move { await_callback(listener, "st4te").await });

        // Connected, never written to, and deliberately still open.
        let _silent = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let body = get(port, "/callback?code=the-code&state=st4te").await;

        assert!(body.contains("Signed in"), "{body}");
        assert_eq!(waiter.await.expect("join").expect("callback"), "the-code");
    }

    /// A refusal has to reach the browser tab too: the terminal is not where
    /// the user is looking at that moment.
    #[tokio::test]
    async fn a_provider_refusal_is_shown_in_the_browser_and_returned() {
        let (listener, port) = bind_loopback(&[0]).await.expect("bind");
        let waiter = tokio::spawn(async move { await_callback(listener, "st4te").await });
        let body = get(port, "/callback?error=access_denied&state=st4te").await;
        assert!(body.contains("access_denied"), "{body}");
        // A refusal *does* end the login — unlike a stray state, the provider
        // has answered and no further callback is coming.
        waiter.await.expect("join").expect_err("refused");
    }

    /// The bound port is reported back so the diagnostic can name the exact
    /// redirect URI that has to be registered.
    #[tokio::test]
    async fn binding_falls_through_to_a_free_port() {
        let (held, port) = bind_loopback(&[0]).await.expect("bind");
        let (_next, other) = bind_loopback(&[port, 0]).await.expect("fall through");
        assert_ne!(other, port, "the occupied port was reused");
        drop(held);
    }

    /// A token endpoint that rotates on every call, so a test can prove the
    /// rotation was persisted. Returns `(base_url, calls_seen)`.
    ///
    /// Written rather than mocked because what is under test is the *ordering*
    /// of a lock, a read, an HTTP call and a write — which a mock at the HTTP
    /// boundary cannot observe.
    async fn fake_idp(
        rotate: bool,
        reject: bool,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let base = format!("http://127.0.0.1:{port}");
        let calls = std::sync::Arc::new(AtomicUsize::new(0));

        let discovery = format!(
            r#"{{"issuer":"{base}","token_endpoint":"{base}/token","authorization_endpoint":"{base}/authorize"}}"#
        );
        let seen = std::sync::Arc::clone(&calls);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();

                let body = if req.contains("openid-configuration") {
                    discovery.clone()
                } else if reject {
                    r#"{"error":"invalid_grant"}"#.to_string()
                } else {
                    let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
                    let rt = if rotate {
                        format!(r#","refresh_token":"rt-{n}""#)
                    } else {
                        String::new()
                    };
                    format!(r#"{{"id_token":"idtok-{n}","expires_in":3600{rt}}}"#)
                };
                let status = if reject && !req.contains("openid-configuration") {
                    "400 Bad Request"
                } else {
                    "200 OK"
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ignored = sock.write_all(resp.as_bytes()).await;
                let _ignored = sock.shutdown().await;
            }
        });
        (base, calls)
    }

    fn stored(dir: &Path, issuer: &str, token: &str) -> SignIn {
        let cfg = SignIn {
            issuer: issuer.to_string(),
            client_id: "client1".into(),
            scopes: vec!["openid".into(), "offline_access".into()],
            redirect_ports: Vec::new(),
        };
        Session {
            issuer: cfg.issuer.clone(),
            client_id: cfg.client_id.clone(),
            refresh_token: token.to_string(),
            subject: None,
            expires_at: None,
            updated_at: None,
        }
        .store(dir)
        .expect("store");
        cfg
    }

    /// The whole point of the lock: a rotated token must be on disk before the
    /// call returns, and the *next* refresh must present the new one. Getting
    /// this wrong locks the user out on the build after this one — the build
    /// that changed nothing.
    #[tokio::test]
    async fn a_rotated_token_is_persisted_and_presented_next_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (base, _calls) = fake_idp(true, false).await;
        let cfg = stored(dir.path(), &base, "rt-0");
        let client = http_client().expect("client");
        let ctoken = hcore::hasync::StdCancellationToken::new();
        let now = SystemTime::UNIX_EPOCH;

        refresh_locked(&client, &cfg, dir.path(), None, now, &ctoken)
            .await
            .expect("first refresh");
        let after = Session::load(dir.path(), &cfg.issuer, &cfg.client_id)
            .expect("load")
            .expect("present");
        assert_eq!(
            after.refresh_token, "rt-1",
            "the rotation was not persisted"
        );

        refresh_locked(&client, &cfg, dir.path(), None, now, &ctoken)
            .await
            .expect("second refresh");
        let after = Session::load(dir.path(), &cfg.issuer, &cfg.client_id)
            .expect("load")
            .expect("present");
        assert_eq!(
            after.refresh_token, "rt-2",
            "the second refresh reused rt-0"
        );
    }

    /// Two refreshes at once must serialise, not interleave. If they did not,
    /// one would present a token the other had already spent.
    #[tokio::test]
    async fn concurrent_refreshes_rotate_sequentially() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (base, calls) = fake_idp(true, false).await;
        let cfg = stored(dir.path(), &base, "rt-0");
        let now = SystemTime::UNIX_EPOCH;

        let mut tasks = Vec::new();
        for _ in 0..4 {
            let cfg = cfg.clone();
            let home = dir.path().to_path_buf();
            tasks.push(tokio::spawn(async move {
                let client = http_client().expect("client");
                let ctoken = hcore::hasync::StdCancellationToken::new();
                refresh_locked(&client, &cfg, &home, None, now, &ctoken).await
            }));
        }
        for t in tasks {
            t.await.expect("join").expect("refresh");
        }

        // Four rotations, and the last one is what is on disk — no refresh
        // started from a token another had already replaced.
        let n = calls.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(n, 4, "expected one refresh per caller");
        let after = Session::load(dir.path(), &cfg.issuer, &cfg.client_id)
            .expect("load")
            .expect("present");
        assert_eq!(after.refresh_token, format!("rt-{n}"));
    }

    /// `invalid_grant` is the ordinary end of a session, and the caller has to
    /// be able to tell it from an outage — one means "log in again", the other
    /// means "wait".
    #[tokio::test]
    async fn a_dead_grant_is_classified_rather_than_reported_as_an_outage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (base, _calls) = fake_idp(false, true).await;
        let cfg = stored(dir.path(), &base, "rt-0");
        let client = http_client().expect("client");
        let ctoken = hcore::hasync::StdCancellationToken::new();

        let err = refresh_locked(
            &client,
            &cfg,
            dir.path(),
            None,
            SystemTime::UNIX_EPOCH,
            &ctoken,
        )
        .await
        .expect_err("rejected");
        assert!(is_invalid_grant(&err), "{err:#}");

        // And the stored token is untouched: a refusal is not a reason to
        // destroy the only durable thing we have.
        let after = Session::load(dir.path(), &cfg.issuer, &cfg.client_id)
            .expect("load")
            .expect("present");
        assert_eq!(after.refresh_token, "rt-0");
    }

    /// An IdP that keeps the same refresh token must not have it blanked.
    #[tokio::test]
    async fn an_idp_that_does_not_rotate_keeps_working() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (base, _calls) = fake_idp(false, false).await;
        let cfg = stored(dir.path(), &base, "rt-forever");
        let client = http_client().expect("client");
        let ctoken = hcore::hasync::StdCancellationToken::new();

        for _ in 0..2 {
            refresh_locked(
                &client,
                &cfg,
                dir.path(),
                None,
                SystemTime::UNIX_EPOCH,
                &ctoken,
            )
            .await
            .expect("refresh");
        }
        let after = Session::load(dir.path(), &cfg.issuer, &cfg.client_id)
            .expect("load")
            .expect("present");
        assert_eq!(after.refresh_token, "rt-forever");
    }

    /// A machine nobody signed in on fails before any network call — a build
    /// that hangs on an unreachable IdP to discover it was never logged in is
    /// the worst of both.
    #[tokio::test]
    async fn refreshing_without_a_session_says_to_log_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = SignIn {
            issuer: "http://127.0.0.1:1".into(),
            client_id: "c".into(),
            scopes: Vec::new(),
            redirect_ports: Vec::new(),
        };
        let client = http_client().expect("client");
        let ctoken = hcore::hasync::StdCancellationToken::new();
        let err = refresh_locked(
            &client,
            &cfg,
            dir.path(),
            None,
            SystemTime::UNIX_EPOCH,
            &ctoken,
        )
        .await
        .expect_err("no session");
        assert!(format!("{err:#}").contains("heph auth login"), "{err:#}");
    }

    async fn get(port: u16, target: &str) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .await
            .expect("write");
        let mut buf = String::new();
        let _ignored = s.read_to_string(&mut buf).await;
        buf
    }
}
