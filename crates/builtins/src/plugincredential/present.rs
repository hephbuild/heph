//! How material reaches a consumer.
//!
//! A presentation is the *shape* a credential arrives in, and nothing else. The
//! rule it exists to enforce is the sharpest one in the design:
//!
//! > A presentation may carry only **material** and the handles needed to use it
//! > — keys, tokens, a token-file path, a helper argv. Anything that **selects
//! > content** (a region, an account, a project, a profile) is an ordinary hashed
//! > input on the consumer.
//!
//! heph cannot check that rule mechanically, and it is worth being precise about
//! how far the vocabulary gets. Three keys, a closed set of helper dialects and a
//! four-token template grammar mean a *key* like `region` is rejected outright —
//! there is nowhere to put it. A content-selecting **value** is not caught:
//! `{"env": {"AWS_REGION": "eu-west-1"}}` parses, because it is the same shape
//! `heph.auth.aws_web_identity` uses to carry a role ARN, which is a handle
//! rather than a selector. Nothing from the outside tells those apart.
//!
//! What keeps that sound is the contract rather than the parser: a target whose
//! *output* depends on which identity ran it is not cacheable and says so with
//! `cache = False`. That is a deliberate, recorded decision — making it
//! structural would cost CI its remote sharing on every private fetch — and the
//! audit it owes in exchange is `heph auth explain` plus a documented rule, not a
//! check. See `docs/CREDENTIALS.md`, "The rule that is easiest to get wrong".

use hcore::htvalue::Value;
use hcore::htvalue::signature::ParamType;
use hplugin::htspec::{FromSpecValue, SpecStruct};
use std::collections::BTreeMap;

/// A callback protocol heph speaks on a tool's behalf.
///
/// Each of these is a **third-party wire format with exactly one correct
/// encoding**, which is the whole reason they are presets rather than something
/// an author hand-rolls: getting a field name wrong is a silent failure inside
/// somebody else's SDK. The set is closed because the number of such formats is
/// small and known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Dialect {
    /// The AWS credential-process contract: a config file naming
    /// `heph __auth-helper aws <addr>` as a profile's `credential_process`.
    Aws,
    /// GCP executable-sourced external-account credentials.
    Gcp,
    /// The Docker credential-helper protocol, via a `docker-credential-heph`
    /// shim and a generated `DOCKER_CONFIG` directory.
    Docker,
    /// The git credential-helper protocol, injected through `GIT_CONFIG_*` so no
    /// gitconfig is written and the developer's own is never touched.
    Git,
    /// The client-go `ExecCredential` object. Unlike the others heph writes no
    /// document: the author templates the kubeconfig and places the argv with
    /// `${helper:command}` and `${helper:args}`.
    Kubernetes,
}

/// Decoded through [`Dialect::parse`] rather than `#[derive(SpecEnum)]`, so the
/// name table and its message exist once: the same string is also parsed out of
/// `heph __auth-helper <dialect>`'s argv, where there is no `Value` to decode.
impl FromSpecValue for Dialect {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        Self::parse(&String::from_spec_value(v)?)
    }

    fn spec_param_type() -> ParamType {
        ParamType::String
    }
}

impl Dialect {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "aws" => Self::Aws,
            "gcp" => Self::Gcp,
            "docker" => Self::Docker,
            "git" => Self::Git,
            "kubernetes" => Self::Kubernetes,
            other => anyhow::bail!(
                "unknown credential helper dialect {other:?} — expected one of aws, gcp, docker, \
                 git, kubernetes. A helper is a protocol heph speaks on a tool's behalf; for a \
                 tool that speaks none, use `env` or `files`"
            ),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aws => "aws",
            Self::Gcp => "gcp",
            Self::Docker => "docker",
            Self::Git => "git",
            Self::Kubernetes => "kubernetes",
        }
    }

    /// Whether heph generates the tool's configuration document itself.
    ///
    /// False only for [`Kubernetes`](Self::Kubernetes): a kubeconfig is not
    /// purely a credential format — it also carries the cluster, which is the
    /// author's — so heph supplies only the callback argv.
    pub fn writes_a_document(self) -> bool {
        !matches!(self, Self::Kubernetes)
    }
}

