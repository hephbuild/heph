//! `go_format` / `go_format_check` — per-package Go formatting.
//!
//! Formatting is a whole-file rewrite (gofmt/gofumpt/goimports), not go/analysis,
//! but it reuses the same hermetic `heph-govet` binary in its `-format` mode so
//! there is no second expensive tool build. Two targets, mirroring lint:
//!
//!   - `format` (`go_format`) — the fixer: rewrites the sources and declares them
//!     `codegen=in_place`, so the engine writes them back over the tracked files;
//!   - `format-check` (`go_format_check`) — runs `-format -check`, failing the
//!     build when any file is not formatted (it writes nothing).
//!
//! Which formatters run + their settings come from the workspace `.golangci.yml`
//! (`formatters:` block), passed via `HEPH_GOVET_GOLANGCI_CONFIG` exactly like
//! the lint config. Absent → gofmt.

use anyhow::Context;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as TPath};
use hplugin::driver::targetdef::{CacheConfig, Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::Spec;
use hproc::proc_exec;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

use crate::plugingo::deps::{DepGroups, dep_origin};
use crate::plugingo::listed;

/// Dep group carrying the `heph-govet` binary (shared with lint; staged read-only).
const GOVET_TOOL_GROUP: &str = "govet_tool";

/// Bump to invalidate cached format results when the cfg/tool contract changes
/// in a way the tool's own input hash does not already capture.
const GO_FORMAT_FORMAT_VERSION: u32 = 1;

/// The output group the fix driver declares its rewritten sources under.
const SRC_OUTPUT_GROUP: &str = "src";

/// The glob naming a package's `.go` sources: every `.go` file directly in
/// `pkg`'s directory. One function builds both the `""` dep group's fs glob
/// ([`format_sources_addr`]) and the fix driver's `in_place` output, so the
/// engine's in-place guard pairs the input with the output by addr.
///
/// `pkg` is a literal path, so its glob metacharacters are escaped with wax's
/// own [`wax::escape`] — the fs walker compiles this pattern with the same wax.
/// `wax::escape` leaves `\` alone (to wax it is the escape character, not a
/// metacharacter), so a `\` in `pkg` is escaped first; otherwise it would escape
/// whatever follows it instead of matching itself.
pub fn format_sources_pattern(pkg: &str) -> String {
    if pkg.is_empty() {
        return "*.go".to_string();
    }
    let backslashes = pkg.replace('\\', "\\\\");
    format!("{}/*.go", wax::escape(&backslashes))
}

/// The `@heph/fs` glob target staging a package's `.go` sources. Going through
/// the fs driver, rather than listing the directory in the provider, gives
/// `format` the fs driver's answer about generated files: a `codegen = "copy"`
/// output written back into the package is not a source and is not staged.
pub fn format_sources_addr(pkg: &str) -> Addr {
    hbuiltins::pluginfs::glob_addr(&format_sources_pattern(pkg), &[])
}

/// Locate the single staged `heph-govet` binary and the optional config path.
fn govet_bin_and_config(
    req: &ManagedRunRequest<'_, '_>,
    deps: &DepGroups,
) -> anyhow::Result<(String, HashMap<String, String>)> {
    let bin = deps
        .staged_paths(req, GOVET_TOOL_GROUP)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("format: heph-govet tool not staged"))?;
    let mut env: HashMap<String, String> = HashMap::new();
    if let Some(cfg) = deps.staged_paths(req, "config")?.into_iter().next() {
        env.insert("HEPH_GOVET_GOLANGCI_CONFIG".to_string(), cfg);
    }
    Ok((bin, env))
}

/// Write the file list to a response file in the sandbox and return the
/// `@<path>` argument for it. Passing thousands of paths on argv would overflow
/// the OS limit (ARG_MAX); heph-govet expands `@file` (one path per line). The
/// list file lives beside the sources but is never a declared output, so it is
/// not collected.
fn arg_file(pkg_dir: &std::path::Path, files: &[String]) -> anyhow::Result<OsString> {
    let path = pkg_dir.join(".heph-format-files");
    std::fs::write(&path, files.join("\n"))
        .with_context(|| format!("write format arg file {path:?}"))?;
    let mut arg = OsString::from("@");
    arg.push(path.as_os_str());
    Ok(arg)
}

