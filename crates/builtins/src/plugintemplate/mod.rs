//! The `template` driver: render a declared template file with declared
//! variables, and nothing else.
//!
//! Recipes reach for `sed -i` and `envsubst` to fill in a config file, which
//! puts the substitution in a shell — where it depends on the host's `sed`, on
//! quoting, and on whatever else is in scope. A template is a better shape for
//! that job, and it does not need a subprocess at all: the inputs are declared,
//! the output is declared, and the rendering happens in-process.
//!
//! Two properties are load-bearing and worth stating rather than discovering:
//!
//! * **A template cannot read an undeclared file.** The environment is built
//!   with no loader, so `{% include %}` and `{% import %}` have nothing to
//!   resolve against and fail rather than reaching into the filesystem.
//! * **An undefined variable is an error, not an empty string.** Strict
//!   undefined behaviour turns a typo into a message naming the variable
//!   instead of a silently truncated config file.

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
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

pub const DRIVER_NAME: &str = "template";

/// Bumped when rendering changes — a minijinja upgrade that alters output, or a
/// change to the environment configured in [`render`].
///
/// In the def hash for the same reason the exec driver hashes its own format
/// version: the rendered bytes are a function of the renderer as well as of the
/// template, and a renderer that moved without moving the key would keep
/// serving the old rendering forever.
const TEMPLATE_FORMAT_VERSION: u32 = 1;

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

/// Render `template` with `vars`.
///
/// The environment is deliberately bare: no loader (so `include`/`import`
/// cannot reach the filesystem), and strict undefined behaviour (so a typo'd
/// variable is an error naming it, not an empty string in a config file).
fn render(template: &str, vars: &BTreeMap<String, String>) -> anyhow::Result<String> {
    let mut env = minijinja::Environment::new();
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
    env.add_template("template", template)
        .context("compile template")?;
    let tmpl = env.get_template("template").context("load template")?;

    // Checked before rendering rather than left to strict mode, because
    // minijinja's own message for this is "undefined value (in template:1)" —
    // it says that something is missing without saying what, which is the
    // single least useful thing an error about a typo can do. Naming the
    // variable, and what *was* supplied, turns it into a fix.
    let missing: Vec<String> = {
        // `undeclared_variables` reports attribute access as a dotted path —
        // `{{ features.split(",") }}` comes back as `features.split` — so the
        // check is on the root segment. Comparing the whole path would reject
        // every template that calls a method or reads a field.
        let mut m: Vec<String> = tmpl
            .undeclared_variables(true)
            .into_iter()
            .map(|name| name.split('.').next().unwrap_or(&name).to_string())
            .filter(|root| !vars.contains_key(root))
            .collect();
        m.sort_unstable();
        m.dedup();
        m
    };
    if !missing.is_empty() {
        let supplied: Vec<&str> = vars.keys().map(String::as_str).collect();
        anyhow::bail!(
            "template uses {} that `vars` does not supply: {}. Supplied: {}",
            if missing.len() == 1 {
                "a variable"
            } else {
                "variables"
            },
            missing.join(", "),
            if supplied.is_empty() {
                "nothing".to_string()
            } else {
                supplied.join(", ")
            },
        );
    }

    tmpl.render(minijinja::Value::from_serialize(vars))
        .context("render template")
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

    #[test]
    fn renders_declared_variables() {
        let vars = BTreeMap::from([
            ("name".to_string(), "world".to_string()),
            ("n".to_string(), "3".to_string()),
        ]);
        let out = render("hello {{ name }} x{{ n }}", &vars).expect("render");
        assert_eq!(out, "hello world x3");
    }

    #[test]
    fn an_undefined_variable_is_an_error_not_an_empty_string() {
        // The whole reason to prefer this over `sed`: a typo must not silently
        // produce a config file with a hole in it.
        let err = render("port = {{ prot }}", &BTreeMap::new()).expect_err("must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("prot"),
            "the error must name the variable: {msg}"
        );
    }

    #[test]
    fn attribute_access_checks_the_root_variable() {
        // minijinja reports `cfg.port` as the undeclared name. Checking the
        // whole path would reject every template that reads a field, so only
        // the root is compared — `cfg` here, which *is* supplied.
        let vars = BTreeMap::from([("cfg".to_string(), "x".to_string())]);
        let err = render("{{ cfg.port }}", &vars).expect_err("strict mode still rejects it");
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("does not supply"),
            "the root variable is supplied, so this must not be reported as missing: {msg}"
        );
    }

    #[test]
    fn a_loop_variable_is_not_reported_as_missing() {
        // `item` is bound by the loop, not supplied by `vars`; reporting it
        // would make the check useless on any template with a loop.
        let vars = BTreeMap::from([("items".to_string(), "ab".to_string())]);
        let out = render("{% for item in items %}[{{ item }}]{% endfor %}", &vars).expect("render");
        assert_eq!(out, "[a][b]");
    }

    #[test]
    fn a_template_cannot_reach_the_filesystem() {
        // No loader is configured, so `include` has nothing to resolve against.
        // A template that could read an undeclared file would be a hole in the
        // sandbox, not a feature.
        let err = render("{% include '/etc/passwd' %}", &BTreeMap::new()).expect_err("must fail");
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("root:"),
            "a template must never read a host file: {msg}"
        );
    }

    #[test]
    fn rendering_is_deterministic_across_var_order() {
        let a = BTreeMap::from([
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
        ]);
        let b: BTreeMap<String, String> = a
            .iter()
            .rev()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        assert_eq!(
            render("{{ a }}{{ b }}", &a).unwrap(),
            render("{{ a }}{{ b }}", &b).unwrap()
        );
    }

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
