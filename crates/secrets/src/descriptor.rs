//! `secret.json` — what a credential target emits, and the line that decides
//! whether the whole feature works.
//!
//! A `secret()` target describes **how to obtain a value, never the value**.
//! Its declaration has two halves that behave completely differently, and
//! conflating them reintroduces the exact disease this design exists to cure:
//!
//! - **Identity — hashed.** Who you are and what you are addressing: role,
//!   audience, scope, impersonated account, and the slot keys (registry host,
//!   netrc machine, profile name, account endpoint). Change any of these and
//!   consumers legitimately re-key, because the target really may compute
//!   something different.
//! - **Acquisition — not hashed.** How the value is fetched: provider, helper
//!   argv, exchange endpoint, TTL, which runner the helper runs under. None of
//!   it changes what the target computes.
//!
//! ## How "not hashed" is enforced
//!
//! Not by a flag anybody has to remember. A consumer's `hashin` folds in the
//! *hashouts of its hashed inputs* (`engine/meta.rs`), and a hashout is the
//! digest of the artifacts the input target produced. So the split is made
//! structural: **the descriptor target writes only [`Identity`] into
//! `secret.json`**, and the acquisition half never becomes an artifact at all.
//! The broker reads it from the target's *spec* instead ([`Acquire`]),
//! which is also where the shape-collision check reads from, and for the same
//! reason — a spec is available without building or minting anything, so both
//! checks still run on a fully warm build where nothing is executed.
//!
//! Editing a helper path therefore re-runs the descriptor target (its def hash
//! moved) and produces byte-identical `secret.json`, so every consumer's
//! `hashin` is unchanged. That is the property that lets one descriptor serve
//! CI and a laptop out of one cache.
//!
//! This mirrors `execrunner::config`, arriving at the opposite answer for the
//! opposite reason: a runner needs a derived `fingerprint` because its config
//! names a reference to something that moves, while a credential recipe names
//! an identity that does not — but only once the machinery for obtaining it has
//! been separated out.

use crate::htspec::{SpecEnum, SpecOneOf, SpecStruct};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The only `secret.json` schema version this heph understands.
///
/// Checked by exact match, following `RUNNER_JSON_VERSION` rather than the
/// `RemoteManifest.version` precedent of a field written everywhere and read
/// nowhere.
pub const SECRET_JSON_VERSION: u32 = 1;

/// The name of the file a `secret()` target produces.
pub const SECRET_JSON: &str = "secret.json";

/// The hashed half: what the credential *is*.
///
/// Every field here reaches every consumer's cache key. Fields are
/// `skip_serializing_if`-empty so that adding a new one later does not re-key
/// descriptors that do not use it — the same discipline `Sandbox` needs for its
/// `secrets` map.
///
/// `BTreeMap` and sorted vectors throughout: this struct is serialized straight
/// into an artifact whose digest is a cache key, so iteration order has to be
/// deterministic across processes and platforms.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    /// AWS role ARN to assume, or any provider's equivalent principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,

    /// The `aud` claim the exchange will check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,

    /// OAuth scope(s) requested, sorted for determinism.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<String>,

    /// Everything else this identity needs, as an open map.
    ///
    /// **A named field earns its place only two ways**, and this is where
    /// everything that does neither lives:
    ///
    /// 1. It is a **parameter of a standard** an exchange speaks — `role`,
    ///    `audience`, `scope`.
    /// 2. It is a **slot key** [`crate::shape::Shape::slots`] reasons about, so
    ///    two credentials cannot silently claim one entry — `machine`,
    ///    `registry`, `profile`, `env`.
    ///
    /// Anything else is one vendor's vocabulary in a format that is frozen into
    /// every consumer's cache key. `bucket`, `account`, `region` and `endpoint`
    /// were named fields in an earlier draft and earned nothing by it: they
    /// were read in one line each — `region` and `endpoint` by the
    /// `aws_profile` renderer, `bucket` and `account` only as `{bucket}` /
    /// `{account}` template substitutions that this map already performs. They
    /// bought a schema surface welded to S3 and cost a release to extend, so an
    /// organization with an internal IdP had no way to say what its identity
    /// was at all.
    ///
    /// The cost, stated so it is a decision rather than a surprise: a map has
    /// no schema, so a misspelled `regoin` renders no region rather than
    /// failing. That is the price of a vocabulary heph does not have to own.
    ///
    /// Hashed, like the rest of the identity half.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,

    /// Slot key for `docker_config`: the registry host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,

    /// Slot key for `netrc` and `git_credential`: the machine / URL prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,

    /// Slot key for `aws_profile`: the profile section name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,

    /// Which shapes this credential renders into the sandbox.
    ///
    /// **Identity, deliberately, and it may not vary per [`Acquire`] entry.** A
    /// shape decides which files and which variables exist in the sandbox, and
    /// that is part of what the target reads — two shapes are two different
    /// environments, so hermeticity requires it in the key.
    ///
    /// It costs nothing in cache sharing, because a shape's *paths and variable
    /// names* are fixed by the shape and its slot key while only the contents
    /// vary. A federated and a non-federated GCP credential both render
    /// `gcloud_adc` at the same path under the same variables; what differs is
    /// whether the ADC inside says `external_account` or carries a token
    /// directly, and contents are exactly what is never hashed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shape: Vec<String>,

    /// Slot keys for the `env` shape: variable name → JSON pointer into the
    /// acquired value (`"$."` for the whole value).
    ///
    /// The *names* are identity because they are slots in the target's
    /// environment namespace and two secrets claiming one name is a collision.
    /// The values they resolve to are not, and never appear here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// A `secret.json`, as written and as read back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretJson {
    /// Schema version. Must equal [`SECRET_JSON_VERSION`].
    pub version: u32,

    /// The descriptor target's canonical address.
    ///
    /// **This is what makes the hashout name a credential rather than merely
    /// describe one, and leaving it out was a cache-poisoning bug.** Every
    /// [`Identity`] field is optional, so two descriptors distinguished only by
    /// their *acquisition* halves — `//creds:prod` reading `PROD_API_KEY` and
    /// `//creds:staging` reading `STAGING_API_KEY` — emitted byte-identical
    /// artifacts. Identical bytes mean an identical hashout, so a consumer of
    /// one was a cache hit on the other's artifact: the staging target was
    /// served output built with the production key, silently, with no execution
    /// and no warning.
    ///
    /// The `execrunner` precedent does *not* transfer here, and the difference
    /// is worth stating. A runner target deliberately keeps its address out of
    /// the key, because `runner.json` fully describes the environment and two
    /// addresses emitting identical config really do describe the same thing.
    /// A `secret.json` deliberately omits the half that determines which
    /// real-world principal you get, so identical bytes imply nothing at all.
    ///
    /// The cost, stated so it is a decision rather than a surprise: **renaming
    /// or moving a descriptor re-keys every consumer.** That is the safe
    /// direction, and it is the price of the acquisition half being unhashed.
    pub addr: String,

    /// The hashed half of the declaration.
    pub identity: Identity,
}

