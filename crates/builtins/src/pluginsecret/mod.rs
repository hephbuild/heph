//! The `secret` driver: declares **how to obtain a credential, never the
//! credential**.
//!
//! A `secret()` target's single output is a `secret.json` holding the *identity*
//! half of the declaration — role, audience, scope, registry, machine, profile.
//! Safe to cache, safe to push to a shared remote cache, safe to print. It
//! becomes a `hashed: true, runtime: false` input on every consumer, exactly
//! like `hash_deps`, so a consumer's cache key moves when the identity moves.
//!
//! The *acquisition* half — provider, helper argv, exchange, TTL, which runner
//! the helper runs under — is deliberately **not** written to the artifact. It
//! lives in the spec, and the broker reads it through [`parse_declaration`], the
//! same way the engine reads a `scratch` declaration. That is what makes "the
//! acquisition half is unhashed" structural rather than a rule somebody has to
//! remember: a consumer's `hashin` folds in its inputs' *hashouts*, and a field
//! that never becomes an artifact has no hashout to contribute.
//!
//! The practical consequence, and the reason the split exists: swapping `oidc`
//! for an `exec` helper, editing a helper path, or bumping a TTL re-runs this
//! target and produces byte-identical bytes — so CI and a laptop share one cache
//! entry for every consumer.
//!
//! Like `scratch`, this driver is only the declaration. Minting, delivery,
//! redaction and expiry live in `crates/secrets` and the engine.
//!
//! ## Access control is CODEOWNERS
//!
//! Which credentials exist, and what identity each names, is a line in a BUILD
//! file under review — not whatever happened to be exported in the shell that
//! ran the build. There is deliberately no new ACL system here.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hcore::htvalue::Value;
use hcore::htvalue::signature::ParamType;
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, RunRequest, RunResponse, TargetAddr, outputartifact,
    targetdef::{
        CacheConfig, Output, TargetDef,
        path::{CodegenMode, Content, Path},
    },
};
use hplugin::htspec::Spec;
use hsecrets::descriptor::{
    Acquire, Descriptor, Exchange, Identity, Protocol, ProviderKind, SECRET_JSON, SecretJson,
    WhenEnv,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

pub const DRIVER_NAME: &str = "secret";

/// Config for a `secret` target.
///
/// The field list is split in two by comment, and the split is the whole design:
/// everything above `provider` reaches every consumer's cache key, and nothing
/// below it does.
#[derive(Spec, Debug)]
struct SecretSpec {
    // ---- identity: hashed, written to secret.json ----
    /// Role or principal to assume (an AWS role ARN, or a provider's equivalent).
    #[spec(ty = ParamType::String)]
    role: Option<String>,
    /// The `aud` claim the exchange will check.
    #[spec(ty = ParamType::String)]
    audience: Option<String>,
    /// OAuth scopes requested. Sorted before hashing, so declaration order is
    /// not a cache-key component.
    scope: Vec<String>,
    /// GCP service account to impersonate after the STS hop.
    #[spec(ty = ParamType::String)]
    impersonate: Option<String>,
    /// GitHub App id, for the installation-token exchange.
    #[spec(ty = ParamType::String)]
    app_id: Option<String>,
    /// GitHub App installation, as `org` or `org/repo`.
    #[spec(ty = ParamType::String)]
    install: Option<String>,
    /// Cloud account id (an AWS account, a Cloudflare account).
    #[spec(ty = ParamType::String)]
    account: Option<String>,
    /// Region. Rendered as a *profile key*, never a scalar environment
    /// variable: no single `AWS_REGION`-shaped variable satisfies boto3, the JS
    /// SDK and the Java SDK at once, and two secrets setting one would collide.
    #[spec(ty = ParamType::String)]
    region: Option<String>,
    /// Bucket the credential is scoped to.
    #[spec(ty = ParamType::String)]
    bucket: Option<String>,
    /// Service endpoint, for non-AWS S3-compatible stores.
    #[spec(ty = ParamType::String)]
    endpoint: Option<String>,
    /// Registry host. The merge key for the `docker_config` shape.
    #[spec(ty = ParamType::String)]
    registry: Option<String>,
    /// Machine / URL prefix. The merge key for `netrc` and `git_credential`.
    #[spec(ty = ParamType::String)]
    machine: Option<String>,
    /// Profile section name. The merge key for `aws_profile`; defaults to
    /// `default`, which is why two unnamed AWS credentials on one target
    /// collide rather than silently overwriting each other.
    #[spec(ty = ParamType::String)]
    profile: Option<String>,
    /// Which shapes this credential renders: `file` (the default), `env`,
    /// `netrc`, `docker_config`, `git_credential`, `aws_profile`, `gcloud_adc`.
    ///
    /// Identity, deliberately: a shape decides which files and variables exist
    /// in the sandbox, and that is part of what the target reads. It costs
    /// nothing in cache sharing, because a shape's paths and variable *names*
    /// are fixed while only the contents vary.
    shape: Vec<String>,
    /// For the `env` shape: variable name → pointer into the acquired value.
    /// `"$."` is the whole value, `"$.<field>"` one field.
    env: std::collections::HashMap<String, String>,

    // ---- acquisition: NOT hashed, never written to secret.json ----
    /// Ordered acquisition candidates. Each is a dict; the first whose
    /// `when_env` guard matches is used, and an entry with no guard is the
    /// catch-all and must come last.
    ///
    /// The flat form below is sugar for a single-entry list. Reach for this
    /// only when one identity genuinely has two routes — ambient in CI, a
    /// stored session on a laptop.
    #[spec(ty = ParamType::list(acquire_param_type()), parse = parse_acquire)]
    acquire: Vec<Acquire>,

    /// Flat form: which provider obtains the value (`static_env`, `exec`, `oidc`).
    #[spec(ty = ParamType::String)]
    provider: Option<String>,
    /// Flat form, `static_env`: the host variable holding the value.
    #[spec(ty = ParamType::String)]
    var: Option<String>,
    /// Flat form, `static_env`: field name → host variable name. Names a
    /// variable, never a literal — the schema has no free-form value field, so
    /// a token cannot be written into a target and pushed to a shared cache.
    vars: std::collections::HashMap<String, String>,
    /// Flat form, `exec`: the helper argv. Its head is the program.
    helper: Vec<String>,
    /// Flat form, `exec`: which wire protocol the helper speaks — `engflow`,
    /// `credential_process`, `docker_credential` or `raw`. Required for `exec`
    /// and never guessed: the four differ in stdin encoding as well as
    /// response shape.
    #[spec(ty = ParamType::String)]
    protocol: Option<String>,
    /// Flat form, `exec`: an exec runner for the helper, as a target address or
    /// the literal `"local"` (the default).
    ///
    /// A helper inherits **no** workspace-wide `runner:` default, unlike a
    /// target — a helper usually needs the real `$HOME` a hermetic runner
    /// exists to hide (`~/.aws/sso/cache`, the login keychain, a desktop-app
    /// session), so inheriting would break every laptop credential the day
    /// someone set one.
    #[spec(ty = ParamType::String)]
    runner: Option<String>,
    /// Flat form: which token exchange turns an assertion into a credential —
    /// `aws`, `gcp`, `gcp_sa_key`, `github_app`, `r2_temp`.
    #[spec(ty = ParamType::String)]
    exchange: Option<String>,
    /// Flat form, `exec`: how long the helper may run (`"120s"`) before the
    /// mint fails. Defaults to 60s.
    ///
    /// Closing stdin stops a helper prompting on stdin, but not a macOS
    /// keychain dialog, a Touch ID prompt, or a helper blocked on an
    /// unreachable endpoint — none of which read stdin, and all of which would
    /// otherwise hang a build nobody is watching.
    #[spec(ty = ParamType::String)]
    timeout: Option<String>,
    /// Flat form: declared lifetime (`"1h"`), used only when nothing better is
    /// known. A `ttl` *longer* than the truth is the dangerous direction.
    #[spec(ty = ParamType::String)]
    ttl: Option<String>,
}

/// What the driver keeps for `run`. Only the identity half: the acquisition
/// half is not the artifact's business, and putting it here would be one
/// refactor away from serializing it.
#[derive(serde::Serialize)]
struct SecretDef {
    /// The descriptor's canonical address. Written into `secret.json`, because
    /// it is what makes the artifact name a credential rather than merely
    /// describe one — see [`hsecrets::descriptor::SecretJson::addr`].
    addr: String,
    identity: Identity,
    out: String,
}

/// The LSP schema for one `acquire` entry.
///
/// A real struct rather than "any dict": the whole reason `acquire` keys are
/// rejected when unknown is that a silently-dropped `when-env` turns a guarded
/// entry into the catch-all, and an editor that can say so before the build is
/// strictly better than an error that can.
fn acquire_param_type() -> ParamType {
    ParamType::strukt(vec![
        (
            "when_env",
            ParamType::union(vec![ParamType::String, ParamType::map(ParamType::String)]),
        ),
        ("provider", ParamType::String),
        ("var", ParamType::String),
        ("vars", ParamType::map(ParamType::String)),
        ("helper", ParamType::list(ParamType::String)),
        ("protocol", ParamType::String),
        ("runner", ParamType::String),
        ("exchange", ParamType::String),
        ("timeout", ParamType::String),
        ("ttl", ParamType::String),
    ])
}

fn str_of(v: &Value, field: &str) -> anyhow::Result<String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        other => anyhow::bail!("`{field}` must be a string, got {other:?}"),
    }
}

