use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::loosespecparser::TargetSpecValue;
use crate::plugingo::factors::Factors;
use anyhow::Context;
use std::collections::HashMap;
use std::hash::Hasher;
use std::path::Path;
use xxhash_rust::xxh3::Xxh3Default;

pub fn build_spec_firstparty(
    addr: Addr,
    import_path: &str,
    module_root: &Path,
    src_dir: &Path,
    factors: &Factors,
    go_bin_addr: &str,
    goroot: &str,
) -> anyhow::Result<TargetSpec> {
    let src_hash = compute_src_hash(module_root, Some(src_dir))?;
    build_spec_inner(
        addr,
        import_path,
        module_root,
        src_hash,
        factors,
        go_bin_addr,
        goroot,
    )
}

pub fn build_spec_thirdparty(
    addr: Addr,
    import_path: &str,
    workspace_root: &Path,
    factors: &Factors,
    go_bin_addr: &str,
    goroot: &str,
) -> anyhow::Result<TargetSpec> {
    let src_hash = compute_src_hash(workspace_root, None)?;
    build_spec_inner(
        addr,
        import_path,
        workspace_root,
        src_hash,
        factors,
        go_bin_addr,
        goroot,
    )
}

fn build_spec_inner(
    addr: Addr,
    import_path: &str,
    module_root: &Path,
    src_hash: String,
    factors: &Factors,
    go_bin_addr: &str,
    goroot: &str,
) -> anyhow::Result<TargetSpec> {
    let module_root_str = module_root
        .to_str()
        .context("module_root must be valid UTF-8")?;

    let flags = factors.go_list_flags();
    let tags_str = if factors.build_tags.is_empty() {
        String::new()
    } else {
        format!(" tags:{}", factors.build_tags.join(","))
    };

    let flags_part = if flags.is_empty() {
        String::new()
    } else {
        format!(
            " {}",
            flags
                .iter()
                .map(|f| shell_quote(f))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };

    let run = format!(
        "# _golist hash:{src_hash} goos:{goos} goarch:{goarch}{tags_str}\n\
         sandbox_dir=\"$PWD\"\n\
         cd \"$GOLIST_MODULE_ROOT\"\n\
         \"$SRC_GO_BIN\" list -json -e -deps{flags_part} \"$GOLIST_IMPORT_PATH\" > \"$sandbox_dir/deps.json\"\n",
        src_hash = src_hash,
        goos = factors.goos,
        goarch = factors.goarch,
        tags_str = tags_str,
        flags_part = flags_part,
    );

    let runtime_env: HashMap<String, TargetSpecValue> = [
        ("GOOS", factors.goos.as_str()),
        ("GOARCH", factors.goarch.as_str()),
        ("GOROOT", goroot),
        ("GOLIST_MODULE_ROOT", module_root_str),
        ("GOLIST_IMPORT_PATH", import_path),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), TargetSpecValue::String(v.to_string())))
    .collect();

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
            "json".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String("deps.json".to_string())]),
        )])),
    );
    config.insert("cache".to_string(), TargetSpecValue::Bool(true));
    config.insert("runtime_env".to_string(), TargetSpecValue::Map(runtime_env));
    config.insert(
        "runtime_pass_env".to_string(),
        TargetSpecValue::List(vec![
            TargetSpecValue::String("GOPATH".to_string()),
            TargetSpecValue::String("GOMODCACHE".to_string()),
            TargetSpecValue::String("GOCACHE".to_string()),
            TargetSpecValue::String("HOME".to_string()),
            TargetSpecValue::String("GONOSUMDB".to_string()),
            TargetSpecValue::String("GOFLAGS".to_string()),
            TargetSpecValue::String("GOPRIVATE".to_string()),
        ]),
    );

    Ok(TargetSpec {
        addr,
        driver: "bash".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    })
}

