use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::htvalue::Value;
use crate::plugingo::addr_util::{import_path_to_dep_group, write_importcfg_script};
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
    src_addrs: &[String],
    go_bin_addr: &str,
    goroot: &str,
    gocache: &str,
    embed_addr: Option<&Addr>,
    embed_file_addrs: &[String],
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
    let mut deps: BTreeMap<String, Value> = transitive_libs
        .iter()
        .map(|(dep_import_path, dep_addr)| {
            let group = import_path_to_dep_group(dep_import_path);
            let addr_str = dep_addr.format();
            (group, Value::List(vec![Value::String(addr_str)]))
        })
        .collect();
    deps.insert(
        String::new(),
        Value::List(src_addrs.iter().map(|s| Value::String(s.clone())).collect()),
    );
    deps.insert(
        "go_bin".to_string(),
        Value::List(vec![Value::String(go_bin_addr.to_string())]),
    );
    if let Some(e) = embed_addr {
        deps.insert(
            "embed".to_string(),
            Value::List(vec![Value::String(e.format())]),
        );
    }
    // Embed files travel as inputs in their own group so the engine stages them
    // into the sandbox at their pkg-relative paths (matching the embedcfg Files
    // map entries). Compile reads them via -embedcfg, not as compile sources, so
    // they must NOT land in the default ("") group.
    if !embed_file_addrs.is_empty() {
        deps.insert(
            "embed_files".to_string(),
            Value::List(
                embed_file_addrs
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("run".to_string(), Value::String(run));
    config.insert("deps".to_string(), Value::Map(deps.into_iter().collect()));
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(
            "a".to_string(),
            Value::List(vec![Value::String(out_file)]),
        )])),
    );
    config.insert(
        "runtime_env".to_string(),
        Value::Map(HashMap::from([
            ("GOOS".to_string(), Value::String(factors.goos.clone())),
            ("GOARCH".to_string(), Value::String(factors.goarch.clone())),
            ("GOROOT".to_string(), Value::String(goroot.to_string())),
            // go tool {compile,asm,link} need GOCACHE on startup for build IDs.
            ("GOCACHE".to_string(), Value::String(gocache.to_string())),
        ])),
    );
    // CGO pin lives in `env` (hashed) so stale CGO=1 archives don't survive
    // cache lookups. `runtime_env` is intentionally excluded from the def hash
    // (see pluginexec/mod.rs:70).
    config.insert(
        "env".to_string(),
        Value::Map(HashMap::from([(
            "CGO_ENABLED".to_string(),
            Value::String("0".to_string()),
        )])),
    );
    config.insert(
        "pass_env".to_string(),
        Value::List(vec![Value::String("GOROOT".to_string())]),
    );

    TargetSpec {
        addr,
        driver: "sh".to_string(),
        config,
        labels: vec!["go-build".to_string()],
        transitive: Default::default(),
    }
}

