//! The `exec.entrypoint` driver: a target that runs in the caller's world —
//! their cwd, their environment, their stdio — rather than in a sandbox.
//!
//! See the tool-shims spec. In short: an entrypoint declares what runs (`run`,
//! appended with the caller's args when `args = True`), with exec's dependency
//! vocabulary (`deps`, `hash_deps`, `runtime_deps`, `tools`, `env`, and the
//! transitive closure of its deps), and is never cached, never an input, and
//! never sandboxed. The engine builds and stages its closure and hands this
//! driver absolute stage paths; this driver turns them into an argv, an env and
//! a cwd. The binary spawns it.
//!
//! Its def is its own [`EntrypointDef`] with its own hash (D26), so no exec def
//! hash moves. `parse` and `apply_transitive` share exec's helpers by name
//! (D19), never a branch on the driver's name.

use super::{
    build_dep_inputs, build_tool_inputs, fold_transitive_inputs, grouped, merged_dep_groups,
    src_key, tool_key,
};
use anyhow::Context;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hplugin::driver::entrypoint::{
    Caller, EntrypointDriver, Invocation, PrepareRequest, PreparedCommand, StagedInput,
};
use hplugin::driver::sandbox::EnvValue;
use hplugin::driver::targetdef::{CacheConfig, Input, TargetDef as EngineTargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, Deferred,
    ParseRequest, ParseResponse, RunRequest, RunResponse,
};
use hplugin::htspec::Spec;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

/// The driver's local name: `exec.entrypoint` in the exec plugin.
pub const DRIVER_LOCAL_NAME: &str = "entrypoint";

const ENTRYPOINT_DEF_FORMAT_VERSION: u32 = 1;

/// Where the process runs.
///
/// An enum, not a flag, so `"sandbox"` can return (D11).
#[derive(
    Clone, Copy, Debug, Default, Hash, PartialEq, Eq, serde::Serialize, crate::htspec::SpecEnum,
)]
pub enum Workdir {
    /// The caller's cwd, captured raw at startup. May be outside the workspace.
    #[default]
    Caller,
    /// The workspace root.
    Root,
}

/// `exec.entrypoint` target config. Only these fields: everything else is
/// refused at parse, by name where the refusal has a reason worth giving.
#[derive(Spec)]
pub(crate) struct EntrypointSpec {
    /// The program and its leading arguments, as an argv list. `$SRC_<group>`
    /// and `$TOOL_<group>` expand to absolute paths in heph's read-only stage.
    /// The caller's args are appended when `args = True`.
    pub run: Vec<Deferred<String>>,
    /// Append the caller's arguments (`heph run //x -- a b`) to `run`. Without
    /// it, trailing arguments are refused before anything builds.
    pub args: bool,
    /// Where the process runs: `"caller"` (the caller's cwd, the default) or
    /// `"root"` (the workspace root).
    pub workdir: Workdir,
    /// Dependencies, grouped by name → list of target addresses, built before
    /// the program runs and exposed as `$SRC_<group>`.
    pub deps: HashMap<String, Vec<String>>,
    /// Dependencies built before the program runs and otherwise ignored.
    pub hash_deps: HashMap<String, Vec<String>>,
    /// Dependencies exposed as `$SRC_<group>`, like `deps`.
    pub runtime_deps: HashMap<String, Vec<String>>,
    /// Tools, grouped by name → list of target addresses; first on `PATH`.
    pub tools: HashMap<String, Vec<String>>,
    /// Environment variables set for the program, over the caller's own.
    pub env: BTreeMap<String, Deferred<String>>,
}

