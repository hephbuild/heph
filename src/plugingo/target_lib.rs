use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::loosespecparser::TargetSpecValue;
use crate::plugingo::addr_util::{dep_group_env_var, import_path_to_dep_group};
use crate::plugingo::factors::Factors;
use crate::plugingo::target_std::archive_filename;
use std::collections::{BTreeMap, HashMap};

#[expect(
    clippy::too_many_arguments,
    reason = "all parameters are required, no natural grouping"
)]
pub fn build_spec(
    addr: Addr,
    import_path: &str,
    package_name: &str,
    factors: &Factors,
    transitive_libs: &[(String, Addr)],
    src_addr: &Addr,
    go_bin_addr: &str,
    goroot: &str,
    embed_addr: Option<&Addr>,
) -> TargetSpec {
    let out_file = archive_filename(import_path);
    // Go requires the main package to be compiled with -p "main" so the linker
    // can find main.main. Other packages use their full import path.
    let p_flag = if package_name == "main" {
        "main"
    } else {
        import_path
    };
    let run = generate_run_script(p_flag, transitive_libs, &out_file, embed_addr.is_some());

    // Default dep group ("") → $SRC / $LIST_SRC populated with the package source files.
    // Named lib_* groups carry compiled archives for the importcfg.
    let mut deps: BTreeMap<String, TargetSpecValue> = transitive_libs
        .iter()
        .map(|(dep_import_path, dep_addr)| {
            let group = import_path_to_dep_group(dep_import_path);
            let addr_str = dep_addr.format();
            (
                group,
                TargetSpecValue::List(vec![TargetSpecValue::String(addr_str)]),
            )
        })
        .collect();
    deps.insert(
        String::new(),
        TargetSpecValue::List(vec![TargetSpecValue::String(src_addr.format())]),
    );
    deps.insert(
        "go_bin".to_string(),
        TargetSpecValue::List(vec![TargetSpecValue::String(go_bin_addr.to_string())]),
    );
    if let Some(e) = embed_addr {
        deps.insert(
            "embed".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String(e.format())]),
        );
    }

    let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
    config.insert("run".to_string(), TargetSpecValue::String(run));
    config.insert(
        "deps".to_string(),
        TargetSpecValue::Map(deps.into_iter().collect()),
    );
    config.insert(
        "out".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            "a".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String(out_file)]),
        )])),
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
    config.insert(
        "pass_env".to_string(),
        TargetSpecValue::List(vec![TargetSpecValue::String("GOROOT".to_string())]),
    );

    TargetSpec {
        addr,
        driver: "bash".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    }
}

