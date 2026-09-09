//! `heph.auth.*` — the ergonomic layer over the credential vocabulary.
//!
//! Every function here is a plain function returning a plain dict a BUILD file
//! could have written by hand. None of them is privileged, and that is the
//! property that keeps this from becoming a place behaviour hides: there is
//! nothing a preset can express that the primitives cannot.
//!
//! What they buy is real all the same. A wrong argument fails at BUILD
//! evaluation instead of at run time, which a magic string like
//! `present = "aws"` can never do; the LSP completes and documents them; and
//! `heph inspect functions` lists them. Above all, the *presentation* presets
//! encode third-party wire formats with exactly one correct spelling — the AWS
//! web-identity variables, Google's `external_account` document, the Docker
//! credHelpers map — and hand-rolling those is how you get a subtly wrong one.
//!
//! # Why there are no source presets
//!
//! There is no `heph.auth.vault()`, no `heph.auth.op()`, no `heph.auth.aws_sso()`,
//! and this is a rule rather than a gap. That list has no end — every secret
//! manager, every cloud CLI, every internal tool at every company — and each entry
//! would be a workflow frozen into a plugin, ageing badly, for no gain over four
//! lines of [`exec`](super::source::SourceKind::Exec) with a field map. A source is
//! `exec` with a field map, or it is a target.
//!
//! A *presentation* preset is the opposite case: the format is fixed by a vendor,
//! the set is small and known, and a hand-typed one fails silently inside somebody
//! else's SDK. That asymmetry is the whole rule.

use async_trait::async_trait;
use hcore::htvalue::Value;
use hcore::htvalue::signature::{FnSignature, Param, ParamType};
use hplugin::provider::{
    ConfigRequest, ConfigResponse, FnArgs, FnCallContext, GetError, GetRequest, GetResponse,
    ListPackageResponse, ListPackagesRequest, ListRequest, ListResponse, ProbeRequest,
    ProbeResponse, Provider as EProvider, ProviderFn, ProviderFunctionDef,
};
use std::collections::HashMap;
use std::sync::Arc;

/// The provider namespace these functions live under: `heph.auth.<fn>`.
pub const PROVIDER_NAME: &str = "auth";

/// A provider that serves no targets and exists only to carry the `heph.auth.*`
/// functions into BUILD files.
///
/// The same shape `query` already uses. Functions reach BUILD files through
/// `Provider::functions()`, and a provider is the only thing that has one — so a
/// namespace with no targets behind it is an inert provider, not a new concept.
pub struct Provider;

impl EProvider for Provider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: PROVIDER_NAME.to_string(),
        })
    }

    fn list<'a>(
        &'a self,
        _req: ListRequest,
        _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> futures::future::BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
    > {
        Box::pin(async {
            Ok(Box::new(std::iter::empty())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListResponse>> + Send,
                >)
        })
    }

    fn list_packages<'a>(
        &'a self,
        _req: ListPackagesRequest,
        _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> futures::future::BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    > {
        Box::pin(async {
            Ok(Box::new(std::iter::empty())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                >)
        })
    }

    fn get<'a>(
        &'a self,
        _req: GetRequest,
        _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> futures::future::BoxFuture<'a, Result<GetResponse, GetError>> {
        Box::pin(async { Err(GetError::NotFound) })
    }

    fn probe<'a>(
        &'a self,
        _req: ProbeRequest,
        _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> futures::future::BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async { Ok(ProbeResponse { states: vec![] }) })
    }

    fn functions(&self) -> Vec<ProviderFunctionDef> {
        definitions()
    }
}

// ---------------------------------------------------------------------------
// Signature helpers
// ---------------------------------------------------------------------------

pub(crate) fn strs() -> ParamType {
    ParamType::union(vec![ParamType::String, ParamType::list(ParamType::String)])
}

fn str_map() -> ParamType {
    ParamType::map(ParamType::String)
}

