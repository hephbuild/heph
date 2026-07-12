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

use crate::plugingo::driver_lint::staged_paths_in_group;

/// Dep group carrying the `heph-govet` binary (shared with lint; staged read-only).
const GOVET_TOOL_GROUP: &str = "govet_tool";

/// Bump to invalidate cached format results when the cfg/tool contract changes
/// in a way the tool's own input hash does not already capture.
const GO_FORMAT_FORMAT_VERSION: u32 = 1;

/// Locate the single staged `heph-govet` binary and the optional config path.
fn govet_bin_and_config(
    req: &ManagedRunRequest<'_, '_>,
) -> anyhow::Result<(String, HashMap<String, String>)> {
    let bin = staged_paths_in_group(req, GOVET_TOOL_GROUP)
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("format: heph-govet tool not staged"))?;
    let mut env: HashMap<String, String> = HashMap::new();
    if let Some(cfg) = staged_paths_in_group(req, "config").into_iter().next() {
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

/// Run `heph-govet` with `args`, returning (exit code or None-if-signal, stdout,
/// stderr). Never fails on a non-zero exit — the caller interprets the code.
async fn exec_govet(
    bin: &str,
    args: Vec<OsString>,
    env: &HashMap<String, String>,
    cwd: &std::path::Path,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<(Option<i32>, Vec<u8>, Vec<u8>)> {
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
    let output = proc_exec::output(spec, ctoken)
        .await
        .context("wait for heph-govet -format")?;
    Ok((output.status.code(), output.stdout, output.stderr))
}

/// Shared config for both format drivers.
#[derive(Spec)]
struct GoFormatSpec {
    /// Dependencies, grouped by name → target addresses. The default (`""`) group
    /// is the package `.go` sources; `govet_tool` the binary; `config` the
    /// optional `.golangci.yml`.
    deps: HashMap<String, Vec<String>>,
    /// Declared outputs (fix driver only): the `.go` files, written back in
    /// place. Absent for the check driver.
    out: HashMap<String, Vec<String>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct GoFormatDef {
    check: bool,
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
        let read_only = group.as_str() == GOVET_TOOL_GROUP;
        let annotations = if read_only {
            BTreeMap::from([(
                hdriver_support::stage::READ_ONLY_ANNOTATION.to_string(),
                "true".to_string(),
            )])
        } else {
            BTreeMap::new()
        };
        for (i, addr_str) in spec.deps.get(group).expect("group key").iter().enumerate() {
            inputs.push(Input {
                r#ref: TargetAddr::parse(addr_str, pkg)
                    .with_context(|| format!("parse dep addr {addr_str}"))?,
                mode: InputMode::Standard,
                origin_id: format!("dep|{group}|{i}"),
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

pub struct GoFormatDriver;

impl GoFormatDriver {
    pub fn new() -> Self {
        Self
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
            name: "go_format".to_string(),
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
        let spec = GoFormatSpec::from(req.target_spec.config.clone()).context("parse go_format")?;
        let inputs = parse_inputs(&spec, &pkg)?;

        let outputs: Vec<Output> = spec
            .out
            .iter()
            .map(|(group, paths)| Output {
                group: group.clone(),
                paths: paths
                    .iter()
                    .map(|p| {
                        let full_path = if pkg_str.is_empty() {
                            p.clone()
                        } else {
                            format!("{pkg_str}/{p}")
                        };
                        TPath {
                            content: Content::FilePath(full_path),
                            codegen_tree: CodegenMode::InPlace,
                            collect: true,
                        }
                    })
                    .collect(),
            })
            .collect();

        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("go_format_{}", req.target_spec.addr.format())
            });
            GoFormatDef { check: false }.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(GoFormatDef { check: false }),
                inputs,
                outputs,
                support_files: vec![],
                cache: CacheConfig::on(true),
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
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let pkg_dir = &req.sandbox_pkg_dir;
        let (bin, env) = govet_bin_and_config(&req)?;
        let files = staged_paths_in_group(&req, "");
        if files.is_empty() {
            return Ok(ManagedRunResponse { artifacts: vec![] });
        }
        let args = vec![OsString::from("-format"), arg_file(pkg_dir, &files)?];

        let (code, _stdout, stderr) = exec_govet(&bin, args, &env, pkg_dir, ctoken).await?;
        if code != Some(0) {
            anyhow::bail!(
                "heph-govet -format failed ({code:?}):\n{}",
                hplugin::error::last_n_lines(String::from_utf8_lossy(&stderr).trim(), 20)
            );
        }
        // The rewritten sources are collected from the declared in_place outputs.
        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

// ---------------------------------------------------------------------------
// Check driver (`go_format_check`): fail when any file is not formatted.
// ---------------------------------------------------------------------------

pub struct GoFormatCheckDriver;

impl GoFormatCheckDriver {
    pub fn new() -> Self {
        Self
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
            name: "go_format_check".to_string(),
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
        let spec =
            GoFormatSpec::from(req.target_spec.config.clone()).context("parse go_format_check")?;
        let inputs = parse_inputs(&spec, &pkg)?;

        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("go_format_check_{}", req.target_spec.addr.format())
            });
            GoFormatDef { check: true }.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(GoFormatDef { check: true }),
                inputs,
                outputs: vec![],
                support_files: vec![],
                cache: CacheConfig::on(true),
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
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let pkg_dir = &req.sandbox_pkg_dir;
        let (bin, env) = govet_bin_and_config(&req)?;
        let files = staged_paths_in_group(&req, "");
        if files.is_empty() {
            return Ok(ManagedRunResponse { artifacts: vec![] });
        }
        let args = vec![
            OsString::from("-format"),
            OsString::from("-check"),
            arg_file(pkg_dir, &files)?,
        ];

        let (code, stdout, stderr) = exec_govet(&bin, args, &env, pkg_dir, ctoken).await?;
        match code {
            Some(0) => Ok(ManagedRunResponse { artifacts: vec![] }),
            // Exit 1 = files need formatting; the tool prints them to stdout.
            Some(1) => {
                let list = String::from_utf8_lossy(&stdout);
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
            other => anyhow::bail!(
                "heph-govet -format -check failed ({other:?}):\n{}",
                hplugin::error::last_n_lines(String::from_utf8_lossy(&stderr).trim(), 20)
            ),
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
    /// Source file target addresses (the `""` dep group).
    pub src_addrs: &'a [String],
    /// Source basenames (declared `in_place` outputs; fix target only).
    pub go_files: &'a [String],
    /// `.golangci.yml` addr → `config` dep group. `None` → gofmt default.
    pub config_addr: Option<&'a Addr>,
}

fn base_deps(p: &FormatParams) -> BTreeMap<String, Value> {
    let str_one = |a: &Addr| Value::List(vec![Value::String(a.format())]);
    let mut deps: BTreeMap<String, Value> = BTreeMap::new();
    deps.insert(
        String::new(),
        Value::List(p.src_addrs.iter().cloned().map(Value::String).collect()),
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
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(
            "src".to_string(),
            Value::List(p.go_files.iter().cloned().map(Value::String).collect()),
        )])),
    );
    TargetSpec {
        addr: p.addr,
        driver: "go_format".to_string(),
        config,
        // The plain `format` target is the FIXER (it rewrites sources), so it owns
        // the plain labels — `go-format` + `format` — plus `fix`. Checking without
        // rewriting is `format-check`.
        labels: vec![
            "go-format".to_string(),
            "format".to_string(),
            "fix".to_string(),
        ],
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
        driver: "go_format_check".to_string(),
        config,
        // The read-only checker (`format-check`): `go-format-check` among go
        // targets, `format-check` across every language.
        labels: vec!["go-format-check".to_string(), "format-check".to_string()],
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
            src_addrs: &[],
            go_files: &[],
            config_addr: cfg,
        }
    }

    fn dep_groups(s: &TargetSpec) -> Vec<String> {
        match s.config.get("deps").unwrap() {
            Value::Map(m) => m.iter().map(|(k, _)| k.clone()).collect(),
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
        let s = build_format_spec(FormatParams {
            go_files: &["a.go".to_string()],
            src_addrs: &["//mylib:a.go".to_string()],
            ..params(&g, None)
        });
        assert_eq!(s.driver, "go_format");
        // Declares src outputs (parse marks them in_place).
        match s.config.get("out").unwrap() {
            Value::Map(m) => assert!(m.iter().any(|(k, _)| k == "src")),
            _ => panic!("out not a map"),
        }
        assert!(dep_groups(&s).contains(&"govet_tool".to_string()));
    }

    #[test]
    fn check_driver_name_no_outputs() {
        let g = govet();
        let s = build_format_check_spec(params(&g, None));
        assert_eq!(s.driver, "go_format_check");
        assert!(s.config.get("out").is_none());
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
