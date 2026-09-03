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
    Acquire, Descriptor, Exchange, Identity, SECRET_JSON, SecretJson, Source,
};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

pub const DRIVER_NAME: &str = "secret";

/// Config for a `secret` target.
///
/// The field list is split in two, and the split is the whole design:
/// everything before `acquire` reaches every consumer's cache key, and nothing
/// after it does.
///
/// The acquisition half is a **tagged union** ([`Source`]) rather than a bag of
/// options. That is what makes the combinations illegal-by-construction:
/// `helper` on a `static_env` is an unknown key, an `exec` without a `protocol`
/// fails at parse, and a `timeout` on an `oidc` cannot be written. The five
/// cross-field rules that used to enforce those by hand are gone, not moved.
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
    /// Identity parameters only one provider or exchange understands — a
    /// GitHub App id and installation, a service account to impersonate, an
    /// Azure tenant.
    ///
    /// The open half of the identity. Naming those as first-class fields put
    /// vendor names into a format frozen into every consumer's cache key, and
    /// made the next vendor a schema change; a map re-keys nothing and needs no
    /// release.
    params: std::collections::HashMap<String, String>,
    /// Which shapes this credential renders: `file` (the default), `env`,
    /// `netrc`, `docker_config`, `git_credential`, `aws_profile`, `gcloud_adc`.
    ///
    /// Identity, deliberately: a shape decides which files and variables exist
    /// in the sandbox, and that is part of what the target reads.
    shape: Vec<String>,
    /// For the `env` shape: variable name → pointer into the acquired value.
    /// `"$."` is the whole value, `"$.<field>"` one field.
    env: std::collections::HashMap<String, String>,

    // ---- acquisition: NOT hashed, never written to secret.json ----
    /// Ordered acquisition candidates. Each is a dict; the first whose
    /// `when_env` guard matches is used, and an entry with no guard is the
    /// catch-all and must come last.
    ///
    /// The inline form below is the same shape spelled without the list. Reach
    /// for this only when one identity genuinely has two routes — ambient in
    /// CI, a stored session on a laptop.
    #[spec(ty = ParamType::list(acquire_param_type()), parse = parse_acquire)]
    acquire: Vec<Acquire>,

    /// The single-route form, written inline on the target.
    ///
    /// Flattened rather than restated: the same [`Source`] parser serves both
    /// spellings, so there is no second key list to fall out of step with the
    /// first. Absent exactly when no `provider` is written at top level.
    #[spec(flatten)]
    source: Option<Source>,
    /// Inline form: what to trade the source's value for. One step or a list.
    #[spec(ty = exchange_param_type(), parse = parse_exchanges_spec)]
    exchange: Vec<Exchange>,
    /// Inline form: declared lifetime (`"1h"`), used only when nothing better
    /// is known. A `ttl` *longer* than the truth is the dangerous direction.
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
fn acquire_param_type() -> ParamType {
    <Acquire as hplugin::htspec::FromSpecValue>::spec_param_type()
}

fn exchange_param_type() -> ParamType {
    let one = <Exchange as hplugin::htspec::FromSpecValue>::spec_param_type();
    ParamType::union(vec![one.clone(), ParamType::list(one)])
}

fn parse_exchanges_spec(v: &Value) -> anyhow::Result<Vec<Exchange>> {
    hsecrets::descriptor::parse_exchange_value(v)
}

/// Parse the `acquire` list: a list of routes, each a tagged map.
fn parse_acquire(v: &Value) -> anyhow::Result<Vec<Acquire>> {
    use hplugin::htspec::FromSpecValue as _;
    let items = match v {
        Value::List(items) => items,
        Value::Null() => return Ok(Vec::new()),
        other => anyhow::bail!("`acquire` must be a list of dicts, got {other:?}"),
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            Acquire::from_spec_value(item).with_context(|| format!("`acquire` entry {i}"))
        })
        .collect()
}

/// Normalize an acquire entry's `runner`.
///
/// `"local"` — the documented explicit opt-out — becomes `None`, and everything
/// else is resolved to a canonical absolute address against the descriptor's own
/// package. Resolving here rather than in the domain crate is deliberate: this
/// is the only layer that knows the declaring package, so `runner = ":devenv"`
/// can mean what it says.
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