impl SecretJson {
    pub fn new(addr: impl Into<String>, identity: Identity) -> Self {
        Self {
            version: SECRET_JSON_VERSION,
            addr: addr.into(),
            identity,
        }
    }

    /// Serialize deterministically: pretty JSON with a trailing newline.
    ///
    /// `serde_json` preserves `BTreeMap` order and struct field order, so two
    /// runs on two platforms produce byte-identical output — which they must,
    /// since these bytes *are* a cache key component.
    pub fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        let mut v = serde_json::to_vec_pretty(self)?;
        v.push(b'\n');
        Ok(v)
    }

    /// Parse and validate the version. The error names the descriptor target
    /// because the bytes arrive with no other provenance.
    pub fn parse(bytes: &[u8], addr: &str) -> anyhow::Result<Self> {
        let d: Self = serde_json::from_slice(bytes)
            .map_err(|e| anyhow::anyhow!("secret {addr}: parse {SECRET_JSON}: {e}"))?;
        if d.version != SECRET_JSON_VERSION {
            anyhow::bail!(
                "secret {addr}: {SECRET_JSON} declares version {} but this heph understands only \
                 version {SECRET_JSON_VERSION}",
                d.version
            );
        }
        Ok(d)
    }
}

/// Which broker provider obtains the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Read a named host environment variable. The honest escape hatch and the
    /// migration path off `pass_env`.
    StaticEnv,
    /// Run a helper subprocess speaking one of the four [`Protocol`]s.
    Exec,
    /// Acquire a workload identity token and exchange it for a scoped
    /// short-lived credential.
    Oidc,
}

/// The wire protocol a helper subprocess speaks.
///
/// "Run a helper and read a credential" sounds like one thing and is four.
/// Implementing only the Bazel-derived spec would have covered none of the
/// laptop paths this feature exists for, so the protocol is an explicit closed
/// field rather than something guessed from output.
#[derive(SpecEnum, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// The Bazel `--credential_helper` protocol. Takes `{"uri": …}` on stdin,
    /// returns `{"headers": {…}, "expires": …}`. The only protocol that carries
    /// expiry natively, which is why the broker's TTL cache prefers it.
    ///
    /// Named for what it is rather than for EngFlow, who authored the spec: the
    /// helpers that speak it are not theirs, and a vendor's name on a protocol
    /// several tools implement misleads the reader about what is required.
    CredentialHelper,
    /// The AWS `credential_process` schema. No stdin; returns
    /// `{"Version":1,"AccessKeyId":…,"SessionToken":…,"Expiration":…}`. The one
    /// protocol heph both reads and *writes* — the same shape it accepts from
    /// `aws-vault` is what it renders for mid-target refresh.
    CredentialProcess,
    /// The Docker credential-helper protocol. Shares a name with the Bazel spec
    /// and nothing else: a bare URL on stdin, `{"ServerURL","Username","Secret"}`
    /// back. heph only ever calls `get`.
    DockerCredential,
    /// stdout *is* the value, minus a trailing newline. A concession rather
    /// than a protocol: it carries no expiry, and a helper that prints a
    /// warning to stdout has just made it part of your credential.
    Raw,
}

impl Protocol {
    /// Whether this protocol can report its own expiry.
    ///
    /// Two of four cannot, which is why `ttl` exists on [`Acquire`] and why the
    /// JWT reader in [`crate::jwt`] is worth having.
    pub fn carries_expiry(self) -> bool {
        matches!(
            self,
            Protocol::CredentialHelper | Protocol::CredentialProcess
        )
    }
}

/// One step that turns what a [`Source`] produced into a usable credential.
///
/// **Standards first; vendors are configuration.** The earlier design was a
/// closed enum of vendor names — `aws`, `gcp`, `github_app`, `r2_temp` — which
/// was wrong twice over. It privileged whichever vendors happened to be in
/// front of the author, so a GitHub App token was a first-class concept while
/// an internal IdP was inexpressible; and adding the next vendor meant changing
/// a schema that other heph versions read back.
///
/// What is actually general is the *grant*, and there are three of them, each
/// with an RFC. AWS STS keeps a variant because `AssumeRoleWithWebIdentity` is
/// the federation entry point for one of the three clouds and speaks none of
/// them. Everything else — a GitHub App installation token, a Cloudflare R2
/// temporary credential, a bespoke internal minting endpoint — is
/// [`Exchange::Http`], described in the BUILD file rather than named in heph.
///
/// A pipeline rather than a single step, because more than one hop is normal:
/// GCP federation is RFC 8693 against its STS endpoint followed by a
/// service-account impersonation call. Two steps, not a special case.
#[derive(SpecOneOf, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[spec(tag = "kind")]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Exchange {
    /// RFC 8693 OAuth 2.0 Token Exchange.
    ///
    /// The standard way to trade an assertion for a scoped credential, and what
    /// GCP's own STS endpoint speaks.
    TokenExchange {
        /// The authorization server, discovered rather than hard-coded.
        ///
        /// Preferred over [`Self::TokenExchange::endpoint`] — see
        /// [`Endpoint`].
        issuer: Option<String>,
        /// The token endpoint, for a server that publishes no discovery
        /// document.
        endpoint: Option<String>,
        audience: Option<String>,
        resource: Option<String>,
        scope: Vec<String>,
        /// `urn:ietf:params:oauth:token-type:…`, where the default is not wanted.
        requested_token_type: Option<String>,
    },
    /// RFC 7523 §2.1 JWT bearer grant.
    ///
    /// What a service-account key actually is: a signed assertion traded for an
    /// access token.
    JwtBearer {
        issuer: Option<String>,
        endpoint: Option<String>,
        scope: Vec<String>,
    },
    /// RFC 6749 §4.4 client credentials.
    ClientCredentials {
        issuer: Option<String>,
        endpoint: Option<String>,
        scope: Vec<String>,
    },
    /// AWS STS `AssumeRoleWithWebIdentity`.
    ///
    /// Not an IETF grant, and kept as a variant anyway: it is how one of the
    /// three clouds federates, it predates RFC 8693, and expressing it as a raw
    /// HTTP call would put XML parsing in a BUILD file. The role comes from the
    /// identity half, not from here.
    AwsSts { endpoint: Option<String> },
    /// A vendor REST call that trades the previous step's value for a credential.
    ///
    /// The escape hatch that keeps heph out of the business of knowing vendors.
    /// A GitHub App installation token and a Cloudflare R2 temporary credential
    /// are each one POST and a couple of JSON pointers; neither needs a name in
    /// this enum, a release to add, or a schema change to remove.
    Http {
        #[spec(required)]
        url: String,
        /// Defaults to `POST`.
        method: Option<String>,
        headers: BTreeMap<String, String>,
        /// Request body. `{token}` interpolates the previous step's value.
        body: Option<String>,
        /// Credential field name → JSON pointer into the response, so the
        /// shapes downstream see named fields rather than a blob.
        fields: BTreeMap<String, String>,
    },
}

