use anyhow::Context;
use borsh::{BorshDeserialize, BorshSerialize};
use hbuiltins::pluginfs;
use hmodel::htaddr::{Addr, parse_addr_with_base};
use hmodel::htpkg::PkgBuf;
use hplugin::driver::TargetAddr;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct GoPackage {
    #[serde(rename = "ImportPath")]
    pub import_path: String,
    #[serde(rename = "Dir", default)]
    pub dir: Option<String>,
    #[serde(rename = "Name", default)]
    pub name: Option<String>,
    #[serde(rename = "GoFiles", default)]
    pub go_files: Vec<String>,
    #[serde(rename = "SFiles", default)]
    pub s_files: Vec<String>,
    /// .h files. Required as inputs whenever asm `.s` files `#include` them
    /// (e.g. `purego/fakecgo` ships its own `abi_arm64.h`). Without staging
    /// the asm step fails to resolve the include.
    #[serde(rename = "HFiles", default)]
    pub h_files: Vec<String>,
    #[serde(rename = "TestGoFiles", default)]
    pub test_go_files: Vec<String>,
    #[serde(rename = "XTestGoFiles", default)]
    pub xtest_go_files: Vec<String>,
    #[serde(rename = "EmbedPatterns", default)]
    pub embed_patterns: Vec<String>,
    #[serde(rename = "EmbedFiles", default)]
    pub embed_files: Vec<String>,
    #[serde(rename = "TestEmbedPatterns", default)]
    pub test_embed_patterns: Vec<String>,
    #[serde(rename = "TestEmbedFiles", default)]
    pub test_embed_files: Vec<String>,
    #[serde(rename = "XTestEmbedPatterns", default)]
    pub xtest_embed_patterns: Vec<String>,
    #[serde(rename = "XTestEmbedFiles", default)]
    pub xtest_embed_files: Vec<String>,
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

/// Per-file Addr strings derived from a `GoPackage` by the golist driver.
/// Each entry mirrors the same-name field in `GoPackage`, preserving order.
#[derive(Debug, Clone, Default, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct PackageAddrs {
    #[serde(rename = "GoFiles", default)]
    pub go_files: Vec<String>,
    #[serde(rename = "SFiles", default)]
    pub s_files: Vec<String>,
    /// Per-file Addrs for .h headers staged into the sandbox so asm can resolve
    /// `#include` directives.
    #[serde(rename = "HFiles", default)]
    pub h_files: Vec<String>,
    /// Addrs for header files this package's asm `#include`s from sibling
    /// packages in the same module (e.g. circl `dh/x448` → `math/fp448`). Not
    /// in `HFiles` (they belong to other packages), discovered by scanning the
    /// asm sources and staged via the module-root `download` target.
    #[serde(rename = "ExtraHFiles", default)]
    pub extra_h_files: Vec<String>,
    #[serde(rename = "TestGoFiles", default)]
    pub test_go_files: Vec<String>,
    #[serde(rename = "XTestGoFiles", default)]
    pub xtest_go_files: Vec<String>,
    #[serde(rename = "EmbedFiles", default)]
    pub embed_files: Vec<String>,
    #[serde(rename = "TestEmbedFiles", default)]
    pub test_embed_files: Vec<String>,
    #[serde(rename = "XTestEmbedFiles", default)]
    pub xtest_embed_files: Vec<String>,
}

