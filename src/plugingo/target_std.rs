use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::loosespecparser::TargetSpecValue;
use crate::plugingo::factors::Factors;
use std::collections::HashMap;

/// Derive a unique archive filename from an import path so that multiple archives
/// can coexist in the same sandbox directory without overwriting each other.
/// e.g. "fmt" → "fmt.a", "internal/chacha8rand" → "internal_chacha8rand.a"
pub fn archive_filename(import_path: &str) -> String {
    let sanitized: String = import_path
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{}.a", sanitized)
}

pub fn build_spec(
    addr: Addr,
    import_path: &str,
    factors: &Factors,
    go_bin_addr: &str,
    goroot: &str,
) -> TargetSpec {
    let out_file = archive_filename(import_path);
    let mut run = String::new();
    run.push_str(&format!(
        "export_path=$(\"$SRC_GO_BIN\" list -export -f '{{{{.Export}}}}' \"{}\")\n",
        import_path
    ));
    run.push_str("if [ -z \"$export_path\" ]; then\n");
    run.push_str(&format!(
        "  echo \"go list -export returned empty path for {}\" >&2\n",
        import_path
    ));
    run.push_str("  exit 1\nfi\n");
    // BSD/macOS cp copies extended attributes by default
    // (com.apple.quarantine from Go's build cache) and fails with EPERM
    // on filesystems that reject xattr writes (e.g. our FUSE overlay).
    // `cp -X` on macOS suppresses that; on GNU cp `-X` means
    // `--one-file-system`, so branch on `uname`.
    run.push_str("if [ \"$(uname)\" = Darwin ]; then CP=\"cp -X\"; else CP=\"cp\"; fi\n");
    run.push_str(&format!("$CP \"$export_path\" \"{}\"\n", out_file));

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
            "a".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String(out_file)]),
        )])),
    );
    config.insert(
        "pass_env".to_string(),
        TargetSpecValue::List(vec![TargetSpecValue::String("GOROOT".to_string())]),
    );
    config.insert(
        "runtime_pass_env".to_string(),
        TargetSpecValue::List(vec![
            TargetSpecValue::String("HOME".to_string()),
            TargetSpecValue::String("GOCACHE".to_string()),
        ]),
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
                // Set GOROOT explicitly so go can find its tools even from a copied binary.
                // pass_env also captures GOROOT for cache invalidation if set in the host env.
                "GOROOT".to_string(),
                TargetSpecValue::String(goroot.to_string()),
            ),
        ])),
    );
    // CGO pin lives in `env` (hashed) so stale CGO=1 archives don't survive
    // cache lookups across the rollout. `runtime_env` is intentionally excluded
    // from the def hash (see pluginexec/mod.rs:70).
    config.insert(
        "env".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            "CGO_ENABLED".to_string(),
            TargetSpecValue::String("0".to_string()),
        )])),
    );

    TargetSpec {
        addr,
        driver: "sh".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htaddr::Addr;
    use crate::htpkg::PkgBuf;

    fn test_addr() -> Addr {
        Addr::new(
            PkgBuf::from("@heph/go/std/fmt"),
            "build_lib".to_string(),
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

    #[test]
    fn test_build_spec_driver() {
        let spec = build_spec(
            test_addr(),
            "fmt",
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        );
        assert_eq!(spec.driver, "sh");
    }

    #[test]
    fn test_build_spec_out_group() {
        let spec = build_spec(
            test_addr(),
            "fmt",
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let out = spec.config.get("out").unwrap();
        assert!(matches!(out, TargetSpecValue::Map(m) if m.contains_key("a")));
    }

    #[test]
    fn test_build_spec_run_uses_go_list_export() {
        let spec = build_spec(
            test_addr(),
            "fmt",
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s,
            _ => panic!(),
        };
        assert!(
            run.contains("list -export"),
            "run should use go list -export: {}",
            run
        );
        assert!(
            run.contains("fmt"),
            "run should reference import path: {}",
            run
        );
        assert!(
            run.contains("$SRC_GO_BIN"),
            "run should use $SRC_GO_BIN: {}",
            run
        );
    }

    #[test]
    fn test_build_spec_has_go_bin_dep() {
        let spec = build_spec(
            test_addr(),
            "fmt",
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(deps.contains_key("go_bin"));
    }

    #[test]
    fn test_build_spec_runtime_env_has_goos_goarch() {
        let spec = build_spec(
            test_addr(),
            "fmt",
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let runtime_env = match spec.config.get("runtime_env").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            matches!(runtime_env.get("GOOS"), Some(TargetSpecValue::String(s)) if s == "linux")
        );
        assert!(
            matches!(runtime_env.get("GOARCH"), Some(TargetSpecValue::String(s)) if s == "amd64")
        );
    }

    #[test]
    fn test_build_spec_env_pins_cgo_disabled() {
        let spec = build_spec(
            test_addr(),
            "fmt",
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let env = match spec.config.get("env").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            matches!(env.get("CGO_ENABLED"), Some(TargetSpecValue::String(s)) if s == "0"),
            "CGO_ENABLED must be pinned to 0 in the hashed `env` map: {:?}",
            env.get("CGO_ENABLED")
        );
    }
}
