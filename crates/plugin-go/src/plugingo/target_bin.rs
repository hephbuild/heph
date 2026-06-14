use crate::plugingo::addr_util::{
    go_bin_tools_config, import_path_to_dep_group, to_run_value, write_importcfg_script,
};
use crate::plugingo::factors::Factors;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hplugin::provider::TargetSpec;
use std::collections::{BTreeMap, HashMap};

pub fn build_spec(
    addr: Addr,
    import_path: &str,
    factors: &Factors,
    // All transitive libs including own build_lib at index 0
    transitive_libs: &[(String, Addr)],
    go_bin_addr: &str,
    goroot: &str,
) -> TargetSpec {
    let binary_name = import_path
        .rsplit('/')
        .next()
        .unwrap_or(import_path)
        .to_string();

    let run = generate_link_script(import_path, &binary_name, transitive_libs);

    let deps: BTreeMap<String, Value> = transitive_libs
        .iter()
        .map(|(dep_import_path, dep_addr)| {
            let group = import_path_to_dep_group(dep_import_path);
            (group, Value::List(vec![Value::String(dep_addr.format())]))
        })
        .collect();

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("run".to_string(), to_run_value(run));
    config.insert("tools".to_string(), go_bin_tools_config(go_bin_addr));
    config.insert(
        "deps".to_string(),
        Value::Map(deps.into_iter().collect::<HashMap<_, _>>()),
    );
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(
            String::new(),
            Value::List(vec![Value::String(binary_name)]),
        )])),
    );
    config.insert(
        "pass_env".to_string(),
        Value::List(vec![Value::String("GOROOT".to_string())]),
    );
    config.insert(
        "runtime_env".to_string(),
        Value::Map(HashMap::from([
            ("GOOS".to_string(), Value::String(factors.goos.clone())),
            ("GOARCH".to_string(), Value::String(factors.goarch.clone())),
            ("GOROOT".to_string(), Value::String(goroot.to_string())),
        ])),
    );
    // CGO pin lives in `env` (hashed) so stale CGO=1 archives don't survive
    // cache lookups (pluginexec/mod.rs:70 excludes runtime_env from the def hash).
    config.insert(
        "env".to_string(),
        Value::Map(HashMap::from([(
            "CGO_ENABLED".to_string(),
            Value::String("0".to_string()),
        )])),
    );

    TargetSpec {
        addr,
        driver: "sh".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    }
}

fn generate_link_script(
    self_import_path: &str,
    binary_name: &str,
    transitive_libs: &[(String, Addr)],
) -> Vec<String> {
    // The main package is passed as a positional arg to the linker, not via importcfg.
    let mut lines = write_importcfg_script(transitive_libs, Some(self_import_path));
    let self_group = import_path_to_dep_group(self_import_path);
    let self_env_var = format!("SRC_{}", self_group.to_uppercase());
    // -buildmode=pie matches Go's default on darwin/arm64 (and most modern
    // platforms). Libs were compiled with -shared, so the link must agree on
    // PIE — otherwise asm relocations from transitive deps fail to resolve.
    lines.push(format!(
        "\"$TOOL_GO\" tool link -importcfg \"$importcfg\" -buildmode=pie -o {} \"${}\"",
        binary_name, self_env_var
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmodel::htpkg::PkgBuf;

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
            "//@heph/bin:go",
            "/usr/local/go",
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
            "//@heph/bin:go",
            "/usr/local/go",
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
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let run = run_str(&spec);
        assert!(run.contains("TOOL_GO"));
        assert!(run.contains("tool link"));
        assert!(run.contains("importcfg"));
    }

    #[test]
    fn test_run_uses_tool_go() {
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let run = run_str(&spec);
        assert!(
            run.contains("\"$TOOL_GO\""),
            "link script must use \"$TOOL_GO\" (quoted): {}",
            run
        );
    }

    #[test]
    fn test_tools_has_go_group() {
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let tools = match spec.config.get("tools").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        let go = match tools.get("go").expect("tools must have go group") {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        assert!(
            matches!(&go[0], Value::String(s) if s.contains("@heph/bin")),
            "go tool should reference go bin addr: {:?}",
            go
        );
    }

    #[test]
    fn test_env_pins_cgo_disabled() {
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
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
    fn test_run_no_src_hash_comment() {
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let run = run_str(&spec);
        assert!(
            !run.contains("src_hash"),
            "link script must not embed src_hash (dep chain handles invalidation): {}",
            run
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
            "//@heph/bin:go",
            "/usr/local/go",
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
