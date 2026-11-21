//! The three check drivers: `buildfile.lint_check`, `buildfile.format_check`
//! and `buildfile.format`, built like the Go plugin's `format` /
//! `format_check`.
//!
//! Each reads only its sandbox: every file a check reads is an fs dep
//! (`//@heph/fs:file@f=<path>`), staged at its workspace-relative path. The
//! work runs under `hcore::blocking::run`. A failure's text
//! ([`report`](super::report)) goes to the run's log, so it reaches the
//! target's failure log; the error itself is the count line.
//!
//! All three are cached locally and never remotely (I7): a check costs
//! milliseconds.

use super::format::{Fix, check_file, fix_file};
use super::lint::{lint, read_under};
use super::loads::LoadWalk;
use super::report;
use super::targets::{CheckKind, format_keys, lint_keys};
use crate::htspec::Spec;
use anyhow::Context;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hdriver_support::run_log::RunLog;
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as TPath};
use hplugin::driver::targetdef::{CacheConfig, Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, RunRequest, TargetAddr,
};
use hproc::proc_exec::StreamId;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

/// The output group `build-format` declares its rewritten files under.
const FORMAT_OUTPUT_GROUP: &str = "build_files";

/// The log a host without `log_capture` keeps for a run.
const LOG_NAME: &str = "log.txt";

/// `build-lint-check`'s config, as the provider writes it.
#[derive(Spec)]
struct LintSpec {
    /// The package's load walk (JSON): every file the check reads and, per
    /// file, what each `load()` string resolved to.
    #[spec(required)]
    walk: String,
    /// `requireTypeAnnotations`.
    #[spec(rename = "requireTypeAnnotations")]
    require_type_annotations: bool,
    /// The BUILD-file patterns the walk resolved directory loads with.
    patterns: Vec<String>,
    /// The `heph.<plugin>.<fn>` names the checker knows, by plugin.
    #[spec(rename = "pluginFns")]
    plugin_fns: HashMap<String, Vec<String>>,
}

/// The format kinds' config, as the provider writes it.
#[derive(Spec)]
struct FormatSpec {
    /// The owned files, workspace-relative.
    files: Vec<String>,
    /// The formatter's indentation width.
    indent: u32,
}

/// `build-lint-check`'s def.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LintDef {
    walk: LoadWalk,
    require_type_annotations: bool,
    /// Sorted and deduplicated.
    plugin_fns: Vec<(String, String)>,
    /// Sorted and deduplicated.
    patterns: Vec<String>,
    /// Heph's version, `hcore::version::current()`.
    version: String,
}

/// The lint key (I4). File bytes reach it through the inputs; this is
/// everything else the verdict depends on:
///
/// - `requireTypeAnnotations`, which adds the annotation rule.
/// - Heph's version, which stands for the checker (heph's code, its starlark
///   fork). A dev build always reports `v0.0.0-dev`, so it can reuse its own
///   stale local verdict after a checker change (E2): local only (I7), so no
///   other machine reads it, and a stamped build is exact.
/// - The `(plugin, fn)` names, sorted and deduplicated. Plugin functions are
///   hashed by name only because the checker sees each as an opaque callable
///   taking and returning anything: a function's signature cannot move the
///   verdict, only whether `heph.<plugin>.<fn>` exists.
/// - The BUILD patterns, sorted and deduplicated: their order is not
///   semantic.
/// - The walk's edges: what each `load()` string resolved to.
impl Hash for LintDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.require_type_annotations.hash(state);
        self.version.hash(state);
        self.plugin_fns.hash(state);
        self.patterns.hash(state);
        self.walk.hash(state);
    }
}

/// The format kinds' def.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FormatDef {
    files: Vec<String>,
    indent: u32,
    /// Heph's version, `hcore::version::current()`.
    version: String,
}

/// The format key (I6): the owned files' bytes reach it through the inputs;
/// besides those, the indent and heph's version, which stands for the
/// formatter. As for lint, a dev build's `v0.0.0-dev` can reuse a stale local
/// verdict (E2). The file list is in the inputs already.
impl Hash for FormatDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.indent.hash(state);
        self.version.hash(state);
    }
}

fn hash_def(kind: CheckKind, addr: &hmodel::htaddr::Addr, def: &impl Hash) -> Vec<u8> {
    let mut h = DebugHasher::new(Xxh3Default::new(), || {
        format!("buildfile_{}_{}", kind.driver_local(), addr.format())
    });
    kind.name().hash(&mut h);
    def.hash(&mut h);
    format!("{:x}", h.finish()).into_bytes()
}

