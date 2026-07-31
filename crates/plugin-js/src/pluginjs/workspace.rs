//! Package-manager-specific slice of discovery: which glob patterns declare a
//! directory a workspace member, for pnpm (`pnpm-workspace.yaml`'s `packages`
//! list) and npm (root `package.json`'s `"workspaces"` array). Both resolve
//! into the same manager-agnostic [`WorkspaceMember`] list — see
//! `ai-docs/js-plugin-plan.md`'s "Package manager support" section.

use crate::pluginjs::PACKAGE_INFO_TARGET;
use anyhow::Context;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use std::path::Path;
use wax::{Glob, Program as _};

/// Which package manager's workspace-member convention applies. Provider-level
/// config (mirrors the Go plugin's `gotool` option) — never a per-target
/// variant, since a workspace has exactly one package manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkgManager {
    Npm,
    Pnpm,
}

impl PkgManager {
    /// Parse the `pkgmanager` provider option's string value.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "npm" => Ok(Self::Npm),
            "pnpm" => Ok(Self::Pnpm),
            other => anyhow::bail!(
                "js provider: unknown `pkgmanager` \"{other}\" (expected \"npm\" or \"pnpm\")"
            ),
        }
    }
}

/// A package that is a member of the workspace (matched by the configured
/// package manager's workspace-member glob patterns), unified across both
/// managers. `addr` is the package's `js_package_info` target address — the
/// only target M0 defines; later milestones add `js_install`/etc. addrs the
/// same package resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMember {
    pub name: String,
    pub addr: Addr,
}

/// npm workspace-member glob patterns: the root `package.json`'s
/// `"workspaces"` array. No root `package.json`, or no `"workspaces"` field,
/// means no workspace members (a plain, non-monorepo npm package).
pub fn read_npm_workspace_globs(workspace_root: &Path) -> anyhow::Result<Vec<String>> {
    let path = workspace_root.join("package.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    match value.get("workspaces") {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    anyhow::anyhow!("{}: `workspaces` entries must be strings", path.display())
                })
            })
            .collect(),
        Some(_) => anyhow::bail!(
            "{}: `workspaces` must be an array of glob strings",
            path.display()
        ),
    }
}

/// pnpm workspace-member glob patterns: `pnpm-workspace.yaml`'s `packages`
/// list. No such file, or an empty/absent `packages` key, means no workspace
/// members.
pub fn read_pnpm_workspace_globs(workspace_root: &Path) -> anyhow::Result<Vec<String>> {
    let path = workspace_root.join("pnpm-workspace.yaml");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;

    #[derive(serde::Deserialize, Default)]
    struct PnpmWorkspaceFile {
        #[serde(default)]
        packages: Vec<String>,
    }

    let parsed: PnpmWorkspaceFile =
        serde_yaml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(parsed.packages)
}

/// Resolve `patterns` (npm `workspaces` globs or pnpm `packages` globs)
/// against the already-discovered `package.json` package set, unifying both
/// managers into one [`WorkspaceMember`] list.
///
/// A pattern's final path component is never crossed by `*` (standard glob
/// semantics, matching what npm/pnpm themselves do): `packages/*` matches
/// `packages/foo`, not `packages/foo/bar` — a nested workspace needs its own
/// `packages/foo/*`-style entry to be picked up.
pub fn resolve_members(
    workspace_root: &Path,
    packages: &[PkgBuf],
    patterns: &[String],
) -> anyhow::Result<Vec<WorkspaceMember>> {
    let globs = patterns
        .iter()
        .map(|p| Glob::new(p).with_context(|| format!("invalid workspace glob '{p}'")))
        .collect::<anyhow::Result<Vec<Glob<'_>>>>()?;

    let mut members = Vec::new();
    for pkg in packages {
        let rel = Path::new(pkg.as_str());
        if !globs.iter().any(|g| g.is_match(rel)) {
            continue;
        }
        let package_json = workspace_root.join(pkg.as_str()).join("package.json");
        let name = read_package_name(&package_json)?;
        let addr = Addr::new(
            pkg.clone(),
            PACKAGE_INFO_TARGET.to_string(),
            Default::default(),
        );
        members.push(WorkspaceMember { name, addr });
    }
    // `packages` comes off a filesystem walk (see `collect_js_packages`), whose
    // order is a directory-listing order, not a meaningful one — sort so the
    // result is stable across runs/platforms rather than leaking walk order.
    members.sort_by_key(|m| m.addr.format());
    Ok(members)
}