/// Where an OAuth grant is sent: an issuer to discover, or a literal endpoint.
///
/// **Discovery is the standard, so `issuer` is the spelling to reach for.**
/// OpenID Connect Discovery 1.0 (and RFC 8414 for plain OAuth) put the token
/// endpoint in `{issuer}/.well-known/openid-configuration`, which is why every
/// IdP publishes one and why an issuer is the thing an administrator can
/// actually tell you. Requiring a hand-written `token_endpoint` was an
/// inconsistency in this design's own terms: `heph auth login` already
/// discovers its endpoints from an issuer, and an exchange had no reason to
/// work differently.
///
/// Discovery also gets a diagnostic for free. The metadata document lists
/// `grant_types_supported`, so asking a server for a grant it does not
/// implement can fail by name, before the request, instead of arriving as a
/// bare `400 unsupported_grant_type`.
///
/// `endpoint` stays as the escape hatch, because not every server publishes
/// metadata — an internal minting service, or an endpoint reached through a
/// gateway on a different host than its issuer claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint<'a> {
    /// Resolve `{issuer}/.well-known/openid-configuration` and read
    /// `token_endpoint` from it.
    Discover(&'a str),
    /// Use this URL directly.
    Literal(&'a str),
}

/// The well-known path appended to an issuer, per OpenID Connect Discovery 1.0.
pub const OIDC_DISCOVERY_PATH: &str = "/.well-known/openid-configuration";

impl Endpoint<'_> {
    /// The URL to fetch metadata from, for the discovery form.
    ///
    /// The issuer's trailing slash is dropped first: OIDC Discovery specifies
    /// concatenation, so an issuer written with one yields a double slash that
    /// some servers 404 on and others do not — a difference nobody should have
    /// to debug from a BUILD file.
    pub fn discovery_url(&self) -> Option<String> {
        match self {
            Endpoint::Discover(issuer) => Some(format!(
                "{}{OIDC_DISCOVERY_PATH}",
                issuer.trim_end_matches('/')
            )),
            Endpoint::Literal(_) => None,
        }
    }
}

/// Resolve the `issuer`/`endpoint` pair for one grant.
///
/// Exactly one, and saying so is the whole rule: silently preferring one when
/// both are given would make which server was contacted depend on a precedence
/// nobody can see at the call site.
fn endpoint_of<'a>(
    issuer: &'a Option<String>,
    endpoint: &'a Option<String>,
    kind: &str,
) -> anyhow::Result<Endpoint<'a>> {
    match (issuer.as_deref(), endpoint.as_deref()) {
        (Some(i), None) => Ok(Endpoint::Discover(i)),
        (None, Some(e)) => Ok(Endpoint::Literal(e)),
        (Some(_), Some(_)) => anyhow::bail!(
            "`{kind}` names both an `issuer` and an `endpoint`; set one. `issuer` discovers the \
             token endpoint from {OIDC_DISCOVERY_PATH}, which is what an IdP publishes and what \
             an administrator can tell you; `endpoint` is for a server that publishes no \
             discovery document."
        ),
        (None, None) => anyhow::bail!(
            "`{kind}` needs an `issuer` (preferred — the token endpoint is discovered from \
             {OIDC_DISCOVERY_PATH}) or an explicit `endpoint`."
        ),
    }
}

impl Exchange {
    /// Where this step sends its request, for the grants that have one.
    ///
    /// `None` for [`Exchange::AwsSts`] and [`Exchange::Http`], which name their
    /// own destinations and speak no OAuth metadata.
    pub fn endpoint(&self) -> anyhow::Result<Option<Endpoint<'_>>> {
        Ok(match self {
            Exchange::TokenExchange {
                issuer, endpoint, ..
            } => Some(endpoint_of(issuer, endpoint, "token_exchange")?),
            Exchange::JwtBearer {
                issuer, endpoint, ..
            } => Some(endpoint_of(issuer, endpoint, "jwt_bearer")?),
            Exchange::ClientCredentials {
                issuer, endpoint, ..
            } => Some(endpoint_of(issuer, endpoint, "client_credentials")?),
            Exchange::AwsSts { .. } | Exchange::Http { .. } => None,
        })
    }

    /// The grant type this step requests, for the metadata check that
    /// discovery makes possible.
    pub fn grant_type(&self) -> Option<&'static str> {
        match self {
            Exchange::TokenExchange { .. } => {
                Some("urn:ietf:params:oauth:grant-type:token-exchange")
            }
            Exchange::JwtBearer { .. } => Some("urn:ietf:params:oauth:grant-type:jwt-bearer"),
            Exchange::ClientCredentials { .. } => Some("client_credentials"),
            Exchange::AwsSts { .. } | Exchange::Http { .. } => None,
        }
    }
}

/// Accept `var = "TOKEN"` as well as `vars = {…}`.
///
/// A bare string is the single-variable case and lands on the primary field, so
/// `$SECRET_<NAME>` and a `"$."` pointer both resolve to it.
fn parse_vars(v: &crate::htvalue::Value) -> anyhow::Result<BTreeMap<String, String>> {
    use crate::htspec::FromSpecValue as _;
    match v {
        crate::htvalue::Value::String(one) => Ok(BTreeMap::from([(
            crate::value::Credential::PRIMARY.to_string(),
            one.clone(),
        )])),
        other => <BTreeMap<String, String>>::from_spec_value(other),
    }
}

fn vars_param_type() -> crate::htvalue::signature::ParamType {
    use crate::htvalue::signature::ParamType;
    ParamType::union(vec![ParamType::String, ParamType::map(ParamType::String)])
}

