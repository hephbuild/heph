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
//!
//! TODO(entrypoint-L3): this whole module is the prepare hook the engine and
//! the binary still call. An entrypoint now runs through the driver's own
//! `run`/`run_shell` with `RunOptions`; the parse side lives in
//! [`super::entrypoint_mode`]. Remove this module with the engine's
//! `PreparedInvocation` and the binary's spawn.

use super::TargetDef;
pub use super::entrypoint_mode::{EntrypointMode, Workdir};
use anyhow::Context;
use hplugin::driver::Deferred;
use hplugin::driver::entrypoint::{
    Caller, EntrypointDriver, Invocation, PrepareRequest, PreparedCommand, StagedInput,
};
use hplugin::driver::targetdef::TargetDef as EngineTargetDef;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

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
                let rendered = super::render_args_shell_init(&words)?;
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
            Workdir::Sandbox => anyhow::bail!(
                "`workdir = \"sandbox\"` on {} runs through the driver's `run`, not this hook",
                req.target.addr.format()
            ),
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
    use std::collections::HashMap;
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
