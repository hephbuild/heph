//! The `credential` driver: declares what identity is needed, how it may be
//! obtained, and the shape it is presented in.
//!
//! A credential is a target for the same reasons a `scratch` is. Settings live in
//! exactly one place, so two consumers cannot disagree about which role to
//! assume; the address gives packages, visibility and `heph query revdeps` for
//! free; and `heph auth status` has something to enumerate. Inline credential
//! config on each consumer would make "which credentials does this workspace
//! need?" an unanswerable question.
//!
//! The contract everything here follows from:
//!
//! > A credential grants **access**; it is not an **input**. A target's outputs
//! > must be identical whichever identity satisfied its credential requirement.
//!
//! This driver is only the **declaration**. It has no inputs, no outputs, never
//! executes, and is never cached as a target — `parse` returns a def and `run` is
//! inert. The chain walk, the acquisition, the store and the presentation all
//! live in the engine, on behalf of the targets that reference it.
//!
//! # Why the parsing lives here rather than in the engine
//!
//! Same reason as `scratch`: the engine reads a declaration from its *spec
//! config* (a `raw_def` is opaque to the host by contract), so host and driver
//! must agree about what a declaration means. Reading it through the same
//! function both call is what makes that agreement structural.

pub mod functions;
pub mod present;
pub mod source;

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hcore::htvalue::Value;
use hcore::htvalue::signature::ParamType;
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, DriverField,
    DriverSchema, ParseRequest, ParseResponse, RunRequest, RunResponse,
    targetdef::{CacheConfig, TargetDef},
};
use std::sync::Arc;
use std::time::Duration;
use xxhash_rust::xxh3::Xxh3;

pub use present::{Dialect, Helper, Presentation};
pub use source::{SourceDecl, SourceKind, When};

pub const DRIVER_NAME: &str = "credential";

/// A parsed credential declaration.
///
/// Public because the engine reads these settings when a consumer references the
/// target — but note it reads them from the *spec config*, not from here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CredentialDef {
    /// The ordered chain. The first applicable source is the source.
    pub sources: Vec<SourceDecl>,
    /// The presentation used by any source that does not override it.
    pub present: Option<Presentation>,
    /// A declared lifetime, for material whose source reports none.
    ///
    /// Set it *under* the true lifetime, never at it. Without one, material with
    /// no expiry is never written to the disk cache — a one-hour session token
    /// cached for an hour is a convenience; a long-lived API token written to
    /// `<home>/auth/` is a durable secret at rest that nothing will ever clean up.
    #[serde(serialize_with = "ser_ttl")]
    pub ttl: Option<Duration>,
}

fn ser_ttl<S: serde::Serializer>(v: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(d) => s.serialize_some(&d.as_secs()),
        None => s.serialize_none(),
    }
}

impl CredentialDef {
    /// The presentation a given source uses: its own override, or the
    /// credential's.
    ///
    /// A source's override exists because a token file and a key pair are
    /// genuinely different shapes — which is forced by the domain rather than
    /// chosen. When the sources agree, `present` on the credential says so once.
    pub fn presentation_for<'a>(
        &'a self,
        source: &'a SourceDecl,
    ) -> anyhow::Result<&'a Presentation> {
        source
            .present
            .as_ref()
            .or(self.present.as_ref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "the {} source has no presentation, and the credential declares none — add \
                     `present = …` to the credential, or to this source if its material has a \
                     different shape from the others",
                    source.label()
                )
            })
    }
}

/// Config for a `credential` target.
///
/// Hand-parsed rather than `#[derive(Spec)]`d: `sources` is a heterogeneous list
/// (an address *or* an inline dict) and `present` is a nested document, neither of
/// which the derive's field shapes describe. The schema below is written out for
/// the same reason.
fn schema() -> DriverSchema {
    let strs = || ParamType::union(vec![ParamType::String, ParamType::list(ParamType::String)]);
    let present_ty = || {
        ParamType::strukt(vec![
            ("env", ParamType::map(ParamType::String)),
            ("files", ParamType::map(ParamType::String)),
            (
                "helper",
                ParamType::union(vec![
                    ParamType::String,
                    ParamType::strukt(vec![
                        ("dialect", ParamType::String),
                        ("registries", strs()),
                        ("hosts", strs()),
                        ("audience", ParamType::String),
                        ("impersonate", ParamType::String),
                    ]),
                ]),
            ),
        ])
    };
    // An address, or one of the five inline kinds. Written out rather than
    // derived because the union is genuinely heterogeneous — which is also why
    // authors write `heph.auth.exec(...)` and get the fields checked by a
    // function signature instead.
    let source_ty = ParamType::union(vec![
        ParamType::String,
        ParamType::strukt(vec![
            ("kind", ParamType::String),
            ("when", ParamType::String),
            ("credentials", strs()),
            ("present", present_ty()),
            ("hint", ParamType::String),
            ("names", strs()),
            ("path", ParamType::String),
            ("run", strs()),
            ("fields", ParamType::map(ParamType::String)),
            ("expires", ParamType::String),
            ("login", ParamType::list(strs())),
            ("runner", ParamType::String),
            ("paths", ParamType::map(ParamType::String)),
            ("provider", ParamType::String),
            ("audience", ParamType::String),
        ]),
    ]);
    DriverSchema {
        fields: vec![
            DriverField {
                name: "sources".to_string(),
                ty: ParamType::list(source_ty),
                doc: "Ordered ways to obtain this credential. Each is a target address or a \
                      `heph.auth.*` constructor. The first *applicable* source is the source: an \
                      acquire failure is terminal, not a fallthrough, so a misconfigured role in \
                      CI cannot silently fall back to an ambient identity."
                    .to_string(),
                required: true,
            },
            DriverField {
                name: "present".to_string(),
                ty: present_ty(),
                doc: "How the material reaches a consumer: `env` (name → template), `files` \
                      (name → content template, written 0600 and deleted at run end) and/or \
                      `helper` (a callback protocol — aws, gcp, docker, git, kubernetes). May \
                      instead be set per source, which is required when two sources yield \
                      different material shapes. A presentation carries material and handles \
                      only: anything that selects content (a region, an account, a project) is an \
                      ordinary hashed input on the consumer."
                    .to_string(),
                required: false,
            },
            DriverField {
                name: "ttl".to_string(),
                ty: ParamType::String,
                doc: "A declared lifetime (`55m`, `6h`) for material whose source reports none. \
                      Set it under the true lifetime, never at it. Without it, material with no \
                      expiry is never written to the disk cache."
                    .to_string(),
                required: false,
            },
        ],
    }
}