fn generate_run_script(
    p_flag: &str,
    transitive_libs: &[(String, Addr)],
    out_file: &str,
    has_embed: bool,
) -> String {
    let mut script = write_importcfg_script(transitive_libs, None);
    let embedcfg_flag = if has_embed {
        " -embedcfg \"$SRC_EMBED\""
    } else {
        ""
    };
    // -shared: PIE-compatible code. Required on platforms where Go's default
    // buildmode is PIE (darwin/arm64, recent linux/amd64). Without it, asm
    // helpers in transitive deps may fail to link with "relocation target X
    // not defined". Cheap to set unconditionally — Go's own build does so.
    script.push_str(&format!(
        "\"$SRC_GO_BIN\" tool compile -p \"{p_flag}\" -trimpath=\"$WORKSPACE_ROOT\" -pack -importcfg \"$importcfg\"{embedcfg_flag} -shared -o \"{out_file}\" \"@${{LIST_SRC}}\"\n",
    ));
    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htpkg::PkgBuf;
    use crate::pluginfs;

    fn test_addr() -> Addr {
        Addr::new(
            PkgBuf::from("mylib"),
            "build_lib".to_string(),
            Default::default(),
        )
    }

    fn src_addrs() -> Vec<String> {
        vec![
            pluginfs::file_addr("mylib/foo.go").format(),
            pluginfs::file_addr("mylib/bar.go").format(),
        ]
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
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        assert_eq!(spec.driver, "sh");
    }

    #[test]
    fn test_out_has_a_group() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let out = spec.config.get("out").unwrap();
        assert!(matches!(out, Value::Map(m) if m.contains_key("a")));
    }

    #[test]
    fn test_deps_default_group_has_pluginfs_src_addrs() {
        let addrs = src_addrs();
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &addrs,
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            deps.contains_key(""),
            "deps must have empty-group entry for $SRC/$LIST_SRC"
        );
        let src_entry = match deps.get("").unwrap() {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        assert_eq!(
            src_entry.len(),
            addrs.len(),
            "default dep group must have one entry per src addr"
        );
        for (entry, addr) in src_entry.iter().zip(addrs.iter()) {
            assert!(
                matches!(entry, Value::String(s) if s == addr),
                "dep entry must match pluginfs addr: {:?}",
                entry
            );
        }
    }

    #[test]
    fn test_deps_has_go_bin_group() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            deps.contains_key("go_bin"),
            "deps must have go_bin group: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
        let go_bin_entry = match deps.get("go_bin").unwrap() {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        assert!(
            matches!(&go_bin_entry[0], Value::String(s) if s.contains("@heph/bin")),
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
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let run = match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
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
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let run = match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
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
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let run = match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(
            !run.contains("heph_PKG_SRC_DIR"),
            "run script must not reference heph_PKG_SRC_DIR: {}",
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
        let dep_addr = Addr::new(
            PkgBuf::from("@heph/go/std/fmt"),
            "build_lib".to_string(),
            Default::default(),
        );
        let transitive_libs = vec![("fmt".to_string(), dep_addr)];
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &transitive_libs,
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let run = match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(run.contains("packagefile fmt="));
        assert!(run.contains("SRC_LIB_FMT"));
    }

    #[test]
    fn test_deps_map_contains_dep_group() {
        let dep_addr = Addr::new(
            PkgBuf::from("@heph/go/std/fmt"),
            "build_lib".to_string(),
            Default::default(),
        );
        let transitive_libs = vec![("fmt".to_string(), dep_addr)];
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &transitive_libs,
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(deps.contains_key("lib_fmt"));
    }

    #[test]
    fn test_run_script_stable_ordering() {
        let addrs: Vec<(String, Addr)> = vec![
            (
                "fmt".to_string(),
                Addr::new(
                    PkgBuf::from("@heph/go/std/fmt"),
                    "build_lib".to_string(),
                    Default::default(),
                ),
            ),
            (
                "io".to_string(),
                Addr::new(
                    PkgBuf::from("@heph/go/std/io"),
                    "build_lib".to_string(),
                    Default::default(),
                ),
            ),
        ];
        let addrs_rev: Vec<(String, Addr)> = addrs.iter().cloned().rev().collect();

        let s1 = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &addrs,
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let s2 = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &addrs_rev,
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );

        let run1 = match s1.config.get("run").unwrap() {
            Value::String(s) => s,
            _ => panic!(),
        };
        let run2 = match s2.config.get("run").unwrap() {
            Value::String(s) => s,
            _ => panic!(),
        };
        assert_eq!(run1, run2);
    }

    #[test]
    fn test_env_pins_cgo_disabled() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let env = match spec.config.get("env").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            matches!(env.get("CGO_ENABLED"), Some(Value::String(s)) if s == "0"),
            "CGO_ENABLED must be pinned to 0 in the hashed `env` map: {:?}",
            env.get("CGO_ENABLED")
        );
    }

    #[test]
    fn test_main_package_uses_p_main_flag() {
        let spec = build_spec(
            test_addr(),
            "example.com/mycmd",
            "main",
            &test_factors(),
            &[],
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let run = match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
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
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let run = match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(
            run.contains("-p \"example.com/mylib\""),
            "non-main package must use full import path as -p flag: {}",
            run
        );
    }

    fn embed_addr() -> Addr {
        Addr::new(
            PkgBuf::from("mylib"),
            "embed".to_string(),
            Default::default(),
        )
    }

    #[test]
    fn test_with_embed_addr_adds_embed_dep_group() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            Some(&embed_addr()),
            &[],
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            deps.contains_key("embed"),
            "deps must have 'embed' group when embed_addr is Some: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
        let embed_entry = match deps.get("embed").unwrap() {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        assert!(
            matches!(&embed_entry[0], Value::String(s) if s.contains("embed")),
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
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            Some(&embed_addr()),
            &[],
        );
        let run = match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
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
    fn test_spec_has_go_build_label() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        assert!(
            spec.labels.contains(&"go-build".to_string()),
            "lib build spec must carry go-build label: {:?}",
            spec.labels
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
            &src_addrs(),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let run = match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            !run.contains("-embedcfg"),
            "run script must not include -embedcfg when embed_addr is None: {run}"
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            !deps.contains_key("embed"),
            "deps must not have 'embed' group when embed_addr is None"
        );
    }
}
