use std::collections::BTreeMap;

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

/// Resolve a single `//go:embed` pattern against a flat list of candidate files
/// (package-relative, `/`-separated paths), mirroring Go's `cmd/go` embed
/// resolution. Returns the files Go would embed for `pattern`.
///
/// Used for the `go_embed_src` lane: those source files are *not* staged into
/// `_golist`, so `go list` never resolves them. The integrated compile driver
/// stages them and calls this to reproduce Go's `EmbedFiles` for the patterns
/// `go list` reported but could not resolve.
///
/// Rules (verified against real `go list`):
/// - `all:` prefix disables the leading-`.`/`_` exclusion during directory
///   recursion (otherwise such entries are skipped).
/// - The pattern is matched segment-by-segment via `path.Match` semantics (no
///   separator crossing). A `*` matches dot/underscore entries at the matched
///   level — Go does too.
/// - A matched file is embedded directly. A matched directory is walked
///   recursively; entries whose name begins with `.` or `_` are skipped unless
///   `all:` was given. The matched directory's own name is exempt from that
///   rule (it was named explicitly, by literal or glob).
pub fn select_pattern_files(pattern: &str, files: &[String]) -> Vec<String> {
    let all = pattern.starts_with("all:");
    let p = pattern.strip_prefix("all:").unwrap_or(pattern);
    let pat_segs: Vec<&str> = p.split('/').collect();

    // Per-segment matcher: exact compare unless the segment carries glob meta.
    let seg_matches = |pat_seg: &str, name: &str| -> bool {
        if pat_seg.contains(['*', '?', '[']) {
            match glob::Pattern::new(pat_seg) {
                // Single path element → no separators to worry about; Go's `*`
                // matches a leading dot, which is glob's default.
                Ok(g) => g.matches(name),
                Err(_) => false,
            }
        } else {
            pat_seg == name
        }
    };
    // Does path `entry` (split into segments) match the pattern exactly?
    let entry_matches = |entry: &str| -> bool {
        let segs: Vec<&str> = entry.split('/').collect();
        segs.len() == pat_segs.len()
            && pat_segs
                .iter()
                .zip(segs.iter())
                .all(|(ps, s)| seg_matches(ps, s))
    };
    // Is `entry` a directory in the candidate set (a strict path-prefix of some
    // file)? Derived purely from the flat file list — no filesystem access.
    let is_dir = |entry: &str| -> bool {
        let prefix = format!("{entry}/");
        files.iter().any(|f| f.starts_with(&prefix))
    };
    // Walk a matched directory, applying Go's leading-`.`/`_` exclusion to every
    // element *below* the matched root.
    let push_dir_files = |dir: &str, out: &mut Vec<String>| {
        let prefix = format!("{dir}/");
        for f in files {
            let Some(rel) = f.strip_prefix(&prefix) else {
                continue;
            };
            if all || !rel.split('/').any(|seg| seg.starts_with(['.', '_'])) {
                out.push(f.clone());
            }
        }
    };

    let mut matched: Vec<String> = Vec::new();
    // Collect the set of matched roots: candidate files matching the pattern
    // directly, plus directories matching it (expanded below).
    for f in files {
        if entry_matches(f) {
            matched.push(f.clone());
        }
    }
    // Directory roots: every ancestor path of a file. Track which we've expanded
    // so a dir shared by many files is walked once.
    let mut seen_dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for f in files {
        let mut acc = String::new();
        for seg in f.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(seg);
            if acc.as_str() == f.as_str() {
                break; // the file itself, not a dir
            }
            if seen_dirs.contains(&acc) {
                continue;
            }
            if is_dir(&acc) && entry_matches(&acc) {
                seen_dirs.insert(acc.clone());
                push_dir_files(&acc, &mut matched);
            }
        }
    }

    matched.sort();
    matched.dedup();
    matched
}

