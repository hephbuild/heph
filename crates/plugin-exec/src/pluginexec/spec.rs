use anyhow::Context;
use hplugin::driver::targetdef::FailureOutput;
use hplugin::driver::targetdef::path::{CodegenMode, Content};
use hplugin::htspec::{Spec, SpecEnum, TargetSpecCache};
use std::collections::{BTreeMap, HashMap};

/// The `codegen` attribute as written: absent/null is `None`, which is never a
/// string. Converted into the def's [`CodegenMode`] by [`Codegen::mode`].
#[derive(SpecEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Codegen {
    #[default]
    #[spec(skip)]
    None,
    Copy,
    InPlace,
    InPlaceOnSuccess,
}

impl Codegen {
    pub(crate) fn mode(self) -> CodegenMode {
        match self {
            Codegen::None => CodegenMode::None,
            Codegen::Copy => CodegenMode::Copy,
            Codegen::InPlace => CodegenMode::InPlace {
                write_on_failure: true,
            },
            Codegen::InPlaceOnSuccess => CodegenMode::InPlace {
                write_on_failure: false,
            },
        }
    }
}

/// Exec-driver target config. `#[derive(Spec)]` generates `TargetSpec::from`
/// (parser) and `TargetSpec::schema` (LSP schema) from these fields, so the two
/// can never drift; doc comments below become the schema docs. `codegen` is a
/// [`SpecEnum`](hplugin::htspec::SpecEnum); `cache` is the shared
/// [`TargetSpecCache`] knob (a bool shorthand or a `{enabled, remote, history}`
/// dict).
#[derive(Spec)]
pub(crate) struct TargetSpec {
    /// Command to execute, as an argv list. `$OUT`, `$SRC_<group>`, `$TOOL_<group>` and declared env vars are available.
    ///
    /// An element may carry a `${read://pkg:name}` reference — the contents of
    /// that target's single output, resolved by the host before the command
    /// runs — in **both** modes.
    ///
    /// In exec mode it is the case with no workaround at all: there is no shell
    /// to expand `$SRC_<GROUP>` in. In bash mode it coexists with the shell
    /// because a reference names an *absolute address*: `${read://a:b}` is an
    /// arithmetic error for a set `read` and the empty string for an unset one,
    /// so nobody writes it on purpose, while every form bash actually uses —
    /// `${src:0:3}`, `${src::3}`, `${FOO:-d}` — fails `template::claims` and is
    /// reproduced byte for byte.
    ///
    /// heph substitutes; it does not quote. In bash the value is spliced into a
    /// shell program rather than filling one argv element, so a producer whose
    /// output is `1.2.3; rm -rf /` runs it — and those bytes may have come from
    /// the shared remote cache. See `docs/DEFERRED_VALUES.md`.
    pub run: Vec<hplugin::driver::Deferred<String>>,
    /// Hashed + runtime dependencies, grouped by name → list of target addresses.
    pub deps: HashMap<String, Vec<String>>,
    /// Dependencies that contribute to the input hash but are not materialized at runtime.
    pub hash_deps: HashMap<String, Vec<String>>,
    /// Dependencies materialized at runtime but excluded from the input hash.
    pub runtime_deps: HashMap<String, Vec<String>>,
    /// Names of `deps`/`runtime_deps` groups staged **read-only**: materialized
    /// once into the shared stage and hardlinked into each sandbox instead of
    /// byte-copied per consumer. Use for deps a target only reads and never
    /// mutates (e.g. a hermetic toolchain). No effect under the FUSE sandbox,
    /// which already shares inputs lazily.
    pub read_only_deps: Vec<String>,
    /// Scratch caches this target uses, as a list of `scratch` target addresses.
    /// Each is mounted at the path *its declaration* names, and its declared
    /// `env` variable is set for the run. A scratch is not an input: it is never
    /// materialized from artifacts and never affects this target's cache key.
    pub scratch: Vec<String>,
    /// Credentials this target needs, as a list of `credential` target addresses.
    /// Each is acquired from whatever environment the build is running in and
    /// presented in the shape its declaration names — environment variables,
    /// `0600` files, or a callback helper.
    ///
    /// A credential is not an input: nothing about it reaches this target's cache
    /// key, in either direction. The contract that makes that sound is that a
    /// credential grants *access* and is not an *input* — this target's outputs
    /// must be identical whichever identity satisfied the requirement. A target
    /// whose output depends on *who* ran it is not cacheable, and says so with
    /// `cache = False`.
    pub credentials: Vec<String>,
    /// Build tools, grouped by name → list of target addresses; symlinked under `tools/`.
    pub tools: HashMap<String, Vec<String>>,
    /// Declared outputs, grouped by name → list of output paths the target writes.
    #[spec(rename = "out")]
    pub outputs: HashMap<String, Vec<String>>,
    /// Extra files materialized into the sandbox but not hashed as deps.
    pub support_files: Vec<String>,
    /// What happens to the source tree after a run. Omit for a normal
    /// (non-codegen) target.
    ///
    /// - `copy`: a passing run's outputs are copied into the tree.
    /// - `in_place`: the outputs are written over the sources they were made
    ///   from. A run that exits non-zero (with an exit code from 1 to 127)
    ///   writes back the outputs it produced too, and the target still fails:
    ///   a fixer like `eslint --fix` keeps the fixes it could make. Nothing is
    ///   written after a signal, an exit code of 128 or above, under
    ///   `--frozen` or in a `--shell`.
    /// - `in_place_on_success`: as `in_place`, but a failed run writes
    ///   nothing back.
    ///
    /// An `in_place` transform must be **idempotent**: running it over its own
    /// output must change nothing. The mode is not part of the cache key, so a
    /// result filed after a write-back is served to the rewritten tree.
    pub codegen: Codegen,
    /// What a failed run leaves behind for whoever fixes it, such as test
    /// reports, traces or screenshots. Same shape as `out`: a path, a list, or
    /// a dict of group → list. Paths are relative to the package and may be
    /// globs; `..`, absolute paths and the package root itself are refused.
    ///
    /// Collected only when the run exits non-zero, into the target's failure
    /// slot (`<home>/failures/<addr-key>/out/<workspace-relative path>`). A
    /// path that matches nothing is skipped. Never an output, never an input,
    /// never cached, and not part of the cache key: a passing run collects
    /// nothing. Refused on an entrypoint.
    pub failure_out: HashMap<String, Vec<String>>,
    /// Caching: bool toggles local+remote, or a dict `{enabled, remote, history}`.
    pub cache: TargetSpecCache,
    /// Environment variables set for the command.
    ///
    /// A value may carry a `${read://pkg:name}` reference, resolved by the host
    /// before the command runs; a name may not. The def hash covers the
    /// reference text, and the producer's content reaches the key as for `run`.
    pub env: HashMap<String, hplugin::driver::Deferred<String>>,
    /// Names of host environment variables passed through (hashed). `"*"` passes all.
    pub pass_env: Vec<String>,
    /// Host env vars passed through at runtime only (not hashed). `"*"` passes all.
    pub runtime_pass_env: Vec<String>,
    /// Environment variables set at runtime only (not hashed).
    ///
    /// A value may carry a `${read://pkg:name}` reference. The value's text
    /// stays out of the key, but every reference is a hashed edge, so the
    /// *producer's* content still reaches it: a runtime-only variable fed by a
    /// reference rebuilds when what it reads changes.
    pub runtime_env: HashMap<String, hplugin::driver::Deferred<String>>,
    /// Exec runner this command runs under: a target address producing a
    /// `runner.json`. Absent (or the literal `"local"`) spawns on this host as
    /// before. The runner is a hashed dependency, so changing it changes this
    /// target's cache key.
    pub runner: String,
    /// Make this target an entrypoint: `heph run` invokes it in the caller's
    /// world — their cwd (or the workspace root), their environment, their
    /// terminal — instead of building it. Never cached and never an input.
    /// `deps`, `hash_deps`, `runtime_deps`, `read_only_deps`, `tools`, `env`,
    /// `credentials` and `run` keep their meaning: the closure is laid out as
    /// for a build, in a sandbox of the invocation's own removed when it
    /// exits, and `$SRC_<group>`, `$TOOL_<group>` and `$WORKSPACE_ROOT` name
    /// paths inside it (`$HEPH_ROOT` is the real workspace root). What only a
    /// build uses is refused: `out`, `failure_out`, `cache`, `codegen`, `support_files`, the
    /// `*_env` fields, `runner`, and `scratch` outside `workdir = "sandbox"`. An `exec`
    /// argv cannot name `$SRC_<group>` (there is no shell): use
    /// `${src://pkg:name}`, or `driver = "bash"`.
    pub entrypoint: bool,
    /// With `entrypoint = True`: append the caller's arguments
    /// (`heph run //x -- a b`) to the command (`"$@"` for `bash`). Without it,
    /// trailing arguments are refused before anything builds.
    pub args: bool,
    /// With `entrypoint = True`: where the program runs. `"caller"` (the
    /// caller's cwd, the default), `"tree_root"` (the workspace root, in the
    /// real tree), `"tree_package"` (the target's package dir, in the real
    /// tree), or `"sandbox"` (the invocation's own sandbox package dir, as a
    /// build runs; nothing written there survives the run).
    pub workdir: super::entrypoint_mode::Workdir,
    /// What the target reserves from the resource pools while it runs, e.g.
    /// `{"cpu": 2, "memory": "4G"}`. Defaults to 1 cpu. Never hashed: moved
    /// onto the engine's def, never into exec's own. Refused on an entrypoint.
    pub resources: hplugin::driver::Resources,
    /// How long the run may take, e.g. `"10m"`, `"1h30m"`, or a number of
    /// seconds. The clock covers staging inputs, the process and collecting
    /// outputs. Past it heph cancels the run (the process group is killed;
    /// for an entrypoint on the caller's terminal, only its direct child),
    /// and the target fails as timed out; nothing is cached. Enforced by the
    /// engine, never hashed, never shown to the run: changing it does not
    /// rebuild. Not applied to `--shell`.
    pub timeout: Option<hplugin::driver::Timeout>,
}

