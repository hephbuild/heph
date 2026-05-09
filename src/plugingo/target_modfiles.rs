use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::loosespecparser::TargetSpecValue;
use std::collections::HashMap;
use std::path::Path;

/// Generates a `_go_mod` target that copies go.mod and go.sum into the sandbox.
/// cache is false so the target always reruns; its output artifact hash then drives
/// downstream cache invalidation automatically via dep hashes.
pub fn build_spec(addr: Addr, mod_files: &[String], module_root: &Path) -> TargetSpec {
    let module_root_str = module_root
        .to_str()
        .expect("module_root must be valid UTF-8")
        .to_string();

    let mut run = String::new();
    for f in mod_files {
        run.push_str(&format!(
            "[ \"$RHEPH_MOD_DIR/{f}\" -ef \"{f}\" ] || cp \"$RHEPH_MOD_DIR/{f}\" \"{f}\"\n"
        ));
    }

    let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
    config.insert("run".to_string(), TargetSpecValue::String(run));
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
    config.insert(
        "runtime_env".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            "RHEPH_MOD_DIR".to_string(),
            TargetSpecValue::String(module_root_str),
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

    #[test]
    fn test_driver_is_bash() {
        let dir = tempfile::tempdir().unwrap();
        let spec = build_spec(test_addr(), &["go.mod".to_string()], dir.path());
        assert_eq!(spec.driver, "bash");
    }

    #[test]
    fn test_out_lists_mod_files() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec!["go.mod".to_string(), "go.sum".to_string()];
        let spec = build_spec(test_addr(), &files, dir.path());
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
    fn test_run_copies_files() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec!["go.mod".to_string(), "go.sum".to_string()];
        let spec = build_spec(test_addr(), &files, dir.path());
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(run.contains("go.mod"));
        assert!(run.contains("go.sum"));
        assert!(run.contains("RHEPH_MOD_DIR"));
        assert!(run.contains("cp"));
    }

    #[test]
    fn test_runtime_env_has_mod_dir() {
        let dir = tempfile::tempdir().unwrap();
        let spec = build_spec(test_addr(), &["go.mod".to_string()], dir.path());
        let runtime_env = match spec.config.get("runtime_env").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!(),
        };
        let dir_str = dir.path().to_str().unwrap();
        assert!(
            matches!(runtime_env.get("RHEPH_MOD_DIR"), Some(TargetSpecValue::String(s)) if s == dir_str)
        );
    }
}