/// Parse the `acquire` list: a list of dicts, each one route.
fn parse_acquire(v: &Value) -> anyhow::Result<Vec<Acquire>> {
    let items = match v {
        Value::List(items) => items,
        Value::Null() => return Ok(Vec::new()),
        other => anyhow::bail!("`acquire` must be a list of dicts, got {other:?}"),
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| acquire_from_map(item).with_context(|| format!("`acquire` entry {i}")))
        .collect()
}

fn acquire_from_map(v: &Value) -> anyhow::Result<Acquire> {
    let m = match v {
        Value::Map(m) => m,
        other => anyhow::bail!("expected a dict, got {other:?}"),
    };

    // Reject unknown keys rather than ignoring them: a typo'd `when-env` that
    // is silently dropped turns a guarded entry into the catch-all, which
    // selects the wrong identity without saying anything.
    const KNOWN: &[&str] = &[
        "when_env", "provider", "var", "vars", "helper", "protocol", "runner", "exchange", "ttl",
    ];
    for k in m.keys() {
        if !KNOWN.contains(&k.as_str()) {
            anyhow::bail!("unknown key {k:?}; expected one of {}", KNOWN.join(", "));
        }
    }

    let get = |k: &str| m.get(k).filter(|v| !matches!(v, Value::Null()));
    let opt_str =
        |k: &str| -> anyhow::Result<Option<String>> { get(k).map(|v| str_of(v, k)).transpose() };

    let when_env = match get("when_env") {
        None => None,
        Some(Value::String(s)) => Some(WhenEnv::Set(s.clone())),
        Some(Value::Map(m)) => Some(WhenEnv::Equals(
            m.iter()
                .map(|(k, v)| Ok((k.clone(), str_of(v, "when_env")?)))
                .collect::<anyhow::Result<BTreeMap<_, _>>>()?,
        )),
        Some(other) => anyhow::bail!(
            "`when_env` must be a variable name or a dict of name → exact value, got {other:?}"
        ),
    };

    let provider = provider_of(
        opt_str("provider")?
            .as_deref()
            .context("`provider` is required on an acquire entry")?,
    )?;

    Ok(Acquire {
        when_env,
        provider,
        var: opt_str("var")?,
        vars: match get("vars") {
            None => BTreeMap::new(),
            Some(Value::Map(m)) => m
                .iter()
                .map(|(k, v)| Ok((k.clone(), str_of(v, "vars")?)))
                .collect::<anyhow::Result<BTreeMap<_, _>>>()?,
            Some(other) => anyhow::bail!("`vars` must be a dict, got {other:?}"),
        },
        helper: match get("helper") {
            None => Vec::new(),
            Some(v) => hcore::htvalue::parse_strings(v).context("`helper`")?,
        },
        protocol: opt_str("protocol")?
            .as_deref()
            .map(protocol_of)
            .transpose()?,
        runner: opt_str("runner")?,
        exchange: opt_str("exchange")?
            .as_deref()
            .map(exchange_of)
            .transpose()?,
        timeout: opt_str("timeout")?,
        ttl: opt_str("ttl")?,
    })
}

