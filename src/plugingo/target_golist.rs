use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::loosespecparser::TargetSpecValue;
use crate::plugingo::factors::Factors;
use std::collections::HashMap;

#[expect(
    clippy::too_many_arguments,
    reason = "all parameters are required, no natural grouping"
)]
pub fn build_spec_firstparty(
    addr: Addr,
    import_path: &str,
    factors: &Factors,
    go_bin_addr: &str,
    goroot: &str,
    go_mod_addr: &Addr,
    go_src_addr: &Addr,
    go_src_query_addr: Option<&Addr>,
) -> anyhow::Result<TargetSpec> {
    let mut srcfiles: Vec<String> = vec![go_src_addr.format()];
    if let Some(q) = go_src_query_addr {
        srcfiles.push(q.format());
    }
    build_spec_inner(
        addr,
        import_path,
        factors,
        go_bin_addr,
        goroot,
        &[
            ("modfiles", &[go_mod_addr.format()][..]),
            ("srcfiles", &srcfiles),
        ],
    )
}

pub fn build_spec_thirdparty(
    addr: Addr,
    import_path: &str,
    factors: &Factors,
    go_bin_addr: &str,
    goroot: &str,
    go_mod_addr: &Addr,
) -> anyhow::Result<TargetSpec> {
    build_spec_inner(
        addr,
        import_path,
        factors,
        go_bin_addr,
        goroot,
        &[("modfiles", &[go_mod_addr.format()][..])],
    )
}

fn build_spec_inner(
    addr: Addr,
    import_path: &str,
    factors: &Factors,
    go_bin_addr: &str,
    goroot: &str,
    extra_deps: &[(&str, &[String])],
) -> anyhow::Result<TargetSpec> {
    let flags = factors.go_list_flags();

    let flags_part = if flags.is_empty() {
        String::new()
    } else {
        format!(
            " {}",
            flags
                .iter()
                .map(|f| shell_quote(f))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };

    // Run go list from the sandbox package dir. Go traverses up to find go.mod
    // (materialized via the modfiles dep), so no cd is needed. All inputs —
    // including codegen-produced .go files from query deps — are already in the
    // sandbox.
    let run = format!(
        "\"$SRC_GO_BIN\" list -json=Dir,ImportPath,Name,GoFiles,TestGoFiles,XTestGoFiles,EmbedPatterns,EmbedFiles,Imports,TestImports,XTestImports,Standard,Module,Match,Incomplete,Error \
-e{flags_part} \"$GOLIST_IMPORT_PATH\" > package.json\n",
        flags_part = flags_part,
    );

    let runtime_env: HashMap<String, TargetSpecValue> = [
        ("GOOS", factors.goos.as_str()),
        ("GOARCH", factors.goarch.as_str()),
        ("GOROOT", goroot),
        ("GOLIST_IMPORT_PATH", import_path),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), TargetSpecValue::String(v.to_string())))
    .collect();

    let mut deps: HashMap<String, TargetSpecValue> = HashMap::from([(
        "go_bin".to_string(),
        TargetSpecValue::List(vec![TargetSpecValue::String(go_bin_addr.to_string())]),
    )]);
    for (group, dep_addrs) in extra_deps {
        deps.insert(
            group.to_string(),
            TargetSpecValue::List(
                dep_addrs
                    .iter()
                    .map(|a| TargetSpecValue::String(a.clone()))
                    .collect(),
            ),
        );
    }

    let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
    config.insert("run".to_string(), TargetSpecValue::String(run));
    config.insert("deps".to_string(), TargetSpecValue::Map(deps));
    config.insert(
        "out".to_string(),
        TargetSpecValue::Map(HashMap::from([
            (
                "json".to_string(),
                TargetSpecValue::List(vec![TargetSpecValue::String("package.json".to_string())]),
            ),
            (
                "source_map".to_string(),
                TargetSpecValue::List(vec![TargetSpecValue::String("source_map.json".to_string())]),
            ),
        ])),
    );
    config.insert("runtime_env".to_string(), TargetSpecValue::Map(runtime_env));
    config.insert(
        "runtime_pass_env".to_string(),
        TargetSpecValue::List(vec![
            TargetSpecValue::String("GOPATH".to_string()),
            TargetSpecValue::String("GOMODCACHE".to_string()),
            TargetSpecValue::String("GOCACHE".to_string()),
            TargetSpecValue::String("HOME".to_string()),
            TargetSpecValue::String("GONOSUMDB".to_string()),
            TargetSpecValue::String("GOFLAGS".to_string()),
            TargetSpecValue::String("GOPRIVATE".to_string()),
        ]),
    );

    Ok(TargetSpec {
        addr,
        driver: "bash".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    })
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htpkg::PkgBuf;

    fn test_addr() -> Addr {
        Addr {
            package: PkgBuf::from("mylib"),
            name: "_golist".to_string(),
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

    fn go_mod_addr() -> Addr {
        Addr {
            package: PkgBuf::from(""),
            name: "_go_mod".to_string(),
            args: Default::default(),
        }
    }

    fn go_src_addr() -> Addr {
        Addr {
            package: PkgBuf::from("mylib"),
            name: "_go_src".to_string(),
            args: Default::default(),
        }
    }

    #[test]
    fn test_driver_is_bash() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
            &go_mod_addr(),
            &go_src_addr(),
            None,
        )
        .unwrap();
        assert_eq!(spec.driver, "bash");
    }

    #[test]
    fn test_out_has_json_group() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
            &go_mod_addr(),
            &go_src_addr(),
            None,
        )
        .unwrap();
        let out = match spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(out.contains_key("json"), "out should have 'json' group");
    }

    #[test]
    fn test_run_contains_go_list() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
            &go_mod_addr(),
            &go_src_addr(),
            None,
        )
        .unwrap();
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            run.contains("list -json=") && run.contains("-e "),
            "run should invoke go list"
        );
        assert!(!run.contains("-deps"), "run must not contain -deps flag");
        assert!(
            run.contains("package.json"),
            "run should output package.json"
        );
        assert!(run.contains("SRC_GO_BIN"), "run should use $SRC_GO_BIN");
        assert!(
            !run.contains("cd "),
            "run must not cd out of the sandbox cwd"
        );
    }

    #[test]
    fn test_deps_has_go_bin() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
            &go_mod_addr(),
            &go_src_addr(),
            None,
        )
        .unwrap();
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(deps.contains_key("go_bin"));
    }

    #[test]
    fn test_firstparty_deps_has_modfiles_and_srcfiles() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
            &go_mod_addr(),
            &go_src_addr(),
            None,
        )
        .unwrap();
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            deps.contains_key("modfiles"),
            "firstparty _golist must dep on modfiles"
        );
        assert!(
            deps.contains_key("srcfiles"),
            "firstparty _golist must dep on srcfiles"
        );
    }

    #[test]
    fn test_thirdparty_deps_has_modfiles_only() {
        let spec = build_spec_thirdparty(
            test_addr(),
            "github.com/foo/bar",
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
            &go_mod_addr(),
        )
        .unwrap();
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            deps.contains_key("modfiles"),
            "thirdparty _golist must dep on modfiles"
        );
        assert!(
            !deps.contains_key("srcfiles"),
            "thirdparty _golist must not dep on srcfiles"
        );
        assert_eq!(spec.driver, "bash");
        let out = match spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("json"));
    }

    #[test]
    fn test_run_no_src_hash_comment() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
            &go_mod_addr(),
            &go_src_addr(),
            None,
        )
        .unwrap();
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(
            !run.contains("hash:"),
            "run script must not embed a manual hash: {}",
            run
        );
    }
}