/// Run `heph-govet` with `args`, returning its whole output. Never fails on a
/// non-zero exit — the caller interprets the code.
async fn exec_govet(
    bin: &str,
    args: Vec<OsString>,
    env: &HashMap<String, String>,
    cwd: &std::path::Path,
    runner: hexecrunner::RunnerRef<'_>,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<std::process::Output> {
    let env_pairs: Vec<(OsString, OsString)> = env
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect();
    let spec = proc_exec::Spec {
        program: std::path::PathBuf::from(bin),
        args,
        env: env_pairs,
        cwd: cwd.to_path_buf(),
        stdin: proc_exec::StdioSpec::Null,
        stdout: proc_exec::StdioSpec::Piped,
        stderr: proc_exec::StdioSpec::Piped,
        setsid: false,
        ctty: false,
    };
    hexecrunner::output(runner, spec, ctoken)
        .await
        .context("wait for heph-govet -format")
}

/// `heph-govet` itself failed: its output goes to the target's log, whose tail
/// the failure box renders, rather than into the message — and to the live
/// stderr of an interactive run, since it was captured rather than streamed.
async fn govet_failed(
    req: &mut ManagedRunRequest<'_, '_>,
    output: &std::process::Output,
    what: &'static str,
) -> anyhow::Error {
    let e = hplugin::error::ProcessFailed::from_output(&req.sandbox_dir.join("log.txt"), output)
        .context(what);
    hdriver_support::driver_managed::echo_failure_log(&mut req.request, &e).await;
    e
}

/// Shared config for both format drivers.
#[derive(Spec)]
struct GoFormatSpec {
    /// Dependencies, grouped by name → target addresses. The default (`""`) group
    /// is the package `.go` sources (one fs glob, see [`format_sources_addr`]);
    /// `govet_tool` the binary; `config` the optional `.golangci.yml`.
    deps: HashMap<String, Vec<String>>,
    /// Exec runner this tool runs under: a target address producing a
    /// `runner.json`, so the tool runs inside a described environment (a devenv
    /// shell, a container) instead of on the bare host. Absent, or the literal
    /// `"local"`, spawns here as before. A hashed dependency, so changing the
    /// environment re-keys this target.
    runner: String,
    /// What the target reserves from the resource pools while it runs, e.g.
    /// `{"cpu": 2}`. Defaults to 1 cpu. Never hashed.
    resources: hplugin::driver::Resources,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct GoFormatDef {
    check: bool,
    /// Exec runner for `gofmt`. Deliberately absent from `Hash` — see
    /// `plugingo::runner`: it reaches the cache key through the hashout of the
    /// input `parse` emits, exactly as a `hash_dep` does.
    runner: Option<hplugin::driver::TargetAddr>,
    /// Deliberately absent from `Hash`: see [`DepGroups`].
    #[serde(default)]
    deps: DepGroups,
}

impl Hash for GoFormatDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        GO_FORMAT_FORMAT_VERSION.hash(state);
        self.check.hash(state);
    }
}

