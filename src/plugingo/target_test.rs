use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::loosespecparser::TargetSpecValue;
use crate::plugingo::factors::Factors;
use std::collections::HashMap;
use std::path::Path;

pub struct GoEnv<'a> {
    pub gomodcache: &'a str,
    pub gopath: &'a str,
    pub gocache: &'a str,
}

pub fn build_test_spec(
    addr: Addr,
    factors: &Factors,
    src_dir: &Path,
    go_bin_addr: &str,
    go_env: &GoEnv<'_>,
) -> TargetSpec {
    let run = "cd \"$RHEPH_PKG_SRC_DIR\"\n\"$SRC_GO_BIN\" test -c -o \"$OLDPWD/test_binary\" .\n"
        .to_string();

    let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
    config.insert("run".to_string(), TargetSpecValue::String(run));
    config.insert(
        "deps".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            "go_bin".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String(go_bin_addr.to_string())]),
        )])),
    );
    config.insert(
        "out".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            "bin".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String("test_binary".to_string())]),
        )])),
    );
    config.insert("cache".to_string(), TargetSpecValue::Bool(true));
    config.insert(
        "runtime_env".to_string(),
        TargetSpecValue::Map(HashMap::from([
            (
                "RHEPH_PKG_SRC_DIR".to_string(),
                TargetSpecValue::String(
                    src_dir
                        .to_str()
                        .expect("src_dir must be valid UTF-8")
                        .to_string(),
                ),
            ),
            (
                "GOOS".to_string(),
                TargetSpecValue::String(factors.goos.clone()),
            ),
            (
                "GOARCH".to_string(),
                TargetSpecValue::String(factors.goarch.clone()),
            ),
            (
                "GOMODCACHE".to_string(),
                TargetSpecValue::String(go_env.gomodcache.to_string()),
            ),
            (
                "GOPATH".to_string(),
                TargetSpecValue::String(go_env.gopath.to_string()),
            ),
            (
                "GOCACHE".to_string(),
                TargetSpecValue::String(go_env.gocache.to_string()),
            ),
        ])),
    );
    config.insert(
        "pass_env".to_string(),
        TargetSpecValue::List(vec![
            TargetSpecValue::String("GOROOT".to_string()),
            TargetSpecValue::String("GOMODCACHE".to_string()),
            TargetSpecValue::String("GOPATH".to_string()),
        ]),
    );

    TargetSpec {
        addr,
        driver: "bash".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    }
}

pub fn test_spec(addr: Addr, build_test_addr: Addr) -> TargetSpec {
    let run = "\"$SRC_BIN\" -test.v".to_string();

    let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
    config.insert("run".to_string(), TargetSpecValue::String(run));
    config.insert(
        "deps".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            "bin".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String(build_test_addr.format())]),
        )])),
    );
    config.insert("out".to_string(), TargetSpecValue::Map(HashMap::new()));
    config.insert("cache".to_string(), TargetSpecValue::Bool(false));

    TargetSpec {
        addr,
        driver: "bash".to_string(),
        config,
        labels: vec!["test".to_string(), "go-test".to_string()],
        transitive: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htpkg::PkgBuf;

    fn mk_addr(pkg: &str, name: &str) -> Addr {
        Addr {
            package: PkgBuf::from(pkg),
            name: name.to_string(),
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
    fn test_build_test_driver() {
        let spec = build_test_spec(
            mk_addr("mypkg", "build_test"),
            &test_factors(),
            Path::new("/src"),
            "//@heph/bin:go",
            &GoEnv {
                gomodcache: "/go/pkg/mod",
                gopath: "/go",
                gocache: "/tmp/gocache",
            },
        );
        assert_eq!(spec.driver, "bash");
    }

    #[test]
    fn test_build_test_out_bin_group() {
        let spec = build_test_spec(
            mk_addr("mypkg", "build_test"),
            &test_factors(),
            Path::new("/src"),
            "//@heph/bin:go",
            &GoEnv {
                gomodcache: "/go/pkg/mod",
                gopath: "/go",
                gocache: "/tmp/gocache",
            },
        );
        let out = match spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("bin"));
    }

    #[test]
    fn test_build_test_cache_true() {
        let spec = build_test_spec(
            mk_addr("mypkg", "build_test"),
            &test_factors(),
            Path::new("/src"),
            "//@heph/bin:go",
            &GoEnv {
                gomodcache: "/go/pkg/mod",
                gopath: "/go",
                gocache: "/tmp/gocache",
            },
        );
        assert!(matches!(
            spec.config.get("cache"),
            Some(TargetSpecValue::Bool(true))
        ));
    }

    #[test]
    fn test_build_test_run_uses_go_test() {
        let spec = build_test_spec(
            mk_addr("mypkg", "build_test"),
            &test_factors(),
            Path::new("/src"),
            "//@heph/bin:go",
            &GoEnv {
                gomodcache: "/go/pkg/mod",
                gopath: "/go",
                gocache: "/tmp/gocache",
            },
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(run.contains("SRC_GO_BIN"));
        assert!(run.contains("test -c"));
    }

    #[test]
    fn test_build_test_uses_src_go_bin() {
        let spec = build_test_spec(
            mk_addr("mypkg", "build_test"),
            &test_factors(),
            Path::new("/src"),
            "//@heph/bin:go",
            &GoEnv {
                gomodcache: "/go/pkg/mod",
                gopath: "/go",
                gocache: "/tmp/gocache",
            },
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(
            run.contains("\"$SRC_GO_BIN\""),
            "run script must use \"$SRC_GO_BIN\" (quoted): {}",
            run
        );
    }

    #[test]
    fn test_build_test_deps_has_go_bin_group() {
        let spec = build_test_spec(
            mk_addr("mypkg", "build_test"),
            &test_factors(),
            Path::new("/src"),
            "//@heph/bin:go",
            &GoEnv {
                gomodcache: "/go/pkg/mod",
                gopath: "/go",
                gocache: "/tmp/gocache",
            },
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
    fn test_test_spec_cache_false() {
        let build_addr = mk_addr("mypkg", "build_test");
        let spec = test_spec(mk_addr("mypkg", "test"), build_addr);
        assert!(matches!(
            spec.config.get("cache"),
            Some(TargetSpecValue::Bool(false))
        ));
    }

    #[test]
    fn test_test_spec_deps_on_build_test() {
        let build_addr = mk_addr("mypkg", "build_test");
        let spec = test_spec(mk_addr("mypkg", "test"), build_addr.clone());
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!(),
        };
        assert!(deps.contains_key("bin"));
        let bin_dep = match deps.get("bin").unwrap() {
            TargetSpecValue::List(v) => v,
            _ => panic!(),
        };
        assert!(matches!(&bin_dep[0], TargetSpecValue::String(s) if s.contains("build_test")));
    }
}
