//! `entrypoint = True` on an `exec` or `bash` target: a target that runs in
//! the caller's world — their cwd, their environment, their stdio — rather
//! than in a sandbox.
//!
//! See the tool-shims spec. In short: an entrypoint declares what runs (`run`,
//! plus the caller's args when `args = True`), with exec's dependency
//! vocabulary (`deps`, `hash_deps`, `runtime_deps`, `tools`, `env`, and the
//! transitive closure of its deps), and is never cached, never an input, and
//! never sandboxed. It is exec's own def with an [`EntrypointMode`]; the engine
//! reads the def's `entrypoint` attribute, builds and stages the closure, and
//! hands this module's [`Entrypoint`] hook absolute stage paths, which it turns
//! into an argv, an env and a cwd. The binary spawns it.
//!
//! `exec` runs `run` as an argv, with the closure's `$SRC_<group>` /
//! `$TOOL_<group>` expanded by heph and the args appended. `bash` runs `run` as
//! a script through exec's bash invocation, with the args as its positional
//! parameters (`"$@"`); the shell expands the variables itself.

use super::TargetDef;
use anyhow::Context;
use hplugin::driver::Deferred;
use hplugin::driver::entrypoint::{
    Caller, EntrypointDriver, Invocation, PrepareRequest, PreparedCommand, StagedInput,
};
use hplugin::driver::targetdef::TargetDef as EngineTargetDef;
use hplugin::htspec::Spec;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

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

/// What `entrypoint = True` adds to an exec def. Present only with the flag,
/// so a def without it hashes and serializes as it always did; with it, all of
/// it is hashed.
#[derive(Clone, Debug, Hash, serde::Serialize)]
pub struct EntrypointMode {
    /// Append the caller's arguments to the command.
    pub args: bool,
    pub workdir: Workdir,
}

impl EntrypointMode {
    /// What the engine reads off the def ([`EngineTargetDef::entrypoint`]).
    // TODO(entrypoint-L2): exec's own `Workdir` gains `Sandbox` (E21).
    pub(crate) fn engine_attribute(&self) -> hplugin::driver::targetdef::Entrypoint {
        use hplugin::driver::targetdef::Workdir as EngineWorkdir;
        hplugin::driver::targetdef::Entrypoint::new(
            self.args,
            match self.workdir {
                Workdir::Caller => EngineWorkdir::Caller,
                Workdir::Root => EngineWorkdir::Root,
            },
        )
    }
}

/// Fields that mean something only on an entrypoint; refused without the flag.
pub(crate) const ENTRYPOINT_ONLY_FIELDS: &[&str] = &["args", "workdir"];

/// An entrypoint's `env`, which — unlike a sandboxed target's — may hold a
/// `${read://…}`: the hook resolves it when it prepares the command (D46).
#[derive(Spec)]
struct EntrypointEnv {
    env: BTreeMap<String, Deferred<String>>,
}

type DeferredEnv = HashMap<String, Deferred<String>>;

