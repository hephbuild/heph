//! The platform axis for third-party JS dependency caching.
//!
//! Optional/native packages (e.g. `@esbuild/darwin-arm64` via
//! `optionalDependencies`, or any package that compiles a native `.node`
//! binding at install time) produce different bytes per host even when
//! `(name, version, integrity)` — the source tarball — is identical, so
//! `js_install`'s cache key must carry the building machine's platform. See
//! `ai-docs/js-plugin-plan.md`'s Variants section: "reuse the existing
//! goos/goarch-style factor machinery verbatim."
//!
//! Rather than inventing a second os/arch detector, this reuses
//! `hcore::htplatform` — the same primitive `plugin-go`'s
//! `factors::current_goos`/`current_goarch` wrap — for the canonical
//! (Go/OCI) naming (`linux`/`darwin`, `amd64`/`arm64`) that feeds the
//! `js_install` addr args and `Def` hash. npm/pnpm lockfiles instead record
//! platform restrictions (`os`/`cpu`) in Node's own naming convention
//! (`darwin`/`linux`/`win32`, `x64`/`arm64`/`ia32`), so [`npm_cpu`] maps the
//! canonical arch to Node's for matching against those restriction lists
//! (`npm`'s `os` values already match the canonical ones on both supported
//! OSes, so no os mapping is needed).

/// Host operating system in canonical (Go/OCI) naming — see
/// `hcore::htplatform::os`.
pub fn current_goos() -> String {
    hcore::htplatform::os().to_string()
}

/// Host architecture in canonical (Go/OCI) naming — see
/// `hcore::htplatform::arch`.
pub fn current_goarch() -> String {
    hcore::htplatform::arch().to_string()
}

/// Canonical (Go/OCI) `goarch` → Node `process.arch`. Node spells x86_64
/// `"x64"`, not Go's `"amd64"`; `"arm64"` already matches on both.
pub fn npm_cpu(goarch: &str) -> &str {
    match goarch {
        "amd64" => "x64",
        other => other,
    }
}

/// Whether `list` (an npm/pnpm `os`/`cpu` restriction list) permits `value`.
/// npm's own semantics (see `npm-package-arch`/`checkPlatform`): an empty
/// list is unrestricted; a list of plain entries is an allowlist (`value`
/// must match one); a `!`-prefixed entry is an exclusion (`value` must not
/// match any of them) — the two forms are never mixed by real npm/pnpm
/// lockfiles, so this checks for negation form first and only falls back to
/// the positive-match form otherwise.
fn matches_restriction_list(list: &[String], value: &str) -> bool {
    if list.is_empty() {
        return true;
    }
    let mut saw_negation = false;
    for entry in list {
        if let Some(excluded) = entry.strip_prefix('!') {
            saw_negation = true;
            if excluded == value {
                return false;
            }
        }
    }
    if saw_negation {
        return true;
    }
    list.iter().any(|entry| entry == value)
}

/// Whether a package's lockfile-recorded `os`/`cpu` restriction lists (npm's
/// `package.json` `os`/`cpu` fields, mirrored verbatim into both lockfile
/// formats) permit the given platform. An empty list means "no restriction"
/// (npm/pnpm convention: absent `os`/`cpu` applies to every platform). Both
/// the positive (`["darwin"]`) and `!`-prefixed exclusion (`["!win32"]`) npm
/// conventions are recognized — see [`matches_restriction_list`].
pub fn matches_platform(os: &[String], cpu: &[String], goos: &str, goarch: &str) -> bool {
    matches_restriction_list(os, goos) && matches_restriction_list(cpu, npm_cpu(goarch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_cpu_maps_amd64_to_x64() {
        assert_eq!(npm_cpu("amd64"), "x64");
        assert_eq!(npm_cpu("arm64"), "arm64");
    }

    #[test]
    fn matches_platform_empty_lists_are_unrestricted() {
        assert!(matches_platform(&[], &[], "linux", "amd64"));
    }

    #[test]
    fn matches_platform_checks_both_os_and_cpu() {
        let os = vec!["darwin".to_string()];
        let cpu = vec!["arm64".to_string()];
        assert!(matches_platform(&os, &cpu, "darwin", "arm64"));
        assert!(!matches_platform(&os, &cpu, "linux", "amd64"));
        assert!(!matches_platform(&os, &cpu, "darwin", "amd64"));
    }

    #[test]
    fn current_platform_values_are_non_empty() {
        assert!(!current_goos().is_empty());
        assert!(!current_goarch().is_empty());
    }

    #[test]
    fn matches_platform_negated_os_excludes_only_the_named_platform() {
        let os = vec!["!win32".to_string()];
        assert!(matches_platform(&os, &[], "linux", "amd64"));
        assert!(matches_platform(&os, &[], "darwin", "arm64"));
        assert!(!matches_platform(&os, &[], "win32", "amd64"));
    }

    #[test]
    fn matches_platform_negated_cpu_excludes_only_the_named_arch() {
        let cpu = vec!["!ia32".to_string()];
        assert!(matches_platform(&[], &cpu, "linux", "amd64"));
        assert!(!matches_platform(&[], &cpu, "linux", "ia32"));
    }
}