/// A helper presentation: which dialect, and the handles that dialect needs.
///
/// Every field here is a *handle* — which registries this credential covers,
/// which git hosts, which workload-identity audience to present it as. None of
/// them selects content, which is the test §2 of the design applies to
/// everything on this side of the line.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default, SpecStruct)]
pub struct Helper {
    /// Which callback protocol heph speaks.
    pub dialect: Option<Dialect>,
    /// Docker: registries this credential authenticates. Empty is an error —
    /// a credHelpers map with no entries authenticates nothing.
    pub registries: Vec<String>,
    /// Git: hosts this credential authenticates.
    pub hosts: Vec<String>,
    /// GCP: the workload-identity-pool audience the token is presented for.
    pub audience: Option<String>,
    /// GCP: a service account to impersonate. Some Google services do not accept
    /// a federated identity directly, which is why this exists at all.
    pub impersonate: Option<String>,
}

impl Helper {
    pub fn dialect(&self) -> anyhow::Result<Dialect> {
        self.dialect
            .ok_or_else(|| anyhow::anyhow!("credential `helper` needs a `dialect`"))
    }
}

/// How material reaches the sandbox.
///
/// Three shapes, in preference order, and they compose: a kubernetes helper is a
/// `files` kubeconfig plus an `env` pointing at it plus the callback argv.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default, SpecStruct)]
pub struct Presentation {
    /// Name → template. Injected as **runtime** environment — never `env`, never
    /// `pass_env` — so it cannot reach a def hash by construction.
    pub env: BTreeMap<String, String>,
    /// Name → content template. Written by the host at `0600` under the sandbox
    /// root, beside the workspace directory and never inside it, and deleted at
    /// run end whatever the outcome. Reachable as `${file:<name>}`.
    pub files: BTreeMap<String, String>,
    /// A callback protocol. The only presentation that survives expiry.
    #[spec(parse = parse_helper)]
    pub helper: Option<Helper>,
}

impl Presentation {
    pub fn is_empty(&self) -> bool {
        self.env.is_empty() && self.files.is_empty() && self.helper.is_none()
    }