/// Split an entrypoint's config into everything but `env`, for exec's spec
/// parse, and `env` parsed as deferrable values, kept as their raw text (the
/// def is a pure function of the spec; the hook resolves at prepare).
pub(crate) fn split_env(
    config: &HashMap<String, hcore::htvalue::Value>,
) -> anyhow::Result<(HashMap<String, hcore::htvalue::Value>, DeferredEnv)> {
    let mut rest = config.clone();
    let Some(env) = rest.remove("env") else {
        return Ok((rest, HashMap::new()));
    };
    let env = EntrypointEnv::from(&HashMap::from([("env".to_string(), env)]))?
        .env
        .into_iter()
        .collect();
    Ok((rest, env))
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
        "read_only_deps",
        "an entrypoint has no sandbox; every dep it uses is staged read-only already",
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

/// Refuse the fields an entrypoint does not take, by name, before the generic
/// parser sees them.
pub(crate) fn refuse_fields(
    config: &HashMap<String, hcore::htvalue::Value>,
    addr: &str,
    driver: &str,
) -> anyhow::Result<()> {
    // Sorted, so a target with several gets the same error every time.
    let mut keys: Vec<&str> = config.keys().map(String::as_str).collect();
    keys.sort_unstable();
    for key in keys {
        if let Some((_, why)) = REFUSED_FIELDS.iter().find(|(f, _)| *f == key) {
            anyhow::bail!(
                "`{key}` is not accepted on {addr}, an entrypoint `{driver}` target \
                 (`entrypoint = True`): {why}"
            );
        }
    }
    if let Some(hcore::htvalue::Value::String(w)) = config.get("workdir")
        && w == "sandbox"
    {
        anyhow::bail!(
            "`workdir = \"sandbox\"` is not supported yet on {addr}, an entrypoint `{driver}` \
             target: `workdir` is \"caller\" (the caller's cwd, the default) or \"root\" (the \
             workspace root)"
        );
    }
    Ok(())
}

/// The `$SRC_*`/`$TOOL_*`/`WORKSPACE_ROOT` values for a prepared invocation.
fn closure_vars(
    def: &TargetDef,
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
        super::merged_dep_groups(&def.dep_group_inputs, &def.runtime_dep_group_inputs)
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
        vars.insert(super::src_key(group), value);
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
            vars.insert(super::tool_key(group), value);
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
/// closure's variables expanded otherwise (when `vars` is given). Never both —
/// expanding after resolving would re-interpret a producer's bytes.
fn field_value(
    req: &PrepareRequest<'_>,
    d: &Deferred<String>,
    vars: Option<&BTreeMap<String, String>>,
) -> anyhow::Result<String> {
    if hcore::template::has_deferred_ref(d.raw()) {
        Ok(req.resolve(d)?.to_string())
    } else {
        Ok(match vars {
            Some(vars) => expand_closure_vars(d.raw(), vars),
            None => d.raw().to_string(),
        })
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

/// The hook that prepares an `exec` or `bash` entrypoint for `Engine::invoke`.
/// Registered next to the driver; the two differ only in how `run` becomes a
/// command.
#[derive(Debug)]
pub struct Entrypoint {
    /// `bash`: `run` is a script, run through bash with the args as `"$@"`.
    bash: bool,
}

impl Entrypoint {
    /// The hook for the `exec` driver: `run` is an argv.
    pub fn exec() -> Self {
        Self { bash: false }
    }

    /// The hook for the `bash` driver: `run` is a script.
    pub fn bash() -> Self {
        Self { bash: true }
    }

    fn def(target: &EngineTargetDef) -> &TargetDef {
        target.def::<TargetDef>()
    }

    fn mode<'d>(
        def: &'d TargetDef,
        target: &EngineTargetDef,
    ) -> anyhow::Result<&'d EntrypointMode> {
        def.entrypoint.as_ref().with_context(|| {
            format!(
                "{} is not an entrypoint (`entrypoint = True`)",
                target.addr.format()
            )
        })
    }

    /// The command's words before the caller's args: for exec, `run` with the
    /// closure's variables expanded; for bash, exec's bash invocation of the
    /// script, with `bash` as `$0` so the args land in `"$@"`.
    fn words(
        &self,
        req: &PrepareRequest<'_>,
        def: &TargetDef,
        vars: &BTreeMap<String, String>,
    ) -> anyhow::Result<Vec<OsString>> {
        if !self.bash {
            let mut argv = Vec::with_capacity(def.run.len() + req.caller.args.len());
            for (i, d) in def.run.iter().enumerate() {
                argv.push(OsString::from(
                    field_value(req, d, Some(vars)).with_context(|| format!("`run[{i}]`"))?,
                ));
            }
            if argv.is_empty() {
                anyhow::bail!("`run` is empty on {}", req.target.addr.format());
            }
            return Ok(argv);
        }
        // The shell expands `$SRC_*` itself, from the environment; heph only
        // resolves `${read://…}`, as exec's bash does.
        let lines = def
            .run
            .iter()
            .enumerate()
            .map(|(i, d)| field_value(req, d, None).with_context(|| format!("`run[{i}]`")))
            .collect::<anyhow::Result<Vec<String>>>()?;
        let mut argv = super::bash_args(
            vec![
                "-u".to_string(),
                "-e".to_string(),
                "-c".to_string(),
                lines.join("\n"),
            ],
            vec!["--norc".to_string()],
        );
        // `$0`, so the caller's args are `$1…`.
        argv.push("bash".to_string());
        let mut argv: Vec<OsString> = argv.into_iter().map(OsString::from).collect();
        if let Some(program) = argv.first_mut() {
            *program = super::resolve_on_host_path("bash", "bash")?.into_os_string();
        }
        Ok(argv)
    }

    /// The program's environment: the caller's, then the target's `env`, then
    /// the closure's transitive env, then `$SRC_*`/`$TOOL_*`, with `PATH`
    /// composed. `OsString` end to end: a caller variable that is not UTF-8, or
    /// is very long, arrives unchanged (D29).
    fn environment(
        req: &PrepareRequest<'_>,
        def: &TargetDef,
        vars: &BTreeMap<String, String>,
    ) -> anyhow::Result<Vec<(OsString, OsString)>> {
        let caller: &Caller = req.caller;
        let mut env: BTreeMap<OsString, OsString> = caller.env.iter().cloned().collect();
        let caller_path = env.get(OsStr::new("PATH")).cloned();
        let mut declared_path: Option<OsString> = None;
        for (k, v) in &def.env {
            let v = OsString::from(
                field_value(req, v, Some(vars)).with_context(|| format!("`env.{k}`"))?,
            );
            if k == "PATH" {
                declared_path = Some(v);
            } else {
                env.insert(OsString::from(k), v);
            }
        }
        // The transitive closure's literals, in exec's order: hashed ones
        // (folded into `pass_env`), then unhashed ones (`runtime_env`).
        let runtime_env = def.runtime_env.iter().map(|(k, v)| (k, v.raw()));
        for (k, v) in def
            .pass_env
            .iter()
            .map(|(k, v)| (k, v.as_str()))
            .chain(runtime_env)
        {
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
        Self::def(target)
            .entrypoint
            .as_ref()
            .is_some_and(|m| m.args)
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
        let mode = Self::mode(def, req.target)?;
        if !mode.args && !req.caller.args.is_empty() {
            // The engine refuses this before anything builds; this is the
            // backstop for a caller that did not ask.
            anyhow::bail!(
                "{} takes no arguments; declare `args = True` to pass them",
                req.target.addr.format()
            );
        }
        let vars = closure_vars(def, req.inputs, req.toolbin, req.root)?;
        let mut argv = self.words(&req, def, &vars)?;
        let argv = match req.shell_init {
            None => {
                argv.extend(req.caller.args.iter().cloned());
                argv
            }
            // `--shell` (D13): an interactive bash in the program's world, whose
            // `run` is the command plus the args. The args are bash's positional
            // parameters, so they arrive byte for byte; `init.sh` keeps them at
            // top level.
            Some(init) => {
                let words = argv
                    .iter()
                    .map(|w| {
                        w.to_str().map(str::to_string).with_context(|| {
                            format!("command word {w:?} is not valid UTF-8, which `--shell` needs")
                        })
                    })
                    .collect::<anyhow::Result<Vec<String>>>()?;
                let rendered = super::render_entrypoint_shell_init(&words)?;
                std::fs::write(init, rendered)
                    .with_context(|| format!("write the shell init file {}", init.display()))?;
                let bash = super::resolve_on_host_path("bash", "entrypoint --shell")?;
                let mut shell: Vec<OsString> = vec![
                    bash.into_os_string(),
                    "--noprofile".into(),
                    "--rcfile".into(),
                    init.as_os_str().to_os_string(),
                    "-i".into(),
                    "-s".into(),
                    "--".into(),
                ];
                shell.extend(req.caller.args.iter().cloned());
                shell
            }
        };

        let env = Self::environment(&req, def, &vars)?;
        let cwd = match mode.workdir {
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
    use hdriver_support::driver_managed::ManagedDriver as _;
    use hmodel::htaddr::parse_addr;
    use hplugin::driver::ParseRequest;
    use std::sync::Arc;

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn driver(name: &str) -> super::super::Driver {
        match name {
            "exec" => super::super::Driver::new_exec(),
            _ => super::super::Driver::new_bash(),
        }
    }

    /// Parse `//tools:go` on `driver_name` with `run = ["go"]` plus `extra`.
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
        Ok(driver(driver_name)
            .parse(req, &StdCancellationToken::new())
            .await?
            .target_def)
    }

    fn flag() -> (&'static str, Value) {
        ("entrypoint", Value::Bool(true))
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
                "read_only_deps",
                Value::List(vec![s("d")]),
                &["`read_only_deps`"],
            ),
            (
                "pass_env",
                Value::List(vec![s("HOME")]),
                &["`pass_env`", "whole environment"],
            ),
            (
                "runtime_pass_env",
                Value::List(vec![s("HOME")]),
                &["`runtime_pass_env`", "whole environment"],
            ),
            (
                "runtime_env",
                Value::Map(HashMap::from([("A".to_string(), s("1"))])),
                &["`runtime_env`"],
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
        for driver_name in ["exec", "bash"] {
            for (field, value, needles) in &rows {
                let err = match parse(driver_name, vec![flag(), (field, value.clone())]).await {
                    Ok(_) => panic!("{driver_name}: {field} = {value:?} must be refused"),
                    Err(e) => format!("{e:#}"),
                };
                assert!(
                    err.contains("//tools:go"),
                    "{driver_name} {field}: names the target: {err}"
                );
                for needle in *needles {
                    assert!(
                        err.contains(needle),
                        "{driver_name} {field}: {needle:?} missing from: {err}"
                    );
                }
            }
        }
    }

    /// `args` and `workdir` shape an invocation: without `entrypoint = True`
    /// (absent or `False`) they are refused, naming the target and the flag
    /// (D18).
    #[tokio::test]
    async fn args_and_workdir_refused_outside_entrypoint() {
        for driver_name in ["exec", "bash"] {
            for (field, value) in [("args", Value::Bool(true)), ("workdir", s("root"))] {
                for flag in [None, Some(Value::Bool(false))] {
                    let mut extra = vec![(field, value.clone())];
                    if let Some(f) = &flag {
                        extra.push(("entrypoint", f.clone()));
                    }
                    let err = match parse(driver_name, extra).await {
                        Ok(_) => panic!("{driver_name} must refuse `{field}` with {flag:?}"),
                        Err(e) => format!("{e:#}"),
                    };
                    assert!(err.contains("//tools:go"), "{err}");
                    assert!(err.contains(&format!("`{field}`")), "{err}");
                    assert!(err.contains("entrypoint = True"), "{err}");
                }
            }
        }
    }

    /// The flag reaches the engine's def, which forces nothing else: the
    /// driver itself already says uncached.
    #[tokio::test]
    async fn the_flag_reaches_the_engine_def() -> anyhow::Result<()> {
        for driver_name in ["exec", "bash"] {
            let on = parse(driver_name, vec![flag()]).await?;
            assert!(on.entrypoint.is_some(), "{driver_name}");
            assert!(!on.cache.enabled, "{driver_name}");
            let off = parse(driver_name, vec![("entrypoint", Value::Bool(false))]).await?;
            assert!(off.entrypoint.is_none(), "{driver_name}");
        }
        Ok(())
    }

    /// The workspace `runner:` default never applies to an entrypoint (D24).
    #[tokio::test]
    async fn default_runner_not_applied_to_entrypoint() -> anyhow::Result<()> {
        let mut d = super::super::Driver::new_exec();
        d.default_runner = Some("//tools/devenv:runner".to_string());
        let def = d
            .parse(
                ParseRequest {
                    request_id: "t".to_string(),
                    target_spec: Arc::new(hplugin::provider::TargetSpec {
                        addr: parse_addr("//tools:go")?,
                        driver: "exec".to_string(),
                        config: HashMap::from([
                            ("run".to_string(), Value::List(vec![s("go")])),
                            ("entrypoint".to_string(), Value::Bool(true)),
                        ]),
                        ..Default::default()
                    }),
                },
                &StdCancellationToken::new(),
            )
            .await?
            .target_def;
        assert!(def.inputs.iter().all(|i| i.origin_id != "runner"));
        Ok(())
    }

    /// The `shim` label is generation's and validate's to check, never
    /// parse's (D36).
    #[tokio::test]
    async fn shim_label_is_not_refused_at_parse() -> anyhow::Result<()> {
        let def = super::super::Driver::new_exec()
            .parse(
                ParseRequest {
                    request_id: "t".to_string(),
                    target_spec: Arc::new(hplugin::provider::TargetSpec {
                        addr: parse_addr("//tools:go")?,
                        driver: "exec".to_string(),
                        config: HashMap::from([
                            ("run".to_string(), Value::List(vec![s("go")])),
                            ("entrypoint".to_string(), Value::Bool(true)),
                            ("args".to_string(), Value::Bool(false)),
                        ]),
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

    /// Run `snippet` in a non-interactive bash after sourcing the rendered
    /// `init.sh` with `args` as its positional parameters — what the
    /// interactive shell does with `--rcfile … -s -- args`.
    fn in_shell(bash: &str, init: &Path, args: &[&str], snippet: &str) -> (String, String) {
        let out = std::process::Command::new(bash)
            .args(["--noprofile", "--norc", "-c"])
            .arg(format!(". \"$0\" \"$@\" >/dev/null 2>&1; {snippet}"))
            .arg(init)
            .args(args)
            .env_clear()
            .output()
            .expect("run bash");
        (
            String::from_utf8(out.stdout).expect("utf8 stdout"),
            String::from_utf8(out.stderr).expect("utf8 stderr"),
        )
    }

    /// Every bash we can find: the one on PATH, and macOS's 3.2, which is
    /// where `set -u` trips on an empty array.
    fn bashes() -> Vec<String> {
        let bashes: Vec<String> = ["bash", "/bin/bash"]
            .into_iter()
            .filter(|b| which::which(b).is_ok())
            .map(str::to_string)
            .collect();
        assert!(!bashes.is_empty(), "no bash to test with");
        bashes
    }

    /// The rendered `init.sh` hands the program its argv plus the args exactly,
    /// whatever bytes they hold; a helper's own args replace the originals for
    /// that call only; `show` quotes them; there is no `runN` (D13).
    #[test]
    fn entrypoint_shell_init_args_exact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let init = dir.path().join("init.sh");
        let argv = ["printf", "<%s>", "fixed"].map(str::to_string);
        std::fs::write(
            &init,
            super::super::render_entrypoint_shell_init(&argv).expect("render"),
        )
        .expect("write init.sh");
        let expect = |args: &[&str]| {
            let mut s = "<fixed>".to_string();
            for a in args {
                s.push_str(&format!("<{a}>"));
            }
            s
        };

        for bash in &bashes() {
            let rows: &[&[&str]] = &[
                &[],
                &[""],
                &[" "],
                &["a\nb"],
                &["'", "\"", "it's \"q\""],
                &["$HOME", "`id`", "$(id)"],
                &["-n", "--quiet"],
                &["é", "日本"],
                &["\u{1}\u{7f}"],
                &["", "x", ""],
            ];
            for args in rows {
                for helper in ["run", "xrun"] {
                    let (out, err) = in_shell(bash, &init, args, helper);
                    assert_eq!(
                        out,
                        expect(args),
                        "{bash} {helper} with {args:?}; stderr: {err}"
                    );
                }
            }

            let originals = ["orig1", "orig 2"];
            for (snippet, want) in [
                ("run a 'b c'", expect(&["a", "b c"])),
                ("run ''", expect(&[""])),
                ("xrun z", expect(&["z"])),
                (
                    "run x; run",
                    format!("{}{}", expect(&["x"]), expect(&originals)),
                ),
                ("run", expect(&originals)),
                // One command line, so no `runN`/`xrunN`.
                (
                    "for f in run1 xrun1; do declare -F \"$f\" >/dev/null && echo \"$f defined\"; done; echo none",
                    "none\n".to_string(),
                ),
            ] {
                let (out, err) = in_shell(bash, &init, &originals, snippet);
                assert_eq!(out, want, "{bash}: `{snippet}`; stderr: {err}");
            }

            let (out, _) = in_shell(bash, &init, &["a b", ""], "show");
            assert_eq!(out, "'printf' '<%s>' 'fixed' a\\ b ''\n", "{bash}: show");
            let (out, _) = in_shell(bash, &init, &["a b"], "show 'c d'");
            assert_eq!(
                out, "'printf' '<%s>' 'fixed' c\\ d\n",
                "{bash}: show with args"
            );
            let (out, _) = in_shell(bash, &init, &[], "showl");
            assert_eq!(out, "1 │ 'printf' '<%s>' 'fixed'\n", "{bash}: showl");

            // The real shape: an interactive bash reading the file as its
            // rcfile, the args as its positional parameters, the commands on
            // stdin. The rcfile must already see them.
            let mut child = std::process::Command::new(bash)
                .args(["--noprofile", "--rcfile"])
                .arg(&init)
                .args(["-i", "-s", "--", "a b", ""])
                .env_clear()
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn interactive bash");
            {
                use std::io::Write as _;
                let mut stdin = child.stdin.take().expect("stdin");
                stdin
                    .write_all(b"echo; echo BEGIN; run; echo; exit 3\n")
                    .expect("type");
            }
            let out = child.wait_with_output().expect("wait");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let after = stdout.split("BEGIN\n").nth(1).unwrap_or_default();
            assert_eq!(
                after,
                format!("{}\n", expect(&["a b", ""])),
                "{bash}: {stdout}"
            );
            assert_eq!(out.status.code(), Some(3), "{bash}: the shell's status");
        }
    }

    /// A `bash` entrypoint is its script run through exec's bash invocation,
    /// the args as `"$@"` — directly, and as `--shell`'s `run`.
    #[tokio::test]
    async fn bash_entrypoint_runs_the_script_with_args() -> anyhow::Result<()> {
        let target = parse(
            "bash",
            vec![
                flag(),
                ("args", Value::Bool(true)),
                (
                    "run",
                    Value::List(vec![s("printf '<%s>' \"$@\""), s("printf '[%s]' \"$0\"")]),
                ),
            ],
        )
        .await?;
        let dir = tempfile::tempdir()?;
        let caller = Caller {
            cwd: dir.path().to_path_buf(),
            env: vec![],
            args: ["a b", "", "$HOME"].map(OsString::from).to_vec(),
        };
        let deferred = BTreeMap::new();
        let token = Invocation::for_engine_invoke();
        let prepare = |shell_init: Option<&Path>| {
            Entrypoint::bash().prepare(
                &token,
                PrepareRequest {
                    target: &target,
                    inputs: &[],
                    toolbin: None,
                    deferred: &deferred,
                    caller: &caller,
                    root: dir.path(),
                    shell_init,
                },
            )
        };

        let cmd = prepare(None)?;
        let words: Vec<&OsStr> = cmd.argv.iter().map(OsString::as_os_str).collect();
        let tail: Vec<&str> = ["--noprofile", "--norc", "-o", "pipefail", "-u", "-e", "-c"].into();
        assert_eq!(
            words.get(1..8).map(<[&OsStr]>::to_vec),
            Some(tail.iter().map(OsStr::new).collect::<Vec<_>>()),
            "exec's bash invocation: {words:?}"
        );
        let (program, rest) = cmd.argv.split_first().expect("argv");
        let out = std::process::Command::new(program)
            .args(rest)
            .env_clear()
            .output()?;
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8(out.stdout)?, "<a b><><$HOME>[bash]");

        // `--shell`: `run` is the same command line, the args the shell's
        // positional parameters.
        let init = dir.path().join("init.sh");
        let shell = prepare(Some(&init))?;
        assert_eq!(
            shell.argv.iter().rev().take(3).rev().collect::<Vec<_>>(),
            caller.args.iter().collect::<Vec<_>>()
        );
        for bash in &bashes() {
            let (out, err) = in_shell(bash, &init, &["a b", "", "$HOME"], "run");
            assert_eq!(out, "<a b><><$HOME>[bash]", "{bash}; stderr: {err}");
            let (out, err) = in_shell(bash, &init, &["a b"], "run x");
            assert_eq!(out, "<x>[bash]", "{bash}; stderr: {err}");
        }
        Ok(())
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
