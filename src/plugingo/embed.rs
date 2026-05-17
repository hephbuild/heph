#[cfg(test)]
use crate::engine::provider::TargetSpec;
#[cfg(test)]
use crate::htaddr::Addr;
#[cfg(test)]
use crate::loosespecparser::TargetSpecValue;
use anyhow::Context;
use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::HashMap;
use std::path::Path;

/// Build the `embed` target spec.
/// Resolves each Go embed pattern against src_dir and encodes the result as a
/// static embedcfg JSON written out at build time via `printf`.
/// `embed_files` is Go's authoritative resolved file list from `go list`; it is
/// merged into the `Files` map alongside pattern-resolved files.
#[cfg(test)]
pub fn build_spec(
    addr: Addr,
    embed_patterns: &[String],
    embed_files: &[String],
    src_dir: &Path,
) -> anyhow::Result<TargetSpec> {
    let cfg_json = compute_embed_cfg_json(embed_patterns, embed_files, src_dir)
        .context("compute embed cfg json")?;

    // Escape single quotes so the JSON can be embedded in a bash single-quoted string.
    let escaped_json = cfg_json.replace('\'', "'\\''");
    let run = format!("printf '%s\\n' '{escaped_json}' > embedcfg\n");

    let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
    config.insert("run".to_string(), TargetSpecValue::String(run));
    config.insert(
        "out".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            "cfg".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String("embedcfg".to_string())]),
        )])),
    );
    Ok(TargetSpec {
        addr,
        driver: "bash".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    })
}

/// Walk a single glob pattern relative to src_dir and return all matched files
/// as paths relative to src_dir.  Directories are expanded recursively.
pub(crate) fn resolve_embed_pattern(src_dir: &Path, pattern: &str) -> anyhow::Result<Vec<String>> {
    let full_pattern = src_dir.join(pattern);
    let pattern_str = full_pattern
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("embed pattern path is not valid UTF-8: {pattern}"))?;

    let mut files = Vec::new();

    for entry in glob::glob(pattern_str)
        .with_context(|| format!("invalid glob pattern: {pattern}"))?
        .flatten()
    {
        if entry.is_dir() {
            for walk_entry in walkdir::WalkDir::new(&entry)
                .into_iter()
                .flatten()
                .filter(|e| e.file_type().is_file())
            {
                let rel = walk_entry.path().strip_prefix(src_dir).with_context(|| {
                    format!(
                        "strip prefix {} from {}",
                        src_dir.display(),
                        walk_entry.path().display()
                    )
                })?;
                files.push(rel.to_string_lossy().into_owned());
            }
        } else if let Ok(rel) = entry.strip_prefix(src_dir) {
            files.push(rel.to_string_lossy().into_owned());
        }
    }

    if files.is_empty() {
        anyhow::bail!("embed pattern {pattern}: no matching files found");
    }

    files.sort();
    Ok(files)
}

