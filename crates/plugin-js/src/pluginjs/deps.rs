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

use crate::pluginjs::lockfile::{self, DepResolution, Lockfile, ResolvedGraph};
use crate::pluginjs::package_json::PackageManifest;
use crate::pluginjs::{platform, thirdparty};
use anyhow::Context;
use std::collections::BTreeMap;

/// One resolved dependency edge, grouped by the `package.json` field it came
/// from (`"dependencies"` / `"dev_dependencies"`) — mirrors the Go plugin's
/// grouped-`deps` config convention (see `GoGolistSpec::deps`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDep {
    pub group: &'static str,
    /// The declared dependency name this addr came from — lets a caller
    /// (e.g. `js_typecheck`'s on-demand third-party-type-input resolution in
    /// `provider.rs`) look a specific addr back up by name without
    /// re-deriving the whole list.
    pub name: String,
    pub addr: String,
}

/// Resolve every dependency `manifest` declares (from its own `package.json`)
/// to a target addr.
///
/// `consuming_pkg` is the declaring package's workspace-relative path — see
/// [`resolve_one_dependency`]'s doc for why it's threaded separately from
/// `lockfile_pkg`. `lockfile_pkg` is that same package's path *relative to
/// the lockfile root that resolves it* (`""` at that root) — see
/// `Provider::lockfile`'s doc for why this isn't always workspace-relative: a
/// heph workspace can contain more than one independent npm/pnpm project,
/// each with its own lockfile nested at a different point. `member_addrs_by_name`
/// maps a workspace member's package **name**
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
///
/// Real npm/pnpm semantics extend one step further than "no resolution at
/// all": an `optionalDependencies` entry can be resolved in the lockfile
/// (recorded there because it applies on *some* platform) while still not
/// applying to the platform actually building right now — the flagship case
/// being one npm package per platform under `optionalDependencies` (e.g.
/// `@esbuild/darwin-arm64`). `resolved_graph` (the same lockfile's
/// [`ResolvedGraph`], carrying each resolved package's `os`/`cpu`
/// restriction) lets this be checked at wiring time, so a platform mismatch
/// on an optional dependency is skipped here — no `js_install` target-dep
/// edge is ever wired for it — rather than reaching `Provider::get` for that
/// addr and hard-failing there (see `platform::matches_platform` and
/// `Provider::thirdparty_install_spec`, which performs the equivalent check
/// at resolution time for an addr that does get wired). A required
/// dependency that resolves to a platform-restricted package which does not
/// match the current platform stays a hard error — an unresolvable required
/// dependency is a real, actionable problem, not a case for silent omission.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors resolve_one_dependency's own parameter set — this is its per-manifest loop"
)]
pub fn resolve_package_deps(
    consuming_pkg: &str,
    lockfile_pkg: &str,
    manifest: &PackageManifest,
    lockfile: Option<&Lockfile>,
    resolved_graph: Option<&ResolvedGraph>,
    member_addrs_by_name: &BTreeMap<String, String>,
    goos: &str,
    goarch: &str,
) -> anyhow::Result<Vec<ResolvedDep>> {
    let mut out = Vec::new();
    for (group, deps) in manifest.dependency_groups() {
        for name in deps.keys() {
            if let Some(addr) = resolve_one_dependency(
                consuming_pkg,
                lockfile_pkg,
                name,
                manifest,
                lockfile,
                resolved_graph,
                member_addrs_by_name,
                goos,
                goarch,
            )? {
                out.push(ResolvedDep {
                    group,
                    name: name.clone(),
                    addr,
                });
            }
        }
    }
    Ok(out)
}