/// Parse the shared inputs (sources + tool + config) into engine inputs. The
/// `config` group is unhashed-annotation-free (it is hashed like the rest so a
/// config edit re-formats), the tool is staged read-only.
fn parse_inputs(spec: &GoFormatSpec, pkg: &hmodel::htpkg::PkgBuf) -> anyhow::Result<Vec<Input>> {
    let mut inputs: Vec<Input> = Vec::new();
    let mut groups: Vec<&String> = spec.deps.keys().collect();
    groups.sort();
    for group in groups {
        // Read-only (one shared tool) *and* per-file: whole-tree read-only staging
        // symlinks the subtree in and lists only its root directory, which
        // `single_staged_path` would then hand to exec (a directory → EACCES).
        // Per-file staging hardlinks each file and lists it, so we exec the binary.
        let read_only = group.as_str() == GOVET_TOOL_GROUP;
        let annotations = if read_only {
            BTreeMap::from([
                (
                    hdriver_support::stage::READ_ONLY_ANNOTATION.to_string(),
                    "true".to_string(),
                ),
                (
                    hdriver_support::stage::STAGE_PER_FILE_ANNOTATION.to_string(),
                    "true".to_string(),
                ),
            ])
        } else {
            BTreeMap::new()
        };
        for (i, addr_str) in spec.deps.get(group).expect("group key").iter().enumerate() {
            inputs.push(Input {
                r#ref: TargetAddr::parse(addr_str, pkg)
                    .with_context(|| format!("parse dep addr {addr_str}"))?,
                mode: InputMode::Standard,
                origin_id: dep_origin(group, i),
                annotations: annotations.clone(),
                hashed: true,
                runtime: true,
            });
        }
    }
    Ok(inputs)
}

// ---------------------------------------------------------------------------
// Fix driver (`go_format`): reformat sources, codegen=in_place.
// ---------------------------------------------------------------------------

pub struct GoFormatDriver {
    /// Default exec runner for this driver's tool, from the plugin's `runner:`
    /// option in the config yaml. A target's own `runner` field overrides it,
    /// and `"local"` there opts back out. See [`crate::plugingo::runner`].
    default_runner: Option<String>,
}

impl GoFormatDriver {
    pub fn new() -> Self {
        Self {
            default_runner: None,
        }
    }

    /// Set the driver-wide default runner (the plugin's `runner:` option).
    /// `None` and `Some("local")` both mean "spawn on the host".
    #[must_use]
    pub fn with_default_runner(mut self, runner: Option<String>) -> Self {
        self.default_runner = runner;
        self
    }
}

impl Default for GoFormatDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ManagedDriver for GoFormatDriver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "format".to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        GoFormatSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let pkg = req.target_spec.addr.package.clone();
        let pkg_str = pkg.as_str();
        let spec = GoFormatSpec::from(&req.target_spec.config).context("parse go_format")?;
        let mut inputs = parse_inputs(&spec, &pkg)?;
        let (runner, runner_input) = super::runner::parse_runner_with_default(
            &spec.runner,
            self.default_runner.as_deref(),
            &pkg,
        )?;
        inputs.extend(runner_input);

        // The rewritten sources, written back in place: the same glob the `""`
        // group stages, so the output covers exactly what was staged (generated
        // files are never staged, so never collected or written back) and the
        // engine's in-place guard pairs it with that input by addr.
        let outputs = vec![Output {
            group: SRC_OUTPUT_GROUP.to_string(),
            paths: vec![TPath {
                content: Content::Glob(format_sources_pattern(pkg_str)),
                codegen_tree: CodegenMode::InPlace,
                collect: true,
            }],
        }];

        let def = GoFormatDef {
            check: false,
            runner,
            deps: DepGroups::of(&spec.deps),
        };
        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("go_format_{}", req.target_spec.addr.format())
            });
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                outputs,
                cache: CacheConfig::on(true),
                hash,
                resources: spec.resources,
                ..Default::default()
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
        mut req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let pkg_dir = &req.sandbox_pkg_dir;
        let def = req.request.target.def_de::<GoFormatDef>();
        let runner = super::runner::runner_ref(req.request.request_id, def.runner.as_ref());
        let (bin, env) = govet_bin_and_config(&req, &def.deps)?;
        let files = def.deps.staged_paths(&req, "")?;
        if files.is_empty() {
            return Ok(ManagedRunResponse { artifacts: vec![] });
        }
        let args = vec![OsString::from("-format"), arg_file(pkg_dir, &files)?];

        let output = exec_govet(&bin, args, &env, pkg_dir, runner, ctoken).await?;
        if !output.status.success() {
            return Err(govet_failed(&mut req, &output, "heph-govet -format failed").await);
        }
        // The rewritten sources are collected from the declared in_place outputs.
        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

// ---------------------------------------------------------------------------
// Check driver (`go_format_check`): fail when any file is not formatted.
// ---------------------------------------------------------------------------