/// One hashed fs dep per file the check reads.
fn file_inputs(addrs: impl IntoIterator<Item = hmodel::htaddr::Addr>) -> Vec<Input> {
    addrs
        .into_iter()
        .enumerate()
        .map(|(i, addr)| Input {
            r#ref: TargetAddr {
                r#ref: addr,
                ..Default::default()
            },
            mode: InputMode::Standard,
            origin_id: format!("file_{i}"),
            annotations: BTreeMap::new(),
            hashed: true,
            runtime: true,
        })
        .collect()
}

fn target_def(
    req: &ParseRequest,
    raw_def: Arc<dyn hplugin::driver::targetdef::RawDef>,
    inputs: Vec<Input>,
    outputs: Vec<Output>,
    hash: Vec<u8>,
) -> TargetDef {
    TargetDef {
        addr: req.target_spec.addr.clone(),
        labels: req.target_spec.labels.clone(),
        raw_def,
        inputs,
        outputs,
        cache: CacheConfig::on(false),
        hash,
        ..Default::default()
    }
}

/// Parse a check's spec into its def, with `version` standing for heph's.
fn parse_at(req: &ParseRequest, kind: CheckKind, version: &str) -> anyhow::Result<ParseResponse> {
    match kind {
        CheckKind::LintCheck => parse_lint(req, version),
        kind => parse_format(req, kind, version),
    }
}

/// Test seam: `spec` parsed by its driver, as heph `version` would.
#[cfg(test)]
pub(crate) fn parse_spec(spec: hplugin::provider::TargetSpec, version: &str) -> TargetDef {
    let kind = CheckKind::ALL
        .into_iter()
        .find(|k| k.driver() == spec.driver)
        .expect("a check driver");
    let req = ParseRequest {
        request_id: "t".to_string(),
        target_spec: Arc::new(spec),
    };
    parse_at(&req, kind, version).expect("parses").target_def
}

fn parse_lint(req: &ParseRequest, version: &str) -> anyhow::Result<ParseResponse> {
    let spec = LintSpec::from(&req.target_spec.config).context("parsing build-lint-check")?;
    let walk: LoadWalk = serde_json::from_str(&spec.walk)
        .with_context(|| format!("decoding `{}`", lint_keys::WALK))?;
    let plugin_fns: BTreeSet<(String, String)> = spec
        .plugin_fns
        .into_iter()
        .flat_map(|(p, fns)| fns.into_iter().map(move |f| (p.clone(), f)))
        .collect();
    let patterns: BTreeSet<String> = spec.patterns.into_iter().collect();
    let def = LintDef {
        walk,
        require_type_annotations: spec.require_type_annotations,
        plugin_fns: plugin_fns.into_iter().collect(),
        patterns: patterns.into_iter().collect(),
        version: version.to_string(),
    };
    let hash = hash_def(CheckKind::LintCheck, &req.target_spec.addr, &def);
    let inputs = file_inputs(def.walk.input_addrs());
    Ok(ParseResponse {
        target_def: target_def(req, Arc::new(def), inputs, Vec::new(), hash),
    })
}

fn parse_format(
    req: &ParseRequest,
    kind: CheckKind,
    version: &str,
) -> anyhow::Result<ParseResponse> {
    let spec = FormatSpec::from(&req.target_spec.config)
        .with_context(|| format!("parsing {}", kind.name()))?;
    if spec.files.is_empty() {
        anyhow::bail!("`{}` names no file", format_keys::FILES);
    }
    let def = FormatDef {
        files: spec.files,
        indent: spec.indent,
        version: version.to_string(),
    };
    let hash = hash_def(kind, &req.target_spec.addr, &def);
    let inputs = file_inputs(def.files.iter().map(|f| hbuiltins::pluginfs::file_addr(f)));
    // `build-format` writes the owned files back in place: the same paths as
    // its fs inputs, which the engine's in-place guard pairs exactly. Nothing
    // is written back on failure: a failed run is an unparseable file, and
    // every file stays as it was.
    let outputs = if kind.has_codegen() {
        vec![Output {
            group: FORMAT_OUTPUT_GROUP.to_string(),
            paths: def
                .files
                .iter()
                .map(|f| TPath {
                    content: Content::FilePath(f.clone()),
                    codegen_tree: CodegenMode::InPlace {
                        write_on_failure: false,
                    },
                    collect: true,
                })
                .collect(),
        }]
    } else {
        Vec::new()
    };
    Ok(ParseResponse {
        target_def: target_def(req, Arc::new(def), inputs, outputs, hash),
    })
}