/// Accept either one exchange or a pipeline of them.
///
/// A single hop is the common case and should not have to be spelled as a
/// one-element list; two hops must be expressible, because GCP needs them.
pub fn parse_exchange_value(v: &crate::htvalue::Value) -> anyhow::Result<Vec<Exchange>> {
    use crate::htspec::FromSpecValue as _;
    match v {
        crate::htvalue::Value::Null() => Ok(Vec::new()),
        crate::htvalue::Value::List(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                Exchange::from_spec_value(item).map_err(|e| anyhow::anyhow!("exchange[{i}]: {e}"))
            })
            .collect(),
        other => Exchange::from_spec_value(other).map(|e| vec![e]),
    }
}

/// How the raw material for a credential is obtained.
///
/// A tagged union rather than a bag of options: the tag says which shape this
/// is, so a field belonging to another provider is an unknown key rather than a
/// silently ignored one, and a field the provider requires is missing at parse
/// time rather than in hand-written cross-field validation. `protocol` on an
/// `exec` is required *by construction*; `helper` on a `static_env` cannot be
/// written at all.
#[derive(SpecOneOf, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[spec(tag = "provider")]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum Source {
    /// Read named host environment variables.
    ///
    /// The honest escape hatch and the migration path off `pass_env`. Names a
    /// variable, never a literal — the schema has no free-form value field at
    /// all, so a token cannot be written into a target and pushed to a shared
    /// cache.
    StaticEnv {
        /// Credential field name → host variable name.
        ///
        /// Written as `vars = {"aws_access_key_id": "AWS_…"}`, or as
        /// `var = "TOKEN"` for the single-variable case — one field with two
        /// spellings, so there is no `var`/`vars` pair left to get wrong and no
        /// mutual-exclusion rule to enforce downstream.
        #[spec(alias = "var", parse = parse_vars, ty = vars_param_type())]
        vars: BTreeMap<String, String>,
    },
    /// Run a helper subprocess speaking one of the four wire protocols.
    Exec {
        /// The helper argv. Its head is the program.
        helper: Vec<String>,
        /// Required, and never guessed: the four protocols differ in stdin
        /// encoding as well as in response shape.
        #[spec(required)]
        protocol: Protocol,
        /// An exec runner for the helper: a target address, or `"local"`.
        ///
        /// A helper inherits **no** workspace-wide `runner:` default, unlike a
        /// target — it usually needs the real `$HOME` a hermetic runner exists
        /// to hide.
        runner: Option<String>,
        /// How long the helper may run, e.g. `"120s"`. Defaults to
        /// [`DEFAULT_HELPER_TIMEOUT`].
        timeout: Option<String>,
    },
    /// Present the ambient workload identity token, or the stored session.
    ///
    /// It has no configuration of its own: what it asserts is the identity
    /// half's `audience`, and what it becomes is the `exchange` pipeline.
    Oidc {},
}

impl Source {
    /// Which provider implementation handles this source.
    pub fn kind(&self) -> ProviderKind {
        match self {
            Source::StaticEnv { .. } => ProviderKind::StaticEnv,
            Source::Exec { .. } => ProviderKind::Exec,
            Source::Oidc {} => ProviderKind::Oidc,
        }
    }
}

/// The guard that selects an [`Acquire`] entry.
///
/// An environment variable rather than a closed list of CI systems, because
/// every CI system already announces itself that way — `GITHUB_ACTIONS`,
/// `GITLAB_CI`, `BUILDKITE`, `CI` — so heph needs to know about none of them,
/// and a team running something bespoke sets a marker of their own. There is no
/// enum to extend and no release to wait for.
///
/// This is the one place heph reads the ambient environment by design. It is
/// safe here for a precise reason: the guard selects only an acquisition route,
/// and nothing in the acquisition half is hashed, so no ambient value can reach
/// a cache key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WhenEnv {
    /// Matches when the named variable is set and non-empty.
    ///
    /// *Set but empty counts as unset*, because CI systems routinely blank a
    /// variable to mean "off" and treating that as true is a reliable way to
    /// select the wrong route.
    Set(String),
    /// Matches when every named variable holds exactly that value. Exact string
    /// comparison, no globs and no patterns: a file this close to cache keys is
    /// the wrong place for an expression language.
    Equals(BTreeMap<String, String>),
}

impl WhenEnv {
    /// Evaluate against an environment lookup.
    pub fn matches(&self, env: &dyn Fn(&str) -> Option<String>) -> bool {
        match self {
            WhenEnv::Set(name) => env(name).is_some_and(|v| !v.is_empty()),
            WhenEnv::Equals(map) => map
                .iter()
                .all(|(k, want)| env(k).is_some_and(|got| got == *want)),
        }
    }

    /// A one-line rendering for the "no entry matched" diagnostic, which must
    /// say what was looked for as well as what was found.
    pub fn describe(&self) -> String {
        match self {
            WhenEnv::Set(name) => format!("{name} is set"),
            WhenEnv::Equals(map) => map
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", "),
        }
    }
}

/// One route to the value. The unhashed half.
///
/// Three fields, where there were ten. Everything provider-specific moved into
/// [`Source`], which means the combinations that used to need hand-written
/// cross-field validation — a `helper` on a `static_env`, a `runner` on an
/// `oidc`, an `exec` with no `protocol` — are now unwritable rather than
/// merely rejected.
#[derive(SpecStruct, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Acquire {
    /// The guard. `None` always matches, so an unguarded entry is the catch-all
    /// and must come last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[spec(parse = parse_when_env, ty = when_env_param_type())]
    pub when_env: Option<WhenEnv>,

    /// How the raw material is obtained. Flattened, so a route is written as
    /// one map — `{"provider": "exec", "helper": [...], "protocol": "raw"}` —
    /// rather than nesting the provider's own fields a level deeper.
    #[serde(flatten)]
    #[spec(flatten)]
    pub source: Source,

    /// Steps that turn what the source produced into the final credential.
    ///
    /// Empty for a source that already yields one, which is the common case for
    /// `static_env` and `exec`. A pipeline, because GCP federation is two hops.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[spec(parse = parse_exchange_value, ty = exchange_param_type())]
    pub exchange: Vec<Exchange>,

    /// Declared lifetime, used only when nothing better is known.
    ///
    /// A declaration, not an observation — see [`crate::expiry`] for the
    /// precedence order and why a `ttl` *longer* than the truth is the
    /// dangerous direction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

/// The schema type for a single-or-list `exchange`.
fn exchange_param_type() -> crate::htvalue::signature::ParamType {
    use crate::htspec::FromSpecValue as _;
    let one = Exchange::spec_param_type();
    crate::htvalue::signature::ParamType::union(vec![
        one.clone(),
        crate::htvalue::signature::ParamType::list(one),
    ])
}