pub struct GoFormatCheckDriver {
    /// Default exec runner for this driver's tool, from the plugin's `runner:`
    /// option in the config yaml. A target's own `runner` field overrides it,
    /// and `"local"` there opts back out. See [`crate::plugingo::runner`].
    default_runner: Option<String>,
}

impl GoFormatCheckDriver {
    pub fn new() -> Self {
        Self {
            default_runner: None,
        }
    }

    /// Set the driver-wide default runner (the plugin's `runner:` option).
    /// `None` and `Some("local")` both mean "spawn on the host".
    #[must_use]
    pub fn with_default_runner(mut self, runner: Option<String>) -> Self {
        self.default_runner = runner;
        self
    }
}

impl Default for GoFormatCheckDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ManagedDriver for GoFormatCheckDriver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "format_check".to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        GoFormatSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let pkg = req.target_spec.addr.package.clone();
        let spec = GoFormatSpec::from(&req.target_spec.config).context("parse go_format_check")?;
        let mut inputs = parse_inputs(&spec, &pkg)?;
        let (runner, runner_input) = super::runner::parse_runner_with_default(
            &spec.runner,
            self.default_runner.as_deref(),
            &pkg,
        )?;
        inputs.extend(runner_input);

        let def = GoFormatDef {
            check: true,
            runner,
            deps: DepGroups::of(&spec.deps),
        };
        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("go_format_check_{}", req.target_spec.addr.format())
            });
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                cache: CacheConfig::on(true),
                hash,
                resources: spec.resources,
                ..Default::default()
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
        mut req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let pkg_dir = &req.sandbox_pkg_dir;
        let def = req.request.target.def_de::<GoFormatDef>();
        let runner = super::runner::runner_ref(req.request.request_id, def.runner.as_ref());
        let (bin, env) = govet_bin_and_config(&req, &def.deps)?;
        let files = def.deps.staged_paths(&req, "")?;
        if files.is_empty() {
            return Ok(ManagedRunResponse { artifacts: vec![] });
        }
        let args = vec![
            OsString::from("-format"),
            OsString::from("-check"),
            arg_file(pkg_dir, &files)?,
        ];

        let output = exec_govet(&bin, args, &env, pkg_dir, runner, ctoken).await?;
        let stdout = &output.stdout;
        match output.status.code() {
            Some(0) => Ok(ManagedRunResponse { artifacts: vec![] }),
            // Exit 1 = files need formatting; the tool prints them to stdout.
            Some(1) => {
                let list = String::from_utf8_lossy(stdout);
                let files: Vec<&str> = list.lines().filter(|l| !l.is_empty()).collect();
                anyhow::bail!(
                    "{} file(s) need formatting (run the `format` target):\n{}",
                    files.len(),
                    files
                        .iter()
                        .map(|f| format!("  {}", basename(f)))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
            _ => Err(govet_failed(&mut req, &output, "heph-govet -format -check failed").await),
        }
    }
}

/// Best-effort basename for a sandbox path, for a readable failure message.
fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

// ---------------------------------------------------------------------------
// Provider-facing spec builders
// ---------------------------------------------------------------------------

use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hplugin::provider::TargetSpec;

/// Parameters shared by both format spec builders.
pub struct FormatParams<'a> {
    pub addr: Addr,
    /// `build` addr of the `heph-govet` binary (host factors).
    pub govet_addr: &'a Addr,
    /// `.golangci.yml` addr → `config` dep group. `None` → gofmt default.
    pub config_addr: Option<&'a Addr>,
}

fn base_deps(p: &FormatParams) -> BTreeMap<String, Value> {
    let str_one = |a: &Addr| Value::List(vec![Value::String(a.format())]);
    let mut deps: BTreeMap<String, Value> = BTreeMap::new();
    deps.insert(
        String::new(),
        str_one(&format_sources_addr(p.addr.package.as_str())),
    );
    deps.insert(GOVET_TOOL_GROUP.to_string(), str_one(p.govet_addr));
    if let Some(cfg) = p.config_addr {
        deps.insert("config".to_string(), str_one(cfg));
    }
    deps
}

