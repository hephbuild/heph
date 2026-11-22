//! The three check drivers: `buildfile.lint_check`, `buildfile.format_check`
//! and `buildfile.format`, built like the Go plugin's `format` /
//! `format_check`.
//!
//! Each reads only its sandbox: every file a check reads is an fs dep
//! (`//@heph/fs:file@f=<path>`), staged at its workspace-relative path. The
//! work runs under `hcore::blocking::run`. A failure's text
//! ([`report`](super::report)) goes to the run's log, so it reaches the
//! target's failure log; the error itself is the count line, with a pointer
//! to `heph inspect failure`.
//!
//! All three are cached locally and never remotely (I7): a check costs
//! milliseconds.

use super::format::{Fix, check_file, fix_file};
use super::lint::{lint, read_under};
use super::loads::LoadWalk;
use super::report::{self, Failure};
use super::targets::{CheckKind, format_keys, lint_keys, plugin_fns_digest};
use crate::htspec::Spec;
use crate::pluginbuildfile::provider::{DriverSource, FunctionSource};
use crate::pluginbuildfile::types::DriverKwargs;
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
use std::collections::{BTreeMap, BTreeSet};
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
    /// [`plugin_fns_digest`] of the `heph.<plugin>.<fn>` names the checker
    /// knows; the driver reads the names from the function registry.
    #[spec(rename = "pluginFnsDigest", required)]
    plugin_fns_digest: String,
    /// [`DriverKwargs::digest`] of the `heph.types.<driver>.Kwargs` the checker
    /// sees; the driver rebuilds them from the engine's driver schemas.
    #[spec(rename = "driverKwargsDigest", required)]
    driver_kwargs_digest: String,
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
    /// [`plugin_fns_digest`] of the names: sorted and deduplicated, by name
    /// only.
    plugin_fns_digest: String,
    /// [`DriverKwargs::digest`] of every driver's `Kwargs`.
    driver_kwargs_digest: String,
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
/// - The `(plugin, fn)` names, sorted and deduplicated, as one digest
///   ([`plugin_fns_digest`]). Plugin functions are hashed by name only
///   because the checker sees each as an opaque callable taking and returning
///   anything: a function's signature cannot move the verdict, only whether
///   `heph.<plugin>.<fn>` exists. The run checks that the registry it reads
///   the names from has this digest.
/// - Every driver's `heph.types.<driver>.Kwargs`, as one digest
///   ([`DriverKwargs::digest`]): each key and its Starlark type, so a schema
///   change the checker sees re-runs every package's lint, and a doc or
///   `required` change does not. The run checks the kwargs it rebuilds have
///   this digest.
/// - The BUILD patterns, sorted and deduplicated: their order is not
///   semantic.
/// - The walk's edges: what each `load()` string resolved to.
impl Hash for LintDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.require_type_annotations.hash(state);
        self.version.hash(state);
        self.plugin_fns_digest.hash(state);
        self.driver_kwargs_digest.hash(state);
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
    let patterns: BTreeSet<String> = spec.patterns.into_iter().collect();
    let def = LintDef {
        walk,
        require_type_annotations: spec.require_type_annotations,
        plugin_fns_digest: spec.plugin_fns_digest,
        driver_kwargs_digest: spec.driver_kwargs_digest,
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

/// End a run: `failure`, when there is one, has its log text written to the
/// run's log (so it is the target's failure log), and the run fails with its
/// summary line plus where to find that log. The pointer is on the error
/// only, never in the log, whose text is the contract.
async fn finish(
    req: &mut RunRequest<'_, '_>,
    failure: Option<Failure>,
) -> anyhow::Result<ManagedRunResponse> {
    let Some(Failure { log: text, summary }) = failure else {
        return Ok(ManagedRunResponse { artifacts: vec![] });
    };
    let mut log = RunLog::from_request(req, LOG_NAME);
    log.write(StreamId::Stderr, format!("{text}\n").as_bytes())
        .await;
    let err = anyhow::anyhow!(
        "{summary} (see heph inspect failure {})",
        req.target.addr.format()
    );
    let err = match log.log_artifact() {
        Some(artifact) => hplugin::error::with_failure_artifacts(err, vec![artifact]),
        None => err,
    };
    hdriver_support::driver_managed::echo_failure_log(req, &err).await;
    Err(err)
}

/// The `(plugin, fn)` names in `registry`, checked against the digest the
/// def was keyed on: a verdict is only ever computed against the names its
/// key covers.
fn registry_fns<'r>(
    registry: &'r hplugin::function::FunctionRegistry,
    digest: &str,
) -> anyhow::Result<Vec<(&'r str, &'r str)>> {
    let names: Vec<(&str, &str)> = registry.iter().map(|(p, f, _)| (p, f)).collect();
    let actual = plugin_fns_digest(names.iter().copied());
    if actual != digest {
        anyhow::bail!(
            "the plugin functions changed between the check's parse and its run \
             (digest {digest}, now {actual}); run it again"
        );
    }
    Ok(names)
}

