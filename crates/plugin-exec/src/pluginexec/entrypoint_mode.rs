//! `entrypoint = True` on an `exec` or `bash` target, as `parse` sees it.
//!
//! An entrypoint is *invoked* in the caller's world rather than built: `heph
//! run` builds its closure, then runs it through this driver's ordinary
//! `run`/`run_shell` with the caller's cwd, environment, args and terminal
//! carried in the request's generic `RunOptions`. Nothing in `run` asks whether
//! a target is an entrypoint; only `parse` knows the word. It
//!
//! - validates the options only an entrypoint takes (`args`, `workdir`) and
//!   refuses the ones that make no sense for it, naming the field, the target
//!   and what to do instead ([`refuse_fields`]);
//! - records an [`EntrypointMode`] on exec's own def (hashed when present,
//!   absent from hash and serde when not, so no other def moves);
//! - reports the engine's [`Entrypoint`](hplugin::driver::targetdef::Entrypoint)
//!   attribute ([`EntrypointMode::engine_attribute`]).

use hcore::htvalue::Value;
use std::collections::HashMap;

/// Where the program runs. The engine resolves it into the run's cwd.
#[derive(
    Clone, Copy, Debug, Default, Hash, PartialEq, Eq, serde::Serialize, crate::htspec::SpecEnum,
)]
pub enum Workdir {
    /// The caller's cwd, captured raw at startup. May be outside the workspace.
    #[default]
    Caller,
    /// The workspace root.
    Root,
    /// The invocation's own sandbox package dir, as a build runs in. Nothing
    /// written there survives the run.
    Sandbox,
}

/// What `entrypoint = True` adds to an exec def. Present only with the flag,
/// so a def without it hashes and serializes as it always did; with it, all of
/// it is hashed (R15).
#[derive(Clone, Debug, Hash, serde::Serialize)]
pub struct EntrypointMode {
    /// Append the caller's arguments to the command.
    pub args: bool,
    pub workdir: Workdir,
}

impl EntrypointMode {
    /// What the engine reads off the def (`TargetDef::entrypoint`).
    pub(crate) fn engine_attribute(&self) -> hplugin::driver::targetdef::Entrypoint {
        use hplugin::driver::targetdef::Workdir as EngineWorkdir;
        hplugin::driver::targetdef::Entrypoint::new(
            self.args,
            match self.workdir {
                Workdir::Caller => EngineWorkdir::Caller,
                Workdir::Root => EngineWorkdir::Root,
                Workdir::Sandbox => EngineWorkdir::Sandbox,
            },
        )
    }
}

/// Fields that mean something only on an entrypoint; refused without the flag.
pub(crate) const ENTRYPOINT_ONLY_FIELDS: &[&str] = &["args", "workdir"];

/// Fields an entrypoint refuses, each with why and what to do instead. The
/// error names the field and the target.
const REFUSED_FIELDS: &[(&str, &str)] = &[
    (
        "out",
        "an entrypoint has no outputs: it runs in your world, and nothing it writes is collected \
         or cached. Drop `out`; to produce files for other targets, make a separate build target",
    ),
    ("cache", "an entrypoint is never cached. Drop `cache`"),
    (
        "codegen",
        "an entrypoint has no outputs to write back. Drop `codegen`, or make the generator a \
         build target with `codegen`",
    ),
    (
        "support_files",
        "an entrypoint collects nothing, so it has no support files. Reach a file through a dep \
         (`deps` and `$SRC_<group>`) instead",
    ),
    (
        "read_only_deps",
        "an entrypoint gets its own writable copy of every input, so a tool may write next to \
         what it reads without touching heph's shared stage. Drop `read_only_deps`",
    ),
    (
        "pass_env",
        "an entrypoint already runs with the caller's whole environment. Drop `pass_env`",
    ),
    (
        "runtime_pass_env",
        "an entrypoint already runs with the caller's whole environment. Drop `runtime_pass_env`",
    ),
    (
        "runtime_env",
        "an entrypoint is never hashed, so `env` is already runtime-only. Move these into `env`",
    ),
    (
        "runner",
        "not supported yet: an entrypoint under a runner needs the runner protocol to carry the \
         caller's environment. Drop `runner`; the workspace default runner does not apply to an \
         entrypoint",
    ),
];

