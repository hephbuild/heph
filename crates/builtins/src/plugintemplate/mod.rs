//! The `template` driver: render a declared template file with declared
//! variables, and nothing else.
//!
//! Recipes reach for `sed -i` and `envsubst` to fill in a config file, which
//! puts the substitution in a shell — where it depends on the host's `sed`, on
//! quoting, and on whatever else is in scope. A template is a better shape for
//! that job, and it does not need a subprocess at all: the inputs are declared,
//! the output is declared, and the rendering happens in-process.
//!
//! The rendering itself, and the two safety properties that go with it, live in
//! `crates/template` — shared with the `tmpl` applet, because configuration that
//! exists in two places drifts.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hcore::htvalue::signature::ParamType;
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, RunRequest, RunResponse, TargetAddr, outputartifact,
    targetdef::{
        CacheConfig, Input, InputMode, Output, TargetDef,
        path::{CodegenMode, Content, Path},
    },
};
use hplugin::htspec::Spec;
use htemplate::{TEMPLATE_FORMAT_VERSION, render};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

pub const DRIVER_NAME: &str = "template";

/// Config for a `template` target. `#[derive(Spec)]` provides the parser and
/// the LSP schema.
#[derive(Spec)]
struct TemplateSpec {
    /// Address of the target whose single output file is the template (required).
    #[spec(required)]
    src: String,
    /// Output filename; defaults to the target name.
    #[spec(ty = ParamType::String)]
    out: Option<String>,
    /// Variables the template can reference by name.
    vars: HashMap<String, String>,
    /// Mark the rendered file executable.
    executable: bool,
}

#[derive(serde::Serialize)]
struct TemplateDef {
    out: String,
    /// Ordered, because a `HashMap`'s iteration order is randomized per process
    /// and this is hashed: an unordered fold would give the same target a
    /// different def hash on every run and never hit cache.
    vars: BTreeMap<String, String>,
    executable: bool,
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
        TemplateSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let spec = TemplateSpec::from(&req.target_spec.config).context("parse template config")?;
        let out_rel = spec
            .out
            .unwrap_or_else(|| req.target_spec.addr.name.clone());
        let executable = spec.executable;
        let vars: BTreeMap<String, String> = spec.vars.into_iter().collect();

        let pkg = req.target_spec.addr.package.clone();
        let src = TargetAddr::parse(&spec.src, &pkg)
            .with_context(|| format!("parsing template src '{}'", spec.src))?;

        let out = if pkg.as_str().is_empty() {
            out_rel
        } else {
            format!("{}/{out_rel}", pkg.as_str())
        };

