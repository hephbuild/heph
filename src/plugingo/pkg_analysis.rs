use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufReader, Read};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoPackage {
    #[serde(rename = "ImportPath")]
    pub import_path: String,
    #[serde(rename = "Dir", default)]
    pub dir: Option<String>,
    #[serde(rename = "Name", default)]
    pub name: Option<String>,
    #[serde(rename = "GoFiles", default)]
    pub go_files: Vec<String>,
    #[serde(rename = "TestGoFiles", default)]
    pub test_go_files: Vec<String>,
    #[serde(rename = "XTestGoFiles", default)]
    pub xtest_go_files: Vec<String>,
    #[serde(rename = "EmbedPatterns", default)]
    pub embed_patterns: Vec<String>,
    #[serde(rename = "EmbedFiles", default)]
    pub embed_files: Vec<String>,
    #[serde(rename = "Imports", default)]
    pub imports: Vec<String>,
    #[serde(rename = "TestImports", default)]
    pub test_imports: Vec<String>,
    #[serde(rename = "XTestImports", default)]
    pub xtest_imports: Vec<String>,
    #[serde(rename = "Standard", default)]
    pub standard: bool,
    #[serde(rename = "Module")]
    pub module: Option<GoModule>,
    #[serde(rename = "Match", default)]
    pub match_: Vec<String>,
    #[serde(rename = "Incomplete", default)]
    pub incomplete: bool,
    #[serde(rename = "Error")]
    pub error: Option<GoPackageError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoModule {
    #[serde(rename = "Path")]
    pub path: String,
    #[serde(rename = "Version")]
    pub version: Option<String>,
    #[serde(rename = "Dir")]
    pub dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoPackageError {
    #[serde(rename = "Err")]
    pub err: String,
}

/// Parse packages from a reader, streaming without loading the full content.
/// Returns a map from import_path → GoPackage built in a single pass.
pub fn parse_go_list_reader(reader: impl Read) -> anyhow::Result<HashMap<String, GoPackage>> {
    serde_json::Deserializer::from_reader(BufReader::with_capacity(64 * 1024, reader))
        .into_iter::<GoPackage>()
        .map(|r| {
            r.context("parse go list json")
                .map(|p| (p.import_path.clone(), p))
        })
        .collect()
}

/// Returns `true` if `import_path` is a Go standard-library import path.
///
/// The heuristic: if the first path component contains no dot, it's stdlib.
/// Examples: `fmt` → true, `net/http` → true, `testing/internal/testdeps` → true,
///           `github.com/foo` → false, `golang.org/x/net` → false.
pub fn is_stdlib_import_path(import_path: &str) -> bool {
    let first = import_path.split('/').next().unwrap_or(import_path);
    !first.contains('.')
}

/// Parse the `module` directive from a go.mod file content.
pub fn parse_go_mod_module_path(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("module") {
            let path = rest.trim().trim_matches('"');
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// Parse `require` directives from a go.mod file content.
/// Returns a list of `(module_path, version)` pairs.
pub fn parse_go_mod_requires(content: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();
    let mut in_require_block = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if in_require_block {
            if trimmed == ")" {
                in_require_block = false;
                continue;
            }
            // strip inline comment
            let line_no_comment = trimmed.split("//").next().unwrap_or("").trim();
            let mut parts = line_no_comment.split_whitespace();
            if let (Some(mod_path), Some(version)) = (parts.next(), parts.next()) {
                result.push((mod_path.to_string(), version.to_string()));
            }
            continue;
        }

        // Single-line require: `require module/path v1.2.3`
        if let Some(rest) = trimmed.strip_prefix("require ") {
            let rest = rest.trim();
            if rest.starts_with('(') {
                in_require_block = true;
            } else {
                let line_no_comment = rest.split("//").next().unwrap_or("").trim();
                let mut parts = line_no_comment.split_whitespace();
                if let (Some(mod_path), Some(version)) = (parts.next(), parts.next()) {
                    result.push((mod_path.to_string(), version.to_string()));
                }
            }
        } else if trimmed == "require (" || trimmed == "require(" {
            in_require_block = true;
        }
    }

    result
}

/// Find which module in `requires` best matches `import_path`.
/// Returns `Some((module_path, version))` for the longest matching prefix.
pub fn find_module_for_import(
    import_path: &str,
    requires: &[(String, String)],
) -> Option<(String, String)> {
    let mut best: Option<&(String, String)> = None;
    for req in requires {
        let module = &req.0;
        // exact match or import_path starts with module + "/"
        if (import_path == module.as_str() || import_path.starts_with(&format!("{}/", module)))
            && best.is_none_or(|(b, _)| module.len() > b.len())
        {
            best = Some(req);
        }
    }
    best.cloned()
}

/// Run `go list -json -e` (no -deps) directly as a subprocess.
/// Only used in tests to provide package data to the test executor.
#[cfg(test)]
pub(crate) async fn run_go_list(
    import_path: &str,
    factors: &crate::plugingo::factors::Factors,
    module_root: &std::path::Path,
) -> anyhow::Result<HashMap<String, GoPackage>> {
    let mut cmd = tokio::process::Command::new("go");
    cmd.arg("list")
        .arg("-json")
        .arg("-e")
        .args(factors.go_list_flags())
        .arg(import_path)
        .current_dir(module_root)
        .env("GOOS", &factors.goos)
        .env("GOARCH", &factors.goarch)
        .env_remove("GOFLAGS");

    for var in &["GOROOT", "GOPATH", "GOMODCACHE", "HOME", "PATH", "GOCACHE"] {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }

    let output = cmd.output().await.context("spawn go list")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("go list failed: {}", stderr);
    }

    parse_go_list_reader(output.stdout.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_stdlib_import_path_simple() {
        assert!(is_stdlib_import_path("fmt"));
        assert!(is_stdlib_import_path("net/http"));
        assert!(is_stdlib_import_path("testing"));
        assert!(is_stdlib_import_path("testing/internal/testdeps"));
        assert!(is_stdlib_import_path("os"));
        assert!(is_stdlib_import_path("reflect"));
    }

    #[test]
    fn test_is_stdlib_import_path_third_party() {
        assert!(!is_stdlib_import_path("github.com/foo/bar"));
        assert!(!is_stdlib_import_path("golang.org/x/net"));
        assert!(!is_stdlib_import_path("k8s.io/apimachinery"));
    }

    #[test]
    fn test_parse_go_mod_requires_block() {
        let content = "module example.com/mymod\n\ngo 1.21\n\nrequire (\n\tgithub.com/foo/bar v1.2.3\n\tgolang.org/x/net v0.5.0\n)\n";
        let reqs = parse_go_mod_requires(content);
        assert_eq!(reqs.len(), 2);
        assert!(reqs.contains(&("github.com/foo/bar".to_string(), "v1.2.3".to_string())));
        assert!(reqs.contains(&("golang.org/x/net".to_string(), "v0.5.0".to_string())));
    }

    #[test]
    fn test_parse_go_mod_requires_single_line() {
        let content = "module example.com/mymod\n\nrequire github.com/foo/bar v1.0.0\n";
        let reqs = parse_go_mod_requires(content);
        assert_eq!(reqs.len(), 1);
        assert!(reqs.contains(&("github.com/foo/bar".to_string(), "v1.0.0".to_string())));
    }

    #[test]
    fn test_parse_go_mod_requires_with_comment() {
        let content = "require (\n\tgithub.com/foo/bar v1.2.3 // indirect\n)\n";
        let reqs = parse_go_mod_requires(content);
        assert_eq!(reqs.len(), 1);
        assert!(reqs.contains(&("github.com/foo/bar".to_string(), "v1.2.3".to_string())));
    }

    #[test]
    fn test_find_module_for_import_exact() {
        let requires = vec![("github.com/foo/bar".to_string(), "v1.2.3".to_string())];
        let result = find_module_for_import("github.com/foo/bar", &requires);
        assert_eq!(
            result,
            Some(("github.com/foo/bar".to_string(), "v1.2.3".to_string()))
        );
    }

    #[test]
    fn test_find_module_for_import_subpath() {
        let requires = vec![("github.com/foo/bar".to_string(), "v1.2.3".to_string())];
        let result = find_module_for_import("github.com/foo/bar/pkg", &requires);
        assert_eq!(
            result,
            Some(("github.com/foo/bar".to_string(), "v1.2.3".to_string()))
        );
    }

    #[test]
    fn test_find_module_for_import_longest_prefix() {
        let requires = vec![
            ("github.com/foo".to_string(), "v1.0.0".to_string()),
            ("github.com/foo/bar".to_string(), "v1.2.3".to_string()),
        ];
        let result = find_module_for_import("github.com/foo/bar/pkg", &requires);
        assert_eq!(
            result,
            Some(("github.com/foo/bar".to_string(), "v1.2.3".to_string()))
        );
    }

    #[test]
    fn test_find_module_for_import_not_found() {
        let requires = vec![("github.com/foo/bar".to_string(), "v1.2.3".to_string())];
        let result = find_module_for_import("github.com/baz/qux", &requires);
        assert!(result.is_none());
    }

    const SAMPLE_GO_LIST_JSON: &str = r#"{
	"Dir": "/tmp/mymod/mypkg",
	"ImportPath": "example.com/mymod/mypkg",
	"Name": "mypkg",
	"GoFiles": [
		"file.go"
	],
	"Imports": [
		"fmt"
	],
	"Standard": false,
	"Module": {
		"Path": "example.com/mymod",
		"Version": "v0.0.0"
	}
}
{
	"Dir": "/usr/local/go/src/fmt",
	"ImportPath": "fmt",
	"Name": "fmt",
	"GoFiles": [
		"format.go"
	],
	"Imports": [],
	"Standard": true
}"#;

    #[test]
    fn test_parse_go_list_reader() {
        let pkgs = parse_go_list_reader(SAMPLE_GO_LIST_JSON.as_bytes()).unwrap();
        assert_eq!(pkgs.len(), 2);

        let mypkg = pkgs.get("example.com/mymod/mypkg").unwrap();
        assert_eq!(mypkg.name.as_deref(), Some("mypkg"));
        assert_eq!(mypkg.go_files, vec!["file.go"]);
        assert_eq!(mypkg.imports, vec!["fmt"]);
        assert!(!mypkg.standard);

        let fmt_pkg = pkgs.get("fmt").unwrap();
        assert!(fmt_pkg.standard);
    }

    #[test]
    fn test_parse_go_list_empty() {
        let pkgs = parse_go_list_reader("".as_bytes()).unwrap();
        assert!(pkgs.is_empty());
    }

    #[test]
    fn test_parse_go_list_missing_optional_fields() {
        let json = r#"{"Dir":"/tmp","ImportPath":"foo","Name":"foo"}"#;
        let pkgs = parse_go_list_reader(json.as_bytes()).unwrap();
        assert_eq!(pkgs.len(), 1);
        let pkg = pkgs.get("foo").unwrap();
        assert!(pkg.go_files.is_empty());
        assert!(pkg.imports.is_empty());
        assert!(pkg.module.is_none());
    }
}
