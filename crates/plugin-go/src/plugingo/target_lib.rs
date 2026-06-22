use crate::plugingo::addr_util::{
    go_build_env, go_host_pass_env_config, go_run_prelude, go_sdk_dep, go_sdk_read_only_config,
    import_path_to_dep_group, to_run_value, write_importcfg_script,
};
use crate::plugingo::factors::Factors;
use crate::plugingo::target_std::archive_filename;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hplugin::provider::TargetSpec;
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
    go_version: &str,
    embed_addr: Option<&Addr>,
    embed_file_addrs: &[String],
    embed_src_addrs: &[String],
) -> TargetSpec {
    let out_file = archive_filename(import_path);
    // Go requires the main package to be compiled with -p "main" so the linker
    // can find main.main. Other packages use their full import path.
    let p_flag = if package_name == "main" {
        "main"
    } else {
        import_path
    };
    let mut run = go_run_prelude(go_version);
    run.extend(generate_run_script(
        p_flag,
        transitive_libs,
        &out_file,
        embed_addr.is_some(),
    ));

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
    // `go_embed_src` assets (e.g. a frontend bundle) staged into their own group
    // at pkg-relative paths so `-embedcfg` finds them. Marked read-only below so
    // they materialize once into the shared stage and hardlink into both this
    // sandbox and the `embed` target's — no second byte-copy of large assets.
    if !embed_src_addrs.is_empty() {
        deps.insert(
            "embed_src".to_string(),
            Value::List(
                embed_src_addrs
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if let Some((sdk_group, sdk_val)) = go_sdk_dep(go_version) {
        deps.insert(sdk_group, sdk_val);
    }

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("run".to_string(), to_run_value(run));
    config.insert("deps".to_string(), Value::Map(deps.into_iter().collect()));
    let mut read_only_groups: Vec<Value> = Vec::new();
    if let Some((_, Value::List(v))) = go_sdk_read_only_config(go_version) {
        read_only_groups.extend(v);
    }
    if !embed_src_addrs.is_empty() {
        read_only_groups.push(Value::String("embed_src".to_string()));
    }
    if !read_only_groups.is_empty() {
        config.insert("read_only_deps".to_string(), Value::List(read_only_groups));
    }
    if let Some((pe_k, pe_v)) = go_host_pass_env_config(go_version) {
        config.insert(pe_k, pe_v);
    }
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
        ])),
    );
    // CGO/toolchain pins live in `env` (hashed) so stale archives don't survive
    // cache lookups. `runtime_env` is excluded from the def hash (pluginexec/mod.rs:70).
    config.insert("env".to_string(), go_build_env());

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
) -> Vec<String> {
    let mut lines = write_importcfg_script(transitive_libs, None);
    let embedcfg_flag = if has_embed {
        " -embedcfg \"$SRC_EMBED\""
    } else {
        ""
    };
    // -shared: PIE-compatible code. Required on platforms where Go's default
    // buildmode is PIE (darwin/arm64, recent linux/amd64). Without it, asm
    // helpers in transitive deps may fail to link with "relocation target X
    // not defined". Cheap to set unconditionally — Go's own build does so.
    lines.push(format!(
        "\"$GO\" tool compile -p \"{p_flag}\" -trimpath=\"$WORKSPACE_ROOT\" -pack -importcfg \"$importcfg\"{embedcfg_flag} -shared -o \"{out_file}\" \"@${{LIST_SRC}}\"",
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbuiltins::pluginfs;
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
            V,
            None,
            &[],
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
            V,
            None,
            &[],
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
            V,
            None,
            &[],
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
    fn test_deps_include_hermetic_sdk() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addrs(),
            V,
            None,
            &[],
            &[],
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        let sdk = deps
            .get(crate::plugingo::addr_util::GO_SDK_DEP_GROUP)
            .expect("lib compile must dep on the hermetic SDK");
        // A single SDK output serves every consumer (symlinked in read-only),
        // so the dep is the bare toolchain addr — no output-group selector.
        let sdk_ref = match sdk {
            Value::List(v) => match &v[0] {
                Value::String(s) => s.as_str(),
                _ => panic!("sdk dep entry not a string"),
            },
            _ => panic!("sdk dep not a list"),
        };
        assert!(
            !sdk_ref.contains('|'),
            "SDK dep must reference the single toolchain output, no selector: {sdk_ref}"
        );
        assert!(
            !spec.config.contains_key("tools"),
            "no host go tool should be referenced"
        );
    }

    #[test]
    fn test_sdk_dep_marked_read_only() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addrs(),
            V,
            None,
            &[],
            &[],
        );
        // The SDK is read-only — staged once, hardlinked into each sandbox.
        let ro = match spec
            .config
            .get("read_only_deps")
            .expect("read_only_deps set")
        {
            Value::List(v) => v,
            _ => panic!("read_only_deps must be a list"),
        };
        assert!(
            ro.iter().any(|e| matches!(e, Value::String(s)
                if s == crate::plugingo::addr_util::GO_SDK_DEP_GROUP)),
            "the gosdk group must be marked read-only: {ro:?}"
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
            V,
            None,
            &[],
            &[],
        );
        let run = run_str(&spec);
        assert!(
            run.contains("@${LIST_SRC}"),
            "run script should use @${{LIST_SRC}} response file: {}",
            run
        );
        assert!(
            run.contains("\"$GO\""),
            "run script should invoke the hermetic $GO: {}",
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
    fn test_run_uses_tool_go() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addrs(),
            V,
            None,
            &[],
            &[],
        );
        let run = run_str(&spec);
        assert!(
            run.contains("\"$GO\""),
            "run script must use the hermetic \"$GO\" (quoted): {}",
            run
        );
        assert!(
            !run.contains("TOOL_GO"),
            "run script must not use the host go tool: {}",
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
            V,
            None,
            &[],
            &[],
        );
        let run = run_str(&spec);
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
            V,
            None,
            &[],
            &[],
        );
        let run = run_str(&spec);
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
            V,
            None,
            &[],
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
            V,
            None,
            &[],
            &[],
        );
        let s2 = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &addrs_rev,
            &src_addrs(),
            V,
            None,
            &[],
            &[],
        );

        let run1 = run_str(&s1);
        let run2 = run_str(&s2);
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
            V,
            None,
            &[],
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
            V,
            None,
            &[],
            &[],
        );
        let run = run_str(&spec);
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
            V,
            None,
            &[],
            &[],
        );
        let run = run_str(&spec);
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
            V,
            Some(&embed_addr()),
            &[],
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
    fn test_embed_src_addrs_add_read_only_group() {
        // go_embed_src assets get their own dep group, staged read-only so they
        // share the embed target's stage instead of being copied a second time.
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addrs(),
            V,
            Some(&embed_addr()),
            &[],
            &["//mylib:frontend".to_string()],
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            deps.contains_key("embed_src"),
            "deps must have embed_src group: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
        let ro = match spec
            .config
            .get("read_only_deps")
            .expect("read_only_deps set")
        {
            Value::List(v) => v,
            _ => panic!("read_only_deps must be a list"),
        };
        assert!(
            ro.iter()
                .any(|e| matches!(e, Value::String(s) if s == "embed_src")),
            "embed_src group must be marked read-only: {ro:?}"
        );
    }

    #[test]
    fn test_no_embed_src_group_when_absent() {
        let spec = build_spec(
            test_addr(),
            "example.com/mylib",
            "mylib",
            &test_factors(),
            &[],
            &src_addrs(),
            V,
            None,
            &[],
            &[],
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            !deps.contains_key("embed_src"),
            "no embed_src group when no go_embed_src addrs"
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
            V,
            Some(&embed_addr()),
            &[],
            &[],
        );
        let run = run_str(&spec);
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
            V,
            None,
            &[],
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
            V,
            None,
            &[],
            &[],
        );
        let run = run_str(&spec);
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
