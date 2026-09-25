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
/// plus its dependency fields (declared semver ranges, not resolved
/// versions — resolution goes through the lockfile).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageManifest {
    pub name: String,
    /// The package's own `"main"` field — `js_bundle`'s default entry point
    /// when no `entry=` addr arg overrides it (see
    /// `pluginjs::provider::Provider::default_entry_for_package`). `None`
    /// when the field is absent; a package with no `"main"` and no `entry=`
    /// override simply has no default `js_bundle` target listed (mirrors
    /// `js_test`'s "no matched files, no listed target" shape).
    pub main: Option<String>,
    pub dependencies: BTreeMap<String, String>,
    pub dev_dependencies: BTreeMap<String, String>,
    pub optional_dependencies: BTreeMap<String, String>,
    /// `peerDependencies` — deliberately **not** folded into `dependencies`
    /// (unlike `optionalDependencies`): a peer dependency is not this
    /// package's own install/build dependency to wire a target-dep edge for
    /// (`deps::resolve_package_deps` never reads this field), it's a
    /// contract the *consumer* is expected to satisfy. It is still a
    /// perfectly legitimate thing for this package's own source to `import`,
    /// though (the single most common real npm pattern for
    /// component/plugin libraries), so `importgraph::declared_closure` folds
    /// it in for phantom-dependency-check purposes — see that module.
    pub peer_dependencies: BTreeMap<String, String>,
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
    main: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    peer_dependencies: BTreeMap<String, String>,
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
        main: parsed.main,
        dependencies,
        dev_dependencies: parsed.dev_dependencies,
        optional_dependencies: parsed.optional_dependencies,
        peer_dependencies: parsed.peer_dependencies,
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

    /// `peerDependencies` are parsed into their own field, and — unlike
    /// `optionalDependencies` — are deliberately *not* folded into
    /// `dependencies` (they're not a target-dep-wiring input; see the field's
    /// doc comment and `importgraph::declared_closure`).
    #[test]
    fn peer_dependencies_are_parsed_and_not_folded_into_dependencies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            r#"{
                "name": "a",
                "peerDependencies": { "react": "^18.0.0" }
            }"#,
        );
        let manifest = read_package_manifest(&path).expect("parse manifest");
        assert_eq!(
            manifest.peer_dependencies.get("react").map(String::as_str),
            Some("^18.0.0")
        );
        assert!(!manifest.dependencies.contains_key("react"));
    }

    #[test]
    fn reads_main_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), r#"{"name": "a", "main": "src/index.ts"}"#);
        let manifest = read_package_manifest(&path).expect("parse manifest");
        assert_eq!(manifest.main.as_deref(), Some("src/index.ts"));
    }

    #[test]
    fn missing_main_field_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), r#"{"name": "a"}"#);
        let manifest = read_package_manifest(&path).expect("parse manifest");
        assert_eq!(manifest.main, None);
    }

    #[test]
    fn missing_name_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "{}");
        read_package_manifest(&path).unwrap_err();
    }
}