fn provider_of(s: &str) -> anyhow::Result<ProviderKind> {
    Ok(match s {
        "static_env" => ProviderKind::StaticEnv,
        "exec" => ProviderKind::Exec,
        "oidc" => ProviderKind::Oidc,
        other => anyhow::bail!(
            "unknown `provider` {other:?} — expected \"static_env\", \"exec\" or \"oidc\""
        ),
    })
}

fn protocol_of(s: &str) -> anyhow::Result<Protocol> {
    Ok(match s {
        "engflow" => Protocol::Engflow,
        "credential_process" => Protocol::CredentialProcess,
        "docker_credential" => Protocol::DockerCredential,
        "raw" => Protocol::Raw,
        other => anyhow::bail!(
            "unknown `protocol` {other:?} — expected \"engflow\", \"credential_process\", \
             \"docker_credential\" or \"raw\". It is never guessed from output: the four differ \
             in stdin encoding as well as response shape."
        ),
    })
}

fn exchange_of(s: &str) -> anyhow::Result<Exchange> {
    Ok(match s {
        "aws" => Exchange::Aws,
        "gcp" => Exchange::Gcp,
        "gcp_sa_key" => Exchange::GcpSaKey,
        "github_app" => Exchange::GithubApp,
        "r2_temp" => Exchange::R2Temp,
        other => anyhow::bail!(
            "unknown `exchange` {other:?} — expected \"aws\", \"gcp\", \"gcp_sa_key\", \
             \"github_app\" or \"r2_temp\""
        ),
    })
}

