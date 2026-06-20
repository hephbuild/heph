use crate::plugingo::addr_util::go_sdk_dep;
use crate::plugingo::factors::Factors;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hplugin::provider::TargetSpec;
use std::collections::HashMap;

#[expect(
    clippy::too_many_arguments,
    reason = "all parameters are required, no natural grouping"
)]
pub fn build_spec_firstparty(
    addr: Addr,
    import_path: &str,
    factors: &Factors,
    go_version: &str,
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
        go_version,
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
    go_version: &str,
) -> anyhow::Result<TargetSpec> {
    build_spec_inner(addr, import_path, factors, go_version, &[])
}

pub fn build_spec_thirdparty(
    addr: Addr,
    import_path: &str,
    factors: &Factors,
    go_version: &str,
    go_mod_addr: &Addr,
    download_addr: &Addr,
) -> anyhow::Result<TargetSpec> {
    let mut spec = build_spec_inner(
        addr,
        import_path,
        factors,
        go_version,
        &[("modfiles", &[go_mod_addr.format()][..])],
    )?;
    // Threaded through to the driver so `resolve_package_addrs` can emit
    // `download`-filter refs for thirdparty per-file addresses. NOT a runtime
    // dep — `go list` still queries metadata via the host GOMODCACHE.
    spec.config.insert(
        "thirdparty_download_addr".to_string(),
        Value::String(download_addr.format()),
    );
    Ok(spec)
}