/// Refuse the fields an entrypoint does not take, by name, before the generic
/// parser sees them.
pub(crate) fn refuse_fields(
    config: &HashMap<String, Value>,
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
    // A scratch mounts in the sandbox package dir, which is the working
    // directory only in sandbox mode (E21).
    let sandbox_workdir = matches!(config.get("workdir"), Some(Value::String(w)) if w == "sandbox");
    if config.contains_key("scratch") && !sandbox_workdir {
        anyhow::bail!(
            "`scratch` is not accepted on {addr}, an entrypoint `{driver}` target \
             (`entrypoint = True`), unless it runs in its sandbox: a scratch is mounted in the \
             sandbox package dir, which is the working directory only with \
             `workdir = \"sandbox\"`. Set that, or drop `scratch`"
        );
    }
    Ok(())
}

/// The variables exec's environment defines and an argv cannot expand: exec
/// has no shell, so `$SRC_x` in a `run` element reaches the program as those
/// literal bytes.
const CLOSURE_VARS: [&str; 3] = ["SRC", "TOOL", "OUT"];

/// Refuse a `run` element that names a closure variable as shell syntax
/// (`$SRC_x`, `${TOOL_x}`, `$OUT`) in an exec entrypoint's argv (R19). A
/// sandboxed exec target has the same limitation, but its literal shows up as
/// a failed build; an entrypoint hands it to a tool in the caller's world.
///
/// Matched: a `$`, an optional `{`, then `SRC`, `TOOL` or `OUT` ending the name
/// or followed by `_` (so `$SRC` itself, the default group's variable, is
/// refused too, while `$SRCDIR` is someone else's text).
pub(crate) fn refuse_literal_closure_vars(
    run: &[hplugin::driver::Deferred<String>],
    addr: &str,
) -> anyhow::Result<()> {
    for (i, word) in run.iter().enumerate() {
        if let Some(var) = literal_closure_var(word.raw()) {
            anyhow::bail!(
                "`run[{i}]` of {addr}, an entrypoint `exec` target, names `{var}`. exec passes \
                 its argv as written, with no shell of its own, and heph cannot tell a `{var}` \
                 meant for a shell you start (`sh -c`) from literal text handed to the \
                 program — so it is refused even inside `sh -c`. Use \
                 `${{src://<pkg>:<name>}}` for a dep's path, or `driver = \"bash\"`, where \
                 the shell expands `$SRC_<group>` (single-quote a literal, as in awk's \
                 '$OUT')"
            );
        }
    }
    Ok(())
}

