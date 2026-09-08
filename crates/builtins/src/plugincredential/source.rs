//! The chain: ordered ways to obtain material, and the rule that orders them.
//!
//! > A probe answers **"is this source applicable here?"**, not **"will it
//! > succeed?"**. The first applicable source *is* the source; an acquire failure
//! > is terminal, not a fallthrough.
//!
//! Every reader's first instinct is the opposite, which is why it is written at
//! the top of the module that implements it. It is what makes the ordering
//! meaningful — `oidc` sits above `exec` because on a laptop its probe fails
//! cleanly and on a runner it wins — and it is why `exec`'s probe being merely
//! "the program exists" is sufficient. If a chain fell through on failure, a
//! misconfigured role in CI would silently fall back to whatever ambient identity
//! the runner happened to have, which is precisely the class of accident this
//! feature exists to prevent.

use super::present::{Presentation, str_map, string, strings};
use hcore::htvalue::Value;
use std::collections::BTreeMap;

/// The fixed `when` vocabulary.
///
/// Policy, not logic, and deliberately not an expression language: the moment it
/// becomes one, BUILD files start containing credential-selection programs that
/// nobody can audit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum When {
    /// Any CI provider heph can detect.
    Ci,
    /// One named CI provider (`ci:github_actions`).
    CiProvider(String),
    /// A terminal is attached, so a human could answer a prompt.
    Interactive,
    /// A named host environment variable is set and non-empty.
    Env(String),
    /// `os:linux` / `os:darwin`.
    Os(String),
}

impl When {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(match s.split_once(':') {
            None => match s {
                "ci" => Self::Ci,
                "interactive" => Self::Interactive,
                other => anyhow::bail!(
                    "unknown `when` value {other:?} — the vocabulary is `ci`, `ci:<provider>`, \
                     `interactive`, `env:NAME`, `os:linux`, `os:darwin`. It is deliberately a \
                     fixed list rather than an expression language"
                ),
            },
            Some(("ci", p)) => Self::CiProvider(p.to_string()),
            Some(("env", n)) if !n.is_empty() => Self::Env(n.to_string()),
            Some(("os", o @ ("linux" | "darwin"))) => Self::Os(o.to_string()),
            Some(("os", other)) => {
                anyhow::bail!("unknown `when = \"os:{other}\"` — heph supports linux and darwin")
            }
            Some(_) => anyhow::bail!(
                "unknown `when` value {s:?} — the vocabulary is `ci`, `ci:<provider>`, \
                 `interactive`, `env:NAME`, `os:linux`, `os:darwin`"
            ),
        })
    }

    pub fn as_str(&self) -> String {
        match self {
            Self::Ci => "ci".to_string(),
            Self::CiProvider(p) => format!("ci:{p}"),
            Self::Interactive => "interactive".to_string(),
            Self::Env(n) => format!("env:{n}"),
            Self::Os(o) => format!("os:{o}"),
        }
    }
}

/// Which CI provider a `when` names, and how heph recognizes it.
///
/// Detection is by the provider's own marker variable, never by a generic `CI`
/// alone: `CI=true` is set by a dozen things including a developer's shell
/// profile, and picking a source on it would be exactly the kind of accidental
/// identity selection the chain exists to prevent.
pub fn detected_ci_provider(env: &dyn Fn(&str) -> Option<String>) -> Option<&'static str> {
    const MARKERS: &[(&str, &str)] = &[
        ("github_actions", "GITHUB_ACTIONS"),
        ("gitlab_ci", "GITLAB_CI"),
        ("buildkite", "BUILDKITE"),
        ("circleci", "CIRCLECI"),
        ("jenkins", "JENKINS_URL"),
    ];
    MARKERS
        .iter()
        .find(|(_, marker)| env(marker).is_some_and(|v| !v.is_empty()))
        .map(|(name, _)| *name)
}