/// Resolve a single declared dependency `name` to a target addr — the
/// per-name primitive [`resolve_package_deps`] loops over, factored out so a
/// caller that only needs *one specific* name's resolution (e.g.
/// `js_typecheck`'s on-demand third-party-type-input resolution in
/// `provider.rs::typecheck_deps_config`, which only needs the handful of
/// names actually reached by an unresolved import, not every declared
/// dependency) doesn't have to resolve — and therefore require a lockfile
/// entry for — names it was never going to look up.
///
/// `None` means "no resolution, and that's fine" (an `optionalDependencies`
/// entry the package manager never installed, on this platform or at all) —
/// see [`resolve_package_deps`]'s doc for the full semantics this mirrors.
///
/// `consuming_pkg` is the workspace-relative package this dependency is
/// declared *by* (distinct from `lockfile_pkg`, which is relative to
/// whichever lockfile root resolves it — see `Provider::lockfile_relative_pkg`'s
/// doc for why those two differ in a multi-project workspace). A resolved
/// third-party dependency needs it to build the
/// [`thirdparty::node_modules_addr`] this fn returns instead of a raw
/// `js_install` addr — see that fn's doc for why every consumer needs its
/// own relocated view rather than sharing one.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors resolve_package_deps's own parameter set — this is its per-name primitive"
)]
pub fn resolve_one_dependency(
    consuming_pkg: &str,
    lockfile_pkg: &str,
    name: &str,
    manifest: &PackageManifest,
    lockfile: Option<&Lockfile>,
    resolved_graph: Option<&ResolvedGraph>,
    member_addrs_by_name: &BTreeMap<String, String>,
    goos: &str,
    goarch: &str,
) -> anyhow::Result<Option<String>> {
    if let Some(addr) = member_addrs_by_name.get(name) {
        return Ok(Some(addr.clone()));
    }

    // A transitive fallback (via `resolved_graph`) when the direct/importer
    // lookup misses: a name reachable only through one of `manifest`'s own
    // dependencies — e.g. `@eslint/js` pulled in by a declared
    // `typescript-eslint` — must resolve to the same addr here that
    // `importgraph::transitive_declared_closure`'s phantom-dependency check
    // already accepts it under, or `Provider::get` could succeed with a
    // `TargetDef` missing the Input the real toolchain run needs. See
    // `crate::pluginjs::lockfile::resolve_transitive`'s doc.
    let resolution = match (lockfile, resolved_graph) {
        (Some(lf), Some(rg)) => {
            crate::pluginjs::lockfile::resolve_transitive(lf, rg, lockfile_pkg, manifest, name)
                .with_context(|| format!("resolving `{name}` declared by {lockfile_pkg:?}"))?
        }
        (Some(lf), None) => lf
            .resolve_dependency(lockfile_pkg, name)
            .with_context(|| format!("resolving `{name}` declared by {lockfile_pkg:?}"))?,
        (None, _) => None,
    };

    match resolution {
        Some(DepResolution::Workspace) => {
            // A lockfile-recorded `link:`/`file:` to a workspace member
            // whose name we didn't already match (e.g. a scoped alias) —
            // fall back to the same hard-error path as an unresolved
            // required dep rather than guessing.
            anyhow::bail!(
                "{lockfile_pkg:?}: `{name}` resolves to a workspace link in the lockfile but no \
                 discovered workspace member has that name — is the workspace-member list \
                 stale?"
            );
        }
        Some(DepResolution::ThirdParty {
            name: resolved_name,
            version,
        }) => {
            if let Some(resolved) = resolved_graph.and_then(|g| g.get(&resolved_name, &version))
                && !platform::matches_platform(&resolved.os, &resolved.cpu, goos, goarch)
            {
                if manifest.is_optional(&resolved_name) {
                    return Ok(None);
                }
                anyhow::bail!(
                    "{lockfile_pkg:?}: `{name}` resolves to {resolved_name}@{version}, which is \
                     restricted to os={:?} cpu={:?} — that does not include the current \
                     platform {goos}/{goarch}",
                    resolved.os,
                    resolved.cpu
                );
            }
            // The addr wired into a consumer's `Input` list is the
            // *relocated* one, not `js_install`'s own raw addr: nothing
            // else places a `js_install` download's output at
            // `node_modules/<name>` inside a sandbox — see
            // `thirdparty::node_modules_addr`'s doc. `name` (the local
            // specifier, what this package's own source/config actually
            // imports), not `resolved_name`, names the node_modules
            // directory the relocation lands in — they differ for an
            // npm/pnpm alias, and only `name` is what `require`/`import`
            // will look for.
            let addr = thirdparty::node_modules_addr(
                consuming_pkg,
                name,
                &resolved_name,
                &version,
                goos,
                goarch,
            );
            Ok(Some(addr.format()))
        }
        None => {
            if manifest.is_optional(name) {
                Ok(None)
            } else {
                // Diagnosability: this message alone can't distinguish "no
                // lockfile was loaded at all" (wrong/absent workspace_root)
                // from "a lockfile loaded but has no entry for this name"
                // (genuinely stale) — both produce the same `None`. Report
                // which one it was, and how many packages the loaded
                // lockfile (if any) actually parsed, so a report of this
                // error carries enough to tell the two apart without a
                // back-and-forth.
                let lockfile_state = match resolved_graph {
                    Some(g) => format!(
                        "a lockfile loaded with {} resolved package(s)",
                        g.packages.len()
                    ),
                    None => "no lockfile was loaded at all".to_string(),
                };
                anyhow::bail!(
                    "{lockfile_pkg:?}: `{name}` is declared in package.json but has no \
                     lockfile resolution ({lockfile_state}) — the lockfile is likely stale; \
                     re-run the package manager's install to regenerate it"
                );
            }
        }
    }
}