fn generate_run_script(
    p_flag: &str,
    transitive_libs: &[(String, Addr)],
    out_file: &str,
    has_embed: bool,
) -> String {
    let mut script = String::new();
    script.push_str("importcfg=\"$PWD/importcfg\"\n");
    script.push_str("> \"$importcfg\"\n");

    // One printf per transitive dep (sorted by import path for deterministic output)
    let mut sorted_libs: Vec<&(String, Addr)> = transitive_libs.iter().collect();
    sorted_libs.sort_by_key(|(ip, _)| ip.as_str());

    for (dep_import_path, _dep_addr) in &sorted_libs {
        let group = import_path_to_dep_group(dep_import_path);
        let env_var = dep_group_env_var(&group);
        script.push_str(&format!(
            "printf \"packagefile {}=%s\\n\" \"${}\" >> \"$importcfg\"\n",
            dep_import_path, env_var
        ));
    }

    // -embedcfg is only emitted when the package has //go:embed directives.
    let embedcfg_flag = if has_embed {
        " -embedcfg \"$SRC_EMBED\""
    } else {
        ""
    };

    // @${LIST_SRC} passes source files via a response file, safe for large packages.
    // -p sets the package import path embedded in the archive so the linker can verify it.
    script.push_str(&format!(
        "\"$SRC_GO_BIN\" tool compile -p \"{p_flag}\" -trimpath -pack -importcfg \"$importcfg\"{embedcfg_flag} -o \"{out_file}\" \"@${{LIST_SRC}}\"\n",
    ));

    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htpkg::PkgBuf;

    fn test_addr() -> Addr {
        Addr {
            package: PkgBuf::from("mylib"),
            name: "build_lib".to_string(),
            args: Default::default(),
        }
    }

    fn src_addr() -> Addr {
        Addr {
            package: PkgBuf::from("mylib"),
            name: "_go_src".to_string(),
            args: Default::default(),
        }
    }

    fn test_factors() -> Factors {
        Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
        }
    }

    #[test]
    fn test_driver_is_bash() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        assert_eq!(spec.driver, "bash");
    }

    #[test]
    fn test_out_has_a_group() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        let out = spec.config.get("out").unwrap();
        assert!(matches!(out, TargetSpecValue::Map(m) if m.contains_key("a")));
    }

    #[test]
    fn test_deps_default_group_has_src_addr() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            deps.contains_key(""),
            "deps must have empty-group entry for $SRC/$LIST_SRC"
        );
        let src_entry = match deps.get("").unwrap() {
            TargetSpecValue::List(v) => v,
            _ => panic!("expected list"),
        };
        assert!(
            matches!(&src_entry[0], TargetSpecValue::String(s) if s.contains("_go_src")),
            "empty dep group should reference _go_src: {:?}",
            src_entry
        );
    }

    #[test]
    fn test_deps_has_go_bin_group() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
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
        let go_bin_entry = match deps.get("go_bin").unwrap() {
            TargetSpecValue::List(v) => v,
            _ => panic!("expected list"),
        };
        assert!(
            matches!(&go_bin_entry[0], TargetSpecValue::String(s) if s.contains("@heph/bin")),
            "go_bin dep should reference go bin addr: {:?}",
            go_bin_entry
        );
    }

    #[test]
    fn test_run_uses_list_src() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            run.contains("@${LIST_SRC}"),
            "run script should use @${{LIST_SRC}} response file: {}",
            run
        );
        assert!(
            run.contains("SRC_GO_BIN"),
            "run script should use $SRC_GO_BIN: {}",
            run
        );
        assert!(run.contains("tool compile"));
        assert!(
            !run.contains("while IFS="),
            "run script must not manually loop over source files: {}",
            run
        );
    }

    #[test]
    fn test_run_uses_src_go_bin() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            run.contains("\"$SRC_GO_BIN\""),
            "run script must use \"$SRC_GO_BIN\" (quoted): {}",
            run
        );
        assert!(
            !run.contains("^go "),
            "run script must not invoke bare `go` command: {}",
            run
        );
    }

    #[test]
    fn test_run_no_manual_src_scan() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        // Must not manually scan a directory for source files
        assert!(
            !run.contains("RHEPH_PKG_SRC_DIR"),
            "run script must not reference RHEPH_PKG_SRC_DIR: {}",
            run
        );
        assert!(
            !run.contains("*.go"),
            "run script must not use a glob to find source files: {}",
            run
        );
    }

    #[test]
    fn test_run_contains_importcfg_printf_for_dep() {
        let dep_addr = Addr {
            package: PkgBuf::from("@heph/go/std/fmt"),
            name: "build_lib".to_string(),
            args: Default::default(),
        };
        let transitive_libs = vec![("fmt".to_string(), dep_addr)];
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &transitive_libs,
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(run.contains("packagefile fmt="));
        assert!(run.contains("SRC_LIB_FMT"));
    }

    #[test]
    fn test_deps_map_contains_dep_group() {
        let dep_addr = Addr {
            package: PkgBuf::from("@heph/go/std/fmt"),
            name: "build_lib".to_string(),
            args: Default::default(),
        };
        let transitive_libs = vec![("fmt".to_string(), dep_addr)];
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &transitive_libs,
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!(),
        };
        assert!(deps.contains_key("lib_fmt"));
    }

    #[test]
    fn test_run_script_stable_ordering() {
        let addrs: Vec<(String, Addr)> = vec![
            (
                "fmt".to_string(),
                Addr {
                    package: PkgBuf::from("@heph/go/std/fmt"),
                    name: "build_lib".to_string(),
                    args: Default::default(),
                },
            ),
            (
                "io".to_string(),
                Addr {
                    package: PkgBuf::from("@heph/go/std/io"),
                    name: "build_lib".to_string(),
                    args: Default::default(),
                },
            ),
        ];
        let addrs_rev: Vec<(String, Addr)> = addrs.iter().cloned().rev().collect();

        let s1 = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &addrs,
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        let s2 = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &addrs_rev,
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );

        let run1 = match s1.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s,
            _ => panic!(),
        };
        let run2 = match s2.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s,
            _ => panic!(),
        };
        assert_eq!(run1, run2);
    }

    #[test]
    fn test_main_package_uses_p_main_flag() {
        let spec = build_spec(
            test_addr(),
            "example.com/mycmd",
            "main",
            &test_factors(),
            &[],
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(
            run.contains("-p \"main\""),
            "main package must be compiled with -p \"main\": {}",
            run
        );
        assert!(
            !run.contains("-p \"example.com/mycmd\""),
            "main package must not use full import path as -p flag: {}",
            run
        );
    }

    #[test]
    fn test_non_main_package_uses_import_path_as_p_flag() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(
            run.contains("-p \"example.com/mylib\""),
            "non-main package must use full import path as -p flag: {}",
            run
        );
    }

    fn embed_addr() -> Addr {
        Addr {
            package: PkgBuf::from("mylib"),
            name: "embed".to_string(),
            args: Default::default(),
        }
    }

    #[test]
    fn test_with_embed_addr_adds_embed_dep_group() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            Some(&embed_addr()),
        );
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            deps.contains_key("embed"),
            "deps must have 'embed' group when embed_addr is Some: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
        let embed_entry = match deps.get("embed").unwrap() {
            TargetSpecValue::List(v) => v,
            _ => panic!("expected list"),
        };
        assert!(
            matches!(&embed_entry[0], TargetSpecValue::String(s) if s.contains("embed")),
            "embed dep should reference embed addr: {:?}",
            embed_entry
        );
    }

    #[test]
    fn test_with_embed_addr_run_contains_embedcfg_flag() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            Some(&embed_addr()),
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            run.contains("-embedcfg"),
            "run script must include -embedcfg when embed_addr is Some: {run}"
        );
        assert!(
            run.contains("$SRC_EMBED"),
            "run script must reference $SRC_EMBED: {run}"
        );
    }

    #[test]
    fn test_without_embed_addr_no_embedcfg_flag() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addr(),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            !run.contains("-embedcfg"),
            "run script must not include -embedcfg when embed_addr is None: {run}"
        );
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            !deps.contains_key("embed"),
            "deps must not have 'embed' group when embed_addr is None"
        );
    }
}