/// `failure_out` resolved against the target's package, one entry per group in
/// group order, each path workspace-relative.
///
/// Bound for the engine's def only: never exec's own def, its hash, its
/// outputs, env, argv or sandbox (D19).
pub(crate) fn failure_outputs(
    failure_out: &HashMap<String, Vec<String>>,
    pkg: &str,
) -> anyhow::Result<Vec<FailureOutput>> {
    let groups: BTreeMap<&String, &Vec<String>> = failure_out.iter().collect();
    groups
        .into_iter()
        .map(|(group, paths)| {
            let paths = paths
                .iter()
                .map(|raw| {
                    failure_out_path(raw, pkg).with_context(|| {
                        if group.is_empty() {
                            format!("`failure_out` path {raw:?}")
                        } else {
                            format!("`failure_out` group {group:?}, path {raw:?}")
                        }
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(FailureOutput {
                group: group.clone(),
                paths,
            })
        })
        .collect()
}

/// One `failure_out` path, checked and made workspace-relative.
///
/// Checked on the path as written, before the join: `join_rel_checked` would
/// accept `/etc/x` as `<pkg>/etc/x` and `../sibling` as a path in the parent
/// package. The package root (`.`, or a pattern of nothing but `*`/`**`) is
/// refused too: it would sweep up the whole sandbox package dir.
fn failure_out_path(raw: &str, pkg: &str) -> anyhow::Result<Content> {
    if raw.starts_with('/') {
        anyhow::bail!(
            "an absolute path is not allowed: failure outputs are collected from the sandbox, \
             relative to the package"
        );
    }
    let segments: Vec<&str> = raw
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if segments.contains(&"..") {
        anyhow::bail!("`..` is not allowed: a failure output must be inside the package");
    }
    if segments.iter().all(|s| matches!(*s, "*" | "**")) {
        anyhow::bail!(
            "the package root is not allowed: name the files or directories to keep, \
             e.g. \"test-results/\""
        );
    }
    path_content(raw, pkg)
}

/// `raw`, a path declared in package `pkg`, as the def's workspace-relative
/// [`Content`]: a glob if `raw` has a pattern character, a directory if it
/// ends in `/`, a file otherwise.
///
/// Only `raw` is the user's pattern text: the kind is decided on `raw` alone,
/// and a glob gets the package directory escaped in front of it
/// (`escape_glob_literal`). Classifying the joined path instead made a
/// package such as `app/[slug]` turn every path into a glob whose `[slug]` is
/// a character class, which matched nothing. A package with no `? * [ ]`
/// escapes to itself, so its patterns — and def hashes — are what they
/// always were.
pub(crate) fn path_content(raw: &str, pkg: &str) -> anyhow::Result<Content> {
    if raw.contains(['*', '?', '[']) {
        // `..` pops whole components and escaping never adds a `/`, so the
        // escaped package joins exactly as the literal one would.
        let escaped = hplugin::driver::targetdef::path::escape_glob_literal(pkg);
        return Ok(Content::Glob(hmodel::htpkg::join_rel_checked(
            &escaped, raw,
        )?));
    }
    let path = hmodel::htpkg::join_rel_checked(pkg, raw)?;
    Ok(if path.ends_with('/') {
        Content::DirPath(path)
    } else {
        Content::FilePath(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::htvalue::Value;
    use hcore::htvalue::signature::ParamType;
    use hplugin::driver::DriverField;

    fn make_spec(
        extra: impl IntoIterator<Item = (&'static str, Value)>,
    ) -> anyhow::Result<TargetSpec> {
        let mut m = HashMap::from([("run".to_string(), Value::String("echo".to_string()))]);
        m.extend(extra.into_iter().map(|(k, v)| (k.to_string(), v)));
        TargetSpec::from(&m)
    }

    #[test]
    fn test_schema_lists_known_fields_with_types() {
        let schema = TargetSpec::schema();
        let by_name: HashMap<&str, &DriverField> =
            schema.fields.iter().map(|f| (f.name.as_str(), f)).collect();
        // Every config key TargetSpec::from understands must appear in the schema,
        // so the two lists are caught drifting apart.
        for key in [
            "run",
            "out",
            "cache",
            "support_files",
            "codegen",
            "failure_out",
            "deps",
            "hash_deps",
            "runtime_deps",
            "read_only_deps",
            "tools",
            "env",
            "pass_env",
            "runtime_pass_env",
            "runtime_env",
            "runner",
            "credentials",
            "entrypoint",
            "args",
            "workdir",
        ] {
            assert!(by_name.contains_key(key), "schema missing field `{key}`");
        }
        // Types are unions mirroring the `parse_*` helpers.
        let str_or_list =
            ParamType::union(vec![ParamType::String, ParamType::list(ParamType::String)]);
        assert_eq!(by_name["run"].ty, str_or_list);
        assert_eq!(by_name["run"].ty.render(), "string | list[string]");
        assert_eq!(
            by_name["deps"].ty,
            ParamType::union(vec![
                ParamType::String,
                ParamType::list(ParamType::String),
                ParamType::map(str_or_list.clone()),
            ])
        );
        assert_eq!(
            by_name["env"].ty,
            ParamType::union(vec![ParamType::String, ParamType::map(ParamType::String)])
        );
        // Same shape as `out`: a path, a list, or a group → list dict.
        assert_eq!(by_name["failure_out"].ty, by_name["out"].ty);
        assert!(!by_name["failure_out"].doc.is_empty());
        assert!(!by_name["run"].doc.is_empty());
    }

    #[test]
    fn test_cache_bool_true() {
        let spec = make_spec([("cache", Value::Bool(true))]).unwrap();
        assert!(spec.cache.local);
        assert!(spec.cache.remote);
        assert_eq!(spec.cache.history, 1);
    }

    #[test]
    fn test_cache_bool_false_disables_both() {
        let spec = make_spec([("cache", Value::Bool(false))]).unwrap();
        assert!(!spec.cache.local);
        assert!(!spec.cache.remote);
        assert_eq!(spec.cache.history, 1);
    }

    #[test]
    fn test_cache_default_when_absent() {
        let spec = make_spec([]).unwrap();
        assert!(spec.cache.local);
        assert!(spec.cache.remote);
        assert_eq!(spec.cache.history, 1);
    }

    #[test]
    fn test_cache_dict_partial_keys_use_defaults() {
        // Only `remote` set — `enabled` and `history` fall back to defaults.
        let spec = make_spec([(
            "cache",
            Value::Map(HashMap::from([("remote".to_string(), Value::Bool(false))])),
        )])
        .unwrap();
        assert!(spec.cache.local, "enabled defaults to true");
        assert!(!spec.cache.remote);
        assert_eq!(spec.cache.history, 1);
    }

    #[test]
    fn test_cache_dict_history_honored() {
        let spec = make_spec([(
            "cache",
            Value::Map(HashMap::from([("history".to_string(), Value::Int(5))])),
        )])
        .unwrap();
        assert_eq!(spec.cache.history, 5);
        assert!(spec.cache.local);
        assert!(spec.cache.remote);
    }

    #[test]
    fn test_cache_dict_unknown_key_errors() {
        let err = match make_spec([(
            "cache",
            Value::Map(HashMap::from([("bogus".to_string(), Value::Bool(true))])),
        )]) {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        // `cache` dict is a `SpecStruct`: unknown keys are rejected with the
        // generic "unknown entries found" message.
        assert!(
            format!("{err:#}").contains("unknown entries found"),
            "{err:#}"
        );
    }

    #[test]
    fn test_cache_dict_zero_history_errors() {
        let err = match make_spec([(
            "cache",
            Value::Map(HashMap::from([("history".to_string(), Value::Int(0))])),
        )]) {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains(">= 1"), "{err:#}");
    }

    #[test]
    fn test_pass_env_parsed() {
        let spec = make_spec([(
            "pass_env",
            Value::List(vec![
                Value::String("VAR1".to_string()),
                Value::String("VAR2".to_string()),
            ]),
        )])
        .unwrap();
        assert_eq!(spec.pass_env, vec!["VAR1", "VAR2"]);
    }

    #[test]
    fn test_runtime_pass_env_parsed() {
        let spec = make_spec([(
            "runtime_pass_env",
            Value::List(vec![Value::String("HOME".to_string())]),
        )])
        .unwrap();
        assert_eq!(spec.runtime_pass_env, vec!["HOME"]);
    }

    #[test]
    fn test_runtime_env_parsed() {
        let spec = make_spec([(
            "runtime_env",
            Value::Map(HashMap::from([(
                "KEY".to_string(),
                Value::String("value".to_string()),
            )])),
        )])
        .unwrap();
        assert_eq!(spec.runtime_env.get("KEY").map(|d| d.raw()), Some("value"));
    }

    #[test]
    fn env_and_runtime_env_accept_a_deferred_reference() {
        let env = |r: &str| Value::Map(HashMap::from([("K".to_string(), Value::String(r.into()))]));
        let spec = make_spec([
            ("env", env("${read://a:b}")),
            ("runtime_env", env("prefix-${read://c:d}")),
        ])
        .unwrap();
        assert_eq!(spec.env["K"].raw(), "${read://a:b}");
        assert_eq!(spec.runtime_env["K"].raw(), "prefix-${read://c:d}");
    }

    #[test]
    fn test_pass_env_empty_by_default() {
        let spec = make_spec([]).unwrap();
        assert!(spec.pass_env.is_empty());
        assert!(spec.runtime_pass_env.is_empty());
        assert!(spec.runtime_env.is_empty());
    }

    #[test]
    fn test_tools_parsed() {
        let spec = make_spec([(
            "tools",
            Value::List(vec![Value::String("//some:tool".to_string())]),
        )])
        .unwrap();
        assert_eq!(spec.tools.get(""), Some(&vec!["//some:tool".to_string()]));
    }

    #[test]
    fn test_tools_empty_by_default() {
        let spec = make_spec([]).unwrap();
        assert!(spec.tools.is_empty());
    }

    #[test]
    fn test_support_files_parsed() {
        let spec = make_spec([(
            "support_files",
            Value::List(vec![
                Value::String("foo.txt".to_string()),
                Value::String("data/*.json".to_string()),
                Value::String("subdir/".to_string()),
            ]),
        )])
        .unwrap();
        assert_eq!(
            spec.support_files,
            vec!["foo.txt", "data/*.json", "subdir/"]
        );
    }

    #[test]
    fn test_support_files_empty_by_default() {
        let spec = make_spec([]).unwrap();
        assert!(spec.support_files.is_empty());
    }

    #[test]
    fn test_hash_deps_parsed() {
        let spec = make_spec([(
            "hash_deps",
            Value::List(vec![Value::String("//some:hash".to_string())]),
        )])
        .unwrap();
        assert_eq!(
            spec.hash_deps.get(""),
            Some(&vec!["//some:hash".to_string()])
        );
    }

    #[test]
    fn test_runtime_deps_parsed() {
        let spec = make_spec([(
            "runtime_deps",
            Value::Map(HashMap::from([(
                "rt".to_string(),
                Value::String("//some:rt".to_string()),
            )])),
        )])
        .unwrap();
        assert_eq!(
            spec.runtime_deps.get("rt"),
            Some(&vec!["//some:rt".to_string()])
        );
    }

    #[test]
    fn test_hash_runtime_deps_empty_by_default() {
        let spec = make_spec([]).unwrap();
        assert!(spec.hash_deps.is_empty());
        assert!(spec.runtime_deps.is_empty());
    }

    #[test]
    fn test_tools_grouped() {
        let spec = make_spec([(
            "tools",
            Value::Map(HashMap::from([(
                "cc".to_string(),
                Value::String("//toolchain:gcc".to_string()),
            )])),
        )])
        .unwrap();
        assert_eq!(
            spec.tools.get("cc"),
            Some(&vec!["//toolchain:gcc".to_string()])
        );
    }

    #[test]
    fn test_codegen_in_place_parsed() {
        let spec = make_spec([("codegen", Value::String("in_place".to_string()))]).unwrap();
        assert_eq!(spec.codegen, Codegen::InPlace);
        assert_eq!(
            spec.codegen.mode(),
            CodegenMode::InPlace {
                write_on_failure: true
            }
        );
    }

    #[test]
    fn test_codegen_copy_parsed() {
        let spec = make_spec([("codegen", Value::String("copy".to_string()))]).unwrap();
        assert_eq!(spec.codegen.mode(), CodegenMode::Copy);
    }

    #[test]
    fn codegen_in_place_on_success_parsed() {
        let spec =
            make_spec([("codegen", Value::String("in_place_on_success".to_string()))]).unwrap();
        assert_eq!(spec.codegen, Codegen::InPlaceOnSuccess);
        assert_eq!(
            spec.codegen.mode(),
            CodegenMode::InPlace {
                write_on_failure: false
            }
        );
    }

    #[test]
    fn unknown_codegen_lists_values() {
        let err = match make_spec([("codegen", Value::String("in-place".to_string()))]) {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        // Every value, and only those: `None` is the absent attribute, never a
        // string to write.
        assert!(
            msg.contains("expected one of [copy, in_place, in_place_on_success]"),
            "{msg}"
        );
        assert!(msg.contains("codegen"), "names the field: {msg}");
        assert!(
            msg.contains("\"in-place\""),
            "names what was written: {msg}"
        );
    }

    fn failure_out_of(value: Value, pkg: &str) -> anyhow::Result<Vec<FailureOutput>> {
        let spec = make_spec([("failure_out", value)])?;
        failure_outputs(&spec.failure_out, pkg)
    }

    fn shape(fo: &[FailureOutput]) -> Vec<(String, Vec<String>)> {
        fo.iter()
            .map(|o| {
                (
                    o.group.clone(),
                    o.paths.iter().map(|p| p.to_string()).collect(),
                )
            })
            .collect()
    }

    /// The shape of `out`: a bare path and a list land in the unnamed group,
    /// a dict keeps its groups (sorted); paths become workspace-relative and
    /// keep their kind.
    #[test]
    fn failure_out_shapes() {
        let s = |v: &str| Value::String(v.to_string());
        assert_eq!(
            shape(&failure_out_of(s("junit.xml"), "app").unwrap()),
            [(String::new(), vec!["FilePath(app/junit.xml)".to_string()])]
        );
        assert_eq!(
            shape(
                &failure_out_of(
                    Value::List(vec![s("test-results/"), s("**/*.png"), s("./a/b.txt")]),
                    "app/web",
                )
                .unwrap()
            ),
            [(
                String::new(),
                vec![
                    "DirPath(app/web/test-results/)".to_string(),
                    "Glob(app/web/**/*.png)".to_string(),
                    "FilePath(app/web/a/b.txt)".to_string(),
                ]
            )]
        );
        let dict = Value::Map(HashMap::from([
            ("report".to_string(), s("playwright-report/")),
            ("junit".to_string(), Value::List(vec![s("junit.xml")])),
        ]));
        assert_eq!(
            shape(&failure_out_of(dict, "").unwrap()),
            [
                ("junit".to_string(), vec!["FilePath(junit.xml)".to_string()]),
                (
                    "report".to_string(),
                    vec!["DirPath(playwright-report/)".to_string()]
                ),
            ]
        );
        assert!(failure_outputs(&HashMap::new(), "app").unwrap().is_empty());
    }

    /// In a package like `app/[slug]` the kind is decided on the path as
    /// written: a plain file stays a file, and a glob escapes the package so
    /// `[slug]` is not read as a character class.
    #[test]
    fn failure_out_in_a_metachar_package() {
        let s = |v: &str| Value::String(v.to_string());
        assert_eq!(
            shape(
                &failure_out_of(
                    Value::List(vec![s("junit.xml"), s("out/"), s("*.png")]),
                    "app/[slug]",
                )
                .unwrap()
            ),
            [(
                String::new(),
                vec![
                    "FilePath(app/[slug]/junit.xml)".to_string(),
                    "DirPath(app/[slug]/out/)".to_string(),
                    "Glob(app/[[]slug[]]/*.png)".to_string(),
                ]
            )]
        );
    }

    /// D4: a failure output stays inside the package, and is never the
    /// package itself. Checked on the path as written: `join_rel_checked`
    /// alone would turn `/etc/x` into `app/etc/x` and accept `../x` as a file
    /// of the parent package.
    #[test]
    fn failure_out_refuses_escaping_and_root_paths() {
        let cases: &[(&str, &str)] = &[
            ("../x", "`..`"),
            ("a/../../x", "`..`"),
            ("a/../b", "`..`"),
            ("..", "`..`"),
            ("/etc/passwd", "absolute"),
            ("/", "absolute"),
            ("//x", "absolute"),
            (".", "package root"),
            ("./", "package root"),
            ("", "package root"),
            ("*", "package root"),
            ("**", "package root"),
            ("./**/*", "package root"),
        ];
        for (raw, why) in cases {
            for pkg in ["", "app"] {
                let err = failure_out_of(Value::String(raw.to_string()), pkg)
                    .err()
                    .unwrap_or_else(|| panic!("{raw:?} in {pkg:?} must be refused"));
                let msg = format!("{err:#}");
                assert!(msg.contains(why), "{raw:?}: {msg}");
                assert!(msg.contains("failure_out"), "names the field: {msg}");
                assert!(msg.contains(&format!("{raw:?}")), "names the path: {msg}");
                if *why == "package root" {
                    assert!(
                        msg.contains("\"test-results/\""),
                        "says what to write: {msg}"
                    );
                }
            }
        }
        // A group is named in the error.
        let err = failure_out_of(
            Value::Map(HashMap::from([(
                "report".to_string(),
                Value::String("../x".to_string()),
            )])),
            "app",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("\"report\""), "{err:#}");

        // Near misses stay accepted: a dotted name, a pattern under a dir.
        for ok in ["..foo", "a..b/c", ".hidden/", "out/*", "a/**"] {
            failure_out_of(Value::String(ok.to_string()), "app")
                .unwrap_or_else(|e| panic!("{ok:?} must be accepted: {e:#}"));
        }
    }

    #[test]
    fn test_codegen_link_now_errors() {
        let err = match make_spec([("codegen", Value::String("link".to_string()))]) {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        // `codegen` is now a `SpecEnum`: an unknown string lists the valid
        // variants and the field name (from the `parse \`codegen\`` context).
        let msg = format!("{err:#}");
        assert!(msg.contains("codegen"), "{msg}");
        assert!(msg.contains("expected one of"), "{msg}");
        assert!(msg.contains("link"), "{msg}");
    }

    #[test]
    fn test_codegen_default_none() {
        let spec = make_spec([]).unwrap();
        assert_eq!(spec.codegen.mode(), CodegenMode::None);
    }
}