/// One way to obtain material.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum SourceKind {
    /// Host environment variables. Probes that each is set.
    Env { names: Vec<String> },
    /// A file on the host, read raw or picked apart as JSON.
    File {
        path: String,
        fields: BTreeMap<String, String>,
        expires: Option<String>,
    },
    /// One command whose **stdout** is the credential.
    ///
    /// There is no way to name files this command wrote, because a command that
    /// writes files is a target — see [`SourceKind::Target`].
    Exec {
        run: Vec<String>,
        fields: BTreeMap<String, String>,
        expires: Option<String>,
        /// A *list* of argvs: some tools keep more than one login store, and
        /// `heph auth login` runs whichever the probe found stale.
        login: Vec<Vec<String>>,
        /// Where the probe and the command run. Both, necessarily: probing the
        /// host for a program that lives in a devenv shell answers the wrong
        /// question.
        runner: Option<String>,
    },
    /// Host paths and variables exposed in place — `~/.aws`, `~/.config/gcloud`.
    /// Nothing is copied into the credential store.
    Passthrough {
        paths: BTreeMap<String, String>,
        login: Vec<Vec<String>>,
    },
    /// A CI provider's OIDC endpoint. Yields `${id_token}`.
    Oidc { provider: String, audience: String },
    /// A target address.
    ///
    /// The driver of the referenced target decides what this means, so it needs
    /// no syntax of its own: a `credential` target is a delegation, anything else
    /// is a producer whose outputs become material.
    Target { addr: String },
}

impl SourceKind {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Env { .. } => "env",
            Self::File { .. } => "file",
            Self::Exec { .. } => "exec",
            Self::Passthrough { .. } => "passthrough",
            Self::Oidc { .. } => "oidc",
            Self::Target { .. } => "target",
        }
    }
}

/// One entry in a credential's chain.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SourceDecl {
    pub kind: SourceKind,
    pub when: Option<When>,
    /// Credentials presented to this source's own acquire subprocess, exactly as
    /// they would be to a consumer. A source that shells out to a secret manager
    /// has to authenticate to the secret manager; this is that, and it costs
    /// nothing new to implement because the acquire subprocess is just another
    /// consumer of the presentation machinery.
    pub credentials: Vec<String>,
    /// Overrides the credential's presentation, for this source only. Required
    /// when two sources yield genuinely different material shapes — a token file
    /// and a key pair are not the same thing.
    pub present: Option<Presentation>,
    /// Shown when this source's probe fails. Every kind ships a default.
    pub hint: Option<String>,
}

impl SourceDecl {
    /// A one-line label for `heph auth status` / `heph auth explain`.
    pub fn label(&self) -> String {
        match &self.kind {
            SourceKind::Env { names } => format!("env({})", names.join(",")),
            SourceKind::File { path, .. } => format!("file({path})"),
            SourceKind::Exec { run, .. } => {
                format!("exec({})", run.first().map(String::as_str).unwrap_or(""))
            }
            SourceKind::Passthrough { paths, .. } => {
                let mut names: Vec<&str> = paths.keys().map(String::as_str).collect();
                names.sort_unstable();
                format!("passthrough({})", names.join(","))
            }
            SourceKind::Oidc { provider, .. } => format!("oidc({provider})"),
            SourceKind::Target { addr } => format!("target({addr})"),
        }
    }

    /// What to tell a human when this source's probe fails.
    ///
    /// Every kind ships a default so a chain that applies nowhere is never a bare
    /// "no source applies": the fix is attached to the line that failed. An
    /// author who knows better overrides it with `hint`.
    pub fn default_hint(&self) -> String {
        match &self.kind {
            SourceKind::Env { names } => format!(
                "set {} in this environment, or add it to the job's secrets",
                names.join(", ")
            ),
            SourceKind::File { path, .. } => {
                format!("create {path}, or sign in with the tool that writes it")
            }
            SourceKind::Exec { run, login, .. } => {
                let program = run.first().map(String::as_str).unwrap_or("the command");
                if login.is_empty() {
                    format!("install {program}, or add it to the target's tools")
                } else {
                    format!(
                        "install {program}, or add it to the target's tools; then run \
                         `heph auth login`"
                    )
                }
            }
            SourceKind::Passthrough { paths, .. } => {
                let mut ps: Vec<&str> = paths.values().map(String::as_str).collect();
                ps.sort_unstable();
                format!("sign in with the tool that writes {}", ps.join(", "))
            }
            SourceKind::Oidc { provider, .. } if provider == "github_actions" => {
                "add `permissions: { id-token: write }` to the job".to_string()
            }
            SourceKind::Oidc { provider, .. } => {
                format!("no {provider} OIDC endpoint is present in this environment")
            }
            SourceKind::Target { .. } => {
                "this source is selected by `when`, and no `when` matched here".to_string()
            }
        }
    }