/// The schema type for a `when_env` guard: a variable name, or a map of exact
/// values.
fn when_env_param_type() -> crate::htvalue::signature::ParamType {
    use crate::htvalue::signature::ParamType;
    ParamType::union(vec![
        ParamType::String,
        ParamType::map(ParamType::String),
        ParamType::Null,
    ])
}

fn parse_when_env(v: &crate::htvalue::Value) -> anyhow::Result<Option<WhenEnv>> {
    use crate::htvalue::Value;
    Ok(match v {
        Value::Null() => None,
        Value::String(name) => Some(WhenEnv::Set(name.clone())),
        Value::Map(m) => Some(WhenEnv::Equals(
            m.iter()
                .map(|(k, val)| match val {
                    Value::String(s) => Ok((k.clone(), s.clone())),
                    other => anyhow::bail!("`when_env` values must be strings, got {other:?}"),
                })
                .collect::<anyhow::Result<BTreeMap<_, _>>>()?,
        )),
        other => anyhow::bail!(
            "`when_env` must be a variable name or a map of name → exact value, got {other:?}"
        ),
    })
}

/// How long an `exec` helper may run before the mint fails.
///
/// Long enough for a network round trip and an STS call, short enough that a
/// build waiting on a desktop approval nobody is watching fails with a message
/// rather than hanging until someone notices.
pub const DEFAULT_HELPER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl Acquire {
    /// The helper deadline: the declared one, or [`DEFAULT_HELPER_TIMEOUT`].
    ///
    /// Meaningful only for [`Source::Exec`]; a deadline on any other source is
    /// now unwritable rather than something to reject.
    pub fn helper_timeout(&self) -> anyhow::Result<std::time::Duration> {
        let declared = match &self.source {
            Source::Exec { timeout, .. } => timeout.as_deref(),
            _ => None,
        };
        match declared {
            None => Ok(DEFAULT_HELPER_TIMEOUT),
            Some(s) => humantime::parse_duration(s)
                .map_err(|e| anyhow::anyhow!("invalid timeout {s:?}: {e}")),
        }
    }

    /// The parsed [`Self::ttl`], if any.
    pub fn ttl_duration(&self) -> anyhow::Result<Option<std::time::Duration>> {
        match &self.ttl {
            None => Ok(None),
            Some(s) => humantime::parse_duration(s)
                .map(Some)
                .map_err(|e| anyhow::anyhow!("invalid ttl {s:?}: {e}")),
        }
    }

    /// What remains to check once the shape is a tagged union.
    ///
    /// Three rules, where there were eight. The five that went away did not
    /// move somewhere else — they became unwritable: `helper` on a
    /// `static_env`, `var` on an `exec`, a `runner` or `timeout` on an `oidc`,
    /// an `exec` with no `protocol`, and the `var`/`vars` pair, which collapsed
    /// into one map. What is left is the genuinely cross-field part: a
    /// non-empty argv, an `oidc` that has something to exchange its assertion
    /// for, and durations that parse.
    pub fn validate(&self, addr: &str, index: usize) -> anyhow::Result<()> {
        let at = || format!("secret {addr}: acquire[{index}]");
        match &self.source {
            Source::StaticEnv { vars } => {
                if vars.is_empty() {
                    anyhow::bail!(
                        "{}: static_env needs `var` (one variable) or `vars` (several)",
                        at()
                    );
                }
            }
            Source::Exec { helper, .. } => {
                if helper.is_empty() {
                    anyhow::bail!("{}: exec needs a `helper` argv", at());
                }
                self.helper_timeout()
                    .map_err(|e| anyhow::anyhow!("{}: {e}", at()))?;
            }
            Source::Oidc {} => {
                if self.exchange.is_empty() {
                    anyhow::bail!(
                        "{}: oidc needs an `exchange`. An assertion is not a credential: say \
                         what to trade it for — a `token_exchange`, a `jwt_bearer`, an `aws_sts`, \
                         or an `http` call.",
                        at()
                    );
                }
            }
        }
        // Each step's destination resolves at declaration time, so a grant
        // naming neither an issuer nor an endpoint — or both — fails here
        // rather than on the first mint of a build that got that far.
        for (i, step) in self.exchange.iter().enumerate() {
            step.endpoint()
                .map_err(|e| anyhow::anyhow!("{}: exchange[{i}]: {e}", at()))?;
        }
        if self.ttl.is_some() {
            self.ttl_duration()
                .map_err(|e| anyhow::anyhow!("{}: {e}", at()))?;
        }
        Ok(())
    }
}

/// A whole `secret()` declaration: both halves, as the driver parses it and as
/// the broker consumes it.
///
/// Only [`Self::identity`] is ever written to an artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    /// Canonical address of the descriptor target, for diagnostics.
    pub addr: String,
    pub identity: Identity,
    /// Ordered candidates. Non-empty after [`Self::validate`].
    pub acquire: Vec<Acquire>,
    /// Which targets may hold this credential, as an `htmatcher` query —
    /// `"//svc/... + label(deploy)"`. Empty permits any.
    ///
    /// Access control without a new ACL system: which credentials exist is
    /// CODEOWNERS on the declaring package, and which targets may *use* one is
    /// this line, in the same reviewed file.
    ///
    /// Unhashed, and deliberately so. It decides whether a build is permitted,
    /// not what it computes — a target that passes the check produces exactly
    /// what it would have without one, so folding it into a key would
    /// invalidate every consumer for a policy edit that changed no output.
    pub allow: Option<String>,
}

/// Which entry was chosen, and why — so `heph auth show` can report a route
/// that is otherwise invisible in the output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection<'a> {
    pub index: usize,
    pub entry: &'a Acquire,
    /// `None` for the unguarded catch-all.
    pub matched: Option<String>,
}