/// Resolve each Go source file in `pkg` to a target-address string.
///
/// First-party: codegen-produced files (those present in `source_map`, keyed by
/// `pkg/file`) become `TargetAddr` strings with a filter pinning the specific
/// file. As an optimization, when the source addr is itself an `@heph/fs`
/// glob, the output collapses to a plain `pluginfs::file_addr(rel)` — same
/// artifact without the glob walk + filter; when it is already an `@heph/fs`
/// file the filter is dropped (single-file target). The rest fall back to
/// `pluginfs::file_addr` references.
///
/// Thirdparty: when `download_addr` is `Some`, every file becomes a `TargetAddr`
/// against the module-root `download` target filtered to the `pkg_str/file`
/// path. The download target's artifact entries are keyed by ws-relative paths
/// (the bash driver prefixes outputs with the target's package), so the filter
/// string `pkg_str/file` matches verbatim. `source_map` is ignored for
/// thirdparty — codegen never lands inside a vendored module.
pub fn resolve_package_addrs(
    pkg: &GoPackage,
    pkg_str: &str,
    source_map: &HashMap<String, String>,
    download_addr: Option<&Addr>,
) -> PackageAddrs {
    let resolve = |files: &[String]| -> Vec<String> {
        files
            .iter()
            .map(|f| {
                let rel = if pkg_str.is_empty() {
                    f.clone()
                } else {
                    format!("{pkg_str}/{f}")
                };
                if let Some(dl) = download_addr {
                    TargetAddr {
                        r#ref: dl.clone(),
                        output: None,
                        filters: vec![rel],
                    }
                    .to_string()
                } else if let Some(src_target) = source_map.get(&rel) {
                    let src_ref = parse_addr_with_base(src_target, &PkgBuf::from(""))
                        .unwrap_or_else(|_| Addr::default());
                    // fs glob filtered to a single file is equivalent to a fs
                    // file target for `rel`; skip the glob walk + downstream
                    // filter. fs file already produces exactly one artifact,
                    // so the filter is a no-op.
                    if pluginfs::is_glob_addr(&src_ref) {
                        pluginfs::file_addr(&rel).format()
                    } else if pluginfs::is_file_addr(&src_ref) {
                        src_target.clone()
                    } else {
                        TargetAddr {
                            r#ref: src_ref,
                            output: None,
                            filters: vec![rel],
                        }
                        .to_string()
                    }
                } else {
                    pluginfs::file_addr(&rel).format()
                }
            })
            .collect()
    };
    PackageAddrs {
        go_files: resolve(&pkg.go_files),
        s_files: resolve(&pkg.s_files),
        h_files: resolve(&pkg.h_files),
        // Cross-package asm headers need filesystem scanning; the golist driver
        // fills this in after resolve (it can't be derived from `go list` JSON).
        extra_h_files: Vec::new(),
        test_go_files: resolve(&pkg.test_go_files),
        xtest_go_files: resolve(&pkg.xtest_go_files),
        embed_files: resolve(&pkg.embed_files),
        test_embed_files: resolve(&pkg.test_embed_files),
        xtest_embed_files: resolve(&pkg.xtest_embed_files),
    }
}

/// Parse `#include "path"` directives out of Go assembly (or header) source.
///
/// Go's assembler only honours double-quoted includes; angle-bracket form is
/// not used. Returns the quoted paths verbatim, in source order.
pub fn parse_asm_includes(content: &str) -> impl Iterator<Item = &str> {
    content.lines().filter_map(|line| {
        let rest = line.trim_start().strip_prefix("#include")?.trim_start();
        let rest = rest.strip_prefix('"')?;
        rest.split_once('"').map(|(inc, _)| inc)
    })
}

/// Resolve `.`/`..` components in `p` lexically (no filesystem access), keeping
/// any root/prefix component intact so `..` can never climb above the root.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    let mut anchored = false;
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Don't pop the root/prefix anchor or a leading `..`.
                if out.len() > usize::from(anchored)
                    && out.last().map(|s| s.as_os_str() != "..").unwrap_or(true)
                {
                    out.pop();
                } else if !anchored {
                    out.push("..".into());
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                anchored = true;
                out.push(comp.as_os_str().to_os_string());
            }
            Component::Normal(c) => out.push(c.to_os_string()),
        }
    }
    out.iter().collect()
}