/// Produce the JSON string expected by `go tool compile -embedcfg`.
/// `embed_files` (from `go list EmbedFiles`) is Go's authoritative resolved
/// file list and is merged into `Files` so we don't miss files that Go includes
/// via its own exclusion rules (no `.`/`_` prefix, etc.).
pub(crate) fn compute_embed_cfg_json(
    embed_patterns: &[String],
    embed_files: &[String],
    src_dir: &Path,
) -> anyhow::Result<String> {
    let mut patterns_map: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut files_map: BTreeMap<String, String> = BTreeMap::new();

    for pattern in embed_patterns {
        let resolved = resolve_embed_pattern(src_dir, pattern)
            .with_context(|| format!("resolving embed pattern {pattern}"))?;
        for file in &resolved {
            let abs_path = src_dir.join(file);
            files_map.insert(file.clone(), abs_path.to_string_lossy().into_owned());
        }
        patterns_map.insert(pattern.as_str(), resolved);
    }

    for file in embed_files {
        files_map
            .entry(file.clone())
            .or_insert_with(|| src_dir.join(file).to_string_lossy().into_owned());
    }

    Ok(serde_json::json!({
        "Patterns": patterns_map,
        "Files": files_map,
    })
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htpkg::PkgBuf;
    use std::fs;

    fn test_addr() -> Addr {
        Addr {
            package: PkgBuf::from("mylib"),
            name: "embed".to_string(),
            args: Default::default(),
        }
    }

    fn fixture_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/plugingo/testdata/with_embed/server")
    }

    fn make_spec_from_fixture() -> TargetSpec {
        let dir = fixture_dir();
        build_spec(test_addr(), &["static/index.html".to_string()], &[], &dir).unwrap()
    }

    #[test]
    fn test_embed_driver_is_bash() {
        let spec = make_spec_from_fixture();
        assert_eq!(spec.driver, "bash");
    }

    #[test]
    fn test_embed_out_cfg_group() {
        let spec = make_spec_from_fixture();
        let out = spec.config.get("out").unwrap();
        assert!(matches!(out, TargetSpecValue::Map(m) if m.contains_key("cfg")));
    }

    #[test]
    fn test_embed_run_emits_embedcfg() {
        let spec = make_spec_from_fixture();
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(
            run.contains("embedcfg"),
            "run must reference embedcfg: {run}"
        );
    }

    #[test]
    fn test_embed_run_contains_pattern() {
        let spec = make_spec_from_fixture();
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(
            run.contains("static/index.html"),
            "run must contain the embed pattern in the JSON: {run}"
        );
    }

    #[test]
    fn test_embed_run_contains_absolute_path() {
        let dir = fixture_dir();
        let spec = build_spec(test_addr(), &["static/index.html".to_string()], &[], &dir).unwrap();
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!(),
        };
        let abs = dir.join("static/index.html");
        assert!(
            run.contains(abs.to_str().unwrap()),
            "run must contain the absolute path to embed files: {run}"
        );
    }

    #[test]
    fn test_embed_cfg_json_structure() {
        let dir = fixture_dir();
        let json = compute_embed_cfg_json(&["static/index.html".to_string()], &[], &dir).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["Patterns"]["static/index.html"].is_array());
        assert!(v["Files"]["static/index.html"].is_string());
    }

    #[test]
    fn test_embed_cfg_dir_pattern_expands_files() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("assets");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("a.txt"), "a").unwrap();
        fs::write(sub.join("b.txt"), "b").unwrap();

        let json = compute_embed_cfg_json(&["assets/*".to_string()], &[], tmp.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();

        let pattern_files = v["Patterns"]["assets/*"].as_array().unwrap();
        assert_eq!(pattern_files.len(), 2);
        assert!(v["Files"]["assets/a.txt"].is_string());
        assert!(v["Files"]["assets/b.txt"].is_string());
    }

    #[test]
    fn test_embed_pattern_no_match_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let result = build_spec(test_addr(), &["nonexistent/*".to_string()], &[], tmp.path());
        assert!(result.is_err(), "should error when pattern matches nothing");
    }

    #[test]
    fn test_embed_files_merged_into_files_map() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("assets");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("a.txt"), "a").unwrap();

        // embed_files contains a file not covered by any pattern
        let embed_files = vec!["assets/extra.txt".to_string()];
        let json = compute_embed_cfg_json(&["assets/a.txt".to_string()], &embed_files, tmp.path())
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert!(
            v["Files"]["assets/a.txt"].is_string(),
            "pattern-resolved file must be in Files"
        );
        assert!(
            v["Files"]["assets/extra.txt"].is_string(),
            "embed_files entry must be merged into Files"
        );
    }

    #[test]
    fn test_embed_files_does_not_duplicate_pattern_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("assets");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("a.txt"), "a").unwrap();

        // same file in both embed_patterns (resolved) and embed_files
        let embed_files = vec!["assets/a.txt".to_string()];
        let json = compute_embed_cfg_json(&["assets/a.txt".to_string()], &embed_files, tmp.path())
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();

        let files = v["Files"].as_object().unwrap();
        assert_eq!(
            files
                .keys()
                .filter(|k| k.as_str() == "assets/a.txt")
                .count(),
            1,
            "file must appear exactly once in Files map"
        );
    }
}
