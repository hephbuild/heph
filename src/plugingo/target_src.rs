use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::loosespecparser::TargetSpecValue;
use std::collections::HashMap;
use std::path::Path;

/// Generates a `_go_src` target that copies the given .go files from src_dir into the sandbox.
/// The output group is the default ("") so dependents receive `$SRC` / `$LIST_SRC`.
/// cache is false so the target always reruns; its output artifact hash then drives downstream
/// cache invalidation automatically via dep hashes.
pub fn build_spec(addr: Addr, go_files: &[String], src_dir: &Path) -> TargetSpec {
    let src_dir_str = src_dir
        .to_str()
        .expect("src_dir must be valid UTF-8")
        .to_string();

    let mut run = String::new();
    for f in go_files {
        // -ef guards against the case where cwd happens to be the source dir
        // (same inode), which can occur when the sandbox path resolves to the
        // same physical directory as RHEPH_PKG_SRC_DIR on macOS via symlinks.
        run.push_str(&format!(
            "[ \"$RHEPH_PKG_SRC_DIR/{f}\" -ef \"{f}\" ] || cp \"$RHEPH_PKG_SRC_DIR/{f}\" \"{f}\"\n"
        ));
    }

    let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
    config.insert("run".to_string(), TargetSpecValue::String(run));
    config.insert(
        "out".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            String::new(),
            TargetSpecValue::List(
                go_files
                    .iter()
                    .map(|f| TargetSpecValue::String(f.clone()))
                    .collect(),
            ),
        )])),
    );
    config.insert(
        "runtime_env".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            "RHEPH_PKG_SRC_DIR".to_string(),
            TargetSpecValue::String(src_dir_str),
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
            package: PkgBuf::from("mylib"),
            name: "_go_src".to_string(),
            args: Default::default(),
        }
    }

    #[test]
    fn test_driver_is_bash() {
        let spec = build_spec(
            test_addr(),
            &["foo.go".to_string()],
            Path::new("/src/mylib"),
        );
        assert_eq!(spec.driver, "bash");
    }

    #[test]
    fn test_out_default_group_lists_files() {
        let files = vec!["foo.go".to_string(), "bar.go".to_string()];
        let spec = build_spec(test_addr(), &files, Path::new("/src"));
        let out = match spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        let group = match out.get("").unwrap() {
            TargetSpecValue::List(v) => v,
            _ => panic!("expected list"),
        };
        assert_eq!(group.len(), 2);
        assert!(matches!(&group[0], TargetSpecValue::String(s) if s == "foo.go"));
        assert!(matches!(&group[1], TargetSpecValue::String(s) if s == "bar.go"));
    }

    #[test]
    fn test_run_copies_each_file() {
        let files = vec!["foo.go".to_string(), "bar.go".to_string()];
        let spec = build_spec(test_addr(), &files, Path::new("/src"));
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(run.contains("foo.go"));
        assert!(run.contains("bar.go"));
        assert!(run.contains("RHEPH_PKG_SRC_DIR"));
        assert!(run.contains("cp"), "run should contain cp: {}", run);
    }

    #[test]
    fn test_runtime_env_has_src_dir() {
        let spec = build_spec(test_addr(), &[], Path::new("/my/src"));
        let runtime_env = match spec.config.get("runtime_env").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!(),
        };
        assert!(
            matches!(runtime_env.get("RHEPH_PKG_SRC_DIR"), Some(TargetSpecValue::String(s)) if s == "/my/src")
        );
    }
}
