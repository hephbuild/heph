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

/// Run `go list -json -e -deps` directly as a subprocess.
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
        .arg("-deps")
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