/// Hash go.mod + go.sum from `module_root`, and optionally all .go files from `src_dir`.
/// The hash is embedded in the run script so that file changes invalidate the target cache.
fn compute_src_hash(module_root: &Path, src_dir: Option<&Path>) -> anyhow::Result<String> {
    let mut h = Xxh3Default::new();

    for file_name in &["go.mod", "go.sum"] {
        let path = module_root.join(file_name);
        if let Ok(content) = std::fs::read(&path) {
            Hasher::write(&mut h, file_name.as_bytes());
            Hasher::write(&mut h, &content);
        }
    }

    if let Some(dir) = src_dir {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                let n = e.file_name();
                n.to_string_lossy().ends_with(".go")
                    && e.file_type().map(|t| t.is_file()).unwrap_or(false)
            })
            .collect();
        files.sort_by_key(|e| e.file_name());

        for entry in files {
            let name = entry.file_name();
            if let Ok(content) = std::fs::read(entry.path()) {
                Hasher::write(&mut h, name.as_encoded_bytes());
                Hasher::write(&mut h, &content);
            }
        }
    }

    Ok(format!("{:x}", h.digest()))
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htpkg::PkgBuf;

    fn test_addr() -> Addr {
        Addr {
            package: PkgBuf::from("mylib"),
            name: "_golist".to_string(),
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

    fn make_module_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("go.mod"),
            "module example.com/mylib\n\ngo 1.21\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("lib.go"), "package mylib\n").unwrap();
        dir
    }

    #[test]
    fn test_driver_is_bash() {
        let tmp = make_module_root();
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            tmp.path(),
            tmp.path(),
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        )
        .unwrap();
        assert_eq!(spec.driver, "bash");
    }

    #[test]
    fn test_cache_true() {
        let tmp = make_module_root();
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            tmp.path(),
            tmp.path(),
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        )
        .unwrap();
        assert!(matches!(
            spec.config.get("cache"),
            Some(TargetSpecValue::Bool(true))
        ));
    }

    #[test]
    fn test_out_has_json_group() {
        let tmp = make_module_root();
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            tmp.path(),
            tmp.path(),
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        )
        .unwrap();
        let out = match spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(out.contains_key("json"), "out should have 'json' group");
    }

    #[test]
    fn test_run_contains_go_list() {
        let tmp = make_module_root();
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            tmp.path(),
            tmp.path(),
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        )
        .unwrap();
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            run.contains("list -json -e -deps"),
            "run should invoke go list"
        );
        assert!(run.contains("deps.json"), "run should output deps.json");
        assert!(run.contains("SRC_GO_BIN"), "run should use $SRC_GO_BIN");
    }

    #[test]
    fn test_run_embeds_goos_goarch_in_comment() {
        let tmp = make_module_root();
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            tmp.path(),
            tmp.path(),
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        )
        .unwrap();
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(
            run.contains("goos:linux"),
            "run comment should include goos"
        );
        assert!(
            run.contains("goarch:amd64"),
            "run comment should include goarch"
        );
    }

    #[test]
    fn test_src_hash_changes_when_file_changes() {
        let tmp = make_module_root();

        let spec1 = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            tmp.path(),
            tmp.path(),
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        )
        .unwrap();

        // Modify a file
        std::fs::write(
            tmp.path().join("lib.go"),
            "package mylib\n\nfunc Foo() {}\n",
        )
        .unwrap();

        let spec2 = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            tmp.path(),
            tmp.path(),
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        )
        .unwrap();

        let run1 = match spec1.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        let run2 = match spec2.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        assert_ne!(
            run1, run2,
            "run script should change when source file changes"
        );
    }

    #[test]
    fn test_deps_has_go_bin() {
        let tmp = make_module_root();
        let spec = build_spec_firstparty(
            test_addr(),
            "example.com/mylib",
            tmp.path(),
            tmp.path(),
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        )
        .unwrap();
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(deps.contains_key("go_bin"));
    }

    #[test]
    fn test_thirdparty_only_hashes_mod_files() {
        let tmp = make_module_root();
        let spec = build_spec_thirdparty(
            test_addr(),
            "github.com/foo/bar",
            tmp.path(),
            &test_factors(),
            "//@heph/bin:go",
            "/usr/local/go",
        )
        .unwrap();
        assert_eq!(spec.driver, "bash");
        let out = match spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("json"));
    }
}
