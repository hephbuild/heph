use hbuiltins::pluginfs;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hplugin::provider::TargetSpec;
use std::collections::HashMap;

/// Generates a `_go_mod` target that groups go.mod / go.sum pluginfs deps so the engine
/// tracks them as proper inputs. Uses the group driver for transparent dep passthrough.
pub fn build_spec(addr: Addr, mod_files: &[String]) -> TargetSpec {
    let pkg = addr.package.as_str();

    let file_deps: Vec<Value> = mod_files
        .iter()
        .map(|f| {
            let rel = if pkg.is_empty() {
                f.clone()
            } else {
                format!("{}/{}", pkg, f)
            };
            Value::String(pluginfs::file_addr(&rel).format())
        })
        .collect();

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("deps".to_string(), Value::List(file_deps));

    TargetSpec {
        addr,
        driver: hbuiltins::plugingroup::DRIVER_NAME.to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
        approval: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmodel::htpkg::PkgBuf;

    fn test_addr() -> Addr {
        Addr::new(PkgBuf::from(""), "_go_mod".to_string(), Default::default())
    }

    fn nested_addr() -> Addr {
        Addr::new(
            PkgBuf::from("subdir"),
            "_go_mod".to_string(),
            Default::default(),
        )
    }

    #[test]
    fn test_deps_has_pluginfs_addrs_for_root_package() {
        let files = vec!["go.mod".to_string(), "go.sum".to_string()];
        let spec = build_spec(test_addr(), &files);
        let deps = match spec.config.get("deps").unwrap() {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        assert_eq!(deps.len(), 2);
        for (entry, file) in deps.iter().zip(files.iter()) {
            let addr_str = match entry {
                Value::String(s) => s,
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
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        let addr_str = match &deps[0] {
            Value::String(s) => s,
            _ => panic!("expected string"),
        };
        assert!(
            addr_str.contains("subdir/go.mod"),
            "nested module should include package prefix in path: {}",
            addr_str
        );
    }
}