/// Parse and validate a `secret()` declaration straight from a target spec.
///
/// The broker and the collision check both call this, for the same reason the
/// engine calls `pluginscratch::parse_declaration`: a `raw_def` is opaque to
/// the host by contract, and a spec is readable **without building or minting
/// anything**. That is what keeps the `allow` and collision checks running on a
/// fully warm build, where every consumer is a cache hit and no descriptor is
/// ever executed.
pub fn parse_declaration(spec: &hplugin::provider::TargetSpec) -> anyhow::Result<Descriptor> {
    let parsed = SecretSpec::from(&spec.config).context("parse secret config")?;
    let d = from_spec(parsed, &spec.addr.format(), &spec.addr.package)?;
    d.validate()?;
    Ok(d)
}

/// Normalize an acquire entry's `runner`.
///
/// `"local"` — the documented explicit opt-out — becomes `None`, and everything
/// else is resolved to a canonical absolute address against the descriptor's own
/// package. Both were previously unimplemented: `runner = "not an addr!!"`
/// parsed, validated and survived all the way to the broker, and `"local"` was
/// promised by two doc comments and understood by no code.
///
/// Resolving here rather than in `Acquire::validate` is deliberate — this is the
/// only layer that knows the declaring package, so `runner = ":devenv"` can mean
/// what it says.
fn normalize_runner(
    raw: Option<String>,
    pkg: &hmodel::htpkg::PkgBuf,
    addr: &str,
    index: usize,
) -> anyhow::Result<Option<String>> {
    let Some(raw) = raw else { return Ok(None) };
    if raw == "local" {
        return Ok(None);
    }
    let parsed = TargetAddr::parse(&raw, pkg).with_context(|| {
        format!(
            "secret {addr}: acquire[{index}] `runner` must be a target address producing a \
             runner.json, or the literal \"local\"; got {raw:?}"
        )
    })?;
    Ok(Some(parsed.to_string()))
}