/// Union Go's authoritative `EmbedFiles` (`go_src`, already resolved by
/// `go list`) with the selector's resolution of the `go_embed_src` files for
/// each pattern. Returns a sorted, deduped package-relative file list.
pub fn merge_embed_files(
    go_src_files: Vec<String>,
    patterns: &[String],
    embed_src_rel: &[String],
) -> Vec<String> {
    let mut files = go_src_files;
    if !embed_src_rel.is_empty() {
        for pattern in patterns {
            files.extend(select_pattern_files(pattern, embed_src_rel));
        }
    }
    files.sort();
    files.dedup();
    files
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

    /// The fixture used to reverse-engineer Go's selection, kept in sync with
    /// the `go list` ground truth captured during development.
    fn selector_fixture() -> Vec<String> {
        [
            "assets/a.txt",
            "assets/b.log",
            "assets/.dot.txt",
            "assets/_u.txt",
            "assets/sub/c.txt",
            "assets/sub/.d.txt",
            "assets/.hid/h.txt",
            "assets/_und/u.txt",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn select_sorted(pattern: &str) -> Vec<String> {
        let mut v = select_pattern_files(pattern, &selector_fixture());
        v.sort();
        v
    }

    #[test]
    fn test_select_plain_dir_excludes_dot_and_underscore() {
        // `assets`: recurse, skip any element starting with . or _.
        assert_eq!(
            select_sorted("assets"),
            vec!["assets/a.txt", "assets/b.log", "assets/sub/c.txt"]
        );
    }

    #[test]
    fn test_select_all_prefix_includes_everything() {
        assert_eq!(
            select_sorted("all:assets"),
            vec![
                "assets/.dot.txt",
                "assets/.hid/h.txt",
                "assets/_u.txt",
                "assets/_und/u.txt",
                "assets/a.txt",
                "assets/b.log",
                "assets/sub/.d.txt",
                "assets/sub/c.txt",
            ]
        );
    }

    #[test]
    fn test_select_star_matches_dotfiles_at_level_then_recurses() {
        // `assets/*`: `*` matches dot/underscore entries at this level; matched
        // dirs recurse with the normal skip rule (so sub/.d.txt is excluded).
        assert_eq!(
            select_sorted("assets/*"),
            vec![
                "assets/.dot.txt",
                "assets/.hid/h.txt",
                "assets/_u.txt",
                "assets/_und/u.txt",
                "assets/a.txt",
                "assets/b.log",
                "assets/sub/c.txt",
            ]
        );
    }

    #[test]
    fn test_select_glob_with_extension() {
        // `assets/*.txt`: matches .txt files at one level, including dotfiles.
        assert_eq!(
            select_sorted("assets/*.txt"),
            vec!["assets/.dot.txt", "assets/_u.txt", "assets/a.txt"]
        );
    }

    #[test]
    fn test_select_nested_plain_dir() {
        assert_eq!(select_sorted("assets/sub"), vec!["assets/sub/c.txt"]);
    }

    #[test]
    fn test_select_literal_file() {
        assert_eq!(select_sorted("assets/a.txt"), vec!["assets/a.txt"]);
    }

    #[test]
    fn test_select_literal_dotfile_is_included() {
        // A literal pattern naming a dotfile embeds it (the skip rule only
        // applies to glob matches and directory recursion).
        assert_eq!(
            select_pattern_files("assets/.dot.txt", &selector_fixture()),
            vec!["assets/.dot.txt"]
        );
    }

    #[test]
    fn test_select_no_match_is_empty() {
        assert!(select_pattern_files("missing/*", &selector_fixture()).is_empty());
    }

    /// Parity guard: when a host `go` is available, diff our selector against
    /// real `go list EmbedFiles` for each pattern. Skips silently otherwise so
    /// CI without a host toolchain still passes.
    #[test]
    fn test_selector_matches_real_go_list() {
        use std::process::Command;
        let go = match which_go() {
            Some(g) => g,
            None => return,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("go.mod"), "module example.com/m\ngo 1.22\n").unwrap();
        let pkg = root.join("p");
        let files = selector_fixture();
        for f in &files {
            let dst = pkg.join(f);
            std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
            std::fs::write(&dst, b"x").unwrap();
        }
        for pattern in [
            "assets",
            "all:assets",
            "assets/*",
            "assets/*.txt",
            "assets/sub",
            "assets/a.txt",
        ] {
            let src = format!(
                "package p\nimport \"embed\"\n//go:embed {pattern}\nvar A embed.FS\nvar _ = A\n"
            );
            std::fs::write(pkg.join("a.go"), src).unwrap();
            let out = Command::new(&go)
                .args(["list", "-json=EmbedFiles", "./p"])
                .current_dir(root)
                .env("GOWORK", "off")
                .env("GOFLAGS", "")
                .output()
                .expect("run go list");
            assert!(
                out.status.success(),
                "go list failed for {pattern}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            let mut want: Vec<String> = v["EmbedFiles"]
                .as_array()
                .map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect())
                .unwrap_or_default();
            want.sort();
            assert_eq!(
                select_sorted(pattern),
                want,
                "mismatch for pattern {pattern}"
            );
        }
    }

    fn which_go() -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|p| p.join("go"))
            .find(|p| p.is_file())
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
