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
    goroot: &str,
    go_mod_addr: &Addr,
    go_src_addr: &Addr,
    go_src_query_addr: Option<&Addr>,
    non_go_src_addrs: &[String],
) -> anyhow::Result<TargetSpec> {
    let mut srcfiles: Vec<String> = vec![go_src_addr.format()];
    if let Some(q) = go_src_query_addr {
        srcfiles.push(q.format());
    }
    srcfiles.extend_from_slice(non_go_src_addrs);
    build_spec_inner(
        addr,
        import_path,
        factors,
        goroot,
        &[
            ("modfiles", &[go_mod_addr.format()][..]),
            ("srcfiles", &srcfiles),
        ],
    )
}

pub fn build_spec_stdlib(
    addr: Addr,
    import_path: &str,
    factors: &Factors,
    goroot: &str,
) -> anyhow::Result<TargetSpec> {
    build_spec_inner(addr, import_path, factors, goroot, &[])
}

pub fn build_spec_thirdparty(
    addr: Addr,
    import_path: &str,
    factors: &Factors,
    goroot: &str,
    go_mod_addr: &Addr,
) -> anyhow::Result<TargetSpec> {
    build_spec_inner(
        addr,
        import_path,
        factors,
        goroot,
        &[("modfiles", &[go_mod_addr.format()][..])],
    )
}

fn build_spec_inner(
    addr: Addr,
    import_path: &str,
    factors: &Factors,
    goroot: &str,
    extra_deps: &[(&str, &[String])],
) -> anyhow::Result<TargetSpec> {
    let mut deps: HashMap<String, TargetSpecValue> = HashMap::new();
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
    config.insert(
        "import_path".to_string(),
        TargetSpecValue::String(import_path.to_string()),
    );
    config.insert(
        "goos".to_string(),
        TargetSpecValue::String(factors.goos.to_string()),
    );
    config.insert(
        "goarch".to_string(),
        TargetSpecValue::String(factors.goarch.to_string()),
    );
    config.insert(
        "goroot".to_string(),
        TargetSpecValue::String(goroot.to_string()),
    );
    if !factors.build_tags.is_empty() {
        config.insert(
            "build_tags".to_string(),
            TargetSpecValue::List(
                factors
                    .build_tags
                    .iter()
                    .map(|t| TargetSpecValue::String(t.clone()))
                    .collect(),
            ),
        );
    }
    if !deps.is_empty() {
        config.insert("deps".to_string(), TargetSpecValue::Map(deps));
    }
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

    Ok(TargetSpec {
        addr,
        driver: "go_golist".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htpkg::PkgBuf;

    fn test_addr() -> Addr {
        Addr::new(
            PkgBuf::from("mylib"),
            "_golist".to_string(),
            Default::default(),
        )
    }

    fn test_factors() -> Factors {
        Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
        }
    }

    fn go_mod_addr() -> Addr {
        Addr::new(PkgBuf::from(""), "_go_mod".to_string(), Default::default())
    }

    fn go_src_addr() -> Addr {
        Addr::new(
            PkgBuf::from("mylib"),
            "_go_src".to_string(),
            Default::default(),
        )
    }

    #[test]
    fn test_driver_is_go_golist() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            "/usr/local/go",
            &go_mod_addr(),
            &go_src_addr(),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(spec.driver, "go_golist");
    }

    #[test]
    fn test_out_has_json_group() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            "/usr/local/go",
            &go_mod_addr(),
            &go_src_addr(),
            None,
            &[],
        )
        .unwrap();
        let out = match spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(out.contains_key("json"), "out should have 'json' group");
    }

    #[test]
    fn test_config_has_import_path() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            "/usr/local/go",
            &go_mod_addr(),
            &go_src_addr(),
            None,
            &[],
        )
        .unwrap();
        assert!(
            matches!(
                spec.config.get("import_path"),
                Some(TargetSpecValue::String(s)) if s == "example.com/mylib"
            ),
            "config must have import_path"
        );
    }

    #[test]
    fn test_config_has_goos_goarch_goroot() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            "/usr/local/go",
            &go_mod_addr(),
            &go_src_addr(),
            None,
            &[],
        )
        .unwrap();
        assert!(matches!(
            spec.config.get("goos"),
            Some(TargetSpecValue::String(s)) if s == "linux"
        ));
        assert!(matches!(
            spec.config.get("goarch"),
            Some(TargetSpecValue::String(s)) if s == "amd64"
        ));
        assert!(matches!(
            spec.config.get("goroot"),
            Some(TargetSpecValue::String(s)) if s == "/usr/local/go"
        ));
    }

    #[test]
    fn test_config_no_run_key() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            "/usr/local/go",
            &go_mod_addr(),
            &go_src_addr(),
            None,
            &[],
        )
        .unwrap();
        assert!(!spec.config.contains_key("run"), "spec must not have 'run'");
        assert!(
            !spec.config.contains_key("runtime_env"),
            "spec must not have 'runtime_env'"
        );
        assert!(
            !spec.config.contains_key("runtime_pass_env"),
            "spec must not have 'runtime_pass_env'"
        );
    }

    #[test]
    fn test_config_no_go_bin_dep() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            "/usr/local/go",
            &go_mod_addr(),
            &go_src_addr(),
            None,
            &[],
        )
        .unwrap();
        if let Some(TargetSpecValue::Map(deps)) = spec.config.get("deps") {
            assert!(!deps.contains_key("go_bin"), "deps must not contain go_bin");
        }
    }

    #[test]
    fn test_firstparty_deps_has_modfiles_and_srcfiles() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            "/usr/local/go",
            &go_mod_addr(),
            &go_src_addr(),
            None,
            &[],
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
        assert_eq!(spec.driver, "go_golist");
        let out = match spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("json"));
    }

    #[test]
    fn test_build_tags_in_config() {
        let factors = Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec!["integration".to_string()],
        };
        let spec = build_spec_stdlib(test_addr(), "fmt", &factors, "/usr/local/go").unwrap();
        assert!(
            matches!(
                spec.config.get("build_tags"),
                Some(TargetSpecValue::List(tags)) if tags.len() == 1
            ),
            "build_tags must be in config when non-empty"
        );
    }

    #[test]
    fn test_no_build_tags_key_when_empty() {
        let spec = build_spec_stdlib(test_addr(), "fmt", &test_factors(), "/usr/local/go").unwrap();
        assert!(
            !spec.config.contains_key("build_tags"),
            "build_tags must not appear when empty"
        );
    }
}
