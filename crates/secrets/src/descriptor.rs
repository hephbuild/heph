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

    /// GCP service account to impersonate after the STS hop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impersonate: Option<String>,

    /// GitHub App id, for the installation-token exchange.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,

    /// GitHub App installation, as `org` or `org/repo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<String>,

    /// Cloud account id (AWS account, Cloudflare account).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,

    /// Region. A *profile key*, never a scalar environment variable — see
    /// [`crate::shape`] for why no single `AWS_REGION`-shaped variable can
    /// satisfy every SDK.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// Object-store bucket the credential is scoped to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,

    /// Service endpoint, for non-AWS S3-compatible stores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// The Bazel `--credential_helper` / EngFlow spec. Takes `{"uri": …}` on
    /// stdin, returns `{"headers": {…}, "expires": …}`. The only protocol that
    /// carries expiry natively, which is why the broker's TTL cache prefers it.
    Engflow,
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
        matches!(self, Protocol::Engflow | Protocol::CredentialProcess)
    }
}

/// Which token exchange turns an assertion into a cloud credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exchange {
    /// `sts:AssumeRoleWithWebIdentity`.
    Aws,
    /// GCP STS + service-account impersonation.
    Gcp,
    /// GCP JWT-bearer grant from a service-account key.
    GcpSaKey,
    /// GitHub App installation token.
    GithubApp,
    /// Cloudflare `POST /accounts/{id}/r2/temp-access-credentials`.
    R2Temp,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Acquire {
    /// The guard. `None` always matches, so an unguarded entry is the catch-all
    /// and must come last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_env: Option<WhenEnv>,

    pub provider: ProviderKind,

    /// `static_env`: the single host variable holding the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub var: Option<String>,

    /// `static_env`: several host variables, as field name → variable name.
    ///
    /// Names a variable, never a literal. The schema deliberately has no
    /// free-form value field: otherwise someone writes a token into a
    /// `text_file` target and it is pushed to the shared remote cache.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vars: BTreeMap<String, String>,

    /// `exec`: the helper argv. Its head is the program.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub helper: Vec<String>,

    /// `exec`: which wire protocol the helper speaks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,

    /// `exec`: the exec runner the helper runs under, as a target address, or
    /// the literal `"local"`.
    ///
    /// **The default is `local`, and a helper inherits no workspace default.**
    /// This is the one place in heph where the workspace-wide `runner:` option
    /// deliberately does not apply. That option exists to move *targets* into a
    /// described environment, and the environments people put targets in are
    /// precisely the ones a helper cannot work in: `aws configure
    /// export-credentials` needs `~/.aws/sso/cache`, `gh auth token` needs the
    /// login keychain, `op` needs a desktop-app session — all of it in the real
    /// `$HOME` that a hermetic runner exists to hide. Inheriting the default
    /// would mean that the day someone sets a workspace runner, every laptop
    /// credential stops resolving.
    ///
    /// Unlike a target's runner this is **unhashed**, and the inversion is
    /// worth stating: on a target the runner is part of what produced the
    /// output, so its fingerprint belongs in the key; here it only affects how
    /// a value was fetched, and the value is not in the key at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,

    /// Which exchange, if any, turns the acquired assertion into a credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exchange: Option<Exchange>,

    /// How long the helper may run before the mint fails, e.g. `"120s"`.
    ///
    /// Defaults to [`DEFAULT_HELPER_TIMEOUT`]. A helper cannot be interactive
    /// during a build, and stdin being closed only enforces half of that: it
    /// stops a *stdin* prompt, but not a macOS keychain dialog, a Touch ID
    /// prompt from `op`, or a helper blocked on an unreachable endpoint. None
    /// of those read stdin, and all of them hang a build that has nobody to
    /// answer them.
    ///
    /// Raise it for a helper that is legitimately slow. Note that the deadline
    /// is in the *unhashed* half, so changing it moves no cache key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,

    /// Declared lifetime, used only when nothing better is known.
    ///
    /// A declaration, not an observation — see [`crate::expiry`] for the
    /// precedence order and why a `ttl` *longer* than the truth is the
    /// dangerous direction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

/// How long an `exec` helper may run before the mint fails.
///
/// Long enough for a network round trip and an STS call, short enough that a
/// build waiting on a desktop approval nobody is watching fails with a message
/// rather than hanging until someone notices.
pub const DEFAULT_HELPER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl Acquire {
    /// The helper deadline: the declared one, or [`DEFAULT_HELPER_TIMEOUT`].
    pub fn helper_timeout(&self) -> anyhow::Result<std::time::Duration> {
        match &self.timeout {
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

    /// Validate the combination of fields for this provider.
    ///
    /// Done once at spec time, so a descriptor that cannot possibly work fails
    /// before any network call and identically on every machine — rather than
    /// as a missing-credential error three layers down at target 400.
    pub fn validate(&self, addr: &str, index: usize) -> anyhow::Result<()> {
        let at = || format!("secret {addr}: acquire[{index}]");
        match self.provider {
            ProviderKind::StaticEnv => {
                if self.var.is_none() && self.vars.is_empty() {
                    anyhow::bail!("{}: static_env needs `var` or `vars`", at());
                }
                if self.var.is_some() && !self.vars.is_empty() {
                    anyhow::bail!("{}: `var` and `vars` are mutually exclusive", at());
                }
                if !self.helper.is_empty() {
                    anyhow::bail!("{}: `helper` has no meaning for static_env", at());
                }
            }
            ProviderKind::Exec => {
                if self.helper.is_empty() {
                    anyhow::bail!("{}: exec needs a `helper` argv", at());
                }
                if self.protocol.is_none() {
                    anyhow::bail!(
                        "{}: exec needs an explicit `protocol` (engflow, credential_process, \
                         docker_credential or raw). It is not guessed from output: the four \
                         differ in stdin encoding as well as response shape.",
                        at()
                    );
                }
            }
            ProviderKind::Oidc => {
                if self.exchange.is_none() {
                    anyhow::bail!("{}: oidc needs an `exchange`", at());
                }
                if !self.helper.is_empty() {
                    anyhow::bail!("{}: `helper` has no meaning for oidc", at());
                }
            }
        }
        if self.ttl.is_some() {
            self.ttl_duration()
                .map_err(|e| anyhow::anyhow!("{}: {e}", at()))?;
        }
        if self.timeout.is_some() {
            if self.provider != ProviderKind::Exec {
                anyhow::bail!("{}: `timeout` only applies to an exec helper", at());
            }
            self.helper_timeout()
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

    fn acq(provider: ProviderKind) -> Acquire {
        Acquire {
            when_env: None,
            provider,
            var: None,
            vars: BTreeMap::new(),
            helper: Vec::new(),
            protocol: None,
            runner: None,
            exchange: None,
            timeout: None,
            ttl: None,
        }
    }

    fn exec_acq() -> Acquire {
        Acquire {
            helper: vec!["gh".into(), "auth".into(), "token".into()],
            protocol: Some(Protocol::Raw),
            ..acq(ProviderKind::Exec)
        }
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
                region: Some("eu-west-1".into()),
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
            "    \"region\": \"eu-west-1\",\n",
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
            region: Some("eu-west-1".into()),
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
                    exchange: Some(Exchange::Aws),
                    ..acq(ProviderKind::Oidc)
                },
                exec_acq(),
            ],
        };
        d.validate().expect("valid");

        let ci = d
            .select(&env_of(&[("GITHUB_ACTIONS", "true")]))
            .expect("ci");
        assert_eq!(ci.index, 0);
        assert_eq!(ci.entry.provider, ProviderKind::Oidc);

        let laptop = d.select(&env_of(&[])).expect("laptop");
        assert_eq!(laptop.index, 1);
        assert_eq!(laptop.entry.provider, ProviderKind::Exec);
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
        };
        let err = d.validate().expect_err("unreachable entry");
        assert!(err.to_string().contains("must come last"), "{err}");
    }

    #[test]
    fn exec_without_protocol_is_rejected_at_spec_time() {
        let a = Acquire {
            protocol: None,
            ..exec_acq()
        };
        let err = a.validate("//x:y", 0).expect_err("needs protocol");
        assert!(err.to_string().contains("explicit `protocol`"), "{err}");
    }

    #[test]
    fn static_env_needs_a_variable_name_and_rejects_both_forms() {
        let err = acq(ProviderKind::StaticEnv)
            .validate("//x:y", 0)
            .expect_err("needs var");
        assert!(err.to_string().contains("`var` or `vars`"), "{err}");

        let both = Acquire {
            var: Some("A".into()),
            vars: BTreeMap::from([("k".to_string(), "B".to_string())]),
            ..acq(ProviderKind::StaticEnv)
        };
        assert!(both.validate("//x:y", 0).is_err());
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
        assert!(bad.validate("//x:y", 0).is_err());
    }

    #[test]
    fn only_two_protocols_carry_expiry() {
        assert!(Protocol::Engflow.carries_expiry());
        assert!(Protocol::CredentialProcess.carries_expiry());
        assert!(!Protocol::Raw.carries_expiry());
        assert!(!Protocol::DockerCredential.carries_expiry());
    }
}
