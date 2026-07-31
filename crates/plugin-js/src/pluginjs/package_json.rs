//! A package's own declared dependencies, read straight off its
//! `package.json` — the M1 dependency-wiring input per
//! `ai-docs/js-plugin-plan.md`: "dependency wiring can be driven directly
//! from package.json declared dependencies/devDependencies plus the
//! lockfile resolved graph, without parsing actual import statements" (the
//! oxc-based import-graph resolver is M2 scope).

use anyhow::Context;
use std::collections::BTreeMap;
use std::path::Path;

/// The slice of a `package.json` that dependency wiring needs: its own name
/// plus its three dependency fields (declared semver ranges, not resolved
/// versions — resolution goes through the lockfile).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageManifest {
    pub name: String,
    pub dependencies: BTreeMap<String, String>,
    pub dev_dependencies: BTreeMap<String, String>,
    pub optional_dependencies: BTreeMap<String, String>,
}

impl PackageManifest {
    /// Every declared dependency name, grouped by the `package.json` field it
    /// came from (`"dependencies"` / `"devDependencies"` — optional deps are
    /// folded into `"dependencies"`'s group since they use the same
    /// resolution path and only differ in "missing is not an error").
    pub fn dependency_groups(&self) -> [(&'static str, &BTreeMap<String, String>); 2] {
        [
            ("dependencies", &self.dependencies),
            ("dev_dependencies", &self.dev_dependencies),
        ]
    }

    pub fn is_optional(&self, name: &str) -> bool {
        self.optional_dependencies.contains_key(name)
    }
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawManifest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
}

/// Read and parse a package's `package.json`. A missing `"name"` is a hard
/// error (a malformed package), matching [`super::workspace::read_package_name`].
pub fn read_package_manifest(package_json: &Path) -> anyhow::Result<PackageManifest> {
    let raw = std::fs::read_to_string(package_json)
        .with_context(|| format!("reading {}", package_json.display()))?;
    let parsed: RawManifest = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", package_json.display()))?;
    let name = parsed.name.ok_or_else(|| {
        anyhow::anyhow!("{}: missing required `name` field", package_json.display())
    })?;
    // Optional deps are also declared dependencies for name-collision
    // purposes (a name can't be both required and optional at once in a
    // well-formed manifest); dependencies/devDependencies stay authoritative
    // for group membership.
    let mut dependencies = parsed.dependencies;
    for (k, v) in &parsed.optional_dependencies {
        dependencies.entry(k.clone()).or_insert_with(|| v.clone());
    }
    Ok(PackageManifest {
        name,
        dependencies,
        dev_dependencies: parsed.dev_dependencies,
        optional_dependencies: parsed.optional_dependencies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, contents: &str) -> std::path::PathBuf {
        let path = dir.join("package.json");
        fs::write(&path, contents).expect("write fixture file");
        path
    }

    #[test]
    fn reads_dependencies_and_dev_dependencies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            r#"{
                "name": "a",
                "dependencies": { "lodash": "^4.17.21" },
                "devDependencies": { "vitest": "^1.0.0" }
            }"#,
        );
        let manifest = read_package_manifest(&path).expect("parse manifest");
        assert_eq!(manifest.name, "a");
        assert_eq!(
            manifest.dependencies.get("lodash").map(String::as_str),
            Some("^4.17.21")
        );
        assert_eq!(
            manifest.dev_dependencies.get("vitest").map(String::as_str),
            Some("^1.0.0")
        );
    }

    #[test]
    fn optional_dependencies_are_recognized_and_folded_into_dependencies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            r#"{
                "name": "a",
                "optionalDependencies": { "fsevents": "^2.3.0" }
            }"#,
        );
        let manifest = read_package_manifest(&path).expect("parse manifest");
        assert!(manifest.is_optional("fsevents"));
        assert!(manifest.dependencies.contains_key("fsevents"));
    }

    #[test]
    fn missing_name_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "{}");
        read_package_manifest(&path).unwrap_err();
    }
}
