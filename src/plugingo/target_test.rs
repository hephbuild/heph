use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::loosespecparser::TargetSpecValue;
use crate::plugingo::factors::Factors;
use crate::pluginfs;
use std::collections::HashMap;
use std::path::Path;

pub struct GoEnv<'a> {
    pub gomodcache: &'a str,
    pub gopath: &'a str,
    pub gocache: &'a str,
}

/// Build the `build_test` spec.
///
/// Source files are declared as a pluginfs glob dep (addr.package/*.go) so the engine tracks
/// them. Module files (go.mod, go.sum) are declared as individual pluginfs file deps and land
/// at the correct level in the sandbox for `go test -c .` to find go.mod.
///
/// `firstparty_src` provides the source files for firstparty transitive deps so that
/// `go test -c .` can find them. Each entry is `(pkg_rel_path, go_files)` where
/// `pkg_rel_path` is the workspace-relative directory (e.g. `"go/large/base/omega"`) and
/// `go_files` is the list of filenames (e.g. `["omega.go"]`).
pub fn build_test_spec(
    addr: Addr,
    factors: &Factors,
    go_bin_addr: &str,
    go_env: &GoEnv<'_>,
    workspace_root: &Path,
    module_root: &Path,
    mod_files: &[String],
    embed_files: &[String],
    firstparty_src: &[(String, Vec<String>)],
) -> TargetSpec {
    let pkg = addr.package.as_str();
    let src_glob = if pkg.is_empty() {
        "*.go".to_string()
    } else {
        format!("{}/*.go", pkg)
    };
    let src_dep = pluginfs::glob_addr(&src_glob, &[]).format();

    let module_rel = module_root
        .strip_prefix(workspace_root)
        .unwrap_or_else(|_| Path::new(""))
        .to_str()
        .expect("module_root must be valid UTF-8");

    let mod_dep_addrs: Vec<TargetSpecValue> = mod_files
        .iter()
        .map(|f| {
            let rel = if module_rel.is_empty() {
                f.clone()
            } else {
                format!("{}/{}", module_rel, f)
            };
            TargetSpecValue::String(pluginfs::file_addr(&rel).format())
        })
        .collect();

    let run = "\"$SRC_GO_BIN\" test -c -o test_binary .\n".to_string();

    let mut deps: HashMap<String, TargetSpecValue> = HashMap::new();
    deps.insert(
        "go_bin".to_string(),
        TargetSpecValue::List(vec![TargetSpecValue::String(go_bin_addr.to_string())]),
    );
    deps.insert(
        String::new(),
        TargetSpecValue::List(vec![TargetSpecValue::String(src_dep)]),
    );
    if !mod_dep_addrs.is_empty() {
        deps.insert(
            "modfiles".to_string(),
            TargetSpecValue::List(mod_dep_addrs),
        );
    }
    if !embed_files.is_empty() {
        let embed_dep_addrs: Vec<TargetSpecValue> = embed_files
            .iter()
            .map(|f| {
                let rel = if pkg.is_empty() {
                    f.clone()
                } else {
                    format!("{}/{}", pkg, f)
                };
                TargetSpecValue::String(pluginfs::file_addr(&rel).format())
            })
            .collect();
        deps.insert("embed_files".to_string(), TargetSpecValue::List(embed_dep_addrs));
    }
    // Per-package firstparty source files so `go test -c .` can find transitive deps.
    // Each dep package contributes its own files, tracked individually for correct caching.
    if !firstparty_src.is_empty() {
        let fp_addrs: Vec<TargetSpecValue> = firstparty_src
            .iter()
            .flat_map(|(pkg_rel, go_files)| {
                go_files.iter().map(move |f| {
                    let path = if pkg_rel.is_empty() {
                        f.clone()
                    } else {
                        format!("{}/{}", pkg_rel, f)
                    };
                    TargetSpecValue::String(pluginfs::file_addr(&path).format())
                })
            })
            .collect();
        deps.insert("fp_src".to_string(), TargetSpecValue::List(fp_addrs));
    }

    let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
    config.insert("run".to_string(), TargetSpecValue::String(run));
    config.insert("deps".to_string(), TargetSpecValue::Map(deps));
    config.insert(
        "out".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            "bin".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String("test_binary".to_string())]),
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

    fn go_env() -> GoEnv<'static> {
        GoEnv {
            gomodcache: "/go/pkg/mod",
            gopath: "/go",
            gocache: "/tmp/gocache",
        }
    }

    fn make_spec(pkg: &str) -> TargetSpec {
        let workspace = Path::new("/workspace");
        let module_root = Path::new("/workspace");
        build_test_spec(
            mk_addr(pkg, "build_test"),
            &test_factors(),
            "//@heph/bin:go",
            &go_env(),
            workspace,
            module_root,
            &["go.mod".to_string(), "go.sum".to_string()],
            &[],
            &[],
        )
    }

    #[test]
    fn test_build_test_driver() {
        assert_eq!(make_spec("mypkg").driver, "bash");
    }

    #[test]
    fn test_build_test_out_bin_group() {
        let out = match make_spec("mypkg").config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m.clone(),
            _ => panic!(),
        };
        assert!(out.contains_key("bin"));
    }

    #[test]
    fn test_build_test_run_uses_go_test() {
        let run = match make_spec("mypkg").config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(run.contains("SRC_GO_BIN"));
        assert!(run.contains("test -c"));
    }

    #[test]
    fn test_build_test_uses_src_go_bin() {
        let run = match make_spec("mypkg").config.get("run").unwrap() {
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
        let deps = match make_spec("mypkg").config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m.clone(),
            _ => panic!("expected map"),
        };
        assert!(
            deps.contains_key("go_bin"),
            "deps must have go_bin group: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_build_test_deps_has_src_glob() {
        let deps = match make_spec("mypkg").config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m.clone(),
            _ => panic!("expected map"),
        };
        let src = match deps.get("").unwrap() {
            TargetSpecValue::List(v) => v.clone(),
            _ => panic!("expected list"),
        };
        let addr_str = match &src[0] {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            addr_str.contains("@heph/fs") && addr_str.contains("glob"),
            "src dep should be a pluginfs glob addr: {}",
            addr_str
        );
        assert!(
            addr_str.contains("mypkg") && addr_str.contains(".go"),
            "glob should cover the package .go files: {}",
            addr_str
        );
    }

    #[test]
    fn test_build_test_deps_has_modfiles() {
        let deps = match make_spec("mypkg").config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m.clone(),
            _ => panic!("expected map"),
        };
        assert!(
            deps.contains_key("modfiles"),
            "deps must have modfiles group: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
        let modfiles = match deps.get("modfiles").unwrap() {
            TargetSpecValue::List(v) => v.clone(),
            _ => panic!("expected list"),
        };
        assert_eq!(modfiles.len(), 2, "should have go.mod and go.sum deps");
        for entry in &modfiles {
            let addr_str = match entry {
                TargetSpecValue::String(s) => s,
                _ => panic!("expected string"),
            };
            assert!(
                addr_str.contains("@heph/fs"),
                "modfile dep should be a pluginfs addr: {}",
                addr_str
            );
        }
    }

    #[test]
    fn test_build_test_no_rheph_pkg_src_dir() {
        let runtime_env = match make_spec("mypkg").config.get("runtime_env").unwrap() {
            TargetSpecValue::Map(m) => m.clone(),
            _ => panic!(),
        };
        assert!(
            !runtime_env.contains_key("RHEPH_PKG_SRC_DIR"),
            "build_test must not embed absolute host paths via RHEPH_PKG_SRC_DIR"
        );
    }

    #[test]
    fn test_build_test_nested_module_modfiles_include_module_path() {
        let workspace = Path::new("/workspace");
        let module_root = Path::new("/workspace/services/api");
        let spec = build_test_spec(
            mk_addr("services/api/handler", "build_test"),
            &test_factors(),
            "//@heph/bin:go",
            &go_env(),
            workspace,
            module_root,
            &["go.mod".to_string()],
            &[],
            &[],
        );
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m.clone(),
            _ => panic!(),
        };
        let modfiles = match deps.get("modfiles").unwrap() {
            TargetSpecValue::List(v) => v.clone(),
            _ => panic!(),
        };
        let addr_str = match &modfiles[0] {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(
            addr_str.contains("services/api/go.mod"),
            "modfile dep for nested module must include the module subdirectory: {}",
            addr_str
        );
    }

    #[test]
    fn test_build_test_embed_files_added_as_dep_group() {
        let workspace = Path::new("/workspace");
        let module_root = Path::new("/workspace");
        let spec = build_test_spec(
            mk_addr("mypkg", "build_test"),
            &test_factors(),
            "//@heph/bin:go",
            &go_env(),
            workspace,
            module_root,
            &[],
            &["assets/logo.png".to_string(), "assets/main.js".to_string()],
            &[],
        );
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m.clone(),
            _ => panic!("expected deps map"),
        };
        assert!(
            deps.contains_key("embed_files"),
            "deps must have embed_files group when embed files are provided: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
        let embed_dep = match deps.get("embed_files").unwrap() {
            TargetSpecValue::List(v) => v.clone(),
            _ => panic!("expected list"),
        };
        assert_eq!(embed_dep.len(), 2);
        for entry in &embed_dep {
            let s = match entry {
                TargetSpecValue::String(s) => s,
                _ => panic!("expected string"),
            };
            assert!(
                s.contains("@heph/fs") && s.contains("mypkg"),
                "embed dep should be a pluginfs file addr: {}",
                s
            );
        }
    }

    #[test]
    fn test_build_test_no_module_src_glob() {
        let deps = match make_spec("mypkg").config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m.clone(),
            _ => panic!("expected deps map"),
        };
        assert!(
            !deps.contains_key("module_src"),
            "build_test must not have module_src glob dep (rejected): {:?}",
            deps.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_build_test_fp_src_added_for_firstparty_deps() {
        let workspace = Path::new("/workspace");
        let module_root = Path::new("/workspace");
        let firstparty_src = vec![
            ("base/omega".to_string(), vec!["omega.go".to_string()]),
            ("base/epsilon".to_string(), vec!["epsilon.go".to_string(), "util.go".to_string()]),
        ];
        let spec = build_test_spec(
            mk_addr("mypkg", "build_test"),
            &test_factors(),
            "//@heph/bin:go",
            &go_env(),
            workspace,
            module_root,
            &[],
            &[],
            &firstparty_src,
        );
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m.clone(),
            _ => panic!("expected deps map"),
        };
        assert!(
            deps.contains_key("fp_src"),
            "deps must have fp_src group for firstparty source: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
        let fp_entries = match deps.get("fp_src").unwrap() {
            TargetSpecValue::List(v) => v.clone(),
            _ => panic!("expected list"),
        };
        assert_eq!(fp_entries.len(), 3, "should have one entry per source file");
        for entry in &fp_entries {
            let s = match entry {
                TargetSpecValue::String(s) => s,
                _ => panic!("expected string"),
            };
            assert!(s.contains("@heph/fs"), "fp_src entry must be a pluginfs addr: {}", s);
        }
    }

    #[test]
    fn test_build_test_no_fp_src_when_no_firstparty_deps() {
        let deps = match make_spec("mypkg").config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m.clone(),
            _ => panic!(),
        };
        assert!(
            !deps.contains_key("fp_src"),
            "no fp_src dep group when no firstparty deps"
        );
    }

    #[test]
    fn test_build_test_no_embed_files_no_embed_group() {
        let deps = match make_spec("mypkg").config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m.clone(),
            _ => panic!(),
        };
        assert!(
            !deps.contains_key("embed_files"),
            "no embed_files dep group when no embed files: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_test_spec_deps_on_build_test() {
        let build_addr = mk_addr("mypkg", "build_test");
        let spec = test_spec(mk_addr("mypkg", "test"), build_addr.clone());
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m.clone(),
            _ => panic!(),
        };
        assert!(deps.contains_key("bin"));
        let bin_dep = match deps.get("bin").unwrap() {
            TargetSpecValue::List(v) => v.clone(),
            _ => panic!(),
        };
        assert!(matches!(&bin_dep[0], TargetSpecValue::String(s) if s.contains("build_test")));
    }
}