fn from_spec(
    spec: SecretSpec,
    addr: &str,
    pkg: &hmodel::htpkg::PkgBuf,
) -> anyhow::Result<Descriptor> {
    let mut scope = spec.scope;
    // Sorted, because this reaches a cache key and declaration order must not.
    scope.sort();
    scope.dedup();

    let mut shape = spec.shape;
    if shape.is_empty() {
        shape.push("file".to_string());
    }
    shape.sort();
    shape.dedup();
    for s in &shape {
        hsecrets::shape::Shape::parse(s).with_context(|| format!("secret {addr}"))?;
    }

    let identity = Identity {
        role: spec.role,
        audience: spec.audience,
        scope,
        impersonate: spec.impersonate,
        app_id: spec.app_id,
        install: spec.install,
        account: spec.account,
        region: spec.region,
        bucket: spec.bucket,
        endpoint: spec.endpoint,
        registry: spec.registry,
        machine: spec.machine,
        profile: spec.profile,
        shape,
        // Normalized before it can reach a cache key, the same way `scope` and
        // `shape` are sorted: `"$."`, `"$"` and `"$.token"` name one field, so
        // all three must hash identically.
        env: spec
            .env
            .into_iter()
            .map(|(k, v)| {
                let field = hsecrets::value::normalize_pointer(&v)
                    .with_context(|| format!("secret {addr}: env[{k:?}]"))?;
                Ok((k, format!("$.{field}")))
            })
            .collect::<anyhow::Result<_>>()?,
    };

    // The flat form is sugar for a single-entry list. Mixing the two is a
    // mistake worth naming: it reads as "these compose" and they do not.
    let flat_used = spec.provider.is_some()
        || spec.var.is_some()
        || !spec.vars.is_empty()
        || !spec.helper.is_empty()
        || spec.protocol.is_some()
        || spec.runner.is_some()
        || spec.exchange.is_some()
        || spec.timeout.is_some()
        || spec.ttl.is_some();

    let acquire = match (spec.acquire.is_empty(), flat_used) {
        (false, true) => anyhow::bail!(
            "secret {addr}: the flat form (`provider`, `helper`, …) and `acquire` are two \
             spellings of the same thing, not two things that compose. Put every route in \
             `acquire`, or use the flat form for the single-route case."
        ),
        (false, false) => spec
            .acquire
            .into_iter()
            .enumerate()
            .map(|(i, a)| {
                Ok(Acquire {
                    runner: normalize_runner(a.runner, pkg, addr, i)?,
                    ..a
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
        (true, _) => {
            let provider = spec.provider.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "secret {addr}: no way to acquire a value. Give it a `provider` \
                     (static_env, exec or oidc), or an `acquire` list."
                )
            })?;
            vec![Acquire {
                when_env: None,
                provider: provider_of(provider)?,
                var: spec.var,
                vars: spec.vars.into_iter().collect(),
                helper: spec.helper,
                protocol: spec.protocol.as_deref().map(protocol_of).transpose()?,
                runner: normalize_runner(spec.runner, pkg, addr, 0)?,
                exchange: spec.exchange.as_deref().map(exchange_of).transpose()?,
                timeout: spec.timeout,
                ttl: spec.ttl,
            }]
        }
    };

    Ok(Descriptor {
        addr: addr.to_string(),
        identity,
        acquire,
    })
}

pub struct Driver;

#[async_trait]
impl hplugin::driver::Driver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        SecretSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let desc = parse_declaration(&req.target_spec)?;

        let pkg = req.target_spec.addr.package.as_str();
        let out = if pkg.is_empty() {
            SECRET_JSON.to_string()
        } else {
            format!("{pkg}/{SECRET_JSON}")
        };

        // The def hash covers the *whole* declaration, acquisition included, so
        // editing a helper re-runs this target. That is intentional and costs
        // nothing: the run writes a few hundred bytes, and the bytes it writes
        // are byte-identical, so no consumer's key moves. Excluding the
        // acquisition half here would save a trivial rebuild and lose the
        // property that a target's def reflects its declaration.
        let mut h = Xxh3::new();
        h.update(req.target_spec.addr.format().as_bytes());
        h.update(out.as_bytes());
        h.update(
            serde_json::to_vec(&desc.identity)
                .context("hash secret identity")?
                .as_slice(),
        );
        for a in &desc.acquire {
            h.update(
                serde_json::to_vec(a)
                    .context("hash secret acquisition")?
                    .as_slice(),
            );
        }
        let hash = format!("{:016x}", h.digest()).into_bytes();

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(SecretDef {
                    addr: desc.addr.clone(),
                    identity: desc.identity,
                    out: out.clone(),
                }),
                inputs: vec![],
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![Path {
                        content: Content::FilePath(out),
                        codegen_tree: CodegenMode::None,
                        collect: false,
                    }],
                }],
                support_files: vec![],
                // Local only. A descriptor holds no credential, but it does hold
                // role ARNs, account and project numbers, App ids and internal
                // endpoints — and it is trivially cheap to rebuild, so sharing
                // it org-wide buys nothing worth the disclosure question.
                cache: CacheConfig {
                    enabled: true,
                    remote_enabled: false,
                    history: 1,
                },
                pty: false,
                hash,
                transparent: false,
            },
        })
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        Ok(ApplyTransitiveResponse {
            target_def: req.target_def,
        })
    }

    async fn run<'a, 'io>(
        &self,
        req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        let def = req.target.def::<SecretDef>();
        let data = SecretJson::new(def.addr.clone(), def.identity.clone())
            .to_bytes()
            .context("render secret.json")?;

        let mut h = Xxh3::new();
        h.update(&data);
        h.update(def.out.as_bytes());
        let hashout = format!("{:x}", h.digest());

        Ok(RunResponse {
            artifacts: vec![outputartifact::OutputArtifact {
                group: String::new(),
                name: SECRET_JSON.to_string(),
                r#type: outputartifact::Type::Output,
                content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                    data,
                    path: def.out.clone(),
                    x: false,
                }),
                hashout,
            }],
            ..Default::default()
        })
    }

    async fn run_shell<'a, 'io>(
        &self,
        _req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        anyhow::bail!(
            "`heph shell` is not available on a secret target: it declares a credential recipe \
             and runs no command. Use `heph auth show` to see what a consuming target would get."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hmodel::htaddr::parse_addr;
    use hplugin::driver::Driver as EDriver;
    use hplugin::provider::TargetSpec;
    use std::collections::HashMap;

    fn spec_of(pairs: &[(&str, Value)]) -> TargetSpec {
        TargetSpec {
            addr: parse_addr("//infra/creds:ecr").expect("addr"),
            driver: DRIVER_NAME.to_string(),
            config: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<HashMap<_, _>>(),
            ..Default::default()
        }
    }

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn list(items: &[&str]) -> Value {
        Value::List(items.iter().map(|i| s(i)).collect())
    }

    fn map(pairs: &[(&str, Value)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    async fn parse_def(spec: TargetSpec) -> anyhow::Result<ParseResponse> {
        Driver
            .parse(
                ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: Arc::new(spec),
                },
                &StdCancellationToken::new(),
            )
            .await
    }

    /// Run the driver and return `(def hash, hashout, artifact)`.
    async fn parse_and_run(
        cfg: &[(&str, Value)],
    ) -> (Vec<u8>, String, outputartifact::OutputArtifact) {
        let parsed = parse_def(spec_of(cfg)).await.expect("parse");
        let dir = tempfile::tempdir().expect("tempdir");
        let request_id = "test".to_string();
        let hashin = "hashin".to_string();
        let resp = Driver
            .run(
                RunRequest {
                    request_id: &request_id,
                    target: &parsed.target_def,
                    tree_root_path: dir.path().to_path_buf(),
                    inputs: vec![],
                    hashin: &hashin,
                    stdin: None,
                    stdout: None,
                    stderr: None,
                    sandbox_dir: dir.path().to_path_buf(),
                    scratch: vec![],
                },
                &StdCancellationToken::new(),
            )
            .await
            .expect("run");
        let a = resp.artifacts.first().cloned().expect("one artifact");
        (parsed.target_def.hash.clone(), a.hashout.clone(), a)
    }

    #[test]
    fn the_flat_form_is_sugar_for_a_single_entry_acquire_list() {
        let flat = parse_declaration(&spec_of(&[
            ("role", s("arn:aws:iam::4711:role/heph-read")),
            ("provider", s("exec")),
            ("protocol", s("credential_process")),
            ("helper", list(&["aws", "configure", "export-credentials"])),
        ]))
        .expect("flat");

        let listed = parse_declaration(&spec_of(&[
            ("role", s("arn:aws:iam::4711:role/heph-read")),
            (
                "acquire",
                Value::List(vec![map(&[
                    ("provider", s("exec")),
                    ("protocol", s("credential_process")),
                    ("helper", list(&["aws", "configure", "export-credentials"])),
                ])]),
            ),
        ]))
        .expect("listed");

        assert_eq!(flat, listed);
    }

    /// Mixing the two reads as "these compose", and they do not.
    #[test]
    fn mixing_the_flat_form_with_acquire_is_refused() {
        let err = parse_declaration(&spec_of(&[
            ("provider", s("exec")),
            (
                "acquire",
                Value::List(vec![map(&[("provider", s("static_env")), ("var", s("X"))])]),
            ),
        ]))
        .expect_err("mixed");
        assert!(err.to_string().contains("two spellings"), "{err}");
    }

    /// A typo'd guard key silently dropped would turn a guarded entry into the
    /// catch-all, selecting the wrong identity without saying anything.
    #[test]
    fn an_unknown_acquire_key_is_rejected_rather_than_ignored() {
        let err = parse_declaration(&spec_of(&[(
            "acquire",
            Value::List(vec![map(&[
                ("when-env", s("GITHUB_ACTIONS")),
                ("provider", s("static_env")),
                ("var", s("X")),
            ])]),
        )]))
        .expect_err("typo");
        let msg = format!("{err:#}");
        assert!(msg.contains("when-env"), "{msg}");
        assert!(msg.contains("when_env"), "{msg}");
    }

    #[test]
    fn both_guard_forms_parse() {
        let d = parse_declaration(&spec_of(&[(
            "acquire",
            Value::List(vec![
                map(&[
                    ("when_env", s("GITHUB_ACTIONS")),
                    ("provider", s("oidc")),
                    ("exchange", s("aws")),
                ]),
                map(&[
                    ("when_env", map(&[("CI", s("true"))])),
                    ("provider", s("static_env")),
                    ("var", s("TOK")),
                ]),
                map(&[("provider", s("static_env")), ("var", s("TOK"))]),
            ]),
        )]))
        .expect("guards");
        assert_eq!(d.acquire.len(), 3);
        assert!(matches!(
            d.acquire.first().and_then(|a| a.when_env.as_ref()),
            Some(WhenEnv::Set(_))
        ));
        assert!(matches!(
            d.acquire.get(1).and_then(|a| a.when_env.as_ref()),
            Some(WhenEnv::Equals(_))
        ));
        assert!(d.acquire.get(2).is_some_and(|a| a.when_env.is_none()));
    }

    #[test]
    fn a_runner_on_an_acquire_entry_parses_and_defaults_to_none() {
        let d = parse_declaration(&spec_of(&[(
            "acquire",
            Value::List(vec![
                map(&[
                    ("when_env", s("GITHUB_ACTIONS")),
                    ("provider", s("exec")),
                    ("protocol", s("raw")),
                    ("helper", list(&["aws-cli"])),
                    ("runner", s("//tools/awscli:oci")),
                ]),
                map(&[
                    ("provider", s("exec")),
                    ("protocol", s("raw")),
                    ("helper", list(&["gh", "auth", "token"])),
                ]),
            ]),
        )]))
        .expect("runner");
        assert_eq!(
            d.acquire.first().and_then(|a| a.runner.as_deref()),
            Some("//tools/awscli:oci")
        );
        assert!(
            d.acquire.get(1).is_some_and(|a| a.runner.is_none()),
            "a helper must inherit no workspace runner default"
        );
    }

    /// The load-bearing property: swapping the acquisition half leaves the
    /// emitted artifact byte-identical, so no consumer's `hashin` moves.
    #[tokio::test]
    async fn swapping_acquisition_leaves_the_artifact_byte_identical() {
        let identity: &[(&str, Value)] = &[
            ("role", s("arn:aws:iam::4711:role/heph-read")),
            ("region", s("eu-west-1")),
            ("shape", list(&["aws_profile"])),
            ("profile", s("artifacts")),
        ];

        let mut ci = identity.to_vec();
        ci.extend_from_slice(&[("provider", s("oidc")), ("exchange", s("aws"))]);

        let mut laptop = identity.to_vec();
        laptop.extend_from_slice(&[
            ("provider", s("exec")),
            ("protocol", s("credential_process")),
            ("helper", list(&["aws", "configure", "export-credentials"])),
            ("runner", s("//tools/devenv:runner")),
            ("ttl", s("30m")),
        ]);

        let (ci_defhash, ci_hashout, ci_artifact) = parse_and_run(&ci).await;
        let (laptop_defhash, laptop_hashout, _) = parse_and_run(&laptop).await;

        // The def hash moves (the declaration really did change) …
        assert_ne!(ci_defhash, laptop_defhash);
        // … and the hashout, which is the only thing a consumer keys on, does not.
        assert_eq!(
            ci_hashout, laptop_hashout,
            "swapping the acquisition half moved a consumer's cache key"
        );

        let outputartifact::Content::Raw(raw) = &ci_artifact.content else {
            panic!("expected raw content");
        };
        let text = String::from_utf8(raw.data.clone()).expect("utf8");
        for leaked in [
            "provider", "helper", "oidc", "exec", "ttl", "runner", "aws\":",
        ] {
            assert!(
                !text.contains(leaked),
                "acquisition field {leaked:?} reached the artifact:\n{text}"
            );
        }
        assert!(text.contains("heph-read"), "{text}");
        assert!(text.contains("aws_profile"), "{text}");
    }

    /// Changing the identity half *must* move the hashout — the other half of
    /// the same contract.
    #[tokio::test]
    async fn changing_the_identity_moves_the_hashout() {
        let base: Vec<(&str, Value)> = vec![
            ("role", s("arn:aws:iam::4711:role/a")),
            ("provider", s("static_env")),
            ("var", s("TOK")),
        ];
        let mut other = base.clone();
        other[0] = ("role", s("arn:aws:iam::4711:role/b"));

        let (_, base_hashout, _) = parse_and_run(&base).await;
        let (_, other_hashout, _) = parse_and_run(&other).await;
        assert_ne!(base_hashout, other_hashout);
    }

    #[test]
    fn shapes_are_validated_at_declaration_time() {
        let err = parse_declaration(&spec_of(&[
            ("provider", s("static_env")),
            ("var", s("X")),
            ("shape", list(&["kubeconfig"])),
        ]))
        .expect_err("unknown shape");
        assert!(format!("{err:#}").contains("kubeconfig"), "{err:#}");
    }

    #[test]
    fn a_descriptor_with_no_acquisition_at_all_is_refused() {
        let err = parse_declaration(&spec_of(&[("role", s("arn:x"))])).expect_err("no provider");
        assert!(format!("{err:#}").contains("no way to acquire"), "{err:#}");
    }

    #[test]
    fn exec_without_a_protocol_is_refused_at_declaration_time() {
        let err = parse_declaration(&spec_of(&[
            ("provider", s("exec")),
            ("helper", list(&["gh", "auth", "token"])),
        ]))
        .expect_err("no protocol");
        assert!(
            format!("{err:#}").contains("explicit `protocol`"),
            "{err:#}"
        );
    }

    /// Declaration order must not be a cache-key component.
    #[test]
    fn scopes_and_shapes_are_sorted_before_they_reach_the_artifact() {
        let a = parse_declaration(&spec_of(&[
            ("provider", s("static_env")),
            ("var", s("X")),
            ("scope", list(&["b", "a"])),
            ("shape", list(&["netrc", "env"])),
            ("machine", s("github.com")),
            ("env", map(&[("GH_TOKEN", s("$."))])),
        ]))
        .expect("a");
        let b = parse_declaration(&spec_of(&[
            ("provider", s("static_env")),
            ("var", s("X")),
            ("scope", list(&["a", "b"])),
            ("shape", list(&["env", "netrc"])),
            ("machine", s("github.com")),
            ("env", map(&[("GH_TOKEN", s("$."))])),
        ]))
        .expect("b");
        assert_eq!(a.identity, b.identity);
        assert_eq!(a.identity.scope, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn shape_defaults_to_file() {
        let d = parse_declaration(&spec_of(&[("provider", s("static_env")), ("var", s("X"))]))
            .expect("defaults");
        assert_eq!(d.identity.shape, vec!["file".to_string()]);
    }

    #[test]
    fn unknown_provider_protocol_and_exchange_names_list_the_legal_ones() {
        for (field, value, expect) in [
            ("provider", "vault", "static_env"),
            ("protocol", "netrc", "engflow"),
            ("exchange", "azure", "github_app"),
        ] {
            let mut cfg = vec![("provider", s("exec")), ("helper", list(&["x"]))];
            if field == "provider" {
                cfg = vec![("provider", s(value))];
            } else {
                cfg.push((field, s(value)));
                if field != "protocol" {
                    cfg.push(("protocol", s("raw")));
                }
            }
            let err = parse_declaration(&spec_of(&cfg)).expect_err(field);
            let msg = format!("{err:#}");
            assert!(msg.contains(value), "{field}: {msg}");
            assert!(msg.contains(expect), "{field}: {msg}");
        }
    }

    #[tokio::test]
    async fn a_descriptor_is_not_pushed_to_the_shared_remote_cache() {
        let parsed = parse_def(spec_of(&[("provider", s("static_env")), ("var", s("X"))]))
            .await
            .expect("parse");
        assert!(parsed.target_def.cache.enabled, "local caching is fine");
        assert!(
            !parsed.target_def.cache.remote_enabled,
            "a descriptor carries role ARNs and internal endpoints; it is cheap to rebuild and \
             must not be shared org-wide by default"
        );
    }
}
