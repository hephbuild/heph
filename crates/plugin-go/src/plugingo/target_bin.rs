use crate::plugingo::addr_util::{
    go_build_env, go_host_pass_env_config, go_run_prelude, go_sdk_dep, go_sdk_read_only_config,
    import_path_to_dep_group, to_run_value, write_importcfg_script,
};
use crate::plugingo::factors::Factors;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hplugin::provider::TargetSpec;
use std::collections::{BTreeMap, HashMap};

/// Extra link configuration plumbed onto the `build` (binary) target from a
/// package's `provider_state(provider="go", link={...})`.
///
/// - `flags`: passed verbatim to `go tool link` (e.g. `-X main.version=...`).
/// - `deps`: target addresses staged into the link sandbox (hashed inputs) so
///   `flags` can reference their outputs.
/// - `runtime_deps`: target addresses that travel with the binary at run time
///   only (not hashed — they do not invalidate the link cache).
///
/// `deps`/`runtime_deps` are grouped: a plain list lands in the default group
/// (`""`), while a map lets the BUILD author name groups explicitly. The default
/// (empty) group is a plugin-controlled bucket emitted under the namespaced
/// `link_deps`/`link_runtime_deps` key; user-named groups are emitted verbatim,
/// keeping the userland dep-group namespace flat. The plugin's own groups all
/// carry namespaced prefixes (`lib_*`, `gosdk`, `link_deps*`), so a verbatim
/// user group only collides if it deliberately reuses one of those.
#[derive(Debug, Default, Clone)]
pub struct LinkConfig {
    pub flags: Vec<String>,
    pub deps: BTreeMap<String, Vec<String>>,
    pub runtime_deps: BTreeMap<String, Vec<String>>,
}

/// Map a link dep group name onto the target's dep-group key. The default
/// (empty) group is the plugin-controlled bucket and takes the namespaced
/// `prefix` (`link_deps`/`link_runtime_deps`); a user-named group is used
/// verbatim so authors own a flat namespace.
fn link_group_key(prefix: &str, group: &str) -> String {
    if group.is_empty() {
        prefix.to_string()
    } else {
        group.to_string()
    }
}

pub fn build_spec(
    addr: Addr,
    import_path: &str,
    factors: &Factors,
    // All transitive libs including own build_lib at index 0
    transitive_libs: &[(String, Addr)],
    link: &LinkConfig,
    go_version: &str,
) -> TargetSpec {
    let binary_name = import_path
        .rsplit('/')
        .next()
        .unwrap_or(import_path)
        .to_string();

    let mut run = go_run_prelude(go_version);
    run.extend(generate_link_script(
        import_path,
        &binary_name,
        transitive_libs,
        &link.flags,
    ));

    let mut deps: BTreeMap<String, Value> = transitive_libs
        .iter()
        .map(|(dep_import_path, dep_addr)| {
            let group = import_path_to_dep_group(dep_import_path);
            (group, Value::List(vec![Value::String(dep_addr.format())]))
        })
        .collect();
    if let Some((sdk_group, sdk_val)) = go_sdk_dep(go_version) {
        deps.insert(sdk_group, sdk_val);
    }
    // Link deps from provider_state. The default (empty) group is the
    // plugin-controlled `link_deps` bucket, namespaced away from userland;
    // user-named groups land verbatim.
    for (group, addrs) in &link.deps {
        if addrs.is_empty() {
            continue;
        }
        deps.insert(
            link_group_key("link_deps", group),
            Value::List(addrs.iter().cloned().map(Value::String).collect()),
        );
    }

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("run".to_string(), to_run_value(run));
    config.insert(
        "deps".to_string(),
        Value::Map(deps.into_iter().collect::<HashMap<_, _>>()),
    );
    // Runtime-only link deps: staged with the binary at run time, excluded from
    // the link's def hash (pluginexec excludes runtime_deps from the hash).
    let runtime_deps: HashMap<String, Value> = link
        .runtime_deps
        .iter()
        .filter(|(_, addrs)| !addrs.is_empty())
        .map(|(group, addrs)| {
            (
                link_group_key("link_runtime_deps", group),
                Value::List(addrs.iter().cloned().map(Value::String).collect()),
            )
        })
        .collect();
    if !runtime_deps.is_empty() {
        config.insert("runtime_deps".to_string(), Value::Map(runtime_deps));
    }
    if let Some((ro_k, ro_v)) = go_sdk_read_only_config(go_version) {
        config.insert(ro_k, ro_v);
    }
    if let Some((pe_k, pe_v)) = go_host_pass_env_config(go_version) {
        config.insert(pe_k, pe_v);
    }
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(
            String::new(),
            Value::List(vec![Value::String(binary_name)]),
        )])),
    );
    config.insert(
        "runtime_env".to_string(),
        Value::Map(HashMap::from([
            ("GOOS".to_string(), Value::String(factors.goos.clone())),
            ("GOARCH".to_string(), Value::String(factors.goarch.clone())),
        ])),
    );
    // CGO/toolchain pins live in `env` (hashed) so stale archives don't survive
    // cache lookups (pluginexec/mod.rs:70 excludes runtime_env from the def hash).
    config.insert("env".to_string(), go_build_env());

    TargetSpec {
        addr,
        driver: "sh".to_string(),
        config,
        ..Default::default()
    }
}

