//! Wire a package's declared dependencies into target-dep addrs.
//!
//! A workspace-internal dependency (a workspace member depending on a
//! sibling member, matched **by name**, mirroring how Node itself resolves
//! a hoisted/symlinked sibling) becomes an addr-to-addr edge onto that
//! sibling's own `package_info` target. A third-party dependency becomes a
//! `js_install` target addr keyed by `(name, version)` resolved through the
//! lockfile.
//!
//! Per `ai-docs/js-plugin-plan.md`'s M1 milestone note, this reads
//! `package.json` directly (not the lockfile's own mirrored copy of it) and
//! only resolves *names* through the lockfile's resolved graph — no
//! import-statement parsing (oxc) yet; that's M2.

use crate::pluginjs::lockfile::{DepResolution, Lockfile};
use crate::pluginjs::package_json::PackageManifest;
use crate::pluginjs::thirdparty;
use anyhow::Context;
use std::collections::BTreeMap;

/// One resolved dependency edge, grouped by the `package.json` field it came
/// from (`"dependencies"` / `"dev_dependencies"`) — mirrors the Go plugin's
/// grouped-`deps` config convention (see `GoGolistSpec::deps`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDep {
    pub group: &'static str,
    pub addr: String,
}

/// Resolve every dependency `manifest` declares (from its own `package.json`)
/// to a target addr.
///
/// `pkg` is the declaring package's workspace-relative path (`""` for the
/// root). `member_addrs_by_name` maps a workspace member's package **name**
/// to its own `package_info` target addr string — an internal dependency is
/// recognized by name, the same way Node resolves a workspace-hoisted
/// sibling. `goos`/`goarch` pin the platform of any third-party `js_install`
/// addr this package's deps resolve to (see `thirdparty` module docs).
///
/// A required dependency (`"dependencies"`/`"devDependencies"`) with no
/// lockfile resolution is a hard error — a stale/out-of-date lockfile,
/// caught here rather than silently producing an incomplete dep list (see
/// `.claude/rust.md`'s "fail or fix, never ignore"). An
/// `optionalDependencies` entry with no resolution is expected (a
/// platform-mismatched optional dependency the manager never installs) and
/// is silently omitted — never a hard error.
pub fn resolve_package_deps(
    pkg: &str,
    manifest: &PackageManifest,
    lockfile: Option<&Lockfile>,
    member_addrs_by_name: &BTreeMap<String, String>,
    goos: &str,
    goarch: &str,
) -> anyhow::Result<Vec<ResolvedDep>> {
    let mut out = Vec::new();
    for (group, deps) in manifest.dependency_groups() {
        for name in deps.keys() {
            if let Some(addr) = member_addrs_by_name.get(name) {
                out.push(ResolvedDep {
                    group,
                    addr: addr.clone(),
                });
                continue;
            }

            let resolution = match lockfile {
                Some(lf) => lf
                    .resolve_dependency(pkg, name)
                    .with_context(|| format!("resolving `{name}` declared by {pkg:?}"))?,
                None => None,
            };

            match resolution {
                Some(DepResolution::Workspace) => {
                    // A lockfile-recorded `link:`/`file:` to a workspace
                    // member whose name we didn't already match (e.g. a
                    // scoped alias) — fall back to the same hard-error path
                    // as an unresolved required dep rather than guessing.
                    anyhow::bail!(
                        "{pkg:?}: `{name}` resolves to a workspace link in the lockfile but no \
                         discovered workspace member has that name — is the workspace-member \
                         list stale?"
                    );
                }
                Some(DepResolution::ThirdParty { name, version }) => {
                    let addr = thirdparty::thirdparty_addr(&name, &version, goos, goarch);
                    out.push(ResolvedDep {
                        group,
                        addr: addr.format(),
                    });
                }
                None => {
                    if manifest.is_optional(name) {
                        continue;
                    }
                    anyhow::bail!(
                        "{pkg:?}: `{name}` is declared in package.json but has no lockfile \
                         resolution — the lockfile is likely stale; re-run the package manager's \
                         install to regenerate it"
                    );
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pluginjs::workspace::PkgManager;

    fn manifest(
        deps: &[(&str, &str)],
        dev: &[(&str, &str)],
        optional: &[(&str, &str)],
    ) -> PackageManifest {
        let to_map = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        // Mirror `read_package_manifest`'s own folding of optional deps into
        // `dependencies` (see package_json.rs), so this fixture matches what
        // production code actually hands `resolve_package_deps`.
        let mut dependencies = to_map(deps);
        let optional_dependencies = to_map(optional);
        for (k, v) in &optional_dependencies {
            dependencies.entry(k.clone()).or_insert_with(|| v.clone());
        }
        PackageManifest {
            name: "a".to_string(),
            dependencies,
            dev_dependencies: to_map(dev),
            optional_dependencies,
        }
    }

    fn npm_lockfile() -> Lockfile {
        Lockfile::parse(
            PkgManager::Npm,
            r#"{
                "packages": {
                    "": {},
                    "packages/a": { "name": "a" },
                    "node_modules/lodash": {
                        "version": "4.17.21",
                        "integrity": "sha512-abc"
                    }
                }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn workspace_internal_dep_becomes_sibling_addr() {
        let manifest = manifest(&[("b", "workspace:*")], &[], &[]);
        let mut members = BTreeMap::new();
        members.insert("b".to_string(), "//packages/b:package_info".to_string());
        let deps = resolve_package_deps("packages/a", &manifest, None, &members, "linux", "amd64")
            .unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].addr, "//packages/b:package_info");
        assert_eq!(deps[0].group, "dependencies");
    }

    #[test]
    fn third_party_dep_becomes_js_install_addr() {
        let manifest = manifest(&[("lodash", "^4.17.21")], &[], &[]);
        let lock = npm_lockfile();
        let deps = resolve_package_deps(
            "packages/a",
            &manifest,
            Some(&lock),
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
        .unwrap();
        assert_eq!(deps.len(), 1);
        assert!(
            deps[0].addr.contains("@heph/js/thirdparty/lodash@4.17.21"),
            "{}",
            deps[0].addr
        );
        assert!(deps[0].addr.contains("goos=linux"), "{}", deps[0].addr);
    }

    #[test]
    fn missing_required_dep_resolution_is_a_hard_error() {
        let manifest = manifest(&[("not-in-lockfile", "^1.0.0")], &[], &[]);
        let lock = npm_lockfile();
        let err = resolve_package_deps(
            "packages/a",
            &manifest,
            Some(&lock),
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("not-in-lockfile"));
    }

    #[test]
    fn missing_optional_dep_resolution_is_silently_skipped() {
        let manifest = manifest(&[], &[], &[("fsevents", "^2.3.0")]);
        let lock = npm_lockfile();
        let deps = resolve_package_deps(
            "packages/a",
            &manifest,
            Some(&lock),
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
        .unwrap();
        assert!(deps.is_empty());
    }

    #[test]
    fn no_lockfile_but_required_deps_declared_is_an_error() {
        let manifest = manifest(&[("lodash", "^4.17.21")], &[], &[]);
        resolve_package_deps(
            "packages/a",
            &manifest,
            None,
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
        .unwrap_err();
    }
}