    /// Parse one chain entry.
    ///
    /// A bare string is an address; a dict is an inline source. There is no `ref`
    /// kind to learn, because an address already is one.
    pub fn parse(v: &Value) -> anyhow::Result<Self> {
        match v {
            Value::String(addr) => Ok(Self {
                kind: SourceKind::Target { addr: addr.clone() },
                when: None,
                credentials: vec![],
                present: None,
                hint: None,
            }),
            Value::Map(m) => Self::parse_inline(m),
            other => anyhow::bail!(
                "a credential source must be a target address or a dict (normally written with a \
                 `heph.auth.*` constructor), got {other:?}"
            ),
        }
    }

    fn parse_inline(m: &std::collections::HashMap<String, Value>) -> anyhow::Result<Self> {
        let kind_name = m
            .get("kind")
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "an inline credential source needs a `kind` — one of env, file, exec, \
                     passthrough, oidc. Normally you write `heph.auth.exec([...])` rather than \
                     the dict"
                )
            })
            .and_then(|v| string(v, "kind"))?;

        // Shared fields, taken off first so the per-kind match sees only its own.
        let when = m.get("when").map(|v| string(v, "when")).transpose()?;
        let when = when.as_deref().map(When::parse).transpose()?;
        let credentials = m
            .get("credentials")
            .map(|v| strings(v, "credentials"))
            .transpose()?
            .unwrap_or_default();
        let present = m.get("present").map(Presentation::parse).transpose()?;
        let hint = m.get("hint").map(|v| string(v, "hint")).transpose()?;

        let fields = m
            .get("fields")
            .map(|v| str_map(v, "fields"))
            .transpose()?
            .unwrap_or_default();
        let expires = m.get("expires").map(|v| string(v, "expires")).transpose()?;
        let login = m
            .get("login")
            .map(parse_login)
            .transpose()?
            .unwrap_or_default();

        let kind = match kind_name.as_str() {
            "env" => {
                let names = strings(
                    m.get("names").ok_or_else(|| {
                        anyhow::anyhow!("an `env` credential source needs `names`")
                    })?,
                    "names",
                )?;
                if names.is_empty() {
                    anyhow::bail!("an `env` credential source needs at least one name");
                }
                reject(
                    m,
                    &["kind", "when", "credentials", "present", "hint", "names"],
                    "env",
                )?;
                SourceKind::Env { names }
            }
            "file" => {
                let path = string(
                    m.get("path").ok_or_else(|| {
                        anyhow::anyhow!("a `file` credential source needs a `path`")
                    })?,
                    "path",
                )?;
                reject(
                    m,
                    &[
                        "kind",
                        "when",
                        "credentials",
                        "present",
                        "hint",
                        "path",
                        "fields",
                        "expires",
                    ],
                    "file",
                )?;
                SourceKind::File {
                    path,
                    fields,
                    expires,
                }
            }
            "exec" => {
                let run = strings(
                    m.get("run").ok_or_else(|| {
                        anyhow::anyhow!("an `exec` credential source needs `run`")
                    })?,
                    "run",
                )?;
                if run.is_empty() {
                    anyhow::bail!("an `exec` credential source needs a non-empty `run` argv");
                }
                let runner = m.get("runner").map(|v| string(v, "runner")).transpose()?;
                reject(
                    m,
                    &[
                        "kind",
                        "when",
                        "credentials",
                        "present",
                        "hint",
                        "run",
                        "fields",
                        "expires",
                        "login",
                        "runner",
                    ],
                    "exec",
                )?;
                SourceKind::Exec {
                    run,
                    fields,
                    expires,
                    login,
                    // `"local"` is the spelling for "this host" everywhere else
                    // in heph (the exec driver's `runner`), so it means the same
                    // here rather than naming a target called `local`.
                    runner: runner.filter(|r| r != "local"),
                }
            }
            "passthrough" => {
                let paths = m
                    .get("paths")
                    .map(|v| str_map(v, "paths"))
                    .transpose()?
                    .unwrap_or_default();
                if paths.is_empty() {
                    anyhow::bail!(
                        "a `passthrough` credential source needs `paths` — heph exposes the host \
                         files you name, and takes no view about which product wrote them"
                    );
                }
                reject(
                    m,
                    &[
                        "kind",
                        "when",
                        "credentials",
                        "present",
                        "hint",
                        "paths",
                        "login",
                    ],
                    "passthrough",
                )?;
                SourceKind::Passthrough { paths, login }
            }
            "oidc" => {
                let provider = m
                    .get("provider")
                    .map(|v| string(v, "provider"))
                    .transpose()?
                    .unwrap_or_else(|| "github_actions".to_string());
                if provider != "github_actions" && provider != "generic" {
                    anyhow::bail!(
                        "unknown oidc `provider` {provider:?} — heph knows `github_actions` and \
                         `generic` (a token named by variable or file)"
                    );
                }
                let audience = m
                    .get("audience")
                    .map(|v| string(v, "audience"))
                    .transpose()?
                    .unwrap_or_default();
                reject(
                    m,
                    &[
                        "kind",
                        "when",
                        "credentials",
                        "present",
                        "hint",
                        "provider",
                        "audience",
                    ],
                    "oidc",
                )?;
                SourceKind::Oidc { provider, audience }
            }
            other => anyhow::bail!(
                "unknown credential source kind {other:?} — core knows env, file, exec, \
                 passthrough and oidc, and nothing else. There is deliberately no per-product \
                 source: a secret manager is `exec` with a field map, or a target when its tool \
                 lives in an environment"
            ),
        };

        Ok(Self {
            kind,
            when,
            credentials,
            present,
            hint,
        })
    }
}