/// Parse and validate a credential declaration straight from a target spec.
///
/// The engine calls this when a consumer references a credential. Reading the
/// spec config — which *is* host-visible — through the same function the driver
/// uses keeps one implementation of the parsing and validation rules.
pub fn parse_declaration(spec: &hplugin::provider::TargetSpec) -> anyhow::Result<CredentialDef> {
    let mut sources: Vec<SourceDecl> = Vec::new();
    let mut present: Option<Presentation> = None;
    let mut ttl: Option<Duration> = None;

    for (k, v) in &spec.config {
        match k.as_str() {
            "sources" => {
                let Value::List(items) = v else {
                    anyhow::bail!(
                        "credential `sources` must be a list of sources, got {v:?} — the list is \
                         ordered, and the order is the whole point"
                    );
                };
                sources = items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        SourceDecl::parse(item).with_context(|| format!("sources[{i}]"))
                    })
                    .collect::<anyhow::Result<_>>()?;
            }
            "present" => present = Some(Presentation::parse(v)?),
            "ttl" => {
                let raw = present::string(v, "ttl")?;
                ttl = Some(hcore::units::parse_duration(&raw).context("credential `ttl`")?);
            }
            other => anyhow::bail!(
                "unknown key {other:?} on a `credential` target — it takes `sources`, `present` \
                 and `ttl`"
            ),
        }
    }

    if sources.is_empty() {
        anyhow::bail!(
            "a `credential` target needs at least one entry in `sources` — a credential with an \
             empty chain can never be acquired anywhere"
        );
    }

    // A source that overrides nothing and a credential that presents nothing
    // between them leave a consumer with no identity. Caught here so the failure
    // names the declaration rather than surfacing as a 403 from a tool.
    let def = CredentialDef {
        sources,
        present,
        ttl,
    };
    for (i, s) in def.sources.iter().enumerate() {
        def.presentation_for(s)
            .with_context(|| format!("sources[{i}]"))?;
    }
    Ok(def)
}

pub struct Driver;

#[async_trait]
impl hplugin::driver::Driver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    fn schema(&self) -> DriverSchema {
        schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let def = parse_declaration(&req.target_spec)
            .with_context(|| format!("credential {}", req.target_spec.addr))?;