/// Every third-party package reachable from `manifest`'s own direct
/// dependencies, *transitively* — not just the names first-party source or
/// config directly imports, which is all [`resolve_one_dependency`]/
/// [`resolve_package_deps`] alone ever wire. A real published npm package
/// routinely `require`s/`import`s its own dependencies internally (`axios`
/// needs `follow-redirects`, say); Node's own module resolution has to find
/// those too, so each of them needs the exact same
/// [`thirdparty::node_modules_addr`] relocation a directly-imported name
/// gets, or the moment a resolved package's *own* code runs, it hits the
/// identical `Cannot find module` failure this whole mechanism exists to
/// close — just one dependency edge deeper.
///
/// Reuses [`lockfile::direct_dep_seed_keys`]/[`ResolvedGraph::transitive_reachable`]
/// — the exact same BFS `resolve_transitive`'s on-demand single-name lookup
/// already runs, just walked to completion and collected instead of probed
/// for one name. Deterministic for the same reason that BFS is (see its own
/// doc): a name reachable at more than one version keeps whichever one is
/// first reached in the fixed, `BTreeMap`-ordered walk.
///
/// `consuming_pkg`/`local_name`==`resolved_name`: every entry here comes
/// from the lockfile's own dependency graph, not from a consumer's
/// `package.json` (which is the only place an alias — `"my-alias":
/// "npm:real-pkg@1.0.0"` — can be declared), so there is no local/resolved
/// name split to make here the way [`resolve_one_dependency`]'s `ThirdParty`
/// arm has to.
pub fn resolve_transitive_closure(
    consuming_pkg: &str,
    lockfile_pkg: &str,
    manifest: &PackageManifest,
    lockfile: &Lockfile,
    resolved_graph: &ResolvedGraph,
    goos: &str,
    goarch: &str,
) -> anyhow::Result<Vec<String>> {
    let seeds = lockfile::direct_dep_seed_keys(lockfile, lockfile_pkg, manifest)
        .with_context(|| format!("seeding transitive third-party closure for {lockfile_pkg:?}"))?;
    Ok(resolved_graph
        .transitive_reachable(seeds)
        .into_iter()
        .map(|(name, version)| {
            thirdparty::node_modules_addr(consuming_pkg, &name, &name, &version, goos, goarch)
                .format()
        })
        .collect())
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
            main: None,
            dependencies,
            dev_dependencies: to_map(dev),
            optional_dependencies,
            peer_dependencies: BTreeMap::new(),
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
        let deps = resolve_package_deps(
            "packages/a",
            "packages/a",
            &manifest,
            None,
            None,
            &members,
            "linux",
            "amd64",
        )
        .unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].addr, "//packages/b:package_info");
        assert_eq!(deps[0].group, "dependencies");
    }

    #[test]
    fn third_party_dep_becomes_js_install_addr() {
        let manifest = manifest(&[("lodash", "^4.17.21")], &[], &[]);
        let lock = npm_lockfile();
        let graph = lock.resolved_graph().unwrap();
        let deps = resolve_package_deps(
            "packages/a",
            "packages/a",
            &manifest,
            Some(&lock),
            Some(&graph),
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
        .unwrap();
        assert_eq!(deps.len(), 1);
        // The wired addr is the relocated `node_modules` group (places
        // lodash at `packages/a/node_modules/lodash`), not `js_install`'s
        // own raw addr — see `thirdparty::node_modules_addr`'s doc.
        assert!(
            deps[0].addr.contains("@heph/js/node_modules"),
            "{}",
            deps[0].addr
        );
        assert!(deps[0].addr.contains("name=lodash"), "{}", deps[0].addr);
        assert!(deps[0].addr.contains("version=4.17.21"), "{}", deps[0].addr);
        assert!(deps[0].addr.contains("pkg=packages/a"), "{}", deps[0].addr);
        assert!(deps[0].addr.contains("goos=linux"), "{}", deps[0].addr);
    }

    /// A minimal npm `package-lock.json` where `a` declares
    /// `typescript-eslint`, which itself depends on `@eslint/js` — the same
    /// companion-package pattern `importgraph::transitive_declared_closure`'s
    /// own fixture uses.
    fn transitive_npm_lockfile() -> Lockfile {
        Lockfile::parse(
            PkgManager::Npm,
            r#"{
                "packages": {
                    "": {},
                    "packages/a": { "name": "a" },
                    "node_modules/typescript-eslint": {
                        "version": "8.0.0",
                        "integrity": "sha512-abc",
                        "dependencies": { "@eslint/js": "9.0.0" }
                    },
                    "node_modules/@eslint/js": {
                        "version": "9.0.0",
                        "integrity": "sha512-def"
                    }
                }
            }"#,
        )
        .unwrap()
    }

    /// The gap a hermeticity review caught: widening
    /// `check_phantom_dependencies`'s accepted set without also widening
    /// this function's own resolution would let `Provider::get` succeed
    /// while silently wiring no Input for the import at all. This proves
    /// `resolve_one_dependency` resolves `@eslint/js` to the same addr the
    /// phantom check now accepts it under — reachable only through the
    /// declared `typescript-eslint`, never declared directly.
    #[test]
    fn resolve_one_dependency_falls_back_transitively_through_a_declared_dependency() {
        let manifest = manifest(&[], &[("typescript-eslint", "^8.0.0")], &[]);
        let lock = transitive_npm_lockfile();
        let graph = lock.resolved_graph().unwrap();
        let addr = resolve_one_dependency(
            "packages/a",
            "packages/a",
            "@eslint/js",
            &manifest,
            Some(&lock),
            Some(&graph),
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
        .unwrap()
        .expect("transitively reachable through typescript-eslint");
        assert!(addr.contains("@heph/js/node_modules"), "{addr}");
        assert!(addr.contains("name=@eslint/js"), "{addr}");
        assert!(addr.contains("version=9.0.0"), "{addr}");
    }

    /// Unlike npm's ancestor-`node_modules` walk (which finds any name
    /// hoisted to a common ancestor, phantom or not — see
    /// `lockfile.rs`'s `pnpm_direct_lookup_does_not_see_a_transitive_name`
    /// for why the equivalent npm fixture can't isolate this), pnpm's
    /// strict, `importers`-only direct lookup genuinely cannot see
    /// `unrelated` any other way than the transitive BFS, so this cleanly
    /// proves the BFS itself does not leak into a package with no edge from
    /// anything `a` declares.
    #[test]
    fn resolve_one_dependency_does_not_transitively_resolve_an_unrelated_package() {
        let manifest = manifest(&[], &[("typescript-eslint", "^8.0.0")], &[]);
        let lock = Lockfile::parse(
            PkgManager::Pnpm,
            r#"
lockfileVersion: '9.0'
importers:
  packages/a:
    devDependencies:
      typescript-eslint:
        specifier: ^8.0.0
        version: 8.0.0
packages:
  typescript-eslint@8.0.0:
    resolution: {integrity: sha512-abc}
  '@eslint/js@9.0.0':
    resolution: {integrity: sha512-def}
  unrelated@1.0.0:
    resolution: {integrity: sha512-ghi}
snapshots:
  typescript-eslint@8.0.0:
    dependencies:
      '@eslint/js': 9.0.0
  '@eslint/js@9.0.0': {}
  unrelated@1.0.0: {}
"#,
        )
        .unwrap();
        let graph = lock.resolved_graph().unwrap();
        // Not optional and not declared at all, so a miss here is the same
        // hard-error path `missing_required_dep_resolution_is_a_hard_error`
        // exercises for a direct dependency — this is not a real production
        // path (a name reaching this on-demand lookup was already proven
        // reachable by `check_phantom_dependencies`), but it proves the
        // negative case fails loudly rather than fabricating an addr.
        let err = resolve_one_dependency(
            "packages/a",
            "packages/a",
            "unrelated",
            &manifest,
            Some(&lock),
            Some(&graph),
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("unrelated"), "{err:#}");
    }

    #[test]
    fn missing_required_dep_resolution_is_a_hard_error() {
        let manifest = manifest(&[("not-in-lockfile", "^1.0.0")], &[], &[]);
        let lock = npm_lockfile();
        let graph = lock.resolved_graph().unwrap();
        let err = resolve_package_deps(
            "packages/a",
            "packages/a",
            &manifest,
            Some(&lock),
            Some(&graph),
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
        let graph = lock.resolved_graph().unwrap();
        let deps = resolve_package_deps(
            "packages/a",
            "packages/a",
            &manifest,
            Some(&lock),
            Some(&graph),
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
            "packages/a",
            &manifest,
            None,
            None,
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
        .unwrap_err();
    }

    /// A lockfile entry for `name` *is* resolved (recorded because it applies
    /// on some platform), but its `os`/`cpu` restriction excludes the
    /// current build platform — the flagship `optionalDependencies` use case
    /// (one npm package per platform, e.g. `@esbuild/darwin-arm64`). This
    /// must be silently skipped, exactly like an unresolved optional dep,
    /// never wired as a `js_install` dep and never a hard error.
    fn npm_lockfile_with_platform_restricted_pkg() -> Lockfile {
        Lockfile::parse(
            PkgManager::Npm,
            r#"{
                "packages": {
                    "": {},
                    "packages/a": { "name": "a" },
                    "node_modules/native-thing": {
                        "version": "1.0.0",
                        "integrity": "sha512-xyz",
                        "os": ["darwin"],
                        "cpu": ["arm64"]
                    }
                }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn platform_mismatched_optional_dep_is_silently_skipped_even_when_lockfile_resolved() {
        let manifest = manifest(&[], &[], &[("native-thing", "^1.0.0")]);
        let lock = npm_lockfile_with_platform_restricted_pkg();
        let graph = lock.resolved_graph().unwrap();
        // The declaring workspace runs on linux/amd64; the lockfile-resolved
        // native-thing@1.0.0 is restricted to darwin/arm64 — a mismatch.
        let deps = resolve_package_deps(
            "packages/a",
            "packages/a",
            &manifest,
            Some(&lock),
            Some(&graph),
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
        .unwrap();
        assert!(
            deps.is_empty(),
            "platform-mismatched optional dep must not be wired at all: {deps:?}"
        );
    }

    #[test]
    fn platform_mismatched_required_dep_is_a_hard_error() {
        // Same lockfile-resolved, platform-restricted package as above, but
        // declared as a required (non-optional) dependency this time — a
        // required dep that cannot be installed on this platform is a real,
        // actionable problem and must still hard-fail.
        let manifest = manifest(&[("native-thing", "^1.0.0")], &[], &[]);
        let lock = npm_lockfile_with_platform_restricted_pkg();
        let graph = lock.resolved_graph().unwrap();
        let err = resolve_package_deps(
            "packages/a",
            "packages/a",
            &manifest,
            Some(&lock),
            Some(&graph),
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("native-thing"), "{msg}");
        assert!(msg.contains("darwin"), "{msg}");
    }
}
