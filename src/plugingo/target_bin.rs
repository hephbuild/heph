use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::loosespecparser::TargetSpecValue;
use crate::plugingo::addr_util::{dep_group_env_var, import_path_to_dep_group};
use crate::plugingo::factors::Factors;
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

    let mut deps: BTreeMap<String, TargetSpecValue> = transitive_libs
        .iter()
        .map(|(dep_import_path, dep_addr)| {
            let group = import_path_to_dep_group(dep_import_path);
            (
                group,
                TargetSpecValue::List(vec![TargetSpecValue::String(dep_addr.format())]),
            )
        })
        .collect();
    deps.insert(
        "go_bin".to_string(),
        TargetSpecValue::List(vec![TargetSpecValue::String(go_bin_addr.to_string())]),
    );

    let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
    config.insert("run".to_string(), TargetSpecValue::String(run));
    config.insert(
        "deps".to_string(),
        TargetSpecValue::Map(deps.into_iter().collect::<HashMap<_, _>>()),
    );
    config.insert(
        "out".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            String::new(),
            TargetSpecValue::List(vec![TargetSpecValue::String(binary_name)]),
        )])),
    );
    config.insert(
        "pass_env".to_string(),
        TargetSpecValue::List(vec![TargetSpecValue::String("GOROOT".to_string())]),
    );
    config.insert(
        "runtime_env".to_string(),
        TargetSpecValue::Map(HashMap::from([
            (
                "GOOS".to_string(),
                TargetSpecValue::String(factors.goos.clone()),
            ),
            (
                "GOARCH".to_string(),
                TargetSpecValue::String(factors.goarch.clone()),
            ),
            (
                "GOROOT".to_string(),
                TargetSpecValue::String(goroot.to_string()),
            ),
        ])),
    );

    TargetSpec {
        addr,
        driver: "bash".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    }
}

fn generate_link_script(
    self_import_path: &str,
    binary_name: &str,
    transitive_libs: &[(String, Addr)],
) -> String {
    let mut script = String::new();
    script.push_str("importcfg=\"$PWD/importcfg\"\n");
    script.push_str("> \"$importcfg\"\n");

    let mut sorted_libs: Vec<&(String, Addr)> = transitive_libs.iter().collect();
    sorted_libs.sort_by_key(|(ip, _)| ip.as_str());

    for (dep_import_path, _) in &sorted_libs {
        // The main package is not imported by anyone; pass it as the positional
        // argument to the linker instead of an importcfg entry.
        if dep_import_path == self_import_path {
            continue;
        }
        let group = import_path_to_dep_group(dep_import_path);
        let env_var = dep_group_env_var(&group);
        script.push_str(&format!(
            "printf \"packagefile {}=%s\\n\" \"${}\" >> \"$importcfg\"\n",
            dep_import_path, env_var
        ));
    }

    let self_group = import_path_to_dep_group(self_import_path);
    let self_env_var = dep_group_env_var(&self_group);
    script.push_str(&format!(
        "\"$SRC_GO_BIN\" tool link -importcfg \"$importcfg\" -o {} \"${}\"\n",
        binary_name, self_env_var
    ));

    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htpkg::PkgBuf;

    fn test_factors() -> Factors {
        Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
        }
    }

    fn test_addr() -> Addr {
        Addr {
            package: PkgBuf::from("cmd"),
            name: "build".to_string(),
            args: Default::default(),
        }
    }

    fn lib_addr(pkg: &str) -> Addr {
        Addr {
            package: PkgBuf::from(pkg),
            name: "build_lib".to_string(),
            args: Default::default(),
        }
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
        assert_eq!(spec.driver, "bash");
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
            TargetSpecValue::Map(m) => m,
            _ => panic!(),
        };
        let files = match out.get("").unwrap() {
            TargetSpecValue::List(v) => v,
            _ => panic!(),
        };
        assert!(matches!(&files[0], TargetSpecValue::String(s) if s == "cmd"));
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
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(run.contains("SRC_GO_BIN"));
        assert!(run.contains("tool link"));
        assert!(run.contains("importcfg"));
    }

    #[test]
    fn test_run_uses_src_go_bin() {
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(
            run.contains("\"$SRC_GO_BIN\""),
            "link script must use \"$SRC_GO_BIN\" (quoted): {}",
            run
        );
    }

    #[test]
    fn test_deps_has_go_bin_group() {
        let spec = build_spec(
            test_addr(),
            "example.com/cmd",
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            deps.contains_key("go_bin"),
            "deps must have go_bin group: {:?}",
            deps.keys().collect::<Vec<_>>()
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
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
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
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
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