        // The def hash covers the declaration so the graph sees a target that
        // changes when its config does. It does NOT reach any consumer's
        // `hashin`: a credential is referenced with `hashed: false`, precisely
        // because a target's outputs must be identical whichever identity
        // satisfied its requirement.
        //
        // Serialized rather than field-by-field because the declaration is a
        // tree, and a hand-written walk of it would be one more place for the
        // hash and the type to drift apart. Nothing secret is in it: a
        // declaration says how to obtain material, never what the material is.
        let mut h = Xxh3::new();
        h.update(req.target_spec.addr.format().as_bytes());
        h.update(
            serde_json::to_vec(&def)
                .context("hash credential declaration")?
                .as_slice(),
        );
        let hash = format!("{:016x}", h.digest()).into_bytes();

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs: vec![],
                outputs: vec![],
                support_files: vec![],
                // Never cached as a target. Material has a lifetime of its own,
                // managed by the credential store, which is a different thing.
                cache: CacheConfig::off(),
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
        _req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        // Reachable and not an error, exactly as for `scratch`: resolving a
        // declaration directly (`heph run //auth:aws`) executes it like any other
        // target, and a declaration is inert by construction. Acquiring material
        // here would be wrong twice over — it would put a secret behind an
        // ordinary `run`, and it would make a build sign someone in.
        Ok(RunResponse::default())
    }

    async fn run_shell<'a, 'io>(
        &self,
        req: RunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        self.run(req, ctoken).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmodel::htaddr::Addr;
    use hmodel::htpkg::PkgBuf;
    use std::collections::{BTreeMap, HashMap};

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }
    fn map(pairs: &[(&str, Value)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }
    fn list(vs: Vec<Value>) -> Value {
        Value::List(vs)
    }

    fn spec(config: &[(&str, Value)]) -> hplugin::provider::TargetSpec {
        hplugin::provider::TargetSpec {
            addr: Addr::new(PkgBuf::from("auth"), "aws".to_string(), BTreeMap::new()),
            driver: DRIVER_NAME.to_string(),
            config: config
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect::<HashMap<_, _>>(),
            ..Default::default()
        }
    }

    #[test]
    fn the_acceptance_declaration_parses() {
        let def = parse_declaration(&spec(&[(
            "sources",
            list(vec![
                map(&[
                    ("kind", s("oidc")),
                    ("provider", s("github_actions")),
                    ("audience", s("sts.amazonaws.com")),
                    (
                        "present",
                        map(&[
                            ("files", map(&[("token", s("${id_token}"))])),
                            (
                                "env",
                                map(&[
                                    ("AWS_WEB_IDENTITY_TOKEN_FILE", s("${file:token}")),
                                    ("AWS_ROLE_ARN", s("arn:aws:iam::1:role/deployer")),
                                ]),
                            ),
                        ]),
                    ),
                ]),
                map(&[
                    ("kind", s("exec")),
                    (
                        "run",
                        list(vec![s("aws"), s("configure"), s("export-credentials")]),
                    ),
                    ("present", map(&[("helper", s("aws"))])),
                ]),
            ]),
        )]))
        .expect("parse");
        assert_eq!(def.sources.len(), 2);
        assert_eq!(def.sources[0].label(), "oidc(github_actions)");
        assert_eq!(def.sources[1].label(), "exec(aws)");
    }

    #[test]
    fn an_empty_chain_is_rejected() {
        let err = parse_declaration(&spec(&[("sources", list(vec![]))])).expect_err("must fail");
        assert!(format!("{err:#}").contains("at least one"), "{err:#}");
    }

    #[test]
    fn a_source_with_no_presentation_anywhere_is_rejected_at_the_declaration() {
        let err = parse_declaration(&spec(&[(
            "sources",
            list(vec![map(&[("kind", s("env")), ("names", s("TOKEN"))])]),
        )]))
        .expect_err("must fail");
        assert!(format!("{err:#}").contains("no presentation"), "{err:#}");
    }

    #[test]
    fn a_credential_level_presentation_covers_every_source() {
        let def = parse_declaration(&spec(&[
            (
                "sources",
                list(vec![
                    map(&[("kind", s("env")), ("names", s("TOKEN"))]),
                    s("//auth:other"),
                ]),
            ),
            ("present", map(&[("env", map(&[("T", s("${token}"))]))])),
            ("ttl", s("55m")),
        ]))
        .expect("parse");
        assert_eq!(def.ttl, Some(Duration::from_secs(3300)));
        for src in &def.sources {
            def.presentation_for(src)
                .expect("every source is covered by the credential's presentation");
        }
    }

    #[tokio::test]
    async fn the_def_hash_moves_with_the_declaration_and_never_holds_material() {
        use hplugin::driver::Driver as _;
        let ctoken = hcore::hasync::StdCancellationToken::new();
        let parse = async |ttl: &str| {
            Driver
                .parse(
                    ParseRequest {
                        request_id: "r".to_string(),
                        target_spec: Arc::new(spec(&[
                            (
                                "sources",
                                list(vec![map(&[("kind", s("env")), ("names", s("TOKEN"))])]),
                            ),
                            ("present", map(&[("env", map(&[("T", s("${token}"))]))])),
                            ("ttl", s(ttl)),
                        ])),
                    },
                    &ctoken,
                )
                .await
                .expect("parse")
                .target_def
                .hash
        };
        assert_ne!(parse("55m").await, parse("6h").await);
    }

    #[tokio::test]
    async fn a_declaration_declares_and_does_not_execute() {
        use hplugin::driver::Driver as _;
        let ctoken = hcore::hasync::StdCancellationToken::new();
        let res = Driver
            .parse(
                ParseRequest {
                    request_id: "r".to_string(),
                    target_spec: Arc::new(spec(&[
                        (
                            "sources",
                            list(vec![map(&[("kind", s("env")), ("names", s("TOKEN"))])]),
                        ),
                        ("present", map(&[("env", map(&[("T", s("${token}"))]))])),
                    ])),
                },
                &ctoken,
            )
            .await
            .expect("parse")
            .target_def;
        assert!(res.inputs.is_empty(), "a declaration has no inputs");
        assert!(res.outputs.is_empty(), "a declaration has no outputs");
        assert!(!res.cache.enabled, "material is not a build artifact");
    }
}