/// Build the `format` (`go_format`) spec: reformats + rewrites in place.
pub fn build_format_spec(p: FormatParams) -> TargetSpec {
    let deps = base_deps(&p);
    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("deps".to_string(), Value::Map(deps.into_iter().collect()));
    TargetSpec {
        addr: p.addr,
        driver: listed::DRIVER_FORMAT.to_string(),
        config,
        // The plain `format` target is the FIXER (it rewrites sources), so it owns
        // the plain labels — `go-format` + `format` — plus `fix`. Checking without
        // rewriting is `format-check`.
        labels: listed::owned(listed::FORMAT),
        transitive: Default::default(),
        approval: Default::default(),
    }
}

/// Build the `format` (`go_format_check`) gate spec: fails on unformatted files.
pub fn build_format_check_spec(p: FormatParams) -> TargetSpec {
    let deps = base_deps(&p);
    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("deps".to_string(), Value::Map(deps.into_iter().collect()));
    TargetSpec {
        addr: p.addr,
        driver: listed::DRIVER_FORMAT_CHECK.to_string(),
        config,
        // The read-only checker (`format-check`): `go-format-check` among go
        // targets, `format-check` across every language.
        labels: listed::owned(listed::FORMAT_CHECK),
        transitive: Default::default(),
        approval: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmodel::htpkg::PkgBuf;

    fn addr(name: &str) -> Addr {
        Addr::new(PkgBuf::from("mylib"), name.to_string(), Default::default())
    }

    fn govet() -> Addr {
        Addr::new(
            PkgBuf::from("tools/heph-govet"),
            "build".to_string(),
            Default::default(),
        )
    }

    fn params<'a>(g: &'a Addr, cfg: Option<&'a Addr>) -> FormatParams<'a> {
        FormatParams {
            addr: addr("format"),
            govet_addr: g,
            config_addr: cfg,
        }
    }

    /// The plugin's `runner:` config-yaml option reaches a generated target.
    /// Nothing populates the `runner` spec field for provider-generated Go
    /// targets, so the option is the only path to a non-host runner here.
    #[tokio::test]
    async fn the_format_driver_default_runner_reaches_the_parsed_inputs() {
        let ct = hcore::hasync::StdCancellationToken::new();
        let def = GoFormatDriver::new()
            .with_default_runner(Some("//tools/devenv:runner".to_string()))
            .parse(
                ParseRequest {
                    request_id: "t".to_string(),
                    target_spec: std::sync::Arc::new(build_format_spec(params(&govet(), None))),
                },
                &ct,
            )
            .await
            .expect("parse")
            .target_def;
        let runner: Vec<_> = def
            .inputs
            .iter()
            .filter(|i| i.origin_id == crate::plugingo::runner::RUNNER_ORIGIN)
            .collect();
        assert_eq!(
            runner.len(),
            1,
            "expected exactly one runner input, got {}",
            runner.len()
        );
        let input = runner.first().expect("runner input");
        assert_eq!(input.r#ref.r#ref.format(), "//tools/devenv:runner");
        assert!(input.hashed, "must key the cache");
        assert!(!input.runtime, "must not reach the sandbox");
    }

    /// The plugin's `runner:` config-yaml option reaches a generated target.
    /// Nothing populates the `runner` spec field for provider-generated Go
    /// targets, so the option is the only path to a non-host runner here.
    #[tokio::test]
    async fn the_format_check_driver_default_runner_reaches_the_parsed_inputs() {
        let ct = hcore::hasync::StdCancellationToken::new();
        let def = GoFormatCheckDriver::new()
            .with_default_runner(Some("//tools/devenv:runner".to_string()))
            .parse(
                ParseRequest {
                    request_id: "t".to_string(),
                    target_spec: std::sync::Arc::new(build_format_check_spec(params(
                        &govet(),
                        None,
                    ))),
                },
                &ct,
            )
            .await
            .expect("parse")
            .target_def;
        let runner: Vec<_> = def
            .inputs
            .iter()
            .filter(|i| i.origin_id == crate::plugingo::runner::RUNNER_ORIGIN)
            .collect();
        assert_eq!(
            runner.len(),
            1,
            "expected exactly one runner input, got {}",
            runner.len()
        );
        let input = runner.first().expect("runner input");
        assert_eq!(input.r#ref.r#ref.format(), "//tools/devenv:runner");
        assert!(input.hashed, "must key the cache");
        assert!(!input.runtime, "must not reach the sandbox");
    }

    async fn parse_fix(spec: TargetSpec) -> TargetDef {
        let ct = hcore::hasync::StdCancellationToken::new();
        GoFormatDriver::new()
            .parse(
                ParseRequest {
                    request_id: "t".to_string(),
                    target_spec: std::sync::Arc::new(spec),
                },
                &ct,
            )
            .await
            .expect("parse")
            .target_def
    }

    /// The def hash is part of every `format` / `format-check` cache key, and
    /// moving it invalidates every one of them. Reading sources through an fs
    /// glob (instead of one `file` per source) and declaring a `Glob` output
    /// (instead of one `FilePath` per source) must leave it where it was, so a
    /// package whose source set is unchanged keeps hitting its old entry. The
    /// literals are the hashes from before that change.
    #[tokio::test]
    async fn def_hash_did_not_move_with_the_glob_sources() {
        let fix = parse_fix(build_format_spec(params(&govet(), None))).await;
        assert_eq!(String::from_utf8_lossy(&fix.hash), "d1dd927ed529f768");

        let ct = hcore::hasync::StdCancellationToken::new();
        let check = GoFormatCheckDriver::new()
            .parse(
                ParseRequest {
                    request_id: "t".to_string(),
                    target_spec: std::sync::Arc::new(build_format_check_spec(params(
                        &govet(),
                        None,
                    ))),
                },
                &ct,
            )
            .await
            .expect("parse")
            .target_def;
        assert_eq!(String::from_utf8_lossy(&check.hash), "8cd045b8efcef9bd");
    }

    /// The fix driver's one `in_place` output is the very glob its `""` group
    /// stages — what lets the engine's in-place guard and fixpoint check pair
    /// the input with the output.
    #[tokio::test]
    async fn fix_output_is_the_staged_glob() {
        let spec = build_format_spec(params(&govet(), None));
        let def = parse_fix(spec).await;

        let [output] = def.outputs.as_slice() else {
            panic!("expected one output, got {:?}", def.outputs.len());
        };
        let [path] = output.paths.as_slice() else {
            panic!("expected one output path, got {}", output.paths.len());
        };
        assert!(
            matches!(&path.content, Content::Glob(p) if p == "mylib/*.go"),
            "{:?}",
            path.content
        );
        assert_eq!(path.codegen_tree, CodegenMode::InPlace);

        let sources: Vec<String> = def
            .inputs
            .iter()
            .filter(|i| i.origin_id.starts_with("dep||"))
            .map(|i| i.r#ref.r#ref.format())
            .collect();
        assert_eq!(sources, vec![format_sources_addr("mylib").format()]);
    }

    /// Adversarial packages: every wax metacharacter (`? * $ : < > ( ) [ ] { } ,`)
    /// and `\`, which `wax::escape` leaves alone, must match itself, and only
    /// the trailing `*.go` may stay a wildcard.
    #[test]
    fn format_sources_pattern_escapes_the_package() {
        assert_eq!(format_sources_pattern(""), "*.go");
        assert_eq!(format_sources_pattern("a/b"), "a/b/*.go");
        assert_eq!(
            format_sources_pattern("we[i]rd/{x}<y>"),
            r"we\[i\]rd/\{x\}\<y\>/*.go"
        );
        assert_eq!(format_sources_pattern(r"back\slash"), r"back\\slash/*.go");
        assert_eq!(
            format_sources_pattern("a?b*c$d:e(f),g"),
            r"a\?b\*c\$d\:e\(f\)\,g/*.go"
        );

        // The escaped pattern is a valid wax glob that matches the literal
        // package's sources and nothing that only its unescaped form would.
        for pkg in ["we[i]rd/{x}<y>", "a?b*c$d:e(f),g", "plain"] {
            let pattern = format_sources_pattern(pkg);
            let glob = wax::Glob::new(&pattern)
                .unwrap_or_else(|e| panic!("{pattern:?} must compile: {e}"));
            use wax::Program as _;
            assert!(
                glob.is_match(format!("{pkg}/a.go").as_str()),
                "{pattern:?} must match {pkg}/a.go"
            );
            assert!(
                !glob.is_match(format!("{pkg}/sub/a.go").as_str()),
                "{pattern:?} must not reach a subpackage"
            );
            assert!(
                !glob.is_match(format!("{pkg}/a.txt").as_str()),
                "{pattern:?} must match only .go files"
            );
        }
        let pattern = format_sources_pattern("we[i]rd");
        let glob = wax::Glob::new(&pattern).expect("compile");
        use wax::Program as _;
        assert!(
            !glob.is_match("wei/a.go"),
            "`[i]` must be a literal, not a character class"
        );

        // Known gap: wax 0.7 has no spelling for a literal `\` — its literal
        // parser accepts `\` only before a metacharacter, and its character
        // classes exclude `\` too — so a package path holding one cannot be
        // globbed, and its `format` target fails at the fs glob's parse. Go
        // itself refuses `\` in an import path. Pinned so a wax that learns to
        // spell it turns this red and the gap gets closed.
        let pattern = format_sources_pattern(r"back\slash");
        assert!(
            wax::Glob::new(&pattern).is_err(),
            "wax now compiles {pattern:?}: a `\\` package can be globbed, close the gap"
        );
    }

    fn dep_groups(s: &TargetSpec) -> Vec<String> {
        match s.config.get("deps").unwrap() {
            Value::Map(m) => m.keys().cloned().collect(),
            _ => panic!("deps not a map"),
        }
    }

    /// Same split as lint: the fixer (`format`) owns the plain `go-format`/`format`
    /// labels plus `fix`; the read-only checker (`format-check`) owns
    /// `go-format-check`/`format-check` and never `fix`.
    #[test]
    fn check_and_fix_labels_separate_checking_from_fixing() {
        let g = govet();
        let check = build_format_check_spec(params(&g, None));
        assert_eq!(check.labels, vec!["go-format-check", "format-check"]);

        let fix = build_format_spec(params(&g, None));
        assert_eq!(fix.labels, vec!["go-format", "format", "fix"]);
        assert!(
            !check.labels.contains(&"fix".to_string()),
            "a `--label fix` sweep must not pick up the read-only checker"
        );
    }

    #[test]
    fn fix_driver_name_and_outputs() {
        let g = govet();
        let s = build_format_spec(params(&g, None));
        assert_eq!(s.driver, "go.format");
        // The outputs are the driver's, derived from the package at parse
        // (see `fix_output_is_the_staged_glob`), not a list the spec carries.
        assert!(!s.config.contains_key("out"));
        assert!(dep_groups(&s).contains(&"govet_tool".to_string()));
    }

    #[test]
    fn check_driver_name_no_outputs() {
        let g = govet();
        let s = build_format_check_spec(params(&g, None));
        assert_eq!(s.driver, "go.format_check");
        assert!(!s.config.contains_key("out"));
    }

    #[test]
    fn config_group_present_only_with_config_addr() {
        let g = govet();
        assert!(
            !dep_groups(&build_format_check_spec(params(&g, None))).contains(&"config".to_string())
        );
        let cfg = Addr::new(
            PkgBuf::from(""),
            ".golangci.yml".to_string(),
            Default::default(),
        );
        assert!(
            dep_groups(&build_format_check_spec(params(&g, Some(&cfg))))
                .contains(&"config".to_string())
        );
    }

    #[test]
    fn basename_extracts_last_segment() {
        assert_eq!(basename("/a/b/c.go"), "c.go");
        assert_eq!(basename("c.go"), "c.go");
    }
}