/// End a run: `failure`, when there is one, goes to the run's log (so it is
/// the target's failure log) and the run fails with its count line.
async fn finish(
    req: &mut RunRequest<'_, '_>,
    failure: Option<String>,
) -> anyhow::Result<ManagedRunResponse> {
    let Some(text) = failure else {
        return Ok(ManagedRunResponse { artifacts: vec![] });
    };
    let mut log = RunLog::from_request(req, LOG_NAME);
    log.write(StreamId::Stderr, format!("{text}\n").as_bytes())
        .await;
    let summary = text
        .lines()
        .find(|l| l.contains(" in //"))
        .unwrap_or("check failed")
        .to_string();
    let err = anyhow::anyhow!(summary);
    let err = match log.log_artifact() {
        Some(artifact) => hplugin::error::with_failure_artifacts(err, vec![artifact]),
        None => err,
    };
    hdriver_support::driver_managed::echo_failure_log(req, &err).await;
    Err(err)
}

/// Run `f` off the async workers.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    hcore::blocking::run(f).await
}

fn run_lint(def: &LintDef, ws: &Path) -> anyhow::Result<Option<String>> {
    let findings = lint(
        &def.walk,
        &mut read_under(ws),
        &def.plugin_fns,
        def.require_type_annotations,
    )?;
    Ok(report::lint_failure(&def.walk.pkg, &findings))
}

fn run_format_check(def: &FormatDef, pkg: &str, ws: &Path) -> anyhow::Result<Option<String>> {
    let config = hxstarlark_fmt::Config {
        indent_size: def.indent as usize,
    };
    let mut findings = Vec::new();
    for file in &def.files {
        let src = read_under(ws)(file)?;
        findings.extend(check_file(file, &src, config)?);
    }
    Ok(report::format_failure(pkg, &findings))
}

/// Rewrite every unformatted owned file in the sandbox. An unparseable file
/// fails the run, named; nothing is written back then.
fn run_format(def: &FormatDef, ws: &Path) -> anyhow::Result<Option<String>> {
    let config = hxstarlark_fmt::Config {
        indent_size: def.indent as usize,
    };
    let mut errors = Vec::new();
    for file in &def.files {
        let src = read_under(ws)(file)?;
        let fix = fix_file(file, &src, config);
        match &fix {
            Fix::Unchanged => {}
            Fix::Rewrite(_) => {
                let path = ws.join(file);
                // Replace rather than truncate: a staged input may share its
                // inode with a stage entry.
                std::fs::remove_file(&path)
                    .and_then(|()| std::fs::write(&path, fix.bytes(&src)))
                    .with_context(|| format!("writing {file}"))?;
            }
            Fix::Unparseable(e) => errors.push(format!("{file}: {e:#}")),
        }
    }
    Ok((!errors.is_empty()).then(|| errors.join("\n")))
}

/// A check driver: one per [`CheckKind`].
#[derive(Debug)]
pub(crate) struct CheckDriver {
    kind: CheckKind,
}

impl CheckDriver {
    pub(crate) fn new(kind: CheckKind) -> Self {
        Self { kind }
    }
}

#[async_trait]
impl ManagedDriver for CheckDriver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: self.kind.driver_local().to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        match self.kind {
            CheckKind::LintCheck => LintSpec::schema(),
            CheckKind::FormatCheck | CheckKind::Format => FormatSpec::schema(),
        }
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        parse_at(&req, self.kind, hcore::version::current())
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
        mut req: ManagedRunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let ws = req.sandbox_ws_dir.clone();
        let pkg = req.request.target.addr.package.as_str().to_string();
        let failure = match self.kind {
            CheckKind::LintCheck => {
                let def = req.request.target.def_de::<LintDef>().clone();
                blocking(move || run_lint(&def, &ws)).await?
            }
            CheckKind::FormatCheck => {
                let def = req.request.target.def_de::<FormatDef>().clone();
                blocking(move || run_format_check(&def, &pkg, &ws)).await?
            }
            CheckKind::Format => {
                let def = req.request.target.def_de::<FormatDef>().clone();
                blocking(move || run_format(&def, &ws)).await?
            }
        };
        finish(&mut req.request, failure).await
    }
}