/// The shape a presentation preset returns.
///
/// Written out rather than left as "some map", because the return value **is**
/// checked: a preset returning something a `present =` cannot accept would
/// otherwise fail one layer down, in the credential driver's parser, with no
/// mention of the preset that produced it.
pub(crate) fn presentation_ty() -> ParamType {
    ParamType::strukt(vec![
        ("env", str_map()),
        ("files", str_map()),
        (
            "helper",
            ParamType::union(vec![
                ParamType::String,
                ParamType::strukt(vec![
                    ("dialect", ParamType::String),
                    ("registries", ParamType::list(ParamType::String)),
                    ("hosts", ParamType::list(ParamType::String)),
                    ("audience", ParamType::String),
                    ("impersonate", ParamType::String),
                ]),
            ]),
        ),
    ])
}

/// The shape a source constructor returns: one struct covering every inline
/// kind's fields, which is what the driver's own schema says too.
pub(crate) fn source_ty() -> ParamType {
    ParamType::strukt(vec![
        ("kind", ParamType::String),
        ("when", ParamType::String),
        ("credentials", strs()),
        ("present", presentation_ty()),
        ("hint", ParamType::String),
        ("names", ParamType::list(ParamType::String)),
        ("path", ParamType::String),
        ("run", ParamType::list(ParamType::String)),
        ("fields", str_map()),
        ("expires", ParamType::String),
        ("login", ParamType::list(ParamType::list(ParamType::String))),
        ("runner", ParamType::String),
        ("paths", str_map()),
        ("provider", ParamType::String),
        ("audience", ParamType::String),
    ])
}

/// The four fields every source constructor accepts, appended to its own named
/// params so they are not repeated per row.
fn common_source_params() -> Vec<Param> {
    vec![
        Param::optional("when", ParamType::String, Value::Null()),
        Param::optional("credentials", strs(), Value::Null()),
        Param::optional("present", presentation_ty(), Value::Null()),
        Param::optional("hint", ParamType::String, Value::Null()),
    ]
}