/// Fields an entrypoint refuses, each with why. The error names the field and
/// the target.
const REFUSED_FIELDS: &[(&str, &str)] = &[
    (
        "out",
        "an entrypoint has no outputs: it runs in your world, not a sandbox, and nothing it \
         writes is captured",
    ),
    ("cache", "an entrypoint is never cached"),
    ("codegen", "an entrypoint has no outputs to write back"),
    (
        "support_files",
        "an entrypoint has no sandbox to materialize support files into; reach a file through a \
         dep and `$SRC_<group>`",
    ),
    (
        "scratch",
        "an entrypoint has no sandbox to mount a scratch cache into",
    ),
    (
        "pass_env",
        "an entrypoint already runs with the caller's whole environment",
    ),
    (
        "runtime_pass_env",
        "an entrypoint already runs with the caller's whole environment",
    ),
    (
        "runtime_env",
        "an entrypoint is never hashed, so `env` already is runtime-only",
    ),
    (
        "credentials",
        "presenting a credential to an entrypoint would leak its material into your working tree \
         and environment; a program run from your shell already has your ambient identity",
    ),
    (
        "runner",
        "not supported yet: an entrypoint under a runner needs the runner protocol to carry the \
         caller's environment (base_env, plugin ABI 0.18.0)",
    ),
];

#[derive(Clone, serde::Serialize)]
pub(crate) struct EntrypointDef {
    /// Unresolved, as for exec: a def is a pure function of the spec.
    pub run: Vec<Deferred<String>>,
    pub args: bool,
    pub workdir: Workdir,
    pub dep_group_inputs: BTreeMap<String, Vec<Input>>,
    pub runtime_dep_group_inputs: BTreeMap<String, Vec<Input>>,
    pub tool_group_inputs: BTreeMap<String, Vec<Input>>,
    /// The target's own `env`.
    pub env: BTreeMap<String, Deferred<String>>,
    /// Literal env from the transitive closure, applied over `env` in exec's
    /// order: hashed literals (exec's `pass_env`), then unhashed ones (exec's
    /// `runtime_env`).
    pub transitive_env: BTreeMap<String, String>,
    pub transitive_runtime_env: BTreeMap<String, String>,
}

impl Hash for EntrypointDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        ENTRYPOINT_DEF_FORMAT_VERSION.hash(state);
        // Own domain, so an entrypoint can never share a def hash with an exec
        // target whose fields happen to line up.
        "exec.entrypoint".hash(state);
        self.run.hash(state);
        self.args.hash(state);
        self.workdir.hash(state);
        self.dep_group_inputs.hash(state);
        self.runtime_dep_group_inputs.hash(state);
        self.tool_group_inputs.hash(state);
        self.env.hash(state);
        self.transitive_env.hash(state);
        self.transitive_runtime_env.hash(state);
    }
}

fn def_hash(def: &EntrypointDef, label: impl Fn() -> String) -> Vec<u8> {
    let mut h = DebugHasher::new(Xxh3Default::new(), label);
    def.hash(&mut h);
    format!("{:x}", h.finish()).into_bytes()
}

/// The `exec.entrypoint` driver.
#[derive(Debug, Default)]
pub struct Entrypoint;

impl Entrypoint {
    pub fn new() -> Self {
        Self
    }
}

/// Refuse the fields an entrypoint does not take, by name, before the generic
/// parser reports an unknown one.
fn refuse_fields(
    config: &HashMap<String, hcore::htvalue::Value>,
    addr: &str,
) -> anyhow::Result<()> {
    // Sorted, so a target with several gets the same error every time.
    let mut keys: Vec<&str> = config.keys().map(String::as_str).collect();
    keys.sort_unstable();
    for key in keys {
        if let Some((_, why)) = REFUSED_FIELDS.iter().find(|(f, _)| *f == key) {
            anyhow::bail!("`{key}` is not accepted on the exec.entrypoint target {addr}: {why}");
        }
    }
    if let Some(hcore::htvalue::Value::String(w)) = config.get("workdir")
        && w == "sandbox"
    {
        anyhow::bail!(
            "`workdir = \"sandbox\"` is not supported yet on the exec.entrypoint target {addr}: \
             `workdir` is \"caller\" (the caller's cwd, the default) or \"root\" (the workspace \
             root)"
        );
    }
    Ok(())
}