fn build_spec_inner(
    addr: Addr,
    import_path: &str,
    factors: &Factors,
    go_version: &str,
    extra_deps: &[(&str, &[String])],
) -> anyhow::Result<TargetSpec> {
    let mut deps: HashMap<String, Value> = HashMap::new();
    for (group, dep_addrs) in extra_deps {
        deps.insert(
            group.to_string(),
            Value::List(dep_addrs.iter().map(|a| Value::String(a.clone())).collect()),
        );
    }
    // Hermetic toolchain: stage the SDK into the golist sandbox; the driver
    // derives GOROOT from its staged path. Host toolchain (`gotool = "host"`):
    // no SDK dep — the driver resolves the host `go`/GOROOT at run time.
    if let Some((sdk_group, sdk_val)) = go_sdk_dep(go_version) {
        deps.insert(sdk_group, sdk_val);
    }

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert(
        "import_path".to_string(),
        Value::String(import_path.to_string()),
    );
    config.insert("goos".to_string(), Value::String(factors.goos.to_string()));
    config.insert(
        "goarch".to_string(),
        Value::String(factors.goarch.to_string()),
    );
    // The driver resolves GOROOT to the SDK staged for this version.
    config.insert(
        "go_version".to_string(),
        Value::String(go_version.to_string()),
    );
    if !factors.build_tags.is_empty() {
        config.insert(
            "build_tags".to_string(),
            Value::List(
                factors
                    .build_tags
                    .iter()
                    .map(|t| Value::String(t.clone()))
                    .collect(),
            ),
        );
    }
    if !deps.is_empty() {
        config.insert("deps".to_string(), Value::Map(deps));
    }
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([
            (
                "pkg".to_string(),
                Value::List(vec![Value::String("package.bin".to_string())]),
            ),
            (
                "addrs".to_string(),
                Value::List(vec![Value::String("package_addrs.bin".to_string())]),
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
    use hmodel::htpkg::PkgBuf;

    /// Go version under test.
    const V: &str = crate::plugingo::toolchain::DEFAULT_GO_VERSION;

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
            V,
            &go_mod_addr(),
            &go_src_addr(),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(spec.driver, "go_golist");
    }

    #[test]
    fn test_out_has_pkg_and_addrs_groups() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            V,
            &go_mod_addr(),
            &go_src_addr(),
            None,
            &[],
        )
        .unwrap();
        let out = match spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(out.contains_key("pkg"), "out should have 'pkg' group");
        assert!(out.contains_key("addrs"), "out should have 'addrs' group");
    }

    #[test]
    fn test_config_has_import_path() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            V,
            &go_mod_addr(),
            &go_src_addr(),
            None,
            &[],
        )
        .unwrap();
        assert!(
            matches!(
                spec.config.get("import_path"),
                Some(Value::String(s)) if s == "example.com/mylib"
            ),
            "config must have import_path"
        );
    }

    #[test]
    fn test_config_has_goos_goarch_no_host_goroot() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            V,
            &go_mod_addr(),
            &go_src_addr(),
            None,
            &[],
        )
        .unwrap();
        assert!(matches!(
            spec.config.get("goos"),
            Some(Value::String(s)) if s == "linux"
        ));
        assert!(matches!(
            spec.config.get("goarch"),
            Some(Value::String(s)) if s == "amd64"
        ));
        // GOROOT is no longer threaded from the host; the driver derives it from
        // the staged hermetic SDK.
        assert!(
            !spec.config.contains_key("goroot"),
            "golist spec must not carry a host goroot"
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        let sdk = deps
            .get(crate::plugingo::addr_util::GO_SDK_DEP_GROUP)
            .expect("golist must dep on the hermetic SDK");
        // `go list` resolves std imports against GOROOT/src — must take the full
        // (default, unsuffixed) toolchain group, not the trimmed `tool` group.
        let sdk_ref = match sdk {
            Value::List(v) => match &v[0] {
                Value::String(s) => s.as_str(),
                _ => panic!("sdk dep entry not a string"),
            },
            _ => panic!("sdk dep not a list"),
        };
        assert!(
            !sdk_ref.contains('|'),
            "golist needs GOROOT/src — must select the full SDK group: {sdk_ref}"
        );
    }

    #[test]
    fn test_config_no_run_key() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            V,
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
            V,
            &go_mod_addr(),
            &go_src_addr(),
            None,
            &[],
        )
        .unwrap();
        if let Some(Value::Map(deps)) = spec.config.get("deps") {
            assert!(!deps.contains_key("go_bin"), "deps must not contain go_bin");
        }
    }

    #[test]
    fn test_firstparty_deps_has_modfiles_and_srcfiles() {
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            &test_factors(),
            V,
            &go_mod_addr(),
            &go_src_addr(),
            None,
            &[],
        )
        .unwrap();
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
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

    fn tp_download_addr() -> Addr {
        Addr::new(
            PkgBuf::from("@heph/go/thirdparty/github.com/foo/bar@v1.2.3"),
            "download".to_string(),
            Default::default(),
        )
    }

    #[test]
    fn test_thirdparty_deps_has_modfiles_only() {
        let spec = build_spec_thirdparty(
            test_addr(),
            "github.com/foo/bar",
            &test_factors(),
            V,
            &go_mod_addr(),
            &tp_download_addr(),
        )
        .unwrap();
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
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
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("pkg"));
    }

    #[test]
    fn test_thirdparty_spec_includes_download_addr_config_key() {
        let spec = build_spec_thirdparty(
            test_addr(),
            "github.com/foo/bar",
            &test_factors(),
            V,
            &go_mod_addr(),
            &tp_download_addr(),
        )
        .unwrap();
        let addr_str = match spec.config.get("thirdparty_download_addr").unwrap() {
            Value::String(s) => s.as_str(),
            _ => panic!("expected string"),
        };
        assert!(
            addr_str.contains("@heph/go/thirdparty/github.com/foo/bar@v1.2.3"),
            "config must carry download addr: {addr_str}"
        );
        assert!(
            addr_str.contains(":download"),
            "addr name must be download: {addr_str}"
        );
    }

    #[test]
    fn test_build_tags_in_config() {
        let factors = Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec!["integration".to_string()],
        };
        let spec = build_spec_stdlib(test_addr(), "fmt", &factors, V).unwrap();
        assert!(
            matches!(
                spec.config.get("build_tags"),
                Some(Value::List(tags)) if tags.len() == 1
            ),
            "build_tags must be in config when non-empty"
        );
    }

    #[test]
    fn test_no_build_tags_key_when_empty() {
        let spec = build_spec_stdlib(test_addr(), "fmt", &test_factors(), V).unwrap();
        assert!(
            !spec.config.contains_key("build_tags"),
            "build_tags must not appear when empty"
        );
    }
}