/// Apply the common source fields onto a constructed source dict.
fn apply_common(out: &mut HashMap<String, Value>, args: &FnArgs) {
    for key in ["when", "credentials", "present", "hint"] {
        if let Some(v) = args.named.get(key)
            && !matches!(v, Value::Null())
        {
            out.insert(key.to_string(), v.clone());
        }
    }
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn s(v: impl Into<String>) -> Value {
    Value::String(v.into())
}

/// A named argument that must be present and a string.
fn req_str(args: &FnArgs, name: &str, func: &str) -> anyhow::Result<String> {
    match args.named.get(name) {
        Some(Value::String(v)) if !v.is_empty() => Ok(v.clone()),
        Some(Value::String(_)) | None | Some(Value::Null()) => {
            anyhow::bail!("heph.auth.{func}: `{name}` is required and must be a non-empty string")
        }
        Some(other) => anyhow::bail!("heph.auth.{func}: `{name}` must be a string, got {other:?}"),
    }
}

fn opt_str(args: &FnArgs, name: &str) -> Option<String> {
    match args.named.get(name) {
        Some(Value::String(v)) if !v.is_empty() => Some(v.clone()),
        _ => None,
    }
}

/// The first positional, as a list of strings.
fn positional_strings(args: &FnArgs, func: &str, what: &str) -> anyhow::Result<Vec<String>> {
    let Some(v) = args.positional.first() else {
        anyhow::bail!("heph.auth.{func}: {what} is required")
    };
    let out = super::present::strings(v, what)?;
    if out.is_empty() {
        anyhow::bail!("heph.auth.{func}: {what} must not be empty");
    }
    Ok(out)
}

/// A `ProviderFn` implemented by a plain closure over `FnArgs`.
///
/// Every function here is pure — it reads no filesystem, resolves no target, and
/// never touches the network. That is deliberate and load-bearing: a preset that
/// could *look something up* would be a place where a credential's behaviour
/// depends on where the BUILD file was evaluated, which is precisely what the
/// chain exists to make explicit.
struct PureFn(fn(&FnArgs) -> anyhow::Result<Value>);

#[async_trait]
impl ProviderFn for PureFn {
    async fn call(&self, _ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<Value> {
        (self.0)(&args)
    }
}

fn def(
    name: &str,
    positional: Vec<Param>,
    mut named: Vec<Param>,
    returns: ParamType,
    doc: &str,
    f: fn(&FnArgs) -> anyhow::Result<Value>,
) -> ProviderFunctionDef {
    named.sort_by_key(|p| p.name);
    ProviderFunctionDef {
        name: name.to_string(),
        signature: FnSignature {
            positional,
            named,
            variadic: None,
            returns,
        },
        doc: doc.to_string(),
        func: Arc::new(PureFn(f)),
    }
}

fn source_def(
    name: &str,
    positional: Vec<Param>,
    own: Vec<Param>,
    doc: &str,
    f: fn(&FnArgs) -> anyhow::Result<Value>,
) -> ProviderFunctionDef {
    let mut named = own;
    named.extend(common_source_params());
    def(name, positional, named, source_ty(), doc, f)
}

/// A presentation preset.
fn present_def(
    name: &str,
    positional: Vec<Param>,
    named: Vec<Param>,
    doc: &str,
    f: fn(&FnArgs) -> anyhow::Result<Value>,
) -> ProviderFunctionDef {
    def(name, positional, named, presentation_ty(), doc, f)
}

// ---------------------------------------------------------------------------
// The functions
// ---------------------------------------------------------------------------

pub fn definitions() -> Vec<ProviderFunctionDef> {
    vec![
        // ---- source constructors ----
        source_def(
            "env",
            vec![Param::required("names", strs())],
            vec![],
            "A source reading host environment variables. Probes that each is set. \
             Yields one material field per name, lowercased: `env([\"GITHUB_TOKEN\"])` \
             yields `${github_token}`.",
            |args| {
                let names = positional_strings(args, "env", "`names`")?;
                let mut out = HashMap::from([
                    ("kind".to_string(), s("env")),
                    (
                        "names".to_string(),
                        Value::List(names.into_iter().map(Value::String).collect()),
                    ),
                ]);
                apply_common(&mut out, args);
                Ok(Value::Map(out))
            },
        ),
        source_def(
            "file",
            vec![Param::required("path", ParamType::String)],
            vec![
                Param::optional("fields", str_map(), Value::Null()),
                Param::optional("expires", ParamType::String, Value::Null()),
            ],
            "A source reading a file on the host. Probes that the path exists. With \
             `fields` the file is parsed as JSON and each field is picked out by a \
             (possibly dotted) key; without it the whole file becomes `${value}`. \
             `expires` names the JSON key carrying the expiry — an absolute timestamp \
             or a duration in seconds, told apart by type, since vendors do both.",
            |args| {
                let path = match args.positional.first() {
                    Some(Value::String(p)) if !p.is_empty() => p.clone(),
                    _ => anyhow::bail!("heph.auth.file: `path` is required"),
                };
                let mut out = HashMap::from([
                    ("kind".to_string(), s("file")),
                    ("path".to_string(), s(path)),
                ]);
                for k in ["fields", "expires"] {
                    if let Some(v) = args.named.get(k)
                        && !matches!(v, Value::Null())
                    {
                        out.insert(k.to_string(), v.clone());
                    }
                }
                apply_common(&mut out, args);
                Ok(Value::Map(out))
            },
        ),
        source_def(
            "exec",
            vec![Param::required("run", strs())],
            vec![
                Param::optional("fields", str_map(), Value::Null()),
                Param::optional("expires", ParamType::String, Value::Null()),
                Param::optional("login", ParamType::list(strs()), Value::Null()),
                Param::optional("runner", ParamType::String, Value::Null()),
            ],
            "A source running one command whose **stdout** is the credential. Probes \
             that `run[0]` resolves inside `runner`, then runs it there. Stdout is JSON \
             and `fields` maps material field → JSON key (keys may be dotted paths). \
             `login` is a *list* of argvs, because some tools keep more than one login \
             store; `heph auth login` runs whichever the probe found stale. A command \
             that writes files is a target, not a source — there is deliberately no \
             way to name output files here.",
            |args| {
                let run = positional_strings(args, "exec", "`run`")?;
                let mut out = HashMap::from([
                    ("kind".to_string(), s("exec")),
                    (
                        "run".to_string(),
                        Value::List(run.into_iter().map(Value::String).collect()),
                    ),
                ]);
                for k in ["fields", "expires", "login", "runner"] {
                    if let Some(v) = args.named.get(k)
                        && !matches!(v, Value::Null())
                    {
                        out.insert(k.to_string(), v.clone());
                    }
                }
                apply_common(&mut out, args);
                Ok(Value::Map(out))
            },
        ),
        source_def(
            "passthrough",
            vec![],
            vec![
                Param::optional("paths", str_map(), Value::Null()),
                Param::optional("env", str_map(), Value::Null()),
                Param::optional("login", ParamType::list(strs()), Value::Null()),
            ],
            "A source exposing host files or directories **in place** — nothing is \
             copied into the credential store. Probes that every path exists. Each is \
             reachable as `${file:<name>}`. `env` is a shorthand for the presentation, \
             which is what a passthrough almost always needs: \
             `passthrough(paths = {\"config\": \"~/.config/gcloud\"}, env = \
             {\"CLOUDSDK_CONFIG\": \"${file:config}\"})`. No product names: the author \
             says which path.",
            |args| {
                let Some(paths) = args
                    .named
                    .get("paths")
                    .filter(|v| !matches!(v, Value::Null()))
                else {
                    anyhow::bail!(
                        "heph.auth.passthrough: `paths` is required — heph exposes the host files \
                         you name and takes no view about which product wrote them"
                    )
                };
                let mut out = HashMap::from([
                    ("kind".to_string(), s("passthrough")),
                    ("paths".to_string(), paths.clone()),
                ]);
                if let Some(v) = args.named.get("login")
                    && !matches!(v, Value::Null())
                {
                    out.insert("login".to_string(), v.clone());
                }
                apply_common(&mut out, args);
                // `env =` folds into the presentation, and must not silently
                // overwrite an explicit `present =`.
                if let Some(env) = args
                    .named
                    .get("env")
                    .filter(|v| !matches!(v, Value::Null()))
                {
                    if out.contains_key("present") {
                        anyhow::bail!(
                            "heph.auth.passthrough: pass either `env` (the shorthand) or \
                             `present` (the full presentation), not both"
                        );
                    }
                    out.insert("present".to_string(), map(vec![("env", env.clone())]));
                }
                Ok(Value::Map(out))
            },
        ),
        source_def(
            "oidc",
            vec![Param::optional(
                "provider",
                ParamType::String,
                s("github_actions"),
            )],
            vec![Param::optional(
                "audience",
                ParamType::String,
                Value::Null(),
            )],
            "A source minting a workload-identity token from the CI provider heph \
             detects. Probes that the provider is present — on a laptop that probe \
             fails cleanly, which is what lets this sit above the vendor-CLI source in \
             a chain that serves both. Yields `${id_token}`. `provider` is \
             `github_actions` or `generic`.",
            |args| {
                let provider = match args.positional.first() {
                    Some(Value::String(p)) if !p.is_empty() => p.clone(),
                    _ => "github_actions".to_string(),
                };
                let mut out = HashMap::from([
                    ("kind".to_string(), s("oidc")),
                    ("provider".to_string(), s(provider)),
                ]);
                if let Some(a) = opt_str(args, "audience") {
                    out.insert("audience".to_string(), s(a));
                }
                apply_common(&mut out, args);
                Ok(Value::Map(out))
            },
        ),
        // ---- presentation presets ----
        present_def(
            "aws_process",
            vec![],
            vec![],
            "Present the credential as an AWS **credential process**: heph writes a \
             config file naming `heph __auth-helper aws <addr>` as the profile's \
             `credential_process`, and points `AWS_CONFIG_FILE` at it. The only AWS \
             shape that survives expiry mid-run, because the SDK re-invokes the helper \
             whenever it needs a fresh credential.",
            |_args| Ok(map(vec![("helper", map(vec![("dialect", s("aws"))]))])),
        ),
        present_def(
            "aws_web_identity",
            vec![],
            vec![
                Param::required("role", ParamType::String),
                Param::optional("session_name", ParamType::String, s("heph")),
            ],
            "Present an OIDC token as an AWS web identity: a `0600` token file plus \
             `AWS_WEB_IDENTITY_TOKEN_FILE`, `AWS_ROLE_ARN` and \
             `AWS_ROLE_SESSION_NAME`. heph performs **no token exchange** — the AWS \
             SDK does it, and re-reads the token file on every refresh, so a long \
             build is fine as long as something keeps the file current. \
             Expands to exactly `{\"files\": {\"token\": \"${id_token}\"}, \"env\": \
             {...three variables...}}`.",
            |args| {
                let role = req_str(args, "role", "aws_web_identity")?;
                let session = opt_str(args, "session_name").unwrap_or_else(|| "heph".to_string());
                Ok(map(vec![
                    ("files", map(vec![("token", s("${id_token}"))])),
                    (
                        "env",
                        map(vec![
                            ("AWS_WEB_IDENTITY_TOKEN_FILE", s("${file:token}")),
                            ("AWS_ROLE_ARN", s(role)),
                            ("AWS_ROLE_SESSION_NAME", s(session)),
                        ]),
                    ),
                ]))
            },
        ),
        present_def(
            "gcp",
            vec![],
            vec![
                Param::required("audience", ParamType::String),
                Param::optional("impersonate", ParamType::String, Value::Null()),
            ],
            "Present an OIDC token as Google **workload identity federation**: a \
             `0600` token file plus the `external_account` document ADC consumers know \
             how to use, pointed at by both `GOOGLE_APPLICATION_CREDENTIALS` and \
             `CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE`. Setting the override as well as \
             the ADC variable is what collapses gcloud's two independent logins into \
             one credential. `audience` is the full pool-provider audience \
             (`//iam.googleapis.com/projects/…`); pass `impersonate` when the service \
             does not accept a federated identity directly — Cloud Run, kubectl from \
             inside GKE and HCP Terraform all require it.",
            |args| {
                let audience = req_str(args, "audience", "gcp")?;
                let impersonate = opt_str(args, "impersonate");
                Ok(map(vec![
                    (
                        "files",
                        map(vec![
                            ("token", s("${id_token}")),
                            (
                                "adc",
                                s(gcp_external_account(&audience, impersonate.as_deref())),
                            ),
                        ]),
                    ),
                    (
                        "env",
                        map(vec![
                            ("GOOGLE_APPLICATION_CREDENTIALS", s("${file:adc}")),
                            ("CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE", s("${file:adc}")),
                        ]),
                    ),
                ]))
            },
        ),
        present_def(
            "azure_workload",
            vec![],
            vec![
                Param::required("client_id", ParamType::String),
                Param::required("tenant_id", ParamType::String),
            ],
            "Present an OIDC token as an Azure **federated credential**: a `0600` \
             token file plus `AZURE_FEDERATED_TOKEN_FILE`, `AZURE_CLIENT_ID` and \
             `AZURE_TENANT_ID`. Azure has no generic token-process credential, so this \
             is the only file-shaped door — the SDKs re-read the file on a timer but \
             never re-mint, which is why a build outliving the token needs heph to \
             rotate it.",
            |args| {
                let client_id = req_str(args, "client_id", "azure_workload")?;
                let tenant_id = req_str(args, "tenant_id", "azure_workload")?;
                Ok(map(vec![
                    ("files", map(vec![("token", s("${id_token}"))])),
                    (
                        "env",
                        map(vec![
                            ("AZURE_FEDERATED_TOKEN_FILE", s("${file:token}")),
                            ("AZURE_CLIENT_ID", s(client_id)),
                            ("AZURE_TENANT_ID", s(tenant_id)),
                        ]),
                    ),
                ]))
            },
        ),
        present_def(
            "github",
            vec![],
            vec![Param::optional("hosts", strs(), s("github.com"))],
            "Present a GitHub token as both `GH_TOKEN` and a git credential helper, so \
             `gh`, `git` and `go mod download` over HTTPS all authenticate without any \
             further wiring. The git half is what gives private Go modules a credential \
             path at all.",
            |args| {
                let hosts = match args.named.get("hosts") {
                    Some(v) if !matches!(v, Value::Null()) => super::present::strings(v, "hosts")?,
                    _ => vec!["github.com".to_string()],
                };
                Ok(map(vec![
                    ("env", map(vec![("GH_TOKEN", s("${token}"))])),
                    (
                        "helper",
                        map(vec![
                            ("dialect", s("git")),
                            (
                                "hosts",
                                Value::List(hosts.into_iter().map(Value::String).collect()),
                            ),
                        ]),
                    ),
                ]))
            },
        ),
        present_def(
            "docker",
            vec![Param::required("registries", strs())],
            vec![],
            "Present the credential through the **Docker credential-helper** protocol: \
             heph writes a `DOCKER_CONFIG` directory whose `credHelpers` names each \
             registry, and prepends the directory holding a `docker-credential-heph` \
             shim to `PATH`. This is the only presentation that touches `PATH`, because \
             Docker resolves a helper by executable name.",
            |args| {
                let registries = positional_strings(args, "docker", "`registries`")?;
                Ok(map(vec![(
                    "helper",
                    map(vec![
                        ("dialect", s("docker")),
                        (
                            "registries",
                            Value::List(registries.into_iter().map(Value::String).collect()),
                        ),
                    ]),
                )]))
            },
        ),
        present_def(
            "git",
            vec![Param::required("hosts", strs())],
            vec![],
            "Present the credential through the **git credential-helper** protocol, \
             injected via `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_n` / \
             `GIT_CONFIG_VALUE_n`. No gitconfig is written and the developer's own is \
             never touched.",
            |args| {
                let hosts = positional_strings(args, "git", "`hosts`")?;
                Ok(map(vec![(
                    "helper",
                    map(vec![
                        ("dialect", s("git")),
                        (
                            "hosts",
                            Value::List(hosts.into_iter().map(Value::String).collect()),
                        ),
                    ]),
                )]))
            },
        ),
        present_def(
            "netrc",
            vec![Param::required("machines", strs())],
            vec![
                Param::optional("login", ParamType::String, s("${username}")),
                Param::optional("password", ParamType::String, s("${token}")),
            ],
            "Present the credential as a `0600` netrc file plus `NETRC` pointing at \
             it. The fallback of last resort: no callback and no expiry field, so it is \
             a snapshot. It is here because Go's module fetch over HTTPS honours it, \
             and private Go modules otherwise have no credential path at all.",
            |args| {
                let machines = positional_strings(args, "netrc", "`machines`")?;
                let login = opt_str(args, "login").unwrap_or_else(|| "${username}".to_string());
                let password = opt_str(args, "password").unwrap_or_else(|| "${token}".to_string());
                let body = machines
                    .iter()
                    .map(|m| format!("machine {m} login {login} password {password}\n"))
                    .collect::<String>();
                Ok(map(vec![
                    ("files", map(vec![("netrc", s(body))])),
                    ("env", map(vec![("NETRC", s("${file:netrc}"))])),
                ]))
            },
        ),
    ]
}

/// The `external_account` document `heph.auth.gcp` generates.
///
/// The file-sourced form, rather than the URL-sourced one Google's own Action
/// writes: heph already holds the token and does not need to hand a caller a live
/// minting endpoint plus its bearer token.
///
/// Written out as a function with a test rather than inline, because it is the one
/// preset expansion most likely to be wrong if hand-typed and the failure is a
/// message from inside Google's SDK.
fn gcp_external_account(audience: &str, impersonate: Option<&str>) -> String {
    let mut doc = serde_json::json!({
        "type": "external_account",
        "audience": audience,
        "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
        "token_url": "https://sts.googleapis.com/v1/token",
        "credential_source": { "file": "${file:token}" },
    });
    if let Some(sa) = impersonate
        && let Some(obj) = doc.as_object_mut()
    {
        obj.insert(
            "service_account_impersonation_url".to_string(),
            serde_json::Value::String(format!(
                "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/{sa}:generateAccessToken"
            )),
        );
    }
    doc.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugincredential::present::Presentation;
    use crate::plugincredential::source::SourceDecl;

    /// One case: the function name, its positionals, and its named arguments.
    type Case = (&'static str, Vec<Value>, Vec<(&'static str, Value)>);

    fn call(name: &str, positional: Vec<Value>, named: &[(&str, Value)]) -> anyhow::Result<Value> {
        let defs = definitions();
        let d = defs
            .iter()
            .find(|d| d.name == name)
            .ok_or_else(|| anyhow::anyhow!("no such function {name}"))?;
        let args = FnArgs {
            positional,
            named: named
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        };
        let f = d.func.clone();
        futures::executor::block_on(async move {
            f.call(
                &FnCallContext {
                    pkg: "auth",
                    root: std::path::Path::new("/ws"),
                },
                args,
            )
            .await
        })
    }

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }
    fn list(vs: &[&str]) -> Value {
        Value::List(vs.iter().map(|v| s(v)).collect())
    }

    /// The claim worth being able to check: a preset returns a dict the author
    /// could have written, so every one of them must parse as the primitive it
    /// stands for.
    #[test]
    fn every_source_preset_parses_as_a_source() {
        let cases: Vec<Case> = vec![
            (
                "env",
                vec![list(&["GITHUB_TOKEN"])],
                vec![("when", s("ci"))],
            ),
            (
                "file",
                vec![s("~/.config/svc/creds.json")],
                vec![
                    (
                        "fields",
                        Value::Map(
                            [("token".to_string(), s("access_token"))]
                                .into_iter()
                                .collect(),
                        ),
                    ),
                    ("expires", s("expires_at")),
                ],
            ),
            (
                "exec",
                vec![list(&["vault", "read", "-format=json", "secret/cf"])],
                vec![
                    ("login", Value::List(vec![list(&["vault", "login"])])),
                    ("runner", s("//tools/devenv:runner")),
                    ("credentials", s("//auth:vault")),
                ],
            ),
            (
                "passthrough",
                vec![],
                vec![
                    (
                        "paths",
                        Value::Map(
                            [("config".to_string(), s("~/.config/gcloud"))]
                                .into_iter()
                                .collect(),
                        ),
                    ),
                    (
                        "env",
                        Value::Map(
                            [("CLOUDSDK_CONFIG".to_string(), s("${file:config}"))]
                                .into_iter()
                                .collect(),
                        ),
                    ),
                ],
            ),
            (
                "oidc",
                vec![s("github_actions")],
                vec![("audience", s("sts.amazonaws.com"))],
            ),
        ];
        for (name, positional, named) in cases {
            let v = call(name, positional, &named).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            SourceDecl::parse(&v).unwrap_or_else(|e| panic!("{name} does not parse: {e:#}"));
        }
    }

    #[test]
    fn every_presentation_preset_parses_as_a_presentation() {
        let cases: Vec<Case> = vec![
            ("aws_process", vec![], vec![]),
            (
                "aws_web_identity",
                vec![],
                vec![("role", s("arn:aws:iam::123:role/deployer"))],
            ),
            (
                "gcp",
                vec![],
                vec![
                    ("audience", s("//iam.googleapis.com/projects/1/x")),
                    ("impersonate", s("d@p.iam.gserviceaccount.com")),
                ],
            ),
            (
                "azure_workload",
                vec![],
                vec![("client_id", s("c")), ("tenant_id", s("t"))],
            ),
            ("github", vec![], vec![]),
            ("docker", vec![list(&["ghcr.io"])], vec![]),
            ("git", vec![list(&["git.corp.example"])], vec![]),
            ("netrc", vec![list(&["artifacts.corp"])], vec![]),
        ];
        for (name, positional, named) in cases {
            let v = call(name, positional, &named).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            Presentation::parse(&v).unwrap_or_else(|e| panic!("{name} does not parse: {e:#}"));
        }
    }

    #[test]
    fn aws_web_identity_is_a_token_file_and_three_variables() {
        let v = call(
            "aws_web_identity",
            vec![],
            &[("role", s("arn:aws:iam::123:role/deployer"))],
        )
        .expect("call");
        let p = Presentation::parse(&v).expect("parse");
        assert_eq!(p.files.get("token").map(|d| d.raw()), Some("${id_token}"));
        assert_eq!(
            p.env.get("AWS_WEB_IDENTITY_TOKEN_FILE").map(|d| d.raw()),
            Some("${file:token}")
        );
        assert_eq!(
            p.env.get("AWS_ROLE_ARN").map(|d| d.raw()),
            Some("arn:aws:iam::123:role/deployer")
        );
        assert_eq!(
            p.env.get("AWS_ROLE_SESSION_NAME").map(|d| d.raw()),
            Some("heph")
        );
        assert!(p.helper.is_none(), "web identity is not a callback");
    }

    #[test]
    fn the_gcp_document_is_the_file_sourced_external_account_form() {
        let doc: serde_json::Value =
            serde_json::from_str(&gcp_external_account("//iam.googleapis.com/x", None))
                .expect("valid json");
        assert_eq!(doc["type"], "external_account");
        assert_eq!(doc["audience"], "//iam.googleapis.com/x");
        assert_eq!(doc["credential_source"]["file"], "${file:token}");
        // Absent without `impersonate`: adding it unconditionally would change
        // which identity the credential ends up being.
        assert!(doc.get("service_account_impersonation_url").is_none());

        let imp: serde_json::Value =
            serde_json::from_str(&gcp_external_account("//a", Some("sa@p.iam"))).expect("json");
        assert_eq!(
            imp["service_account_impersonation_url"],
            "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/sa@p.iam:generateAccessToken"
        );
    }

    #[test]
    fn the_netrc_body_is_one_line_per_machine() {
        let v = call("netrc", vec![list(&["a.corp", "b.corp"])], &[]).expect("call");
        let p = Presentation::parse(&v).expect("parse");
        assert_eq!(
            p.files.get("netrc").map(|d| d.raw()),
            Some(
                "machine a.corp login ${username} password ${token}\nmachine b.corp login ${username} password ${token}\n"
            )
        );
    }

    #[test]
    fn a_missing_required_argument_fails_at_build_evaluation() {
        let err = call("aws_web_identity", vec![], &[]).expect_err("must fail");
        assert!(format!("{err:#}").contains("`role` is required"), "{err:#}");
    }

    #[test]
    fn passthrough_refuses_both_shorthand_and_full_presentation() {
        let err = call(
            "passthrough",
            vec![],
            &[
                (
                    "paths",
                    Value::Map([("c".to_string(), s("~/.aws"))].into_iter().collect()),
                ),
                (
                    "env",
                    Value::Map([("A".to_string(), s("${file:c}"))].into_iter().collect()),
                ),
                (
                    "present",
                    Value::Map(
                        [(
                            "env".to_string(),
                            Value::Map([("B".to_string(), s("${file:c}"))].into_iter().collect()),
                        )]
                        .into_iter()
                        .collect(),
                    ),
                ),
            ],
        )
        .expect_err("must fail");
        assert!(format!("{err:#}").contains("not both"), "{err:#}");
    }

    #[test]
    fn there_is_no_source_preset_for_any_product() {
        let names: Vec<&str> = definitions()
            .iter()
            .map(|d| d.name.clone())
            .map(|n| Box::leak(n.into_boxed_str()) as &str)
            .collect();
        for banned in ["vault", "op", "onepassword", "aws_sso", "gcloud", "az"] {
            assert!(
                !names.contains(&banned),
                "`heph.auth.{banned}` must not exist: a source preset is a workflow frozen into a \
                 plugin, and the list has no end"
            );
        }
    }

    #[test]
    fn every_function_is_documented() {
        for d in definitions() {
            assert!(!d.doc.is_empty(), "heph.auth.{} has no doc", d.name);
        }
    }
}