/// Discover header files that asm sources `#include` from *sibling packages* in
/// the same module — files the package's own `HFiles` never lists, so nothing
/// else stages them.
///
/// Example: cloudflare/circl's `dh/x448/curve_amd64.s` does
/// `#include "../../math/fp448/fp_amd64.h"`. That header lives in the `math/
/// fp448` package, not `dh/x448`, so the asm step fails with "no such file"
/// unless it's staged at its module-relative path.
///
/// `pkg_dir` is the package's source directory (host GOMODCACHE path); `subpath`
/// is its module-relative location (e.g. `dh/x448`), used to derive the module
/// root. Returns module-root-relative, forward-slashed paths — deduped and
/// sorted for deterministic output. Headers inside `pkg_dir` itself are skipped
/// (already covered by `HFiles`); includes that don't resolve to a real file
/// (std headers like `textflag.h` via `-I $GOROOT/pkg/include`, or generated
/// `go_asm.h`) are skipped.
pub fn collect_cross_pkg_asm_headers(
    pkg_dir: &str,
    subpath: &str,
    s_files: &[String],
) -> Vec<String> {
    let pkg_dir = Path::new(pkg_dir);
    // Strip the subpath components off pkg_dir to reach the module root.
    let mut module_root = pkg_dir;
    for _ in subpath.split('/').filter(|s| !s.is_empty()) {
        module_root = match module_root.parent() {
            Some(p) => p,
            None => return Vec::new(),
        };
    }

    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();

    let enqueue = |from: &Path, inc: &str, queue: &mut VecDeque<PathBuf>| {
        // Absolute includes would be non-hermetic; Go asm never uses them.
        if inc.starts_with('/') {
            return;
        }
        let base = from.parent().unwrap_or(module_root);
        queue.push_back(lexical_normalize(&base.join(inc)));
    };

    for s in s_files {
        let p = pkg_dir.join(s);
        if let Ok(content) = std::fs::read_to_string(&p) {
            for inc in parse_asm_includes(&content) {
                enqueue(&p, inc, &mut queue);
            }
        }
    }

    while let Some(hdr) = queue.pop_front() {
        if !visited.insert(hdr.clone()) {
            continue;
        }
        // Never stage anything outside the module (or std headers absent here).
        if !hdr.starts_with(module_root) {
            continue;
        }
        let content = match std::fs::read_to_string(&hdr) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // Record only siblings — headers in the origin package ship via HFiles.
        if !hdr.starts_with(pkg_dir)
            && let Ok(rel) = hdr.strip_prefix(module_root)
        {
            out.insert(rel.to_string_lossy().replace('\\', "/"));
        }
        for inc in parse_asm_includes(&content) {
            enqueue(&hdr, inc, &mut queue);
        }
    }

    out.into_iter().collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct GoModule {
    #[serde(rename = "Path")]
    pub path: String,
    #[serde(rename = "Version")]
    pub version: Option<String>,
    #[serde(rename = "Dir")]
    pub dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct GoPackageError {
    #[serde(rename = "Err")]
    pub err: String,
}

/// Parse packages from a reader, streaming without loading the full content.
/// Returns a map from import_path → GoPackage built in a single pass.
#[cfg(test)]
pub fn parse_go_list_reader(reader: impl Read) -> anyhow::Result<HashMap<String, GoPackage>> {
    serde_json::Deserializer::from_reader(std::io::BufReader::with_capacity(64 * 1024, reader))
        .into_iter::<GoPackage>()
        .map(|r| {
            r.context("parse go list json")
                .map(|p| (p.import_path.clone(), p))
        })
        .collect()
}

/// Encode a `GoPackage` to borsh bytes for the `_golist` driver's `package.bin` output.
pub fn encode_go_package(pkg: &GoPackage) -> anyhow::Result<Vec<u8>> {
    borsh::to_vec(pkg).context("borsh encode GoPackage")
}

/// Decode a `GoPackage` from borsh bytes (e.g. `package.bin` artifact).
pub fn decode_go_package(mut reader: impl Read) -> anyhow::Result<GoPackage> {
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).context("read package.bin")?;
    GoPackage::try_from_slice(&buf).context("borsh decode GoPackage")
}

/// Encode `PackageAddrs` to borsh bytes for the `_golist` driver's `package_addrs.bin` output.
pub fn encode_package_addrs(addrs: &PackageAddrs) -> anyhow::Result<Vec<u8>> {
    borsh::to_vec(addrs).context("borsh encode PackageAddrs")
}

/// Decode `PackageAddrs` from borsh bytes.
pub fn decode_package_addrs(mut reader: impl Read) -> anyhow::Result<PackageAddrs> {
    let mut buf = Vec::new();
    reader
        .read_to_end(&mut buf)
        .context("read package_addrs.bin")?;
    PackageAddrs::try_from_slice(&buf).context("borsh decode PackageAddrs")
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

/// Parse the *selected* module versions out of a `go.sum` file.
///
/// `go.sum` carries two line kinds per module version:
///   `<module> <version> h1:<hash>`          — the module's source tree hash
///   `<module> <version>/go.mod h1:<hash>`    — only the module's go.mod hash
///
/// The plain (non-`/go.mod`) line is present exactly for the version whose
/// *source* participates in the build — i.e. the MVS-selected version. The
/// `/go.mod`-only lines are graph entries for versions whose source is never
/// compiled, so they're skipped.
///
/// This fills the gap left by `parse_go_mod_requires`: an untidied `go.mod`
/// (pre-1.17, or never `go mod tidy`'d) lists only direct requires, so indirect
/// modules pulled in transitively by a dependency (e.g. `golang.org/x/net`
/// behind `golang.org/x/oauth2`) appear *only* in `go.sum`. Without them their
/// import paths fail to resolve to a thirdparty addr and get silently dropped.
pub fn parse_go_sum_modules(content: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let (Some(module), Some(version)) = (parts.next(), parts.next()) else {
            continue;
        };
        // Skip the go.mod-only graph entries; keep only source-hash lines.
        if version.ends_with("/go.mod") {
            continue;
        }
        result.push((module.to_string(), version.to_string()));
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
        .arg("-test")
        .args(factors.go_list_flags())
        .arg(import_path)
        .current_dir(module_root)
        .env("GOOS", &factors.goos)
        .env("GOARCH", &factors.goarch)
        .env("CGO_ENABLED", "0")
        .env_remove("GOFLAGS");

    for var in &["GOROOT", "GOPATH", "GOMODCACHE", "HOME", "PATH", "GOCACHE"] {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }

    let output = cmd.output().await.context("spawn go list")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(crate::plugingo::errors::GoListError {
            import_path: import_path.to_string(),
            stderr_tail: hplugin::error::last_n_lines(&stderr, 10),
        }
        .into());
    }

    parse_go_list_reader(output.stdout.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg_with_one_go_file(name: &str) -> GoPackage {
        GoPackage {
            import_path: "example.com/mod/mypkg".to_string(),
            dir: None,
            name: None,
            go_files: vec![name.to_string()],
            s_files: vec![],
            h_files: vec![],
            test_go_files: vec![],
            xtest_go_files: vec![],
            embed_patterns: vec![],
            embed_files: vec![],
            test_embed_patterns: vec![],
            test_embed_files: vec![],
            xtest_embed_patterns: vec![],
            xtest_embed_files: vec![],
            imports: vec![],
            test_imports: vec![],
            xtest_imports: vec![],
            standard: false,
            module: None,
            match_: vec![],
            incomplete: false,
            error: None,
        }
    }

    #[test]
    fn test_parse_go_sum_modules_keeps_source_lines_skips_go_mod() {
        // Two version lines for x/net: only the source-hash (non-/go.mod) line
        // names the selected version whose source is compiled. The /go.mod-only
        // graph entry (an older version) must be skipped.
        let go_sum = "\
golang.org/x/net v0.0.0-20180724234803-3673e40ba225/go.mod h1:mL1N/T3taQHkDXs73rZJwtUhF3w3ftmwwsq0BUmARs4=
golang.org/x/net v0.0.0-20190108225652-1e06a53dbb7e h1:bRhVy7zSSasaqNksaRZiA5EEI+Ei4I1nO5Jh72wfHlg=
golang.org/x/net v0.0.0-20190108225652-1e06a53dbb7e/go.mod h1:mL1N/T3taQHkDXs73rZJwtUhF3w3ftmwwsq0BUmARs4=
golang.org/x/oauth2 v0.0.0-20200107190931-bf48bf16ab8d h1:pE8b58s1HRDMi8RDc79m0HISf9D4TzseP40cEA6IGfs=
golang.org/x/oauth2 v0.0.0-20200107190931-bf48bf16ab8d/go.mod h1:gOpvHmFTYa4IltrdGE7lF6nIHvwfUNPOp7c8zoXwtLw=
";
        let mods = parse_go_sum_modules(go_sum);
        assert_eq!(
            mods,
            vec![
                (
                    "golang.org/x/net".to_string(),
                    "v0.0.0-20190108225652-1e06a53dbb7e".to_string()
                ),
                (
                    "golang.org/x/oauth2".to_string(),
                    "v0.0.0-20200107190931-bf48bf16ab8d".to_string()
                ),
            ],
            "must keep exactly one source-version entry per module, dropping /go.mod graph lines"
        );
    }

    #[test]
    fn test_go_sum_module_resolves_when_absent_from_go_mod() {
        // The regression: an indirect module reachable only through go.sum must
        // still resolve. gogithub's untidied go.mod requires oauth2 but not
        // x/net, yet oauth2/internal imports golang.org/x/net/context/ctxhttp.
        let go_sum = "golang.org/x/net v0.0.0-20190108225652-1e06a53dbb7e h1:bRhVy7zSSasaqNksaRZiA5EEI+Ei4I1nO5Jh72wfHlg=\n";
        let mut requires = parse_go_mod_requires(
            "module example.com/m\n\nrequire golang.org/x/oauth2 v0.0.0-20200107190931-bf48bf16ab8d\n",
        );
        let known: std::collections::HashSet<&str> =
            requires.iter().map(|(m, _)| m.as_str()).collect();
        let extra: Vec<(String, String)> = parse_go_sum_modules(go_sum)
            .into_iter()
            .filter(|(m, _)| !known.contains(m.as_str()))
            .collect();
        requires.extend(extra);

        let resolved = find_module_for_import("golang.org/x/net/context/ctxhttp", &requires)
            .expect("x/net must resolve via go.sum-sourced version");
        assert_eq!(resolved.0, "golang.org/x/net");
        assert_eq!(resolved.1, "v0.0.0-20190108225652-1e06a53dbb7e");
    }

    #[test]
    fn test_resolve_package_addrs_source_map_fs_glob_collapses_to_file() {
        // Source addr is an fs glob — output should drop glob+filter for a
        // direct fs file addr at `pkg/file`.
        let pkg = pkg_with_one_go_file("foo.go");
        let mut sm = HashMap::new();
        sm.insert(
            "mypkg/foo.go".to_string(),
            hbuiltins::pluginfs::glob_addr("mypkg/*.go", &[]).format(),
        );
        let addrs = resolve_package_addrs(&pkg, "mypkg", &sm, None);
        assert_eq!(addrs.go_files.len(), 1);
        let expected = hbuiltins::pluginfs::file_addr("mypkg/foo.go").format();
        let got = addrs.go_files.first().expect("one entry");
        assert_eq!(got, &expected);
        assert!(
            !got.contains(":glob"),
            "glob source must collapse to file addr, got: {got}"
        );
    }

    #[test]
    fn test_resolve_package_addrs_source_map_fs_file_drops_filter() {
        // Source addr is already an fs file — filter is a no-op, drop it.
        let pkg = pkg_with_one_go_file("foo.go");
        let src = hbuiltins::pluginfs::file_addr("mypkg/foo.go").format();
        let mut sm = HashMap::new();
        sm.insert("mypkg/foo.go".to_string(), src.clone());
        let addrs = resolve_package_addrs(&pkg, "mypkg", &sm, None);
        assert_eq!(addrs.go_files, vec![src]);
    }

    #[test]
    fn test_resolve_package_addrs_source_map_non_fs_keeps_filter() {
        // Non-fs codegen source: must still wrap in TargetAddr with filter.
        let pkg = pkg_with_one_go_file("gen.go");
        let mut sm = HashMap::new();
        sm.insert("mypkg/gen.go".to_string(), "//mypkg:gen".to_string());
        let addrs = resolve_package_addrs(&pkg, "mypkg", &sm, None);
        assert_eq!(addrs.go_files.len(), 1);
        let got = addrs.go_files.first().expect("one entry");
        assert!(
            got.contains("mypkg:gen") && got.contains("mypkg/gen.go"),
            "non-fs source must keep TargetAddr+filter, got: {got}"
        );
    }

    #[test]
    fn test_resolve_package_addrs_no_source_map_uses_file_addr() {
        let pkg = pkg_with_one_go_file("foo.go");
        let sm = HashMap::new();
        let addrs = resolve_package_addrs(&pkg, "mypkg", &sm, None);
        assert_eq!(
            addrs.go_files,
            vec![hbuiltins::pluginfs::file_addr("mypkg/foo.go").format()]
        );
    }

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
    fn test_is_stdlib_import_path_dotless_module_name() {
        // A module named without a dot (e.g. "mod-embed") has no dot in its
        // first path component, but it is NOT stdlib — the heuristic returns true
        // here, which is why resolve_import must guard with workspace_module_path
        // before calling this function.
        assert!(is_stdlib_import_path("mod-embed/testutil"));
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

    #[test]
    fn test_parse_asm_includes() {
        let src = concat!(
            "#include \"textflag.h\"\n",
            "  #include \"../../math/fp448/fp_amd64.h\"\n",
            "#include \"curve_amd64.h\"\n",
            "// #include \"not_a_real_one.h\"\n",
            "TEXT ·foo(SB), 0, $0\n",
        );
        let got: Vec<&str> = parse_asm_includes(src).collect();
        assert_eq!(
            got,
            vec!["textflag.h", "../../math/fp448/fp_amd64.h", "curve_amd64.h",]
        );
    }

    #[test]
    fn test_collect_cross_pkg_asm_headers_circl_shape() {
        // Lay out a circl-shaped module: dh/x448 asm `#include`s a sibling
        // header in math/fp448, which itself pulls in a same-dir helper.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let x448 = root.join("dh/x448");
        let fp448 = root.join("math/fp448");
        std::fs::create_dir_all(&x448).unwrap();
        std::fs::create_dir_all(&fp448).unwrap();

        std::fs::write(
            x448.join("curve_amd64.s"),
            concat!(
                "#include \"textflag.h\"\n",
                "#include \"../../math/fp448/fp_amd64.h\"\n",
                "#include \"curve_amd64.h\"\n",
            ),
        )
        .unwrap();
        // Same-package header — must NOT be reported (covered by HFiles).
        std::fs::write(x448.join("curve_amd64.h"), "// macros\n").unwrap();
        // Sibling header pulls a same-dir helper — both must be reported.
        std::fs::write(fp448.join("fp_amd64.h"), "#include \"fp_amd64_helper.h\"\n").unwrap();
        std::fs::write(fp448.join("fp_amd64_helper.h"), "// more macros\n").unwrap();

        let got = collect_cross_pkg_asm_headers(
            &x448.to_string_lossy(),
            "dh/x448",
            &["curve_amd64.s".to_string()],
        );
        assert_eq!(
            got,
            vec![
                "math/fp448/fp_amd64.h".to_string(),
                "math/fp448/fp_amd64_helper.h".to_string(),
            ],
            "must report sibling headers (transitively), module-relative, sorted; \
             never the same-package header nor the std `textflag.h`"
        );
    }

    #[test]
    fn test_collect_cross_pkg_asm_headers_none_when_self_contained() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pkg = dir.path().join("crypto/foo");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("impl_amd64.s"),
            "#include \"textflag.h\"\n#include \"local.h\"\n",
        )
        .unwrap();
        std::fs::write(pkg.join("local.h"), "// macros\n").unwrap();

        let got = collect_cross_pkg_asm_headers(
            &pkg.to_string_lossy(),
            "crypto/foo",
            &["impl_amd64.s".to_string()],
        );
        assert!(
            got.is_empty(),
            "self-contained asm must not stage any cross-package header: {got:?}"
        );
    }
}
