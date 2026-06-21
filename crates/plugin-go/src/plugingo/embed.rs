#[cfg(test)]
use anyhow::Context;
#[cfg(test)]
use hcore::htvalue::Value;
#[cfg(test)]
use hmodel::htaddr::Addr;
#[cfg(test)]
use hplugin::provider::TargetSpec;
use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::HashMap;

/// Build the `embed` target spec.
/// Encodes the embedcfg JSON for `go tool compile -embedcfg` purely from the
/// `embed_patterns` and `embed_files` lists produced by `go list` — no
/// filesystem access. The cfg `Files` map is therefore guaranteed to align with
/// what downstream `build_lib` sandboxes stage (also derived from `EmbedFiles`).
#[cfg(test)]
pub fn build_spec(
    addr: Addr,
    embed_patterns: &[String],
    embed_files: &[String],
) -> anyhow::Result<TargetSpec> {
    let cfg_json =
        compute_embed_cfg_json(embed_patterns, embed_files).context("compute embed cfg json")?;

    // Escape single quotes so the JSON can be embedded in a shell single-quoted string.
    let escaped_json = cfg_json.replace('\'', "'\\''");
    let run = format!("printf '%s\\n' '{escaped_json}' > embedcfg\n");

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("run".to_string(), Value::String(run));
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(
            "cfg".to_string(),
            Value::List(vec![Value::String("embedcfg".to_string())]),
        )])),
    );
    Ok(TargetSpec {
        addr,
        driver: "sh".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    })
}