    /// Parse a presentation from its BUILD-file value, then check it.
    ///
    /// Accepts the map form written by hand and the dicts the `heph.auth.*`
    /// presets return — which are the same thing, deliberately: a preset is a
    /// function returning a value the author could have typed.
    ///
    /// The parse is `#[derive(SpecStruct)]`: the key set, the per-field decoding
    /// and the unknown-key refusal all come from the field list, so the parser
    /// and the schema cannot drift. What stays here is the part that is not
    /// parsing — the emptiness rule and [`validate`](Self::validate).
    pub fn parse(v: &Value) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        let out = Self::from_spec_value(v).context(
            "credential `present` takes `env` (name → template), `files` (name → content \
             template) and/or `helper` (a callback protocol). Configuration that selects content \
             (a region, an account, a project) is a hashed input on the consumer, not part of a \
             presentation",
        )?;
        if out.is_empty() {
            anyhow::bail!(
                "credential `present` is empty — a credential that presents nothing hands its \
                 consumer no identity"
            );
        }
        out.validate()?;
        Ok(out)
    }

    /// Reject a template referring to a kind that does not exist here.
    ///
    /// Done on the declaration so a typo is reported once, at the source, rather
    /// than once per consumer at acquisition time — and long before any material
    /// has been fetched, which is the point at which a mistake is cheapest.
    fn validate(&self) -> anyhow::Result<()> {
        // A presented file's name becomes one path component under the run's
        // credential directory. Anything else escapes it — and the teardown that
        // deletes those files only removes that directory, so an escaped `0600`
        // token would be left behind with nothing to collect it. Checked on the
        // declaration, where the author can see it.
        for name in self.files.keys() {
            validate_file_name(name)?;
        }
        for (name, tmpl) in self.env.iter().chain(self.files.iter()) {
            for piece in hcore::template::parse(tmpl).with_context_name(name)?.iter() {
                let hcore::template::Piece::Ref(r) = piece else {
                    continue;
                };
                match r.kind {
                    // A bare `${name}` is a material field. Which fields exist
                    // depends on which source wins, so it can only be checked at
                    // acquisition — where the error names the credential, the
                    // field and the source that produced the material.
                    None => {}
                    Some("file") => {}
                    Some("helper") => match r.arg {
                        "command" | "args" => {}
                        other => anyhow::bail!(
                            "unknown template token `${{helper:{other}}}` in `{name}` — expected \
                             `${{helper:command}}` (the heph binary) or `${{helper:args}}` (the \
                             argv tail that selects a dialect and this credential)"
                        ),
                    },
                    Some(other) => anyhow::bail!(
                        "unknown template kind {other:?} in `{name}` — a presentation understands \
                         `${{<field>}}`, `${{file:<name>}}`, `${{helper:command}}` and \
                         `${{helper:args}}`. Write a literal `$` as `$$`"
                    ),
                }
            }
        }
        if let Some(h) = &self.helper {
            let dialect = h.dialect()?;
            match dialect {
                Dialect::Docker if h.registries.is_empty() => anyhow::bail!(
                    "the docker credential helper needs at least one entry in `registries` — a \
                     credHelpers map with no entries authenticates nothing"
                ),
                Dialect::Git if h.hosts.is_empty() => anyhow::bail!(
                    "the git credential helper needs at least one entry in `hosts` — a credential \
                     config with no host matches no fetch"
                ),
                Dialect::Gcp if h.audience.is_none() => anyhow::bail!(
                    "the gcp credential helper needs an `audience` — the workload-identity pool \
                     provider this token is presented for"
                ),
                _ => {}
            }
            // The one dialect that writes no document is also the one that
            // cannot be reached without the author placing the callback: a
            // kubernetes helper with no `${helper:command}` anywhere is inert.
            if !dialect.writes_a_document()
                && !self
                    .files
                    .values()
                    .chain(self.env.values())
                    .any(|t| t.contains("${helper:command}"))
            {
                anyhow::bail!(
                    "a `kubernetes` helper writes no document of its own, so nothing calls back \
                     into heph unless the kubeconfig you present places it — put \
                     `${{helper:command}}` and `${{helper:args}}` in the `exec` stanza of the \
                     user you present"
                );
            }
        }
        Ok(())
    }
}

/// `anyhow::Context` for a template parse, naming the field it came from.
trait ContextName<T> {
    fn with_context_name(self, name: &str) -> anyhow::Result<T>;
}

impl<T> ContextName<T> for anyhow::Result<T> {
    fn with_context_name(self, name: &str) -> anyhow::Result<T> {
        use anyhow::Context as _;
        self.with_context(|| format!("in `{name}`"))
    }
}

/// A presented file's name must be a single plain path component.
pub fn validate_file_name(name: &str) -> anyhow::Result<()> {
    let mut comps = std::path::Path::new(name).components();
    let plain =
        matches!(comps.next(), Some(std::path::Component::Normal(_))) && comps.next().is_none();
    if !plain {
        anyhow::bail!(
            "credential file name {name:?} must be a plain name with no `/` and no `..` — it \
             becomes one path component under the run's credential directory, and that directory \
             is what gets deleted at run end"
        );
    }
    Ok(())
}

/// `helper = "kubernetes"` — the shorthand for a dialect that needs no options —
/// or the full dict, whose keys [`Helper`]'s derive owns.
///
/// Shape dispatch rather than a union, for the reason `TargetSpecCache` gives:
/// a map *commits* to the dict arm, so an unknown key inside it reports itself
/// rather than being masked by a generic "expected string | map".
fn parse_helper(v: &Value) -> anyhow::Result<Option<Helper>> {
    let h = match v {
        Value::Null() => return Ok(None),
        Value::String(s) => Helper {
            dialect: Some(Dialect::parse(s)?),
            ..Helper::default()
        },
        other => Helper::from_spec_value(other)
            .map_err(|e| anyhow::anyhow!("{e:#}"))
            .map_err(|e| {
                anyhow::anyhow!("credential `present.helper` must be a dialect name or a dict: {e}")
            })?,
    };
    // A helper with no dialect is inert — nothing to speak.
    h.dialect()?;
    Ok(Some(h))
}