/// `login` is a list of argvs, but one argv is by far the common case, so a
/// single flat list of strings is accepted and read as one command.
fn parse_login(v: &Value) -> anyhow::Result<Vec<Vec<String>>> {
    match v {
        Value::List(items) => {
            if items.iter().all(|i| matches!(i, Value::String(_))) {
                let argv = strings(v, "login")?;
                return Ok(if argv.is_empty() { vec![] } else { vec![argv] });
            }
            items.iter().map(|i| strings(i, "login")).collect()
        }
        other => anyhow::bail!(
            "credential `login` must be an argv or a list of argvs, got {other:?} — a list, \
             because some tools keep more than one login store"
        ),
    }
}

/// Reject a key that means nothing for this kind.
///
/// A silently-ignored `login` on an `oidc` source is a workflow the author
/// believes exists and does not, which surfaces much later as an expiry nobody
/// can repair.
fn reject(
    m: &std::collections::HashMap<String, Value>,
    allowed: &[&str],
    kind: &str,
) -> anyhow::Result<()> {
    let mut unknown: Vec<&str> = m
        .keys()
        .map(String::as_str)
        .filter(|k| !allowed.contains(k))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort_unstable();
    anyhow::bail!(
        "a `{kind}` credential source does not take {} — it accepts {}",
        unknown
            .iter()
            .map(|k| format!("`{k}`"))
            .collect::<Vec<_>>()
            .join(", "),
        allowed
            .iter()
            .map(|k| format!("`{k}`"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn map(pairs: &[(&str, Value)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }
    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }
    fn list(vs: &[&str]) -> Value {
        Value::List(vs.iter().map(|v| s(v)).collect())
    }

    #[test]
    fn a_bare_address_is_a_source_with_no_syntax_of_its_own() {
        let d = SourceDecl::parse(&s("//auth:vault")).expect("parse");
        assert!(matches!(d.kind, SourceKind::Target { .. }));
    }

    #[test]
    fn an_exec_source_carries_its_argv_and_field_map() {
        let d = SourceDecl::parse(&map(&[
            ("kind", s("exec")),
            ("run", list(&["vault", "read", "-format=json", "secret/cf"])),
            ("fields", map(&[("token", s("data.token"))])),
            ("expires", s("lease_duration")),
            ("credentials", s("//auth:vault")),
        ]))
        .expect("parse");
        let SourceKind::Exec { run, fields, .. } = &d.kind else {
            panic!("expected exec")
        };
        assert_eq!(run.first().map(String::as_str), Some("vault"));
        assert_eq!(fields.get("token").map(String::as_str), Some("data.token"));
        assert_eq!(d.credentials, vec!["//auth:vault".to_string()]);
    }

    #[test]
    fn login_accepts_one_argv_and_a_list_of_them() {
        let one = parse_login(&list(&["gcloud", "auth", "login"])).expect("parse");
        assert_eq!(one.len(), 1);
        let two = parse_login(&Value::List(vec![
            list(&["gcloud", "auth", "login"]),
            list(&["gcloud", "auth", "application-default", "login"]),
        ]))
        .expect("parse");
        assert_eq!(two.len(), 2);
    }

    #[test]
    fn there_is_no_produces_on_an_inline_exec_source() {
        // A command that writes files is a target, so naming files here has to
        // fail loudly rather than be quietly ignored.
        let err = SourceDecl::parse(&map(&[
            ("kind", s("exec")),
            ("run", list(&["x"])),
            ("produces", map(&[("cred", s("c.json"))])),
        ]))
        .expect_err("must fail");
        assert!(format!("{err:#}").contains("`produces`"), "{err:#}");
    }

    #[test]
    fn a_product_named_source_kind_is_refused_with_the_reason() {
        let err = SourceDecl::parse(&map(&[("kind", s("vault"))])).expect_err("must fail");
        assert!(format!("{err:#}").contains("field map"), "{err:#}");
    }

    #[test]
    fn the_when_vocabulary_is_closed() {
        assert_eq!(When::parse("ci").expect("parse"), When::Ci);
        assert_eq!(
            When::parse("ci:github_actions").expect("parse"),
            When::CiProvider("github_actions".to_string())
        );
        assert_eq!(
            When::parse("env:CI_JOB").expect("parse"),
            When::Env("CI_JOB".to_string())
        );
        assert_eq!(
            When::parse("os:darwin").expect("parse"),
            When::Os("darwin".to_string())
        );
        // Not an expression language.
        drop(When::parse("ci && !os:darwin").expect_err("not an expression language"));
        drop(When::parse("os:windows").expect_err("not a supported target"));
    }

    #[test]
    fn ci_detection_needs_a_providers_own_marker() {
        let none = |_n: &str| None;
        assert_eq!(detected_ci_provider(&none), None);
        // A bare `CI=true` is not a provider: a shell profile sets it.
        let generic = |n: &str| (n == "CI").then(|| "true".to_string());
        assert_eq!(detected_ci_provider(&generic), None);
        let gha = |n: &str| (n == "GITHUB_ACTIONS").then(|| "true".to_string());
        assert_eq!(detected_ci_provider(&gha), Some("github_actions"));
    }

    #[test]
    fn every_kind_ships_a_hint() {
        let kinds = [
            SourceKind::Env {
                names: vec!["A".to_string()],
            },
            SourceKind::File {
                path: "~/x".to_string(),
                fields: BTreeMap::new(),
                expires: None,
            },
            SourceKind::Exec {
                run: vec!["aws".to_string()],
                fields: BTreeMap::new(),
                expires: None,
                login: vec![],
                runner: None,
            },
            SourceKind::Passthrough {
                paths: BTreeMap::from([("c".to_string(), "~/.aws".to_string())]),
                login: vec![],
            },
            SourceKind::Oidc {
                provider: "github_actions".to_string(),
                audience: "a".to_string(),
            },
            SourceKind::Target {
                addr: "//a:b".to_string(),
            },
        ];
        for kind in kinds {
            let d = SourceDecl {
                kind,
                when: None,
                credentials: vec![],
                present: None,
                hint: None,
            };
            assert!(!d.default_hint().is_empty(), "{} has no hint", d.label());
        }
    }

    #[test]
    fn an_inline_source_needs_a_kind() {
        let err = SourceDecl::parse(&Value::Map(HashMap::new())).expect_err("must fail");
        assert!(format!("{err:#}").contains("needs a `kind`"), "{err:#}");
    }
}