/// The first closure-variable reference in `s`, as written, if any.
fn literal_closure_var(s: &str) -> Option<&str> {
    for (dollar, _) in s.match_indices('$') {
        let after = s.get(dollar + 1..)?;
        let (braced, name_start) = match after.strip_prefix('{') {
            Some(rest) => (true, rest),
            None => (false, after),
        };
        for var in CLOSURE_VARS {
            let Some(rest) = name_start.strip_prefix(var) else {
                continue;
            };
            let ends_name = match rest.chars().next() {
                None => true,
                Some(c) => c == '_' || !(c.is_ascii_alphanumeric()),
            };
            if !ends_name {
                continue;
            }
            // The reference as written, up to the end of its name.
            let name_len = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let end = dollar + 1 + usize::from(braced) + var.len() + name_len;
            return s.get(dollar..end);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;
    use hcore::hasync::StdCancellationToken;
    use hdriver_support::driver_managed::ManagedDriver as _;
    use hmodel::htaddr::parse_addr;
    use hplugin::driver::ParseRequest;
    use hplugin::driver::targetdef::{TargetDef as EngineTargetDef, Workdir as EngineWorkdir};
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

    async fn parse_on(
        d: &super::super::Driver,
        config: HashMap<String, Value>,
    ) -> anyhow::Result<EngineTargetDef> {
        let req = ParseRequest {
            request_id: "t".to_string(),
            target_spec: Arc::new(hplugin::provider::TargetSpec {
                addr: parse_addr("//tools:go")?,
                driver: d.name.clone(),
                config,
                labels: vec!["shim".to_string()],
                ..Default::default()
            }),
        };
        Ok(d.parse(req, &StdCancellationToken::new()).await?.target_def)
    }

    /// Parse `//tools:go` on `driver_name` with `run = ["go"]` plus `extra`.
    async fn parse(
        driver_name: &str,
        extra: Vec<(&str, Value)>,
    ) -> anyhow::Result<EngineTargetDef> {
        let mut config = HashMap::from([("run".to_string(), Value::List(vec![s("go")]))]);
        config.extend(extra.into_iter().map(|(k, v)| (k.to_string(), v)));
        parse_on(&driver(driver_name), config).await
    }

    fn flag() -> (&'static str, Value) {
        ("entrypoint", Value::Bool(true))
    }

    /// Every row of the refusal table, on exec and its bash twin: each error
    /// names the field, the target and what to do (E16, R20).
    #[tokio::test]
    async fn entrypoint_parse_refusals() {
        /// The extra config, and what the error must contain.
        type Row = (Vec<(&'static str, Value)>, &'static [&'static str]);
        let rows: Vec<Row> = vec![
            (
                vec![flag(), ("out", s("bin/x"))],
                &["`out`", "no outputs", "Drop `out`"],
            ),
            (
                vec![flag(), ("cache", Value::Bool(true))],
                &["`cache`", "never cached", "Drop"],
            ),
            (
                vec![flag(), ("codegen", s("copy"))],
                &["`codegen`", "Drop `codegen`"],
            ),
            (
                vec![flag(), ("support_files", Value::List(vec![s("x")]))],
                &["`support_files`", "through a dep"],
            ),
            (
                vec![flag(), ("read_only_deps", Value::List(vec![s("d")]))],
                &["`read_only_deps`", "writable copy", "Drop `read_only_deps`"],
            ),
            (
                vec![flag(), ("pass_env", Value::List(vec![s("HOME")]))],
                &["`pass_env`", "whole environment", "Drop `pass_env`"],
            ),
            (
                vec![flag(), ("runtime_pass_env", Value::List(vec![s("HOME")]))],
                &["`runtime_pass_env`", "whole environment"],
            ),
            (
                vec![
                    flag(),
                    (
                        "runtime_env",
                        Value::Map(HashMap::from([("A".to_string(), s("1"))])),
                    ),
                ],
                &["`runtime_env`", "Move these into `env`"],
            ),
            (
                vec![flag(), ("runner", s("//tools/devenv:runner"))],
                &["`runner`", "not supported yet", "Drop `runner`"],
            ),
            (
                vec![flag(), ("scratch", Value::List(vec![s("//c:gocache")]))],
                &["`scratch`", "workdir = \"sandbox\""],
            ),
            (
                vec![
                    flag(),
                    ("scratch", Value::List(vec![s("//c:gocache")])),
                    ("workdir", s("root")),
                ],
                &["`scratch`", "workdir = \"sandbox\""],
            ),
            (
                vec![flag(), ("workdir", s("nonsense"))],
                &["workdir", "nonsense", "caller", "root", "sandbox"],
            ),
            (vec![flag(), ("workdir", Value::Bool(true))], &["workdir"]),
            (vec![flag(), ("args", s("yes"))], &["args"]),
            (vec![flag(), ("unknown_field", s("x"))], &["unknown_field"]),
            (vec![flag(), ("run", Value::Int(3))], &["run"]),
            (vec![flag(), ("env", Value::List(vec![s("x")]))], &["env"]),
            // Without the flag (absent or `False`), `args` and `workdir` point
            // at it.
            (
                vec![("args", Value::Bool(true))],
                &["`args`", "entrypoint = True"],
            ),
            (
                vec![("workdir", s("root"))],
                &["`workdir`", "entrypoint = True"],
            ),
            (
                vec![
                    ("entrypoint", Value::Bool(false)),
                    ("args", Value::Bool(true)),
                ],
                &["`args`", "entrypoint = True"],
            ),
            (
                vec![
                    ("entrypoint", Value::Bool(false)),
                    ("workdir", s("sandbox")),
                ],
                &["`workdir`", "entrypoint = True"],
            ),
            // A flag that is not a bool.
            (
                vec![("entrypoint", s("yes"))],
                &["`entrypoint`", "True` or `False`"],
            ),
            (
                vec![("entrypoint", Value::Int(1))],
                &["`entrypoint`", "True` or `False`"],
            ),
        ];
        for driver_name in ["exec", "bash"] {
            for (extra, needles) in &rows {
                let err = match parse(driver_name, extra.clone()).await {
                    Ok(_) => panic!("{driver_name}: {extra:?} must be refused"),
                    Err(e) => format!("{e:#}"),
                };
                assert!(
                    err.contains("//tools:go"),
                    "{driver_name} {extra:?}: names the target: {err}"
                );
                for needle in *needles {
                    assert!(
                        err.contains(needle),
                        "{driver_name} {extra:?}: {needle:?} missing from: {err}"
                    );
                }
            }
        }
    }

    /// What an entrypoint does accept, on both drivers: credentials (E18),
    /// `scratch` in sandbox mode (E21), every workdir, and `entrypoint = False`
    /// as no flag at all.
    #[tokio::test]
    async fn entrypoint_parse_accepts() -> anyhow::Result<()> {
        let rows: Vec<Vec<(&str, Value)>> = vec![
            vec![flag(), ("credentials", Value::List(vec![s("//auth:gh")]))],
            vec![
                flag(),
                ("scratch", Value::List(vec![s("//c:gocache")])),
                ("workdir", s("sandbox")),
            ],
            vec![flag(), ("workdir", s("caller"))],
            vec![flag(), ("workdir", s("root"))],
            vec![flag(), ("args", Value::Bool(false))],
            vec![
                flag(),
                (
                    "env",
                    Value::Map(HashMap::from([("A".to_string(), s("1"))])),
                ),
            ],
            vec![("entrypoint", Value::Bool(false))],
        ];
        for driver_name in ["exec", "bash"] {
            for extra in &rows {
                parse(driver_name, extra.clone())
                    .await
                    .with_context(|| format!("{driver_name}: {extra:?} must parse"))?;
            }
        }
        Ok(())
    }

    /// R19 on an exec entrypoint: a closure variable written as shell syntax
    /// is refused with what to do instead; bash, which expands it, and a
    /// non-entrypoint exec target are untouched.
    #[tokio::test]
    async fn entrypoint_exec_refuses_literal_closure_vars() -> anyhow::Result<()> {
        let run = |w: &str| ("run", Value::List(vec![s("go"), s(w)]));
        for word in ["$SRC_sdk/bin/go", "${TOOL_x}", "--out=$OUT", "$SRC"] {
            let err = match parse("exec", vec![flag(), run(word)]).await {
                Ok(_) => panic!("exec: {word:?} must be refused"),
                Err(e) => format!("{e:#}"),
            };
            for needle in ["//tools:go", "`run[1]`", "${src://", "driver = \"bash\""] {
                assert!(err.contains(needle), "{word:?}: {needle:?} missing: {err}");
            }
            parse("bash", vec![flag(), run(word)]).await?;
            parse("exec", vec![run(word)]).await?;
        }
        parse("exec", vec![flag(), run("${src://tools:sdk}/bin/go")]).await?;
        Ok(())
    }

    /// `parse` reports the engine attribute with `accepts_args` and `workdir`,
    /// and forces the cache off; a flag-off target leaves it `None` (E4).
    #[tokio::test]
    async fn entrypoint_sets_target_def_attribute() -> anyhow::Result<()> {
        for driver_name in ["exec", "bash"] {
            for (extra, want) in [
                (vec![], (false, EngineWorkdir::Caller)),
                (
                    vec![("args", Value::Bool(true))],
                    (true, EngineWorkdir::Caller),
                ),
                (vec![("workdir", s("root"))], (false, EngineWorkdir::Root)),
                (
                    vec![("workdir", s("sandbox")), ("args", Value::Bool(true))],
                    (true, EngineWorkdir::Sandbox),
                ),
            ] {
                let mut extra = extra;
                extra.push(flag());
                let def = parse(driver_name, extra).await?;
                let ep = def.entrypoint.expect("an entrypoint");
                assert_eq!((ep.accepts_args, ep.workdir), want, "{driver_name}");
                assert!(!def.cache.enabled, "{driver_name}: never cached");
            }
            let off = parse(driver_name, vec![("entrypoint", Value::Bool(false))]).await?;
            assert!(off.entrypoint.is_none(), "{driver_name}");
            let absent = parse(driver_name, vec![]).await?;
            assert!(absent.entrypoint.is_none(), "{driver_name}");
        }
        Ok(())
    }

    /// The workspace `runner:` default never applies to an entrypoint.
    #[tokio::test]
    async fn default_runner_not_applied_to_entrypoint() -> anyhow::Result<()> {
        let mut d = super::super::Driver::new_exec();
        d.default_runner = Some("//tools/devenv:runner".to_string());
        let def = parse_on(
            &d,
            HashMap::from([
                ("run".to_string(), Value::List(vec![s("go")])),
                ("entrypoint".to_string(), Value::Bool(true)),
            ]),
        )
        .await?;
        assert!(def.inputs.iter().all(|i| i.origin_id != "runner"));
        assert_eq!(
            def.labels,
            vec!["shim".to_string()],
            "labels are not parse's to refuse"
        );
        Ok(())
    }

    /// R1: an entrypoint's transitive tools are copied into its sandbox, never
    /// linked from the shared stage; every other target keeps the link.
    #[tokio::test]
    async fn entrypoint_tools_are_copied_not_linked() -> anyhow::Result<()> {
        use hplugin::driver::ApplyTransitiveRequest;
        let tool = hplugin::driver::sandbox::Tool {
            r#ref: hplugin::driver::TargetAddr::parse(
                "//tools:node",
                &hmodel::htpkg::PkgBuf::from(""),
            )?,
            id: "t0".to_string(),
            group: "node".to_string(),
            hash: true,
        };
        for (flag_on, want_link) in [(true, false), (false, true)] {
            let d = driver("exec");
            let mut extra = vec![];
            if flag_on {
                extra.push(flag());
            }
            let def = parse("exec", extra).await?;
            let mut sandbox = hplugin::driver::sandbox::Sandbox::default();
            sandbox.push_tool(tool.clone());
            let res = d
                .apply_transitive(
                    ApplyTransitiveRequest {
                        request_id: "t".to_string(),
                        target_def: def,
                        sandbox,
                    },
                    &StdCancellationToken::new(),
                )
                .await?;
            let input = res
                .target_def
                .inputs
                .iter()
                .find(|i| i.origin_id == "t0")
                .expect("the tool input");
            assert_eq!(
                hdriver_support::stage::is_read_only(&input.annotations),
                want_link,
                "entrypoint = {flag_on}"
            );
        }
        Ok(())
    }

    /// R19's adversarial rows. `None` means the element is accepted.
    #[test]
    fn literal_closure_var_rows() {
        let rows: &[(&str, Option<&str>)] = &[
            ("$SRC_x", Some("$SRC_x")),
            ("${SRC_x}", Some("${SRC_x")),
            ("$$SRC_x", Some("$SRC_x")),
            ("\\$SRC_x", Some("$SRC_x")),
            ("$SRC", Some("$SRC")),
            ("${SRC}", Some("${SRC")),
            ("$TOOL_go", Some("$TOOL_go")),
            ("$OUT", Some("$OUT")),
            ("$OUT_bin", Some("$OUT_bin")),
            ("--root=$SRC_x/lib", Some("$SRC_x")),
            ("prefix$TOOL_a", Some("$TOOL_a")),
            ("$SRC_é", Some("$SRC_")),
            ("é$SRC_x", Some("$SRC_x")),
            ("$SRCDIR", None),
            ("$SOURCE", None),
            ("$src_x", None),
            ("SRC_x", None),
            ("${src://pkg:name}", None),
            ("${read://pkg:name}", None),
            ("$", None),
            ("${", None),
            ("", None),
            ("$HOME/$TOOLS", None),
        ];
        for (input, want) in rows {
            assert_eq!(literal_closure_var(input), *want, "input {input:?}");
        }
    }
}