/// Read a package's own `"name"` field from its `package.json`. Required: a
/// workspace member with no declared name is a malformed package, not a
/// silently-skipped one.
pub fn read_package_name(package_json: &Path) -> anyhow::Result<String> {
    let raw = std::fs::read_to_string(package_json)
        .with_context(|| format!("reading {}", package_json.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", package_json.display()))?;
    value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("{}: missing required `name` field", package_json.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(path, contents).expect("write fixture file");
    }

    #[test]
    fn pkgmanager_parses_known_values() {
        assert_eq!(PkgManager::parse("npm").unwrap(), PkgManager::Npm);
        assert_eq!(PkgManager::parse("pnpm").unwrap(), PkgManager::Pnpm);
    }

    #[test]
    fn pkgmanager_rejects_unknown_value() {
        PkgManager::parse("yarn").unwrap_err();
    }

    #[test]
    fn read_npm_globs_absent_workspaces_field_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name": "root"}"#);
        let globs = read_npm_workspace_globs(dir.path()).unwrap();
        assert!(globs.is_empty());
    }

    #[test]
    fn read_npm_globs_no_root_package_json_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let globs = read_npm_workspace_globs(dir.path()).unwrap();
        assert!(globs.is_empty());
    }

    #[test]
    fn read_npm_globs_parses_workspaces_array() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"name": "root", "workspaces": ["packages/*", "apps/*"]}"#,
        );
        let globs = read_npm_workspace_globs(dir.path()).unwrap();
        assert_eq!(globs, vec!["packages/*".to_string(), "apps/*".to_string()]);
    }

    #[test]
    fn read_npm_globs_rejects_non_array_workspaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"name": "root", "workspaces": "packages/*"}"#,
        );
        read_npm_workspace_globs(dir.path()).unwrap_err();
    }

    #[test]
    fn read_pnpm_globs_parses_packages_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "pnpm-workspace.yaml",
            "packages:\n  - packages/*\n  - apps/*\n",
        );
        let globs = read_pnpm_workspace_globs(dir.path()).unwrap();
        assert_eq!(globs, vec!["packages/*".to_string(), "apps/*".to_string()]);
    }

    #[test]
    fn read_pnpm_globs_no_file_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let globs = read_pnpm_workspace_globs(dir.path()).unwrap();
        assert!(globs.is_empty());
    }

    #[test]
    fn resolve_members_matches_single_level_glob_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/nested/package.json",
            r#"{"name": "a-nested"}"#,
        );
        let packages = vec![
            PkgBuf::from("packages/a"),
            PkgBuf::from("packages/a/nested"),
        ];
        let members = resolve_members(dir.path(), &packages, &["packages/*".to_string()]).unwrap();
        assert_eq!(
            members.len(),
            1,
            "`packages/*` must not cross into `packages/a/nested`"
        );
        assert_eq!(members[0].name, "a");
    }

    #[test]
    fn resolve_members_missing_name_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", "{}");
        let packages = vec![PkgBuf::from("packages/a")];
        resolve_members(dir.path(), &packages, &["packages/*".to_string()]).unwrap_err();
    }

    #[test]
    fn resolve_members_sorted_deterministically() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/b/package.json", r#"{"name": "b"}"#);
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        // Deliberately reversed input order — the sort, not walk order, must win.
        let packages = vec![PkgBuf::from("packages/b"), PkgBuf::from("packages/a")];
        let members = resolve_members(dir.path(), &packages, &["packages/*".to_string()]).unwrap();
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }
}