#[async_trait]
impl hplugin::driver::Driver for Entrypoint {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_LOCAL_NAME.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        EntrypointSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = req.target_spec.addr.format();
        refuse_fields(&req.target_spec.config, &addr)?;
        let spec = EntrypointSpec::from(&req.target_spec.config).with_context(|| {
            format!(
                "the exec.entrypoint target {addr} (fields: run, args, workdir = \
                 \"caller\"|\"root\", deps, hash_deps, runtime_deps, tools, env)"
            )
        })?;
        let pkg = req.target_spec.addr.package.clone();
        let no_read_only = std::collections::HashSet::new();
        let dep_inputs = build_dep_inputs(&pkg, spec.deps, "dep", true, true, &no_read_only)?;
        let hash_dep_inputs =
            build_dep_inputs(&pkg, spec.hash_deps, "hash_dep", true, false, &no_read_only)?;
        let runtime_dep_inputs = build_dep_inputs(
            &pkg,
            spec.runtime_deps,
            "runtime_dep",
            false,
            true,
            &no_read_only,
        )?;
        let tool_inputs = build_tool_inputs(&pkg, spec.tools)?;

        let def = EntrypointDef {
            run: spec.run,
            args: spec.args,
            workdir: spec.workdir,
            dep_group_inputs: grouped(&dep_inputs),
            runtime_dep_group_inputs: grouped(&runtime_dep_inputs),
            tool_group_inputs: grouped(&tool_inputs),
            env: spec.env,
            transitive_env: BTreeMap::new(),
            transitive_runtime_env: BTreeMap::new(),
        };
        let hash = def_hash(&def, || format!("entrypoint_def_{addr}"));
        Ok(ParseResponse {
            target_def: EngineTargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs: dep_inputs
                    .into_iter()
                    .chain(hash_dep_inputs)
                    .chain(runtime_dep_inputs)
                    .chain(tool_inputs)
                    .map(|(_, v)| v)
                    .collect(),
                outputs: vec![],
                support_files: vec![],
                // Never cached. The engine forces this again from the
                // capability, whatever a driver says (D43); saying it here too
                // keeps `inspect def` honest.
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
        let mut def = req.target_def.clone();
        let mut xdef = def.def::<EntrypointDef>().clone();
        fold_transitive_inputs(
            &req.sandbox,
            &mut def.inputs,
            &mut xdef.tool_group_inputs,
            &mut xdef.dep_group_inputs,
            &mut xdef.runtime_dep_group_inputs,
        );
        for (name, env) in &req.sandbox.env {
            match &env.value {
                // The caller's environment is already the base: a passed
                // variable is there, or the caller did not set it.
                EnvValue::Pass => {}
                EnvValue::Literal(v) => {
                    let map = if env.hash {
                        &mut xdef.transitive_env
                    } else {
                        &mut xdef.transitive_runtime_env
                    };
                    map.insert(name.clone(), v.clone());
                }
            }
        }
        def.hash = def_hash(&xdef, || format!("entrypoint_def_tr_{}", def.addr.format()));
        def.set_def(xdef);
        Ok(ApplyTransitiveResponse { target_def: def })
    }

    async fn run<'a, 'io>(
        &self,
        req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        anyhow::bail!(
            "{} is an exec.entrypoint target: it is invoked with `heph run`, never built",
            req.target.addr.format()
        )
    }

    async fn run_shell<'a, 'io>(
        &self,
        req: RunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        self.run(req, ctoken).await
    }
}

/// The `$SRC_*`/`$TOOL_*`/`WORKSPACE_ROOT` values for a prepared invocation.
fn closure_vars(
    def: &EntrypointDef,
    inputs: &[StagedInput],
    toolbin: Option<&Path>,
    root: &Path,
) -> anyhow::Result<BTreeMap<String, String>> {
    let files_of = |origin_id: &str| -> Vec<&PathBuf> {
        inputs
            .iter()
            .filter(|s| s.origin_id == origin_id)
            .flat_map(|s| s.files.iter())
            .collect()
    };
    let utf8 = |p: &Path| -> anyhow::Result<String> {
        p.to_str()
            .map(str::to_string)
            .with_context(|| format!("staged path {p:?} is not valid UTF-8"))
    };

    let mut vars = BTreeMap::new();
    vars.insert("WORKSPACE_ROOT".to_string(), utf8(root)?);
    for (group, group_inputs) in
        merged_dep_groups(&def.dep_group_inputs, &def.runtime_dep_group_inputs)
    {
        let mut value = String::new();
        for input in group_inputs {
            for f in files_of(&input.origin_id) {
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(&utf8(f)?);
            }
        }
        vars.insert(src_key(group), value);
    }
    if let Some(toolbin) = toolbin {
        for (group, group_inputs) in &def.tool_group_inputs {
            let mut seen = std::collections::HashSet::new();
            let mut value = String::new();
            for input in group_inputs {
                for f in files_of(&input.origin_id) {
                    let Some(name) = f.file_name() else { continue };
                    if !seen.insert(name.to_os_string()) {
                        continue;
                    }
                    if !value.is_empty() {
                        value.push(' ');
                    }
                    value.push_str(&utf8(&toolbin.join(name))?);
                }
            }
            vars.insert(tool_key(group), value);
        }
    }
    Ok(vars)
}

/// Expand `$NAME` and `${NAME}` for the closure's own variables only, in one
/// pass over the author's text. Anything else — another variable, a lone `$`,
/// `${read://…}` — passes through untouched: there is no shell here, and heph
/// substitutes only what it defines.
pub(crate) fn expand_closure_vars(s: &str, vars: &BTreeMap<String, String>) -> String {
    let is_name = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        let (before, from_dollar) = rest.split_at(i);
        out.push_str(before);
        let after = from_dollar.get(1..).unwrap_or("");
        if let Some(braced) = after.strip_prefix('{')
            && let Some(end) = braced.find('}')
        {
            let (name, tail) = braced.split_at(end);
            if let Some(v) = vars.get(name) {
                out.push_str(v);
                rest = tail.get(1..).unwrap_or("");
                continue;
            }
        } else {
            let end = after.find(|c: char| !is_name(c)).unwrap_or(after.len());
            let (name, tail) = after.split_at(end);
            if let Some(v) = vars.get(name) {
                out.push_str(v);
                rest = tail;
                continue;
            }
        }
        out.push('$');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// A field's value: the host's resolution for one holding a `${read://…}`, the
/// closure's variables expanded otherwise. Never both — expanding after
/// resolving would re-interpret a producer's bytes.
fn field_value(
    req: &PrepareRequest<'_>,
    d: &Deferred<String>,
    vars: &BTreeMap<String, String>,
) -> anyhow::Result<String> {
    if hcore::template::has_deferred_ref(d.raw()) {
        Ok(req.resolve(d)?.to_string())
    } else {
        Ok(expand_closure_vars(d.raw(), vars))
    }
}

/// `PATH` for the program: the toolbin, then what the target declared, then
/// the caller's (D9, I3). Empty pieces are dropped.
fn compose_path(
    toolbin: Option<&Path>,
    declared: Option<&OsStr>,
    caller: Option<&OsStr>,
) -> Option<OsString> {
    let parts: Vec<&OsStr> = toolbin
        .map(Path::as_os_str)
        .into_iter()
        .chain(declared)
        .chain(caller)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    let mut out = OsString::new();
    for (i, p) in parts.into_iter().enumerate() {
        if i > 0 {
            out.push(":");
        }
        out.push(p);
    }
    Some(out)
}

impl Entrypoint {
    fn def(target: &EngineTargetDef) -> &EntrypointDef {
        target.def::<EntrypointDef>()
    }

    /// The program's environment: the caller's, then the target's `env`, then
    /// the closure's transitive env, then `$SRC_*`/`$TOOL_*`, with `PATH`
    /// composed. `OsString` end to end: a caller variable that is not UTF-8, or
    /// is very long, arrives unchanged (D29).
    fn environment(
        req: &PrepareRequest<'_>,
        def: &EntrypointDef,
        vars: &BTreeMap<String, String>,
    ) -> anyhow::Result<Vec<(OsString, OsString)>> {
        let caller: &Caller = req.caller;
        let mut env: BTreeMap<OsString, OsString> = caller.env.iter().cloned().collect();
        let caller_path = env.get(OsStr::new("PATH")).cloned();
        let mut declared_path: Option<OsString> = None;
        for (k, d) in &def.env {
            let v =
                OsString::from(field_value(req, d, vars).with_context(|| format!("`env.{k}`"))?);
            if k == "PATH" {
                declared_path = Some(v);
            } else {
                env.insert(OsString::from(k), v);
            }
        }
        for (k, v) in def.transitive_env.iter().chain(&def.transitive_runtime_env) {
            let v = OsString::from(expand_closure_vars(v, vars));
            if k == "PATH" {
                declared_path = Some(v);
            } else {
                env.insert(OsString::from(k), v);
            }
        }
        for (k, v) in vars {
            env.insert(OsString::from(k), OsString::from(v));
        }
        match compose_path(
            req.toolbin,
            declared_path.as_deref(),
            caller_path.as_deref(),
        ) {
            Some(path) => {
                env.insert(OsString::from("PATH"), path);
            }
            None => {
                env.remove(OsStr::new("PATH"));
            }
        }
        Ok(env.into_iter().collect())
    }
}

impl EntrypointDriver for Entrypoint {
    fn accepts_args(&self, target: &EngineTargetDef) -> bool {
        Self::def(target).args
    }

    fn toolbin(
        &self,
        _token: &Invocation,
        target: &EngineTargetDef,
        inputs: &[StagedInput],
    ) -> anyhow::Result<BTreeMap<OsString, PathBuf>> {
        let def = Self::def(target);
        // Exec's rule for a sandbox's `bin/`: groups in order, inputs in order,
        // the first file of a basename wins.
        let mut links = BTreeMap::new();
        for group_inputs in def.tool_group_inputs.values() {
            for input in group_inputs {
                let mut any = false;
                for staged in inputs.iter().filter(|s| s.origin_id == input.origin_id) {
                    for f in &staged.files {
                        let name = f.file_name().with_context(|| {
                            format!("tool file path has no filename: {}", f.display())
                        })?;
                        any = true;
                        links
                            .entry(name.to_os_string())
                            .or_insert_with(|| f.clone());
                    }
                }
                if !any {
                    anyhow::bail!("tool {} produced no files", input.r#ref);
                }
            }
        }
        Ok(links)
    }

    fn prepare(
        &self,
        _token: &Invocation,
        req: PrepareRequest<'_>,
    ) -> anyhow::Result<PreparedCommand> {
        let def = Self::def(req.target);
        let vars = closure_vars(def, req.inputs, req.toolbin, req.root)?;

        let mut argv: Vec<OsString> = Vec::with_capacity(def.run.len() + req.caller.args.len());
        for (i, d) in def.run.iter().enumerate() {
            argv.push(OsString::from(
                field_value(&req, d, &vars).with_context(|| format!("`run[{i}]`"))?,
            ));
        }
        if argv.is_empty() {
            anyhow::bail!("`run` is empty on {}", req.target.addr.format());
        }
        if def.args {
            argv.extend(req.caller.args.iter().cloned());
        } else if !req.caller.args.is_empty() {
            // The engine refuses this before anything builds; this is the
            // backstop for a caller that did not ask.
            anyhow::bail!(
                "{} takes no arguments; declare `args = True` to pass them",
                req.target.addr.format()
            );
        }
        if req.shell_init.is_some() {
            anyhow::bail!(
                "`--shell` is not supported yet on the exec.entrypoint target {}",
                req.target.addr.format()
            );
        }

        let env = Self::environment(&req, def, &vars)?;
        let cwd = match def.workdir {
            Workdir::Caller => req.caller.cwd.clone(),
            Workdir::Root => req.root.to_path_buf(),
        };
        Ok(PreparedCommand { argv, env, cwd })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::parse_addr;
    use hplugin::driver::Driver as _;

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    async fn parse(
        driver_name: &str,
        extra: Vec<(&str, Value)>,
    ) -> anyhow::Result<EngineTargetDef> {
        let mut config = HashMap::from([("run".to_string(), Value::List(vec![s("go")]))]);
        config.extend(extra.into_iter().map(|(k, v)| (k.to_string(), v)));
        let req = ParseRequest {
            request_id: "t".to_string(),
            target_spec: Arc::new(hplugin::provider::TargetSpec {
                addr: parse_addr("//tools:go")?,
                driver: driver_name.to_string(),
                config,
                ..Default::default()
            }),
        };
        let ctoken = StdCancellationToken::new();
        Ok(match driver_name {
            "exec.entrypoint" => Entrypoint.parse(req, &ctoken).await?.target_def,
            "exec" => {
                hdriver_support::driver_managed::ManagedDriver::parse(
                    &super::super::Driver::new_exec(),
                    req,
                    &ctoken,
                )
                .await?
                .target_def
            }
            _ => {
                hdriver_support::driver_managed::ManagedDriver::parse(
                    &super::super::Driver::new_bash(),
                    req,
                    &ctoken,
                )
                .await?
                .target_def
            }
        })
    }

    /// Every refusal names the field and the target; the ones with a reason
    /// give it.
    #[tokio::test]
    async fn entrypoint_parse_refusals() {
        let rows: Vec<(&str, Value, &[&str])> = vec![
            ("out", s("bin/x"), &["`out`", "no outputs"]),
            ("cache", Value::Bool(true), &["`cache`", "never cached"]),
            ("codegen", s("copy"), &["`codegen`"]),
            (
                "support_files",
                Value::List(vec![s("x")]),
                &["`support_files`"],
            ),
            (
                "scratch",
                Value::List(vec![s("//c:gocache")]),
                &["`scratch`"],
            ),
            (
                "pass_env",
                Value::List(vec![s("HOME")]),
                &["`pass_env`", "whole environment"],
            ),
            (
                "credentials",
                Value::List(vec![s("//auth:aws")]),
                &["`credentials`", "leak its material"],
            ),
            (
                "runner",
                s("//tools/devenv:runner"),
                &["`runner`", "not supported yet"],
            ),
            (
                "workdir",
                s("sandbox"),
                &["`workdir = \"sandbox\"`", "\"caller\"", "\"root\""],
            ),
            (
                "workdir",
                s("nonsense"),
                &["workdir", "nonsense", "caller", "root"],
            ),
            ("unknown_field", s("x"), &["unknown_field"]),
            ("run", Value::Int(3), &["run"]),
            ("args", s("yes"), &["args"]),
            ("env", Value::List(vec![s("x")]), &["env"]),
            ("workdir", Value::Bool(true), &["workdir"]),
        ];
        for (field, value, needles) in rows {
            let err = match parse("exec.entrypoint", vec![(field, value.clone())]).await {
                Ok(_) => panic!("{field} = {value:?} must be refused"),
                Err(e) => format!("{e:#}"),
            };
            assert!(
                err.contains("//tools:go"),
                "{field}: names the target: {err}"
            );
            for needle in needles {
                assert!(
                    err.contains(needle),
                    "{field}: {needle:?} missing from: {err}"
                );
            }
        }
    }

    /// `args` belongs to the entrypoint alone (D18).
    #[tokio::test]
    async fn args_refused_outside_entrypoint() {
        for driver in ["exec", "bash"] {
            let err = match parse(driver, vec![("args", Value::Bool(true))]).await {
                Ok(_) => panic!("{driver} must refuse `args`"),
                Err(e) => format!("{e:#}"),
            };
            assert!(err.contains("//tools:go"), "{err}");
            assert!(err.contains("exec.entrypoint"), "{err}");
        }
    }

    /// The `shim` label is generation's and validate's to check, never
    /// parse's (D36).
    #[tokio::test]
    async fn shim_label_is_not_refused_at_parse() -> anyhow::Result<()> {
        let mut config = HashMap::from([("run".to_string(), Value::List(vec![s("go")]))]);
        config.insert("args".to_string(), Value::Bool(false));
        let def = Entrypoint
            .parse(
                ParseRequest {
                    request_id: "t".to_string(),
                    target_spec: Arc::new(hplugin::provider::TargetSpec {
                        addr: parse_addr("//tools:go")?,
                        driver: "exec.entrypoint".to_string(),
                        config,
                        labels: vec!["shim".to_string()],
                        ..Default::default()
                    }),
                },
                &StdCancellationToken::new(),
            )
            .await?
            .target_def;
        assert_eq!(def.labels, vec!["shim".to_string()]);
        Ok(())
    }

    /// Its own def hash, moving with what shapes the invocation (D26).
    #[tokio::test]
    async fn entrypoint_def_hash_is_separate() -> anyhow::Result<()> {
        let base = parse("exec.entrypoint", vec![]).await?.hash;
        let exec = parse("exec", vec![]).await?.hash;
        assert_ne!(base, exec, "an entrypoint never shares exec's def hash");
        for (field, value) in [
            ("run", Value::List(vec![s("gofmt")])),
            (
                "env",
                Value::Map(HashMap::from([("A".to_string(), s("1"))])),
            ),
            ("workdir", s("root")),
            ("args", Value::Bool(true)),
        ] {
            let moved = parse("exec.entrypoint", vec![(field, value)]).await?.hash;
            assert_ne!(base, moved, "{field} must move the def hash");
        }
        assert_eq!(
            base,
            parse("exec.entrypoint", vec![("workdir", s("caller"))])
                .await?
                .hash,
            "the default workdir is \"caller\""
        );
        Ok(())
    }

    #[test]
    fn expand_closure_vars_only_touches_known_names() {
        let vars = BTreeMap::from([
            ("SRC_SDK".to_string(), "/stage/sdk".to_string()),
            ("SRC".to_string(), "/stage/x".to_string()),
        ]);
        for (input, want) in [
            ("$SRC_SDK/bin/go", "/stage/sdk/bin/go"),
            ("${SRC_SDK}bin", "/stage/sdkbin"),
            ("$SRC", "/stage/x"),
            ("$SRC_OTHER", "$SRC_OTHER"),
            ("${read://a:b}", "${read://a:b}"),
            ("$HOME/x", "$HOME/x"),
            ("a$", "a$"),
            ("$$SRC", "$/stage/x"),
            ("${SRC_SDK", "${SRC_SDK"),
            ("é$SRC é", "é/stage/x é"),
            ("", ""),
        ] {
            assert_eq!(expand_closure_vars(input, &vars), want, "{input:?}");
        }
    }

    #[test]
    fn path_is_toolbin_then_declared_then_caller() {
        let p = |s: &str| Some(OsString::from(s));
        assert_eq!(
            compose_path(
                Some(Path::new("/tb")),
                Some(OsStr::new("/decl")),
                Some(OsStr::new("/a:/b"))
            ),
            p("/tb:/decl:/a:/b")
        );
        assert_eq!(compose_path(None, None, Some(OsStr::new("/a"))), p("/a"));
        assert_eq!(compose_path(Some(Path::new("/tb")), None, None), p("/tb"));
        assert_eq!(compose_path(None, Some(OsStr::new("")), None), None);
    }
}