impl Descriptor {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.acquire.is_empty() {
            anyhow::bail!(
                "secret {}: no way to acquire a value. Give it a `provider`, or an `acquire` list.",
                self.addr
            );
        }
        for (i, a) in self.acquire.iter().enumerate() {
            a.validate(&self.addr, i)?;
        }
        // An unguarded entry always matches, so anything after it is dead. Say
        // so at spec time rather than letting a route silently never run.
        if let Some(pos) = self.acquire.iter().position(|a| a.when_env.is_none())
            && pos + 1 < self.acquire.len()
        {
            anyhow::bail!(
                "secret {}: acquire[{pos}] has no `when_env`, so it always matches and the {} \
                 entries after it can never be selected. The unguarded entry is the catch-all and \
                 must come last.",
                self.addr,
                self.acquire.len() - pos - 1
            );
        }
        Ok(())
    }

    /// Pick the first entry whose guard matches.
    ///
    /// **Selection, not fallback.** A chosen entry that fails, fails the build;
    /// heph does not try the next one. Falling through on failure would mean a
    /// broken OIDC exchange in CI quietly reaching for a laptop helper, and
    /// then either failing somewhere far less legible or — worse — succeeding
    /// as a different identity, under a cache key that claims the first one.
    pub fn select(&self, env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Selection<'_>> {
        for (index, entry) in self.acquire.iter().enumerate() {
            match &entry.when_env {
                None => {
                    return Ok(Selection {
                        index,
                        entry,
                        matched: None,
                    });
                }
                Some(w) if w.matches(env) => {
                    return Ok(Selection {
                        index,
                        entry,
                        matched: Some(w.describe()),
                    });
                }
                Some(_) => {}
            }
        }
        // Nothing matched, and the diagnostic has to explain *why* rather than
        // report a missing credential: list each guard beside whether that
        // variable was set, and to what.
        let mut lines = String::new();
        for (i, entry) in self.acquire.iter().enumerate() {
            let Some(w) = &entry.when_env else { continue };
            let state = match w {
                WhenEnv::Set(name) => match env(name) {
                    None => format!("{name} is unset"),
                    Some(v) if v.is_empty() => format!("{name} is set but empty (counts as unset)"),
                    Some(_) => format!("{name} is set"),
                },
                WhenEnv::Equals(map) => map
                    .iter()
                    .map(|(k, want)| match env(k) {
                        None => format!("{k} unset, wanted {want:?}"),
                        Some(got) => format!("{k}={got:?}, wanted {want:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("; "),
            };
            lines.push_str(&format!("\n  acquire[{i}]  {:<28} {state}", w.describe()));
        }
        anyhow::bail!(
            "secret {}: no acquire entry matched this environment.{lines}\n\n  Add an entry with \
             no `when_env` as the catch-all, or set one of the variables above.",
            self.addr
        )
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

    fn acq(source: Source) -> Acquire {
        Acquire {
            when_env: None,
            source,
            exchange: Vec::new(),
            ttl: None,
        }
    }

    fn static_env(var: &str) -> Source {
        Source::StaticEnv {
            vars: BTreeMap::from([(
                crate::value::Credential::PRIMARY.to_string(),
                var.to_string(),
            )]),
        }
    }

    fn exec_source() -> Source {
        Source::Exec {
            helper: vec!["gh".into(), "auth".into(), "token".into()],
            protocol: Protocol::Raw,
            runner: None,
            timeout: None,
        }
    }

    fn exec_acq() -> Acquire {
        acq(exec_source())
    }

    /// A golden freeze of the emitted bytes.
    ///
    /// These bytes **are** a cache-key component: a consumer's `hashin` folds in
    /// the hashout of this artifact. Their layout comes from `serde_json`'s
    /// pretty-printer, which is an implementation detail and not a documented
    /// output contract — so a patch release that changed indentation or the
    /// `": "` separator would move every descriptor's hashout in the world,
    /// re-key every consumer of every credential, and split a mixed-version
    /// fleet permanently off each other's remote cache. It would present as
    /// "heph got slow after the upgrade" and nothing would report it.
    ///
    /// This test turns that into a build failure. If it fails after a dependency
    /// bump, the fix is not to update the constant: it is to decide whether the
    /// cache break is acceptable and, if not, to pin the layout here instead of
    /// borrowing serde's.
    #[test]
    fn the_emitted_bytes_are_frozen() {
        let json = SecretJson::new(
            "//infra/creds:ecr",
            Identity {
                role: Some("arn:aws:iam::4711:role/heph-ci-push".into()),
                params: BTreeMap::from([("region".to_string(), "eu-west-1".to_string())]),
                profile: Some("ecr".into()),
                shape: vec!["aws_profile".into()],
                ..Identity::default()
            },
        );
        let got = String::from_utf8(json.to_bytes().expect("bytes")).expect("utf8");
        let want = concat!(
            "{\n",
            "  \"version\": 1,\n",
            "  \"addr\": \"//infra/creds:ecr\",\n",
            "  \"identity\": {\n",
            "    \"role\": \"arn:aws:iam::4711:role/heph-ci-push\",\n",
            // `region` renders inside `params` and no longer as a field of its
            // own. `SECRET_JSON_VERSION` deliberately does not move for it: the
            // layout changed before anything shipped, and a version bump would
            // promise a migration for artifacts that never existed. What it
            // does change is every consumer's key, which is the correct and
            // only consequence.
            "    \"params\": {\n",
            "      \"region\": \"eu-west-1\"\n",
            "    },\n",
            "    \"profile\": \"ecr\",\n",
            "    \"shape\": [\n",
            "      \"aws_profile\"\n",
            "    ]\n",
            "  }\n",
            "}\n",
        );
        assert_eq!(
            got, want,
            "the secret.json byte layout moved — see the doc above"
        );

        // And it round-trips, so an older heph reading a newer artifact is a
        // version question rather than a parse accident.
        assert_eq!(
            SecretJson::parse(got.as_bytes(), "//infra/creds:ecr").expect("round trip"),
            json
        );
    }

    /// Two descriptors distinguished only by their acquisition halves must not
    /// produce identical artifacts.
    ///
    /// Leaving the address out was a cache-poisoning bug: `//creds:prod` reading
    /// `PROD_API_KEY` and `//creds:staging` reading `STAGING_API_KEY` have equal
    /// (empty) identities, so they emitted byte-identical `secret.json`, and a
    /// consumer of one was a cache hit on the other's artifact — served output
    /// built with the production key, silently.
    #[test]
    fn two_descriptors_with_equal_identities_still_differ() {
        let prod = SecretJson::new("//infra/creds:prod", Identity::default())
            .to_bytes()
            .expect("bytes");
        let staging = SecretJson::new("//infra/creds:staging", Identity::default())
            .to_bytes()
            .expect("bytes");
        assert_ne!(prod, staging, "two credentials produced one artifact");
    }

    /// The load-bearing property of the whole design: the emitted artifact is
    /// the identity half and nothing else, so swapping acquisition leaves every
    /// consumer's `hashin` byte-identical.
    #[test]
    fn secret_json_carries_identity_only() {
        let identity = Identity {
            role: Some("arn:aws:iam::4711:role/heph-read".into()),
            params: BTreeMap::from([("region".to_string(), "eu-west-1".to_string())]),
            profile: Some("artifacts".into()),
            shape: vec!["aws_profile".into()],
            ..Identity::default()
        };

        let ci = SecretJson::new("//infra/creds:artifacts", identity.clone())
            .to_bytes()
            .expect("bytes");

        // Same identity, wildly different acquisition: OIDC in CI, an exec
        // helper under a runner on a laptop, a different TTL.
        let laptop = SecretJson::new("//infra/creds:artifacts", identity)
            .to_bytes()
            .expect("bytes");
        assert_eq!(ci, laptop);

        let text = String::from_utf8(ci).expect("utf8");
        for absent in [
            "provider", "helper", "acquire", "ttl", "runner", "exchange", "when_env",
        ] {
            assert!(
                !text.contains(absent),
                "acquisition field {absent:?} leaked into secret.json:\n{text}"
            );
        }
        assert!(text.contains("heph-read"));
    }

    /// Empty identity fields must not serialize, or adding a field later
    /// re-keys every descriptor in every workspace that does not use it.
    #[test]
    fn empty_identity_fields_are_omitted() {
        let bytes = SecretJson::new(
            "//c:x",
            Identity {
                machine: Some("github.com".into()),
                ..Identity::default()
            },
        )
        .to_bytes()
        .expect("bytes");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.contains("machine"));
        assert!(!text.contains("role"), "absent field serialized:\n{text}");
        assert!(!text.contains("audience"));
    }

    #[test]
    fn version_is_checked_by_exact_match() {
        let good = SecretJson::new("//x:y", Identity::default())
            .to_bytes()
            .expect("b");
        SecretJson::parse(&good, "//x:y").expect("parses");

        let bad = String::from_utf8(good)
            .expect("utf8")
            .replace("\"version\": 1", "\"version\": 2");
        let err = SecretJson::parse(bad.as_bytes(), "//x:y").expect_err("rejects v2");
        assert!(err.to_string().contains("version 2"), "{err}");
        assert!(err.to_string().contains("//x:y"), "{err}");
    }

    #[test]
    fn set_but_empty_counts_as_unset() {
        let w = WhenEnv::Set("GITHUB_ACTIONS".into());
        assert!(w.matches(&env_of(&[("GITHUB_ACTIONS", "true")])));
        assert!(!w.matches(&env_of(&[("GITHUB_ACTIONS", "")])));
        assert!(!w.matches(&env_of(&[])));
    }

    #[test]
    fn equals_guard_wants_every_variable() {
        let w = WhenEnv::Equals(BTreeMap::from([
            ("CI".to_string(), "true".to_string()),
            ("AWS_REGION".to_string(), "eu-west-1".to_string()),
        ]));
        assert!(w.matches(&env_of(&[("CI", "true"), ("AWS_REGION", "eu-west-1")])));
        assert!(!w.matches(&env_of(&[("CI", "true")])));
        assert!(!w.matches(&env_of(&[("CI", "true"), ("AWS_REGION", "us-east-1")])));
    }

    #[test]
    fn first_matching_entry_wins_and_unguarded_is_the_catch_all() {
        let d = Descriptor {
            addr: "//infra/creds:artifacts".into(),
            identity: Identity::default(),
            acquire: vec![
                Acquire {
                    when_env: Some(WhenEnv::Set("GITHUB_ACTIONS".into())),
                    exchange: vec![Exchange::AwsSts { endpoint: None }],
                    ..acq(Source::Oidc {})
                },
                exec_acq(),
            ],
            allow: None,
        };
        d.validate().expect("valid");

        let ci = d
            .select(&env_of(&[("GITHUB_ACTIONS", "true")]))
            .expect("ci");
        assert_eq!(ci.index, 0);
        assert_eq!(ci.entry.source.kind(), ProviderKind::Oidc);

        let laptop = d.select(&env_of(&[])).expect("laptop");
        assert_eq!(laptop.index, 1);
        assert_eq!(laptop.entry.source.kind(), ProviderKind::Exec);
        assert!(laptop.matched.is_none());
    }

    /// The failure has to list each guard beside the observed state. A bare
    /// "no credential" three layers down is the diagnostic this replaces.
    #[test]
    fn no_match_names_every_guard_and_what_was_observed() {
        let d = Descriptor {
            addr: "//infra/creds:ecr".into(),
            identity: Identity::default(),
            acquire: vec![
                Acquire {
                    when_env: Some(WhenEnv::Set("GITHUB_ACTIONS".into())),
                    ..exec_acq()
                },
                Acquire {
                    when_env: Some(WhenEnv::Set("BUILDKITE".into())),
                    ..exec_acq()
                },
            ],
            allow: None,
        };
        let err = d
            .select(&env_of(&[("GITHUB_ACTIONS", "")]))
            .expect_err("no match");
        let msg = err.to_string();
        assert!(msg.contains("//infra/creds:ecr"), "{msg}");
        assert!(msg.contains("set but empty"), "{msg}");
        assert!(msg.contains("BUILDKITE is unset"), "{msg}");
    }

    /// An unguarded entry always matches, so anything after it is dead code in
    /// a file nobody re-reads. Fail at spec time.
    #[test]
    fn entries_after_the_catch_all_are_rejected() {
        let d = Descriptor {
            addr: "//x:y".into(),
            identity: Identity::default(),
            acquire: vec![exec_acq(), exec_acq()],
            allow: None,
        };
        let err = d.validate().expect_err("unreachable entry");
        assert!(err.to_string().contains("must come last"), "{err}");
    }

    /// `protocol` on an `exec` is now required *by construction* — there is no
    /// `Acquire` value that omits it — so the check that used to enforce it is
    /// gone rather than moved. What the tagged union cannot express is an empty
    /// argv, and that is still checked.
    #[test]
    fn exec_with_an_empty_helper_is_rejected_at_spec_time() {
        let a = acq(Source::Exec {
            helper: Vec::new(),
            protocol: Protocol::Raw,
            runner: None,
            timeout: None,
        });
        let err = a.validate("//x:y", 0).expect_err("needs an argv");
        assert!(err.to_string().contains("`helper` argv"), "{err}");
    }

    /// An assertion is not a credential: an `oidc` source with nothing to trade
    /// it for cannot work, and says so at the declaration.
    #[test]
    fn oidc_without_an_exchange_is_rejected_at_spec_time() {
        let err = acq(Source::Oidc {})
            .validate("//x:y", 0)
            .expect_err("needs an exchange");
        let msg = err.to_string();
        assert!(msg.contains("needs an `exchange`"), "{msg}");
        assert!(msg.contains("token_exchange"), "{msg}");
    }

    #[test]
    fn static_env_needs_a_variable_name() {
        let err = acq(Source::StaticEnv {
            vars: BTreeMap::new(),
        })
        .validate("//x:y", 0)
        .expect_err("needs var");
        assert!(err.to_string().contains("`var`"), "{err}");

        acq(static_env("TOKEN"))
            .validate("//x:y", 0)
            .expect("one variable is enough");
    }

    #[test]
    fn ttl_is_parsed_at_spec_time() {
        let good = Acquire {
            ttl: Some("1h".into()),
            ..exec_acq()
        };
        assert_eq!(
            good.ttl_duration().expect("parses"),
            Some(std::time::Duration::from_secs(3600))
        );

        let bad = Acquire {
            ttl: Some("one hour".into()),
            ..exec_acq()
        };
        bad.validate("//x:y", 0).expect_err("unparseable ttl");
    }

    /// A deadline is a property of running a helper, so it exists only on the
    /// variant that runs one. There is no `Acquire` that puts one on a
    /// `static_env`, which is why no check needs to reject that any more.
    #[test]
    fn a_deadline_belongs_to_the_exec_variant_alone() {
        assert_eq!(
            exec_acq().helper_timeout().expect("default"),
            DEFAULT_HELPER_TIMEOUT
        );
        let slow = acq(Source::Exec {
            helper: vec!["op".into()],
            protocol: Protocol::Raw,
            runner: None,
            timeout: Some("5m".into()),
        });
        assert_eq!(
            slow.helper_timeout().expect("override"),
            std::time::Duration::from_secs(300)
        );
        // A non-exec source simply has nowhere to put one.
        assert_eq!(
            acq(static_env("TOK")).helper_timeout().expect("default"),
            DEFAULT_HELPER_TIMEOUT
        );
    }

    // ---- discovery ----

    fn token_exchange(issuer: Option<&str>, endpoint: Option<&str>) -> Exchange {
        Exchange::TokenExchange {
            issuer: issuer.map(str::to_string),
            endpoint: endpoint.map(str::to_string),
            audience: None,
            resource: None,
            scope: Vec::new(),
            requested_token_type: None,
        }
    }

    /// An issuer is the thing an administrator can actually tell you, so it is
    /// the spelling to reach for — and the endpoint comes from the metadata
    /// document every IdP publishes.
    #[test]
    fn an_issuer_resolves_to_its_discovery_document() {
        let e = token_exchange(Some("https://sts.googleapis.com"), None);
        let at = e.endpoint().expect("resolves").expect("has one");
        assert_eq!(
            at.discovery_url().as_deref(),
            Some("https://sts.googleapis.com/.well-known/openid-configuration")
        );
    }

    /// OIDC Discovery specifies concatenation, so an issuer written with a
    /// trailing slash would otherwise produce a double slash that some servers
    /// 404 on and others do not — a difference nobody should debug from a
    /// BUILD file.
    #[test]
    fn a_trailing_slash_on_the_issuer_does_not_double_up() {
        let e = token_exchange(Some("https://org.okta.com/oauth2/default/"), None);
        let at = e.endpoint().expect("resolves").expect("has one");
        assert_eq!(
            at.discovery_url().as_deref(),
            Some("https://org.okta.com/oauth2/default/.well-known/openid-configuration")
        );
    }

    /// The escape hatch: a server that publishes no metadata.
    #[test]
    fn a_literal_endpoint_is_used_as_written_and_discovers_nothing() {
        let e = token_exchange(None, Some("https://internal.example/mint"));
        let at = e.endpoint().expect("resolves").expect("has one");
        assert_eq!(at, Endpoint::Literal("https://internal.example/mint"));
        assert!(at.discovery_url().is_none());
    }

    /// Neither, or both, is a declaration-time failure — silently preferring
    /// one would make which server was contacted depend on an invisible
    /// precedence.
    #[test]
    fn a_grant_needs_exactly_one_of_issuer_and_endpoint() {
        let err = token_exchange(None, None).endpoint().expect_err("neither");
        let msg = err.to_string();
        assert!(msg.contains("issuer"), "{msg}");
        assert!(msg.contains(OIDC_DISCOVERY_PATH), "{msg}");

        let err = token_exchange(Some("https://a"), Some("https://b"))
            .endpoint()
            .expect_err("both");
        assert!(err.to_string().contains("set one"), "{err}");
    }

    /// The two steps that name their own destination speak no OAuth metadata.
    #[test]
    fn vendor_steps_have_no_oauth_endpoint_or_grant() {
        for e in [
            Exchange::AwsSts { endpoint: None },
            Exchange::Http {
                url: "https://api.github.com/x".into(),
                method: None,
                headers: BTreeMap::new(),
                body: None,
                fields: BTreeMap::new(),
            },
        ] {
            assert!(e.endpoint().expect("no error").is_none(), "{e:?}");
            assert!(e.grant_type().is_none(), "{e:?}");
        }
    }

    /// Discovery buys a diagnostic: the metadata lists `grant_types_supported`,
    /// so a server that does not implement the grant can be named before the
    /// request rather than arriving as a bare `400 unsupported_grant_type`.
    #[test]
    fn each_oauth_grant_names_its_urn() {
        assert_eq!(
            token_exchange(Some("https://x"), None).grant_type(),
            Some("urn:ietf:params:oauth:grant-type:token-exchange")
        );
        assert_eq!(
            Exchange::JwtBearer {
                issuer: Some("https://x".into()),
                endpoint: None,
                scope: Vec::new(),
            }
            .grant_type(),
            Some("urn:ietf:params:oauth:grant-type:jwt-bearer")
        );
        assert_eq!(
            Exchange::ClientCredentials {
                issuer: Some("https://x".into()),
                endpoint: None,
                scope: Vec::new(),
            }
            .grant_type(),
            Some("client_credentials")
        );
    }

    /// A malformed grant fails at the declaration, not on the first mint of a
    /// build that already got that far.
    #[test]
    fn a_grant_with_no_destination_fails_when_the_route_is_validated() {
        let a = Acquire {
            exchange: vec![token_exchange(None, None)],
            ..acq(Source::Oidc {})
        };
        let err = a.validate("//x:y", 0).expect_err("no destination");
        let msg = err.to_string();
        assert!(msg.contains("exchange[0]"), "{msg}");
        assert!(msg.contains("issuer"), "{msg}");
    }

    #[test]
    fn only_two_protocols_carry_expiry() {
        assert!(Protocol::CredentialHelper.carries_expiry());
        assert!(Protocol::CredentialProcess.carries_expiry());
        assert!(!Protocol::Raw.carries_expiry());
        assert!(!Protocol::DockerCredential.carries_expiry());
    }
}