        let mut h = Xxh3::new();
        h.update(&TEMPLATE_FORMAT_VERSION.to_le_bytes());
        h.update(req.target_spec.addr.format().as_bytes());
        h.update(out.as_bytes());
        h.update(&[executable as u8]);
        // The template's own bytes reach the key through the dep's hashout, so
        // only the address goes in here.
        h.update(src.r#ref.format().as_bytes());
        for (k, v) in &vars {
            h.update(k.as_bytes());
            h.update(b"=");
            h.update(v.as_bytes());
            h.update(b"\0");
        }
        let hash = format!("{:016x}", h.digest()).into_bytes();

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(TemplateDef {
                    out: out.clone(),
                    vars,
                    executable,
                }),
                inputs: vec![Input {
                    r#ref: src,
                    mode: InputMode::Standard,
                    origin_id: "template:src".to_string(),
                    annotations: BTreeMap::new(),
                    hashed: true,
                    runtime: true,
                }],
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![Path {
                        content: Content::FilePath(out),
                        codegen_tree: CodegenMode::None,
                        collect: false,
                    }],
                }],
                support_files: vec![],
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
        req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        let def = req.target.def::<TemplateDef>();
        let template = read_single_file(&req)?;
        let rendered = render(&template, &def.vars)
            .with_context(|| format!("template {}", req.target.addr.format()))?;
        let data = rendered.into_bytes();

        let mut h = Xxh3::new();
        h.update(&data);
        h.update(def.out.as_bytes());
        h.update(&[def.executable as u8]);
        let hashout = format!("{:x}", h.digest());

        let name = std::path::Path::new(&def.out)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| def.out.clone());

        Ok(RunResponse {
            artifacts: vec![outputartifact::OutputArtifact {
                group: String::new(),
                name,
                r#type: outputartifact::Type::Output,
                content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                    data,
                    path: def.out.clone(),
                    x: def.executable,
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
        anyhow::bail!("run_shell not implemented for the template driver")
    }
}

/// The template's text: the one file the `src` dep produced.
///
/// Requiring exactly one is the point. A `src` that resolves to several files
/// has no defensible answer — picking the first would depend on walk order —
/// so it is refused with the count, and a `src` that produced none names the
/// address that was supposed to produce it.
fn read_single_file(req: &RunRequest<'_, '_>) -> anyhow::Result<String> {
    use std::io::Read as _;

    let mut found: Option<(std::path::PathBuf, String)> = None;
    let mut extra: Vec<std::path::PathBuf> = Vec::new();
    for input in &req.inputs {
        for entry in input
            .artifact
            .content
            .walk()
            .with_context(|| format!("reading template from '{}'", input.source_addr))?
        {
            let entry = entry
                .with_context(|| format!("reading template entry from '{}'", input.source_addr))?;
            if let hcore::hartifactcontent::WalkEntryKind::File { mut data, .. } = entry.kind {
                if found.is_some() {
                    extra.push(entry.path);
                    continue;
                }
                let mut text = String::new();
                data.read_to_string(&mut text).with_context(|| {
                    format!("template {:?} is not valid UTF-8", entry.path.display())
                })?;
                found = Some((entry.path, text));
            }
        }
    }

    match found {
        Some((path, text)) if extra.is_empty() => {
            let _ = path;
            Ok(text)
        }
        Some((path, _)) => anyhow::bail!(
            "`src` must produce exactly one file, but it produced {} — {:?} and {} more. \
             Point `src` at a single file target.",
            extra.len() + 1,
            path.display(),
            extra.len(),
        ),
        None => anyhow::bail!(
            "`src` produced no file to render — check that the address names a target \
             with a file output"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::parse_addr;
    use hplugin::driver::Driver as EDriver;
    use hplugin::provider::TargetSpec;

    fn ctoken() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    fn config(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn vars(pairs: &[(&str, &str)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), s(v)))
                .collect(),
        )
    }

    async fn parse(addr: &str, cfg: HashMap<String, Value>) -> anyhow::Result<TargetDef> {
        Ok(Driver
            .parse(
                ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: Arc::new(TargetSpec {
                        addr: parse_addr(addr).unwrap(),
                        driver: DRIVER_NAME.to_string(),
                        config: cfg,
                        ..Default::default()
                    }),
                },
                &ctoken(),
            )
            .await?
            .target_def)
    }

    // ---- rendering ----

    // ---- parse / cache key ----

    #[tokio::test]
    async fn src_becomes_a_hashed_input() {
        // The template's bytes must reach the consumer's key, which is what
        // makes editing the template rebuild the rendered file.
        let def = parse(
            "//pkg:conf",
            config(&[("src", s(":conf.j2")), ("vars", vars(&[("a", "1")]))]),
        )
        .await
        .expect("parse");
        assert_eq!(def.inputs.len(), 1);
        assert_eq!(def.inputs[0].r#ref.r#ref.format(), "//pkg:conf.j2");
        assert!(def.inputs[0].hashed, "the template must be hashed");
    }

    #[tokio::test]
    async fn out_defaults_to_the_target_name_and_is_package_scoped() {
        let def = parse("//pkg:conf", config(&[("src", s(":conf.j2"))]))
            .await
            .expect("parse");
        let paths = &def.outputs[0].paths;
        assert!(matches!(&paths[0].content, Content::FilePath(p) if p == "pkg/conf"));
    }

    #[tokio::test]
    async fn changing_a_var_moves_the_def_hash() {
        // Variables are rendered into the output but are not an input's
        // hashout, so nothing else would invalidate the target.
        let a = parse(
            "//pkg:conf",
            config(&[("src", s(":conf.j2")), ("vars", vars(&[("port", "80")]))]),
        )
        .await
        .expect("parse");
        let b = parse(
            "//pkg:conf",
            config(&[("src", s(":conf.j2")), ("vars", vars(&[("port", "443")]))]),
        )
        .await
        .expect("parse");
        assert_ne!(a.hash, b.hash);
    }

    #[tokio::test]
    async fn the_def_hash_does_not_depend_on_var_iteration_order() {
        // `vars` arrives as a `HashMap`, whose order is randomized per process.
        // Hashing it unordered would give the same target a different key on
        // every run and never hit cache.
        let one = parse(
            "//pkg:conf",
            config(&[
                ("src", s(":conf.j2")),
                ("vars", vars(&[("a", "1"), ("b", "2"), ("c", "3")])),
            ]),
        )
        .await
        .expect("parse");
        let two = parse(
            "//pkg:conf",
            config(&[
                ("src", s(":conf.j2")),
                ("vars", vars(&[("c", "3"), ("a", "1"), ("b", "2")])),
            ]),
        )
        .await
        .expect("parse");
        assert_eq!(one.hash, two.hash);
    }

    #[tokio::test]
    async fn a_missing_src_is_rejected_at_parse() {
        let err = match parse("//pkg:conf", config(&[])).await {
            Err(e) => e,
            Ok(_) => panic!("a template with no `src` must not parse"),
        };
        assert!(format!("{err:#}").contains("src"), "{err:#}");
    }
}