// `sources` is an ordered, heterogeneous list — a bare address or one of five
// inline kinds, discriminated by `kind` — which is the one shape the derive's
// field types cannot describe, so [`SourceDecl::parse`] dispatches by hand. The
// three helpers below exist only to carry the field name into the message; the
// decoding itself is the shared `FromSpecValue` every other driver uses, so
// "what is a string here" has exactly one answer workspace-wide.

pub(crate) fn string(v: &Value, what: &str) -> anyhow::Result<String> {
    use anyhow::Context as _;
    String::from_spec_value(v).with_context(|| format!("credential `{what}`"))
}

pub(crate) fn strings(v: &Value, what: &str) -> anyhow::Result<Vec<String>> {
    use anyhow::Context as _;
    Vec::<String>::from_spec_value(v).with_context(|| format!("credential `{what}`"))
}

pub(crate) fn str_map(v: &Value, what: &str) -> anyhow::Result<BTreeMap<String, String>> {
    use anyhow::Context as _;
    BTreeMap::<String, String>::from_spec_value(v).with_context(|| format!("credential `{what}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn an_env_presentation_parses() {
        let p = Presentation::parse(&map(&[(
            "env",
            map(&[("CLOUDFLARE_API_TOKEN", s("${token}"))]),
        )]))
        .expect("parse");
        assert_eq!(
            p.env.get("CLOUDFLARE_API_TOKEN").map(String::as_str),
            Some("${token}")
        );
    }

    #[test]
    fn a_region_has_nowhere_to_go() {
        // The design's sharpest rule, enforced by the vocabulary rather than by
        // a check: there is no key a region fits in.
        let err = Presentation::parse(&map(&[("region", s("eu-west-1"))])).expect_err("must fail");
        let msg = format!("{err:#}");
        // The derive names the offending key; the context says where a region
        // does belong.
        assert!(msg.contains("region"), "{msg}");
        assert!(msg.contains("hashed input on the consumer"), "{msg}");
    }

    #[test]
    fn an_unknown_template_kind_is_rejected_at_the_declaration() {
        let err = Presentation::parse(&map(&[("env", map(&[("A", s("${secret:x}"))]))]))
            .expect_err("must fail");
        assert!(
            format!("{err:#}").contains("unknown template kind"),
            "{err:#}"
        );
    }

    #[test]
    fn a_bare_field_reference_is_accepted_because_only_a_source_knows_its_fields() {
        Presentation::parse(&map(&[("env", map(&[("A", s("${anything}"))]))])).expect("parse");
    }

    #[test]
    fn the_helper_shorthand_is_a_dialect_name() {
        let p = Presentation::parse(&map(&[
            ("helper", s("kubernetes")),
            (
                "files",
                map(&[("kubeconfig", s("command: ${helper:command}"))]),
            ),
        ]))
        .expect("parse");
        assert_eq!(p.helper.and_then(|h| h.dialect), Some(Dialect::Kubernetes));
    }

    #[test]
    fn a_kubernetes_helper_that_places_no_callback_is_inert_and_says_so() {
        let err = Presentation::parse(&map(&[("helper", s("kubernetes"))])).expect_err("must fail");
        assert!(format!("{err:#}").contains("${helper:command}"), "{err:#}");
    }

    #[test]
    fn a_docker_helper_with_no_registries_authenticates_nothing() {
        let err = Presentation::parse(&map(&[("helper", map(&[("dialect", s("docker"))]))]))
            .expect_err("must fail");
        assert!(format!("{err:#}").contains("registries"), "{err:#}");
    }

    #[test]
    fn a_file_name_that_escapes_the_credential_directory_is_rejected() {
        for bad in ["../leak", "a/b", "/tmp/leak", "..", "."] {
            let err = Presentation::parse(&map(&[("files", map(&[(bad, s("${token}"))]))]))
                .expect_err("must reject");
            assert!(format!("{err:#}").contains("plain name"), "{bad}: {err:#}");
        }
        Presentation::parse(&map(&[("files", map(&[("token", s("${token}"))]))])).expect("parse");
    }

    #[test]
    fn an_empty_presentation_is_rejected() {
        let err = Presentation::parse(&Value::Map(Default::default())).expect_err("must fail");
        assert!(format!("{err:#}").contains("presents nothing"), "{err:#}");
    }
}