/// `kwargs`, checked against the digest the def was keyed on, as
/// [`registry_fns`] checks the function names.
///
/// Today the provider and this driver share one `DriverSource`, built once, so
/// the check compares a value with itself and a re-read can never trip it. It
/// guards what would make the two differ: a future split of the source (the
/// provider and the driver each building their own), or a def handed from one
/// engine to another whose drivers differ.
fn checked_kwargs<'k>(kwargs: &'k DriverKwargs, digest: &str) -> anyhow::Result<&'k DriverKwargs> {
    let actual = kwargs.digest();
    if actual != digest {
        anyhow::bail!(
            "the driver schemas changed between the check's parse and its run \
             (`heph.types.<driver>.Kwargs` digest {digest}, now {actual}); run it again"
        );
    }
    Ok(kwargs)
}

fn run_lint(
    def: &LintDef,
    ws: &Path,
    plugin_fns: &[(&str, &str)],
    kwargs: &DriverKwargs,
) -> anyhow::Result<Option<Failure>> {
    let findings = lint(
        &def.walk,
        &mut read_under(ws),
        plugin_fns,
        kwargs,
        def.require_type_annotations,
    )?;
    Ok(report::lint_failure(&def.walk.pkg, &findings))
}

fn run_format_check(def: &FormatDef, pkg: &str, ws: &Path) -> anyhow::Result<Option<Failure>> {
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

/// Rewrite every unformatted owned file in the sandbox. A file that does not
/// parse, or whose formatted text would change if formatted again, fails the
/// run, named; nothing is written back then. The engine's in-place write-back
/// trusts the formatter to be idempotent (I5), so this is where a formatter
/// bug that breaks it stops, rather than flipping a file on every run.
fn run_format(def: &FormatDef, pkg: &str, ws: &Path) -> anyhow::Result<Option<Failure>> {
    let config = hxstarlark_fmt::Config {
        indent_size: def.indent as usize,
    };
    let mut log = Vec::new();
    let mut unparseable = Vec::new();
    let mut unstable = Vec::new();
    for file in &def.files {
        let src = read_under(ws)(file)?;
        match fix_file(file, &src, config) {
            Fix::Unchanged => {}
            Fix::Rewrite(out) => {
                if !matches!(fix_file(file, &out, config), Fix::Unchanged) {
                    log.push(format!(
                        "{file}: formatting it twice gives different output; this is a \
                         formatter bug, the file is left as it is"
                    ));
                    unstable.push(file.as_str());
                    continue;
                }
                rewrite(ws, file, &out)?;
            }
            Fix::Unparseable(e) => {
                log.push(format!("{file}: {e:#}"));
                unparseable.push(file.as_str());
            }
        }
    }
    let summary = match (unparseable.is_empty(), unstable.is_empty()) {
        (true, true) => return Ok(None),
        (false, _) => format!(
            "cannot format //{pkg}: {} does not parse; nothing was written",
            unparseable.join(", ")
        ),
        (true, false) => format!(
            "cannot format //{pkg}: the formatter is not idempotent on {}; nothing was written",
            unstable.join(", ")
        ),
    };
    Ok(Some(Failure {
        log: log.join("\n"),
        summary,
    }))
}

/// Replace the sandbox file `rel` with `text`, keeping its mode: the
/// in-place write-back copies the sandbox file's mode onto the user's, so a
/// dropped exec bit would reach the workspace.
fn rewrite(ws: &Path, rel: &str, text: &str) -> anyhow::Result<()> {
    let path = ws.join(rel);
    let perms = std::fs::metadata(&path)
        .with_context(|| format!("reading the mode of {rel}"))?
        .permissions();
    // Replace rather than truncate: a staged input may share its inode with a
    // stage entry.
    std::fs::remove_file(&path)
        .and_then(|()| std::fs::write(&path, text))
        .with_context(|| format!("writing {rel}"))?;
    std::fs::set_permissions(&path, perms).with_context(|| format!("setting the mode of {rel}"))
}

/// A check driver: one per [`CheckKind`].
pub(crate) struct CheckDriver {
    kind: CheckKind,
    /// Where `build-lint-check` reads the `heph.<plugin>.<fn>` names from:
    /// the same source as the provider's, whose spec carries only their
    /// digest.
    functions: FunctionSource,
    /// Where it reads the driver schemas `heph.types.<driver>.Kwargs` is
    /// built from: the provider's source, so both build them once.
    drivers: DriverSource,
}

impl std::fmt::Debug for CheckDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckDriver")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl CheckDriver {
    pub(crate) fn new(kind: CheckKind, functions: FunctionSource, drivers: DriverSource) -> Self {
        Self {
            kind,
            functions,
            drivers,
        }
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
                let registry = self
                    .functions
                    .registry()
                    .context("reading the `heph.<plugin>.<fn>` names for build-lint-check")?;
                let kwargs = self
                    .drivers
                    .kwargs()
                    .context("reading `heph.types.<driver>.Kwargs` for build-lint-check")?;
                hcore::blocking::run(move || {
                    let fns = registry_fns(&registry, &def.plugin_fns_digest)?;
                    let kwargs = checked_kwargs(&kwargs, &def.driver_kwargs_digest)?;
                    run_lint(&def, &ws, &fns, kwargs)
                })
                .await?
            }
            CheckKind::FormatCheck => {
                let def = req.request.target.def_de::<FormatDef>().clone();
                hcore::blocking::run(move || run_format_check(&def, &pkg, &ws)).await?
            }
            CheckKind::Format => {
                let def = req.request.target.def_de::<FormatDef>().clone();
                hcore::blocking::run(move || run_format(&def, &pkg, &ws)).await?
            }
        };
        finish(&mut req.request, failure).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// `build-format` keeps the mode of the file it rewrites: the in-place
    /// write-back copies the sandbox file's mode onto the user's file.
    #[test]
    fn format_keeps_file_mode() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("pkg")).expect("mkdir");
        for (file, mode) in [("pkg/BUILD", 0o755), ("pkg/more.BUILD", 0o644)] {
            let path = ws.join(file);
            std::fs::write(&path, "x    =  1\n").expect("write");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        }
        let def = FormatDef {
            files: vec!["pkg/BUILD".into(), "pkg/more.BUILD".into()],
            indent: 4,
            version: "v1".into(),
        };
        assert_eq!(run_format(&def, "pkg", ws).expect("runs"), None);
        for (file, mode) in [("pkg/BUILD", 0o755), ("pkg/more.BUILD", 0o644)] {
            let path = ws.join(file);
            assert_eq!(std::fs::read_to_string(&path).expect("read"), "x = 1\n");
            let got = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
            assert_eq!(got, mode, "{file}: {got:o}");
        }
    }

    /// An unparseable file fails the run with an error line naming it, and
    /// the log carries the parse error.
    #[test]
    fn format_unparseable_names_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("pkg")).expect("mkdir");
        std::fs::write(ws.join("pkg/BUILD"), "x    =  1\n").expect("write");
        std::fs::write(ws.join("pkg/bad.BUILD"), "x = (\n").expect("write");
        let def = FormatDef {
            files: vec!["pkg/BUILD".into(), "pkg/bad.BUILD".into()],
            indent: 4,
            version: "v1".into(),
        };
        let failure = run_format(&def, "pkg", ws).expect("runs").expect("fails");
        assert_eq!(
            failure.summary,
            "cannot format //pkg: pkg/bad.BUILD does not parse; nothing was written"
        );
        assert!(
            failure.log.starts_with("pkg/bad.BUILD: "),
            "{}",
            failure.log
        );
    }

    /// The lint run reads the names from the registry, and refuses one whose
    /// digest is not the one the def was keyed on.
    #[test]
    fn registry_digest_checked() {
        let registry = hplugin::function::FunctionRegistry::default();
        let empty = plugin_fns_digest(std::iter::empty());
        assert_eq!(registry_fns(&registry, &empty).expect("matches"), []);
        let other = plugin_fns_digest([("p", "f")]);
        let err = registry_fns(&registry, &other).expect_err("mismatch");
        assert!(format!("{err:#}").contains("changed"), "{err:#}");
    }

    /// The lint run refuses driver kwargs whose digest is not the def's.
    #[test]
    fn kwargs_digest_checked() {
        let none = DriverKwargs::default();
        let digest = none.digest().to_string();
        checked_kwargs(&none, &digest).expect("matches");
        let some = crate::pluginbuildfile::types::tests::test_kwargs();
        let err = checked_kwargs(&some, &digest).expect_err("mismatch");
        assert!(
            format!("{err:#}").contains("driver schemas changed"),
            "{err:#}"
        );
    }
}
