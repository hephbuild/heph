use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::loosespecparser::TargetSpecValue;
use crate::pluginfs;
use std::collections::HashMap;

/// Generates a `_go_mod` target that declares go.mod / go.sum as pluginfs deps so the engine
/// tracks them as proper inputs. Files land in the sandbox via dep unpacking; run is a no-op.
pub fn build_spec(addr: Addr, mod_files: &[String]) -> TargetSpec {
    let pkg = addr.package.as_str();

    let file_deps: Vec<TargetSpecValue> = mod_files
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

    let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
    config.insert("run".to_string(), TargetSpecValue::String(":".to_string()));
    config.insert(
        "deps".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            String::new(),
            TargetSpecValue::List(file_deps),
        )])),
    );
    config.insert(
        "out".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            String::new(),
            TargetSpecValue::List(
                mod_files
                    .iter()
                    .map(|f| TargetSpecValue::String(f.clone()))
                    .collect(),
            ),
        )])),
    );

    TargetSpec {
        addr,
        driver: "bash".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htpkg::PkgBuf;

    fn test_addr() -> Addr {
        Addr {
            package: PkgBuf::from(""),
            name: "_go_mod".to_string(),
            args: Default::default(),
        }
    }

    fn nested_addr() -> Addr {
        Addr {
            package: PkgBuf::from("subdir"),
            name: "_go_mod".to_string(),
            args: Default::default(),
        }
    }

    #[test]
    fn test_driver_is_bash() {
        let spec = build_spec(test_addr(), &["go.mod".to_string()]);
        assert_eq!(spec.driver, "bash");
    }

    #[test]
    fn test_out_lists_mod_files() {
        let files = vec!["go.mod".to_string(), "go.sum".to_string()];
        let spec = build_spec(test_addr(), &files);
        let out = match spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        let group = match out.get("").unwrap() {
            TargetSpecValue::List(v) => v,
            _ => panic!("expected list"),
        };
        assert_eq!(group.len(), 2);
        assert!(matches!(&group[0], TargetSpecValue::String(s) if s == "go.mod"));
        assert!(matches!(&group[1], TargetSpecValue::String(s) if s == "go.sum"));
    }

    #[test]
    fn test_deps_has_pluginfs_addrs_for_root_package() {
        let files = vec!["go.mod".to_string(), "go.sum".to_string()];
        let spec = build_spec(test_addr(), &files);
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        let group = match deps.get("").unwrap() {
            TargetSpecValue::List(v) => v,
            _ => panic!("expected list"),
        };
        assert_eq!(group.len(), 2);
        for (entry, file) in group.iter().zip(files.iter()) {
            let addr_str = match entry {
                TargetSpecValue::String(s) => s,
                _ => panic!("expected string"),
            };
            assert!(
                addr_str.contains("@heph/fs"),
                "dep should be a pluginfs addr: {}",
                addr_str
            );
            assert!(
                addr_str.contains(file.as_str()),
                "dep should reference the file: {}",
                addr_str
            );
        }
    }

    #[test]
    fn test_deps_has_package_prefix_for_nested_module() {
        let files = vec!["go.mod".to_string()];
        let spec = build_spec(nested_addr(), &files);
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        let group = match deps.get("").unwrap() {
            TargetSpecValue::List(v) => v,
            _ => panic!("expected list"),
        };
        let addr_str = match &group[0] {
            TargetSpecValue::String(s) => s,
            _ => panic!("expected string"),
        };
        assert!(
            addr_str.contains("subdir/go.mod"),
            "nested module should include package prefix in path: {}",
            addr_str
        );
    }

    #[test]
    fn test_run_is_noop() {
        let spec = build_spec(test_addr(), &["go.mod".to_string()]);
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert_eq!(run, ":", "run script should be the bash no-op");
    }

    #[test]
    fn test_no_runtime_env() {
        let spec = build_spec(test_addr(), &["go.mod".to_string()]);
        assert!(
            !spec.config.contains_key("runtime_env"),
            "target should not embed absolute host paths in runtime_env"
        );
    }
}
