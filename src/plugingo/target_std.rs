use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::htvalue::Value;
use crate::plugingo::addr_util::{build_env_map, go_bin_tools_config, to_run_value};
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
    let run = vec![
        format!(
            "export_path=$(\"$TOOL_GO\" list -export -f '{{{{.Export}}}}' \"{}\")",
            import_path
        ),
        // The if-block stays one element: it's a single compound statement that
        // breaks if its lines are split across separate `run` entries.
        format!(
            "if [ -z \"$export_path\" ]; then\n  \
               echo \"go list -export returned empty path for {}\" >&2\n  \
               exit 1\nfi",
            import_path
        ),
        // BSD/macOS cp copies extended attributes by default
        // (com.apple.quarantine from Go's build cache) and fails with EPERM
        // on filesystems that reject xattr writes (e.g. our FUSE overlay).
        // `cp -X` on macOS suppresses that; on GNU cp `-X` means
        // `--one-file-system`, so branch on `uname`.
        "if [ \"$(uname)\" = Darwin ]; then CP=\"cp -X\"; else CP=\"cp\"; fi".to_string(),
        format!("$CP \"$export_path\" \"{}\"", out_file),
    ];

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("run".to_string(), to_run_value(run));
    config.insert("tools".to_string(), go_bin_tools_config(go_bin_addr));
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(
            "a".to_string(),
            Value::List(vec![Value::String(out_file)]),
        )])),
    );
    config.insert(
        "pass_env".to_string(),
        Value::List(vec![Value::String("GOROOT".to_string())]),
    );
    config.insert(
        "runtime_pass_env".to_string(),
        Value::List(vec![
            Value::String("HOME".to_string()),
            Value::String("GOCACHE".to_string()),
        ]),
    );
    config.insert(
        "runtime_env".to_string(),
        Value::Map(HashMap::from([
            ("GOOS".to_string(), Value::String(factors.goos.clone())),
            ("GOARCH".to_string(), Value::String(factors.goarch.clone())),
            (
                // Set GOROOT explicitly so go can find its tools even from a copied binary.
                // pass_env also captures GOROOT for cache invalidation if set in the host env.
                "GOROOT".to_string(),
                Value::String(goroot.to_string()),
            ),
        ])),
    );
    // CGO pin + build-env factor knobs (GOEXPERIMENT, GODEBUG, …) live in `env`
    // (hashed) so stale archives don't survive cache lookups. `runtime_env` is
    // intentionally excluded from the def hash (see pluginexec/mod.rs:70).
    config.insert("env".to_string(), Value::Map(build_env_map(factors)));

    TargetSpec {
        addr,
        driver: "sh".to_string(),
        config,
        labels: vec!["go-build".to_string()],
        transitive: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htaddr::Addr;
    use crate::htpkg::PkgBuf;

    fn run_str(spec: &TargetSpec) -> String {
        match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
            Value::List(v) => v
                .iter()
                .map(|x| match x {
                    Value::String(s) => s.as_str(),
                    _ => panic!("run entry not a string"),
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => panic!("run not string or list"),
        }
    }

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
            env: Default::default(),
            ldflags: vec![],
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
        assert!(matches!(out, Value::Map(m) if m.contains_key("a")));
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
        let run = run_str(&spec);
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
        assert!(run.contains("$TOOL_GO"), "run should use $TOOL_GO: {}", run);
    }

    #[test]
    fn test_build_spec_has_go_tool() {
        let spec = build_spec(
            test_addr(),
            "fmt",
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let tools = match spec.config.get("tools").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(tools.contains_key("go"));
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
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(matches!(runtime_env.get("GOOS"), Some(Value::String(s)) if s == "linux"));
        assert!(matches!(runtime_env.get("GOARCH"), Some(Value::String(s)) if s == "amd64"));
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
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            matches!(env.get("CGO_ENABLED"), Some(Value::String(s)) if s == "0"),
            "CGO_ENABLED must be pinned to 0 in the hashed `env` map: {:?}",
            env.get("CGO_ENABLED")
        );
    }

    #[test]
    fn test_build_spec_has_go_build_label() {
        let spec = build_spec(
            test_addr(),
            "fmt",
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        );
        assert!(
            spec.labels.contains(&"go-build".to_string()),
            "std lib build spec must carry go-build label: {:?}",
            spec.labels
        );
    }
}