/// Resolve every runner address in a route, in place.
fn resolve_runners(
    mut a: Acquire,
    pkg: &hmodel::htpkg::PkgBuf,
    addr: &str,
    index: usize,
) -> anyhow::Result<Acquire> {
    if let Source::Exec { runner, .. } = &mut a.source {
        *runner = normalize_runner(runner.take(), pkg, addr, index)?;
    }
    Ok(a)
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
        account: spec.account,
        region: spec.region,
        bucket: spec.bucket,
        endpoint: spec.endpoint,
        registry: spec.registry,
        machine: spec.machine,
        profile: spec.profile,
        params: spec.params.into_iter().collect(),
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

    // The inline form is the same route written without the list. Mixing the
    // two is a mistake worth naming: it reads as if they compose.
    let inline_used = spec.source.is_some() || !spec.exchange.is_empty() || spec.ttl.is_some();

    let acquire = match (spec.acquire.is_empty(), inline_used) {
        (false, true) => anyhow::bail!(
            "secret {addr}: the inline form (`provider`, `helper`, …) and `acquire` are two \
             spellings of the same thing, not two things that compose. Put every route in \
             `acquire`, or use the inline form for the single-route case."
        ),
        (false, false) => spec
            .acquire
            .into_iter()
            .enumerate()
            .map(|(i, a)| resolve_runners(a, pkg, addr, i))
            .collect::<anyhow::Result<Vec<_>>>()?,
        (true, _) => {
            let source = spec.source.ok_or_else(|| {
                anyhow::anyhow!(
                    "secret {addr}: no way to acquire a value. Give it a `provider` \
                     (static_env, exec or oidc), or an `acquire` list."
                )
            })?;
            vec![resolve_runners(
                Acquire {
                    when_env: None,
                    source,
                    exchange: spec.exchange,
                    ttl: spec.ttl,
                },
                pkg,
                addr,
                0,
            )?]
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
    use hsecrets::descriptor::WhenEnv;
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

    /// The resolved `runner` of the nth acquire entry, if it has one.
    fn runner_of(d: &Descriptor, index: usize) -> Option<String> {
        match d.acquire.get(index).map(|a| &a.source) {
            Some(Source::Exec { runner, .. }) => runner.clone(),
            _ => None,
        }
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
                    secret_env: Vec::new(),
                    secret_values: Vec::new(),
                },
                &StdCancellationToken::new(),
            )
            .await
            .expect("run");
        let a = resp.artifacts.first().cloned().expect("one artifact");
        (parsed.target_def.hash.clone(), a.hashout.clone(), a)
    }

    // ---- the tagged union ----

    #[test]
    fn the_inline_form_is_the_same_route_as_a_one_entry_acquire_list() {
        let inline = parse_declaration(&spec_of(&[
            ("role", s("arn:aws:iam::4711:role/heph-read")),
            ("provider", s("exec")),
            ("protocol", s("credential_process")),
            ("helper", list(&["aws", "configure", "export-credentials"])),
        ]))
        .expect("inline");

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

        assert_eq!(inline, listed);
    }

    /// The point of the tag: a field belonging to another provider is an
    /// unknown key on the one the author named, rejected at parse rather than
    /// by a hand-written rule further down — or, worse, ignored.
    #[test]
    fn a_field_from_another_provider_is_rejected() {
        for (provider, stray, value) in [
            ("static_env", "helper", list(&["gh", "auth", "token"])),
            ("static_env", "protocol", s("raw")),
            ("oidc", "helper", list(&["gh"])),
            ("oidc", "timeout", s("30s")),
        ] {
            let err = parse_declaration(&spec_of(&[(
                "acquire",
                Value::List(vec![map(&[
                    ("provider", s(provider)),
                    ("var", s("TOK")),
                    (stray, value),
                ])]),
            )]))
            .expect_err("{provider} has no {stray}");
            let msg = format!("{err:#}");
            assert!(msg.contains(stray), "{provider}/{stray}: {msg}");
        }
    }

    /// `protocol` on an `exec` is required by construction — the failure is a
    /// missing required field at parse time, not a cross-field check.
    #[test]
    fn exec_without_a_protocol_fails_at_parse() {
        let err = parse_declaration(&spec_of(&[
            ("provider", s("exec")),
            ("helper", list(&["gh", "auth", "token"])),
        ]))
        .expect_err("no protocol");
        assert!(
            format!("{err:#}").contains("missing required `protocol`"),
            "{err:#}"
        );
    }

    #[test]
    fn an_unknown_provider_lists_the_legal_ones() {
        let err = parse_declaration(&spec_of(&[("provider", s("vault"))])).expect_err("unknown");
        let msg = format!("{err:#}");
        assert!(msg.contains("vault"), "{msg}");
        assert!(msg.contains("static_env"), "{msg}");
        assert!(msg.contains("exec"), "{msg}");
        assert!(msg.contains("oidc"), "{msg}");
    }

    #[test]
    fn an_unknown_protocol_lists_the_legal_ones() {
        let err = parse_declaration(&spec_of(&[
            ("provider", s("exec")),
            ("helper", list(&["x"])),
            ("protocol", s("netrc")),
        ]))
        .expect_err("unknown protocol");
        let msg = format!("{err:#}");
        assert!(msg.contains("netrc"), "{msg}");
        assert!(msg.contains("credential_helper"), "{msg}");
    }

    /// `var` and `vars` are one field with two spellings, so there is no
    /// mutual-exclusion rule left — and writing both is caught by the derive.
    #[test]
    fn var_is_sugar_for_a_single_entry_vars_map() {
        let one = parse_declaration(&spec_of(&[
            ("provider", s("static_env")),
            ("var", s("TOK")),
        ]))
        .expect("var");
        let many = parse_declaration(&spec_of(&[
            ("provider", s("static_env")),
            ("vars", map(&[("token", s("TOK"))])),
        ]))
        .expect("vars");
        assert_eq!(one, many);

        let err = parse_declaration(&spec_of(&[
            ("provider", s("static_env")),
            ("var", s("A")),
            ("vars", map(&[("token", s("B"))])),
        ]))
        .expect_err("both spellings");
        assert!(format!("{err:#}").contains("two spellings"), "{err:#}");
    }

    #[test]
    fn mixing_the_inline_form_with_acquire_is_refused() {
        let err = parse_declaration(&spec_of(&[
            ("provider", s("static_env")),
            ("var", s("A")),
            (
                "acquire",
                Value::List(vec![map(&[("provider", s("static_env")), ("var", s("B"))])]),
            ),
        ]))
        .expect_err("mixed");
        assert!(format!("{err:#}").contains("two spellings"), "{err:#}");
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
        assert!(format!("{err:#}").contains("when-env"), "{err:#}");
    }

    #[test]
    fn both_guard_forms_parse() {
        let d = parse_declaration(&spec_of(&[(
            "acquire",
            Value::List(vec![
                map(&[
                    ("when_env", s("GITHUB_ACTIONS")),
                    ("provider", s("oidc")),
                    ("exchange", map(&[("kind", s("aws_sts"))])),
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

    // ---- standards-first exchanges ----

    /// An exchange is a *grant*, not a vendor. What used to be `exchange =
    /// "github_app"` is an ordinary HTTP call described in the BUILD file, and
    /// heph needs to know nothing about GitHub to run it.
    #[test]
    fn a_vendor_rest_exchange_needs_no_vendor_support() {
        let d = parse_declaration(&spec_of(&[
            ("machine", s("github.com")),
            (
                "params",
                map(&[("app_id", s("1180022")), ("install", s("org/heph"))]),
            ),
            ("provider", s("oidc")),
            (
                "exchange",
                map(&[
                    ("kind", s("http")),
                    (
                        "url",
                        s("https://api.github.com/app/installations/42/access_tokens"),
                    ),
                    ("fields", map(&[("token", s("/token"))])),
                ]),
            ),
        ]))
        .expect("http exchange");
        assert!(matches!(
            d.acquire.first().and_then(|a| a.exchange.first()),
            Some(Exchange::Http { .. })
        ));
        // The GitHub-shaped identity lives in the open map, not in named fields.
        assert_eq!(
            d.identity.params.get("app_id").map(String::as_str),
            Some("1180022")
        );
    }

    /// GCP federation is RFC 8693 followed by an impersonation call: two hops,
    /// which is why an exchange is a pipeline rather than a single step.
    #[test]
    fn an_exchange_pipeline_parses_in_order() {
        let d = parse_declaration(&spec_of(&[
            ("audience", s("//iam.googleapis.com/projects/8801/…")),
            ("provider", s("oidc")),
            (
                "exchange",
                Value::List(vec![
                    map(&[
                        ("kind", s("token_exchange")),
                        ("endpoint", s("https://sts.googleapis.com/v1/token")),
                    ]),
                    map(&[
                        ("kind", s("http")),
                        ("url", s("https://iamcredentials.googleapis.com/…")),
                    ]),
                ]),
            ),
        ]))
        .expect("pipeline");
        let steps = d
            .acquire
            .first()
            .map(|a| a.exchange.as_slice())
            .unwrap_or(&[]);
        assert_eq!(steps.len(), 2);
        assert!(matches!(
            steps.first(),
            Some(Exchange::TokenExchange { .. })
        ));
        assert!(matches!(steps.get(1), Some(Exchange::Http { .. })));
    }

    /// An OAuth grant names an `issuer` and discovers the rest; a literal
    /// `endpoint` is the escape hatch. Naming neither is a declaration-time
    /// failure that says which to reach for.
    #[test]
    fn a_grant_naming_neither_issuer_nor_endpoint_fails_at_the_declaration() {
        let err = parse_declaration(&spec_of(&[
            ("provider", s("oidc")),
            ("exchange", map(&[("kind", s("token_exchange"))])),
        ]))
        .expect_err("no destination");
        let msg = format!("{err:#}");
        assert!(msg.contains("`issuer`"), "{msg}");
        assert!(msg.contains(".well-known/openid-configuration"), "{msg}");
    }

    /// Both is refused too: silently preferring one would make which server was
    /// contacted depend on a precedence nobody can see at the call site.
    #[test]
    fn a_grant_naming_both_issuer_and_endpoint_is_refused() {
        let err = parse_declaration(&spec_of(&[
            ("provider", s("oidc")),
            (
                "exchange",
                map(&[
                    ("kind", s("token_exchange")),
                    ("issuer", s("https://org.okta.com/oauth2/default")),
                    ("endpoint", s("https://internal.example/mint")),
                ]),
            ),
        ]))
        .expect_err("both");
        assert!(format!("{err:#}").contains("set one"), "{err:#}");
    }

    #[test]
    fn an_issuer_resolves_to_its_discovery_document() {
        let d = parse_declaration(&spec_of(&[
            ("provider", s("oidc")),
            (
                "exchange",
                map(&[
                    ("kind", s("token_exchange")),
                    ("issuer", s("https://org.okta.com/oauth2/default")),
                ]),
            ),
        ]))
        .expect("issuer");
        let step = d
            .acquire
            .first()
            .and_then(|a| a.exchange.first())
            .expect("one step");
        assert_eq!(
            step.endpoint()
                .expect("resolves")
                .and_then(|e| e.discovery_url())
                .as_deref(),
            Some("https://org.okta.com/oauth2/default/.well-known/openid-configuration")
        );
    }

    #[test]
    fn an_unknown_exchange_kind_lists_the_legal_ones() {
        let err = parse_declaration(&spec_of(&[
            ("provider", s("oidc")),
            ("exchange", map(&[("kind", s("github_app"))])),
        ]))
        .expect_err("vendor name");
        let msg = format!("{err:#}");
        assert!(msg.contains("github_app"), "{msg}");
        assert!(msg.contains("token_exchange"), "{msg}");
        assert!(msg.contains("http"), "{msg}");
    }

    /// An assertion is not a credential.
    #[test]
    fn oidc_without_an_exchange_is_refused() {
        let err = parse_declaration(&spec_of(&[("provider", s("oidc"))])).expect_err("no exchange");
        assert!(
            format!("{err:#}").contains("needs an `exchange`"),
            "{err:#}"
        );
    }

    // ---- runner ----

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
        assert_eq!(runner_of(&d, 0).as_deref(), Some("//tools/awscli:oci"));
        assert!(
            runner_of(&d, 1).is_none(),
            "a helper must inherit no workspace runner default"
        );
    }

    // ---- the cache-key contract ----

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
        ci.extend_from_slice(&[
            ("provider", s("oidc")),
            ("exchange", map(&[("kind", s("aws_sts"))])),
        ]);

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
            "provider", "helper", "oidc", "exec", "ttl", "runner", "kind",
        ] {
            assert!(
                !text.contains(leaked),
                "acquisition field {leaked:?} reached the artifact:\n{text}"
            );
        }
        assert!(text.contains("heph-read"), "{text}");
        assert!(text.contains("aws_profile"), "{text}");
    }

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

    /// `params` is hashed like the rest of the identity: it is where the
    /// vendor-shaped half of an identity lives, and it still decides what a
    /// consumer keys on.
    #[tokio::test]
    async fn params_are_part_of_the_identity() {
        let base: Vec<(&str, Value)> = vec![
            ("provider", s("static_env")),
            ("var", s("TOK")),
            ("params", map(&[("app_id", s("1"))])),
        ];
        let mut other = base.clone();
        other[2] = ("params", map(&[("app_id", s("2"))]));

        let (_, a, _) = parse_and_run(&base).await;
        let (_, b, _) = parse_and_run(&other).await;
        assert_ne!(a, b, "params must reach the cache key");
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