fn generate_link_script(
    self_import_path: &str,
    binary_name: &str,
    transitive_libs: &[(String, Addr)],
    link_flags: &[String],
) -> Vec<String> {
    // The main package is passed as a positional arg to the linker, not via importcfg.
    let mut lines = write_importcfg_script(transitive_libs, Some(self_import_path));
    let self_group = import_path_to_dep_group(self_import_path);
    let self_env_var = format!("SRC_{}", self_group.to_uppercase());
    // User link flags from provider_state, inserted verbatim before `-o`.
    let flags = if link_flags.is_empty() {
        String::new()
    } else {
        format!(" {}", link_flags.join(" "))
    };
    // -buildmode=pie matches Go's default on darwin/arm64 (and most modern
    // platforms). Libs were compiled with -shared, so the link must agree on
    // PIE — otherwise asm relocations from transitive deps fail to resolve.
    lines.push(format!(
        "\"$GO\" tool link -importcfg \"$importcfg\" -buildmode=pie{flags} -o {} \"${}\"",
        binary_name, self_env_var
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmodel::htpkg::PkgBuf;

    /// Go version under test.
    const V: &str = crate::plugingo::toolchain::DEFAULT_GO_VERSION;

    fn run_str(spec: &TargetSpec) -> String {
        match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
            Value::List(v) => v
                .iter()
                .map(|x| match x {
                    Value::String(s) => s.as_str(),
                    _ => panic!("run entry not a string"),
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => panic!("run not string or list"),
        }
    }

    fn test_factors() -> Factors {
        Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
        }
    }

    fn test_addr() -> Addr {
        Addr::new(PkgBuf::from("cmd"), "build".to_string(), Default::default())
    }

    fn lib_addr(pkg: &str) -> Addr {
        Addr::new(
            PkgBuf::from(pkg),
            "build_lib".to_string(),
            Default::default(),
        )
    }

    #[test]
    fn test_driver_is_bash() {
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            &LinkConfig::default(),
            V,
        );
        assert_eq!(spec.driver, "sh");
    }

    #[test]
    fn test_binary_name_from_import_path() {
        let spec = build_spec(
            test_addr(),
            "example.com/myapp/cmd",
            &test_factors(),
            &[],
            &LinkConfig::default(),
            V,
        );
        let out = match spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        let files = match out.get("").unwrap() {
            Value::List(v) => v,
            _ => panic!(),
        };
        assert!(matches!(&files[0], Value::String(s) if s == "cmd"));
    }

    #[test]
    fn test_run_has_go_tool_link() {
        let libs = vec![
            ("example.com/cmd".to_string(), lib_addr("cmd")),
            ("fmt".to_string(), lib_addr("@heph/go/std/fmt")),
        ];
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &libs,
            &LinkConfig::default(),
            V,
        );
        let run = run_str(&spec);
        assert!(run.contains("tool link"));
        assert!(run.contains("importcfg"));
    }

    #[test]
    fn test_run_uses_hermetic_go() {
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            &LinkConfig::default(),
            V,
        );
        let run = run_str(&spec);
        assert!(
            run.contains("\"$GO\""),
            "link script must invoke the hermetic \"$GO\": {run}"
        );
        assert!(
            run.contains(&format!(
                "$WORKSPACE_ROOT/{}",
                crate::plugingo::toolchain::staged_goroot(V)
            )),
            "GOROOT must point at the staged hermetic SDK: {run}"
        );
        assert!(
            !run.contains("TOOL_GO"),
            "must not use the host go tool: {run}"
        );
    }

    #[test]
    fn test_deps_include_hermetic_sdk() {
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            &LinkConfig::default(),
            V,
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            deps.contains_key(crate::plugingo::addr_util::GO_SDK_DEP_GROUP),
            "bin link must dep on the hermetic SDK: {deps:?}"
        );
        assert!(
            !spec.config.contains_key("tools"),
            "no host tool should be referenced"
        );
    }

    #[test]
    fn test_env_pins_cgo_disabled() {
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            &LinkConfig::default(),
            V,
        );
        let env = match spec.config.get("env").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            matches!(env.get("CGO_ENABLED"), Some(Value::String(s)) if s == "0"),
            "env must pin CGO_ENABLED=0 in the hashed map: {:?}",
            env.get("CGO_ENABLED")
        );
    }

    #[test]
    fn test_link_flags_threaded_into_link_command() {
        let link = LinkConfig {
            flags: vec!["-X main.version=1.2.3".to_string(), "-s".to_string()],
            ..Default::default()
        };
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            &link,
            V,
        );
        let run = run_str(&spec);
        let link_line = run
            .lines()
            .find(|l| l.contains("tool link"))
            .expect("link line present");
        assert!(
            link_line.contains("-X main.version=1.2.3") && link_line.contains(" -s "),
            "link flags must be threaded into the link command: {link_line}"
        );
        let flags_pos = link_line.find("-X main.version").unwrap();
        let o_pos = link_line.find(" -o ").unwrap();
        assert!(flags_pos < o_pos, "flags must precede -o: {link_line}");
    }

    #[test]
    fn test_link_deps_added_to_deps_map() {
        let link = LinkConfig {
            deps: BTreeMap::from([(String::new(), vec!["//some:target".to_string()])]),
            ..Default::default()
        };
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            &link,
            V,
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        let group = match deps.get("link_deps").expect("link_deps group present") {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        assert!(matches!(&group[0], Value::String(s) if s == "//some:target"));
    }

    #[test]
    fn test_link_deps_named_groups_are_verbatim() {
        // A user-named dep group lands under its own key verbatim, keeping the
        // userland namespace flat; the default (empty) group is the
        // plugin-controlled `link_deps` bucket.
        let link = LinkConfig {
            deps: BTreeMap::from([
                (String::new(), vec!["//d:default".to_string()]),
                ("assets".to_string(), vec!["//a:one".to_string()]),
            ]),
            ..Default::default()
        };
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            &link,
            V,
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        let group = match deps.get("assets").expect("assets group present") {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        assert!(matches!(&group[0], Value::String(s) if s == "//a:one"));
        assert!(
            deps.contains_key("link_deps"),
            "default group uses the plugin-controlled link_deps key"
        );
    }

    #[test]
    fn test_link_runtime_deps_added_to_runtime_deps_map() {
        let link = LinkConfig {
            runtime_deps: BTreeMap::from([(String::new(), vec!["//some:rt".to_string()])]),
            ..Default::default()
        };
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            &link,
            V,
        );
        let rdeps = match spec
            .config
            .get("runtime_deps")
            .expect("runtime_deps present")
        {
            Value::Map(m) => m,
            _ => panic!("expected runtime_deps map"),
        };
        let group = match rdeps
            .get("link_runtime_deps")
            .expect("link_runtime_deps group present")
        {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        assert!(matches!(&group[0], Value::String(s) if s == "//some:rt"));
    }

    #[test]
    fn test_link_runtime_deps_named_groups_are_verbatim() {
        let link = LinkConfig {
            runtime_deps: BTreeMap::from([("data".to_string(), vec!["//r:one".to_string()])]),
            ..Default::default()
        };
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            &link,
            V,
        );
        let rdeps = match spec
            .config
            .get("runtime_deps")
            .expect("runtime_deps present")
        {
            Value::Map(m) => m,
            _ => panic!("expected runtime_deps map"),
        };
        let group = match rdeps.get("data").expect("data group present") {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        assert!(matches!(&group[0], Value::String(s) if s == "//r:one"));
    }

    #[test]
    fn test_link_empty_omits_extra_keys() {
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            &LinkConfig::default(),
            V,
        );
        assert!(
            !spec.config.contains_key("runtime_deps"),
            "no runtime_deps key when link config empty"
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(
            !deps.contains_key("link_deps"),
            "no link_deps group when link config empty"
        );
    }

    #[test]
    fn test_run_self_not_in_importcfg() {
        // The main package is passed as a positional arg to the linker, not via importcfg.
        let libs = vec![
            ("example.com/cmd".to_string(), lib_addr("cmd")),
            ("fmt".to_string(), lib_addr("@heph/go/std/fmt")),
        ];
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &libs,
            &LinkConfig::default(),
            V,
        );
        let run = run_str(&spec);
        assert!(
            !run.contains("packagefile example.com/cmd="),
            "link script must not add main package to importcfg: {}",
            run
        );
        assert!(
            run.contains("packagefile fmt="),
            "link script must include non-main deps in importcfg: {}",
            run
        );
    }
}