/// Group `embed_files` (Go's resolved EmbedFiles list) by the pattern that
/// matches them. Pure path-based matching: no filesystem access.
///
/// Pattern semantics mirror `//go:embed`:
/// - `all:` prefix is stripped (Go has already applied its exclusion rules to
///   `embed_files`, so we only need pattern→file grouping here).
/// - Patterns containing `*`, `?`, or `[` use `path.Match`-style globbing with
///   no separator crossing (`require_literal_separator = true`).
/// - Patterns without glob metachars match by exact file equality or by
///   directory-prefix (`p/...`).
fn files_matching_pattern(pattern: &str, files: &[String]) -> Vec<String> {
    let p = pattern.strip_prefix("all:").unwrap_or(pattern);
    let has_meta = p.contains(['*', '?', '[']);
    let mut matched: Vec<String> = if has_meta {
        let glob_pat = match glob::Pattern::new(p) {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let opts = glob::MatchOptions {
            case_sensitive: true,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        files
            .iter()
            .filter(|f| glob_pat.matches_with(f, opts))
            .cloned()
            .collect()
    } else {
        let prefix = format!("{p}/");
        files
            .iter()
            .filter(|f| f.as_str() == p || f.starts_with(&prefix))
            .cloned()
            .collect()
    };
    matched.sort();
    matched
}

/// Produce the JSON string expected by `go tool compile -embedcfg`.
///
/// `embed_files` (from `go list EmbedFiles`) is Go's authoritative resolved
/// file list. The `Patterns` map is built by grouping those files under each
/// pattern via `files_matching_pattern`; the `Files` map is the same list as
/// identity mappings. No filesystem access — keeps the embedcfg in sync with
/// what downstream `build_lib` sandboxes stage (which also comes from
/// `EmbedFiles`).
///
/// Every `//go:embed` pattern must resolve to at least one file — that is a Go
/// language rule, not just a convention. If `go list` ran with `-e` and the
/// embed source files were not staged (e.g. a package whose embed inputs were
/// never wired into the heph go provider), it reports `EmbedPatterns` with an
/// empty `EmbedFiles`, and emitting that cfg makes `go tool compile` panic in
/// `staticdata.WriteEmbed` (`index out of range [0] with length 0`) instead of
/// reporting a clean error. We reject it here so the failure names the offending
/// pattern and points at the missing embed inputs.
pub fn compute_embed_cfg_json(
    embed_patterns: &[String],
    embed_files: &[String],
) -> anyhow::Result<String> {
    let mut patterns_map: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut files_map: BTreeMap<&str, &str> = BTreeMap::new();

    let mut unmatched: Vec<&str> = Vec::new();
    for pattern in embed_patterns {
        let resolved = files_matching_pattern(pattern, embed_files);
        if resolved.is_empty() {
            unmatched.push(pattern.as_str());
        }
        patterns_map.insert(pattern.as_str(), resolved);
    }

    if !unmatched.is_empty() {
        anyhow::bail!(
            "//go:embed pattern(s) matched no files: {}. \
             go list resolved these patterns to zero files — the embed source files \
             are not staged. Declare the embed inputs (e.g. a BUILD2 provider_state(provider=\"go\") \
             with go_src-labeled group targets) so the package's embed sources reach the sandbox.",
            unmatched.join(", "),
        );
    }

    for file in embed_files {
        files_map.insert(file.as_str(), file.as_str());
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
    use hmodel::htpkg::PkgBuf;

    fn test_addr() -> Addr {
        Addr::new(
            PkgBuf::from("mylib"),
            "embed".to_string(),
            Default::default(),
        )
    }

    #[test]
    fn test_embed_driver_is_bash() {
        let spec = build_spec(
            test_addr(),
            &["static/index.html".to_string()],
            &["static/index.html".to_string()],
        )
        .unwrap();
        assert_eq!(spec.driver, "sh");
    }

    #[test]
    fn test_embed_out_cfg_group() {
        let spec = build_spec(
            test_addr(),
            &["static/index.html".to_string()],
            &["static/index.html".to_string()],
        )
        .unwrap();
        let out = spec.config.get("out").unwrap();
        assert!(matches!(out, Value::Map(m) if m.contains_key("cfg")));
    }

    #[test]
    fn test_embed_run_emits_embedcfg() {
        let spec = build_spec(
            test_addr(),
            &["static/index.html".to_string()],
            &["static/index.html".to_string()],
        )
        .unwrap();
        let run = match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(
            run.contains("embedcfg"),
            "run must reference embedcfg: {run}"
        );
    }

    #[test]
    fn test_embed_run_contains_pattern() {
        let spec = build_spec(
            test_addr(),
            &["static/index.html".to_string()],
            &["static/index.html".to_string()],
        )
        .unwrap();
        let run = match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
            _ => panic!(),
        };
        assert!(
            run.contains("static/index.html"),
            "run must contain the embed pattern in the JSON: {run}"
        );
    }

    #[test]
    fn test_embed_files_paths_are_relative() {
        // Files map values must NOT be host-absolute — they must stay pkg-rel so
        // the cfg is portable across sandboxes.
        let json = compute_embed_cfg_json(
            &["static/index.html".to_string()],
            &["static/index.html".to_string()],
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let val = v["Files"]["static/index.html"].as_str().unwrap();
        assert!(
            !std::path::Path::new(val).is_absolute(),
            "expected rel: {val}"
        );
        assert_eq!(val, "static/index.html");
    }

    #[test]
    fn test_embed_cfg_json_structure() {
        let json = compute_embed_cfg_json(
            &["static/index.html".to_string()],
            &["static/index.html".to_string()],
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["Patterns"]["static/index.html"].is_array());
        assert!(v["Files"]["static/index.html"].is_string());
    }

    #[test]
    fn test_dir_pattern_groups_files_recursively() {
        let files = vec![
            "assets/a.txt".to_string(),
            "assets/sub/b.txt".to_string(),
            "other/c.txt".to_string(),
        ];
        let json = compute_embed_cfg_json(&["assets".to_string()], &files).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let pattern_files = v["Patterns"]["assets"].as_array().unwrap();
        let names: Vec<&str> = pattern_files.iter().map(|x| x.as_str().unwrap()).collect();
        assert_eq!(names, vec!["assets/a.txt", "assets/sub/b.txt"]);
    }

    #[test]
    fn test_star_pattern_matches_single_level() {
        let files = vec![
            "assets/a.txt".to_string(),
            "assets/b.txt".to_string(),
            "assets/sub/c.txt".to_string(),
        ];
        let json = compute_embed_cfg_json(&["assets/*".to_string()], &files).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let pattern_files = v["Patterns"]["assets/*"].as_array().unwrap();
        let names: Vec<&str> = pattern_files.iter().map(|x| x.as_str().unwrap()).collect();
        // `*` must not cross `/` — sub/c.txt excluded.
        assert_eq!(names, vec!["assets/a.txt", "assets/b.txt"]);
    }

    #[test]
    fn test_all_prefix_does_not_affect_matching() {
        // Go has already applied `all:` rules to embed_files; matching only
        // needs to strip the prefix for grouping.
        let files = vec!["assets/.hidden".to_string(), "assets/visible".to_string()];
        let json = compute_embed_cfg_json(&["all:assets".to_string()], &files).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let pattern_files = v["Patterns"]["all:assets"].as_array().unwrap();
        let names: Vec<&str> = pattern_files.iter().map(|x| x.as_str().unwrap()).collect();
        assert_eq!(names, vec!["assets/.hidden", "assets/visible"]);
    }

    #[test]
    fn test_files_map_mirrors_embed_files() {
        // Regression: Files map must contain every entry from embed_files and
        // nothing else — otherwise the cfg references files the consumer's
        // sandbox does not stage.
        let files = vec!["a.txt".to_string(), "sub/b.txt".to_string()];
        let json = compute_embed_cfg_json(&["a.txt".to_string()], &files).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files_map = v["Files"].as_object().unwrap();
        let mut keys: Vec<&str> = files_map.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, vec!["a.txt", "sub/b.txt"]);
    }

    #[test]
    fn test_pattern_matching_no_files_errors() {
        // Regression for the `go tool compile` ICE: a pattern that resolves to
        // zero files must be rejected here (Go itself rejects it), not emitted
        // as an embedcfg with an empty file list that panics WriteEmbed.
        let err = compute_embed_cfg_json(
            &[
                "resources/infhostd_ifplugd_nsg101.sh".to_string(),
                "ui_dist/*".to_string(),
            ],
            &[],
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("matched no files"), "msg: {msg}");
        assert!(
            msg.contains("resources/infhostd_ifplugd_nsg101.sh") && msg.contains("ui_dist/*"),
            "error must name the offending patterns: {msg}"
        );
    }

    #[test]
    fn test_partial_unmatched_pattern_errors() {
        // One pattern resolves, the other does not: still an error, and only the
        // unresolved pattern is named.
        let err = compute_embed_cfg_json(
            &["assets/*".to_string(), "missing/*".to_string()],
            &["assets/a.txt".to_string()],
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("missing/*"), "msg: {msg}");
        assert!(
            !msg.contains("assets/*"),
            "resolved pattern must not be named: {msg}"
        );
    }

    #[test]
    fn test_no_dotfile_unless_in_embed_files() {
        // Regression for `.terraform.lock`-style files: the cfg must not list a
        // dotfile that Go's EmbedFiles excluded. Pure path matching achieves
        // this because the file is simply absent from the input list.
        let files = vec!["terraform/main.tf".to_string()];
        let json = compute_embed_cfg_json(&["terraform".to_string()], &files).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files_map = v["Files"].as_object().unwrap();
        assert!(!files_map.contains_key("terraform/.terraform.lock"));
        let pattern_files = v["Patterns"]["terraform"].as_array().unwrap();
        let names: Vec<&str> = pattern_files.iter().map(|x| x.as_str().unwrap()).collect();
        assert_eq!(names, vec!["terraform/main.tf"]);
    }
}
