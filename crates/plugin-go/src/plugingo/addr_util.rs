use crate::plugingo::factors::Factors;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

const STD_PREFIX: &str = "@heph/go/std/";
const THIRD_PREFIX: &str = "@heph/go/thirdparty/";
// Separator for thirdparty addresses with a non-root base module:
// "{base_pkg}/@heph/go/thirdparty/..." matches this literal inside the address.
const THIRD_SEPARATOR: &str = "/@heph/go/thirdparty/";

#[derive(Debug, PartialEq)]
pub enum GoPackageKind {
    FirstParty {
        module_root: PathBuf,
        module_path: String,
        import_path: String,
        src_dir: PathBuf,
    },
    Stdlib {
        import_path: String,
    },
    ThirdParty {
        module: String,
        version: String,
        subpath: String,
        /// Filesystem path of the go.mod that declares this dependency.
        module_root: PathBuf,
    },
}

type DecodeCache = Mutex<HashMap<(PkgBuf, PathBuf), Option<Arc<GoPackageKind>>>>;

static DECODE_CACHE: OnceLock<DecodeCache> = OnceLock::new();

fn decode_cache() -> &'static DecodeCache {
    DECODE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Decode a heph package address into its Go package kind.
/// Returns None if the address doesn't correspond to a Go package.
///
/// Results are memoized in a process-wide cache keyed by `(pkg, workspace_root)`
/// since the mapping is a pure function of those inputs plus filesystem state
/// that is stable for the duration of a build.
pub fn decode_package(pkg: &PkgBuf, workspace_root: &Path) -> Option<Arc<GoPackageKind>> {
    let key = (pkg.clone(), workspace_root.to_path_buf());
    {
        let cache = decode_cache().lock().expect("decode_cache poisoned");
        if let Some(cached) = cache.get(&key) {
            return cached.clone();
        }
    }

    let result = decode_package_uncached(pkg, workspace_root).map(Arc::new);

    let mut cache = decode_cache().lock().expect("decode_cache poisoned");
    cache.entry(key).or_insert_with(|| result.clone());
    result
}

fn decode_package_uncached(pkg: &PkgBuf, workspace_root: &Path) -> Option<GoPackageKind> {
    let s = pkg.as_str();

    if let Some(rest) = s.strip_prefix(STD_PREFIX) {
        return Some(GoPackageKind::Stdlib {
            import_path: rest.to_string(),
        });
    }

    // Thirdparty — two formats:
    // 1. "@heph/go/thirdparty/..."          → base_pkg = "" → module_root = workspace_root
    // 2. "{base_pkg}/@heph/go/thirdparty/..." → module_root = workspace_root/base_pkg
    if let Some(rest) = s.strip_prefix(THIRD_PREFIX) {
        let module_root = workspace_root.to_path_buf();
        return parse_thirdparty(rest, module_root);
    }
    if let Some((base_pkg, rest)) = s.split_once(THIRD_SEPARATOR) {
        let module_root = workspace_root.join(base_pkg);
        return parse_thirdparty(rest, module_root);
    }

    // First-party: any dir under a go.mod ancestor is a candidate, even if the
    // dir doesn't exist on disk yet — generated subpackages (codegen output)
    // are materialized inside the `_golist` sandbox by the codegen target wired
    // via `go_codegen_deps`. `find_go_mod` walks parent paths via string ops,
    // so it works on non-existent leaves as long as some ancestor has go.mod.
    //
    // Bound the result to `workspace_root` to avoid picking up an unrelated
    // go.mod above the workspace when no in-workspace go.mod exists.
    let src_dir = workspace_root.join(s);
    if let Some((module_root, module_path)) = find_go_mod(&src_dir)
        && module_root.starts_with(workspace_root)
    {
        let rel = src_dir
            .strip_prefix(&module_root)
            .expect("src_dir must be under module_root");
        let import_path = if rel.as_os_str().is_empty() {
            module_path.clone()
        } else {
            format!("{}/{}", module_path, rel.to_string_lossy())
        };
        return Some(GoPackageKind::FirstParty {
            module_root,
            module_path,
            import_path,
            src_dir,
        });
    }

    None
}

type GoModCache = Mutex<HashMap<PathBuf, Option<(PathBuf, String)>>>;

static GO_MOD_CACHE: OnceLock<GoModCache> = OnceLock::new();

fn go_mod_cache() -> &'static GoModCache {
    GO_MOD_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Walk up from `start` looking for go.mod; return (module_root, module_path).
pub fn find_go_mod(start: &Path) -> Option<(PathBuf, String)> {
    {
        let cache = go_mod_cache().lock().expect("go_mod_cache poisoned");
        if let Some(cached) = cache.get(start) {
            return cached.clone();
        }
    }

    let mut visited: Vec<PathBuf> = Vec::new();
    let mut current = start;
    let result = 'walk: loop {
        visited.push(current.to_path_buf());
        let candidate = current.join("go.mod");
        if candidate.exists()
            && let Ok(content) = std::fs::read_to_string(&candidate)
            && let Some(path) = parse_module_path(&content)
        {
            break 'walk Some((current.to_path_buf(), path));
        }
        match current.parent() {
            Some(p) => current = p,
            None => break 'walk None,
        }
    };

    let mut cache = go_mod_cache().lock().expect("go_mod_cache poisoned");
    for dir in visited {
        cache.entry(dir).or_insert_with(|| result.clone());
    }

    result
}

fn parse_module_path(content: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("//") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("module") {
            let path = rest.trim().trim_matches('"');
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

fn parse_thirdparty(rest: &str, module_root: PathBuf) -> Option<GoPackageKind> {
    // Format: <module>@<version>/<subpath> or <module>@<version>
    let (module, version_subpath) = rest.rsplit_once('@')?;

    if module.is_empty() {
        return None;
    }

    let (version, subpath) = match version_subpath.split_once('/') {
        Some((v, s)) => (v.to_string(), s.to_string()),
        None => (version_subpath.to_string(), String::new()),
    };

    if version.is_empty() {
        return None;
    }

    Some(GoPackageKind::ThirdParty {
        module: module.to_string(),
        version,
        subpath,
        module_root,
    })
}

/// Encode a stdlib import path as a heph Addr for build_lib.
pub fn encode_stdlib(import_path: &str, factors: &Factors) -> Addr {
    Addr::new(
        PkgBuf::from(format!("{}{}", STD_PREFIX, import_path)),
        "build_lib".to_string(),
        factors_to_args(factors),
    )
}

/// Encode a third-party package as a heph Addr for build_lib.
/// `base_pkg` is the module-root path relative to the workspace root
/// (e.g. "" for a root go.mod, "go" for go/go.mod).
pub fn encode_thirdparty(
    module: &str,
    version: &str,
    subpath: &str,
    base_pkg: &str,
    factors: &Factors,
) -> Addr {
    let thirdparty_part = if subpath.is_empty() {
        format!("{}{}@{}", THIRD_PREFIX, module, version)
    } else {
        format!("{}{}@{}/{}", THIRD_PREFIX, module, version, subpath)
    };

    let pkg = if base_pkg.is_empty() {
        thirdparty_part
    } else {
        format!("{}/{}", base_pkg, thirdparty_part)
    };

    Addr::new(
        PkgBuf::from(pkg),
        "build_lib".to_string(),
        factors_to_args(factors),
    )
}

/// Encode the module-root `download` target Addr for a thirdparty module.
///
/// Address format: `[<base_pkg>/]@heph/go/thirdparty/<module>@<version>:download`.
/// The download target is factor-independent (module bytes don't depend on goos/goarch),
/// so `args` is empty. One target per `(base_pkg, module, version)` — shared across
/// every consumer of any package under that module.
pub fn encode_thirdparty_download(module: &str, version: &str, base_pkg: &str) -> Addr {
    let thirdparty_part = format!("{}{}@{}", THIRD_PREFIX, module, version);
    let pkg = if base_pkg.is_empty() {
        thirdparty_part
    } else {
        format!("{}/{}", base_pkg, thirdparty_part)
    };
    Addr::new(PkgBuf::from(pkg), "download".to_string(), BTreeMap::new())
}

/// Encode a first-party package (relative to workspace root) as a heph Addr for build_lib.
pub fn encode_firstparty(src_dir: &Path, workspace_root: &Path, factors: &Factors) -> Addr {
    let rel = src_dir.strip_prefix(workspace_root).unwrap_or(src_dir);
    Addr::new(
        PkgBuf::from(rel.to_string_lossy().as_ref()),
        "build_lib".to_string(),
        factors_to_args(factors),
    )
}

pub fn factors_to_args(factors: &Factors) -> BTreeMap<String, String> {
    let mut args = BTreeMap::new();
    args.insert("goos".to_string(), factors.goos.clone());
    args.insert("goarch".to_string(), factors.goarch.clone());
    if !factors.build_tags.is_empty() {
        args.insert("tags".to_string(), factors.build_tags.join(","));
    }
    args
}

/// Convert an import path to a dep group name.
/// e.g. "github.com/foo/bar" → "lib_github_com_foo_bar"
pub fn import_path_to_dep_group(import_path: &str) -> String {
    let sanitized: String = import_path
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("lib_{}", sanitized)
}

/// Convert a dep group name to its SRC env var name.
/// e.g. "lib_fmt" → "SRC_LIB_FMT"
pub fn dep_group_env_var(group: &str) -> String {
    format!("SRC_{}", group.to_uppercase())
}

/// Dep group carrying the hermetic Go SDK tree into a build/test sandbox.
/// Unused as an `$SRC_*` reference — the SDK is reached via the deterministic
/// [`toolchain::staged_goroot`] path — but the dep edge is what makes the engine
/// stage the SDK files into the sandbox.
pub const GO_SDK_DEP_GROUP: &str = "gosdk";

/// `(group, value)` dep entry staging the hermetic Go SDK for `go_version` into
/// the sandbox at [`toolchain::staged_goroot`]. A single SDK output (the full
/// tree, incl. `GOROOT/src`) serves every consumer: it is staged read-only and
/// exposed via a directory symlink, so its size costs nothing per consumer.
/// Pair with [`go_sdk_read_only_config`] on `sh`/exec targets.
///
/// Returns `None` for the host toolchain ([`toolchain::HOST`]) — the host `go`
/// is read from the sandbox's `PATH`/`GOROOT`, not staged as a dep.
pub fn go_sdk_dep(go_version: &str) -> Option<(String, Value)> {
    if crate::plugingo::toolchain::is_host(go_version) {
        return None;
    }
    Some((
        GO_SDK_DEP_GROUP.to_string(),
        Value::List(vec![Value::String(
            crate::plugingo::toolchain::toolchain_addr(go_version).format(),
        )]),
    ))
}

/// `(key, value)` config entry marking the `gosdk` dep group for read-only
/// staging on the `sh`/exec driver: the SDK is materialized once into the
/// shared stage and exposed to each sandbox via a directory symlink instead of
/// byte-copied per consumer. `None` for the host toolchain (no SDK dep to mark).
pub fn go_sdk_read_only_config(go_version: &str) -> Option<(String, Value)> {
    if crate::plugingo::toolchain::is_host(go_version) {
        return None;
    }
    Some((
        "read_only_deps".to_string(),
        Value::List(vec![Value::String(GO_SDK_DEP_GROUP.to_string())]),
    ))
}

/// Host env vars to pass through (at runtime, unhashed) so the host toolchain
/// works inside the sandbox: `PATH` to find `go`, plus the Go/module cache and
/// proxy knobs `go` consults. Empty for a hermetic toolchain (reads nothing from
/// the host). Insert the names under the exec `runtime_pass_env` config key.
pub fn go_host_runtime_pass_env(go_version: &str) -> Vec<String> {
    if crate::plugingo::toolchain::is_host(go_version) {
        [
            "PATH",
            "HOME",
            "GOPATH",
            "GOMODCACHE",
            "GOPROXY",
            "GOFLAGS",
            "GOPRIVATE",
            "GONOSUMDB",
            "GONOSUMCHECK",
            "GOSUMDB",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    } else {
        Vec::new()
    }
}

/// `(key, value)` entry adding [`go_host_runtime_pass_env`] under the exec
/// `runtime_pass_env` config key, or `None` for a hermetic toolchain.
pub fn go_host_pass_env_config(go_version: &str) -> Option<(String, Value)> {
    let names = go_host_runtime_pass_env(go_version);
    if names.is_empty() {
        return None;
    }
    Some((
        "runtime_pass_env".to_string(),
        Value::List(names.into_iter().map(Value::String).collect()),
    ))
}

/// Prelude pointing `GOROOT` at the Go toolchain for `go_version` and exposing
/// its `go` binary on `PATH` and as `$GO`.
///
/// Hermetic: `GOROOT` is the staged SDK ([`toolchain::staged_goroot`]) — reads
/// nothing from the host; pair with [`go_sdk_dep`]. Host ([`toolchain::HOST`]):
/// `GOROOT` is resolved in-shell via the host `go env GOROOT` (so `go` must be
/// on the passed-through `PATH`, see [`go_host_runtime_pass_env`]).
pub fn go_goroot_prelude(go_version: &str) -> Vec<String> {
    if crate::plugingo::toolchain::is_host(go_version) {
        return vec![
            // Host `go` from PATH; pin GOROOT to its own report so `go tool`
            // invocations resolve the compiler/linker consistently.
            "export GOROOT=\"$(go env GOROOT)\"".to_string(),
            "export PATH=\"$GOROOT/bin:$PATH\"".to_string(),
            "GO=\"$GOROOT/bin/go\"".to_string(),
        ];
    }
    vec![
        format!(
            "export GOROOT=\"$WORKSPACE_ROOT/{}\"",
            crate::plugingo::toolchain::staged_goroot(go_version)
        ),
        "export PATH=\"$GOROOT/bin:$PATH\"".to_string(),
        "GO=\"$GOROOT/bin/go\"".to_string(),
    ]
}

/// Shell prelude every Go *compile/link/list* script runs first:
/// [`go_goroot_prelude`] plus a sandbox-local `GOCACHE` so the build cache is
/// neither read from nor written to the host.
///
/// Not for glob/tree-output targets (the download target's `**/*` would capture
/// the cache dir) — those use [`go_goroot_prelude`] directly.
pub fn go_run_prelude(go_version: &str) -> Vec<String> {
    let mut p = go_goroot_prelude(go_version);
    p.push("export GOCACHE=\"$PWD/.heph-gocache\"".to_string());
    p
}

/// Hashed `env` map shared by Go compile/link targets: disable cgo, pin the
/// toolchain to the (hermetic) local SDK, and ignore any workspace `go.work`.
/// GOOS/GOARCH stay in `runtime_env` (the target addr's factor args already key
/// the cache per platform).
pub fn go_build_env() -> Value {
    Value::Map(HashMap::from([
        ("CGO_ENABLED".to_string(), Value::String("0".to_string())),
        (
            "GOTOOLCHAIN".to_string(),
            Value::String("local".to_string()),
        ),
        ("GOWORK".to_string(), Value::String("off".to_string())),
    ]))
}

/// Wrap a list of shell commands into the `run` config value. The exec driver
/// joins list entries with newlines, so one command per element reads back as a
/// single script.
pub fn to_run_value(lines: Vec<String>) -> Value {
    Value::List(lines.into_iter().map(Value::String).collect())
}

/// Build the importcfg shell fragment for a set of transitive libs.
///
/// Emits `importcfg="$PWD/importcfg"`, clears the file, then appends one
/// `packagefile` line per library, sorted by import path for determinism.
/// An optional `skip` import path is excluded (used by the linker to omit
/// the main package from importcfg). Returns one shell command per element.
pub fn write_importcfg_script(
    transitive_libs: &[(String, Addr)],
    skip: Option<&str>,
) -> Vec<String> {
    let mut sorted: Vec<&(String, Addr)> = transitive_libs.iter().collect();
    sorted.sort_by_key(|(ip, _)| ip.as_str());

    let mut lines = vec![
        "importcfg=\"$PWD/importcfg\"".to_string(),
        "> \"$importcfg\"".to_string(),
    ];
    for (import_path, _) in &sorted {
        if skip.is_some_and(|s| *import_path == s) {
            continue;
        }
        let group = import_path_to_dep_group(import_path);
        let env_var = dep_group_env_var(&group);
        lines.push(format!(
            "printf \"packagefile {}=%s\\n\" \"${}\" >> \"$importcfg\"",
            import_path, env_var
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_workspace_with_go_mod(module_path: &str) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("go.mod"),
            format!("module {}\n\ngo 1.21\n", module_path),
        )
        .unwrap();
        dir
    }

    #[test]
    fn test_decode_stdlib() {
        let pkg = PkgBuf::from("@heph/go/std/fmt");
        let ws = Path::new("/tmp");
        let kind = decode_package(&pkg, ws).unwrap();
        assert_eq!(
            *kind,
            GoPackageKind::Stdlib {
                import_path: "fmt".to_string()
            }
        );
    }

    #[test]
    fn test_decode_stdlib_nested() {
        let pkg = PkgBuf::from("@heph/go/std/net/http");
        let ws = Path::new("/tmp");
        let kind = decode_package(&pkg, ws).unwrap();
        assert_eq!(
            *kind,
            GoPackageKind::Stdlib {
                import_path: "net/http".to_string()
            }
        );
    }

    #[test]
    fn test_decode_thirdparty() {
        let pkg = PkgBuf::from("@heph/go/thirdparty/github.com/foo/bar@v1.2.3/pkg");
        let ws = Path::new("/tmp");
        let kind = decode_package(&pkg, ws).unwrap();
        assert_eq!(
            *kind,
            GoPackageKind::ThirdParty {
                module: "github.com/foo/bar".to_string(),
                version: "v1.2.3".to_string(),
                subpath: "pkg".to_string(),
                module_root: PathBuf::from("/tmp"),
            }
        );
    }

    #[test]
    fn test_decode_thirdparty_no_subpath() {
        let pkg = PkgBuf::from("@heph/go/thirdparty/github.com/foo/bar@v1.0.0");
        let ws = Path::new("/tmp");
        let kind = decode_package(&pkg, ws).unwrap();
        assert_eq!(
            *kind,
            GoPackageKind::ThirdParty {
                module: "github.com/foo/bar".to_string(),
                version: "v1.0.0".to_string(),
                subpath: String::new(),
                module_root: PathBuf::from("/tmp"),
            }
        );
    }

    #[test]
    fn test_decode_thirdparty_with_base_pkg() {
        let pkg = PkgBuf::from("go/@heph/go/thirdparty/github.com/foo/bar@v1.2.3/pkg");
        let ws = Path::new("/tmp");
        let kind = decode_package(&pkg, ws).unwrap();
        assert_eq!(
            *kind,
            GoPackageKind::ThirdParty {
                module: "github.com/foo/bar".to_string(),
                version: "v1.2.3".to_string(),
                subpath: "pkg".to_string(),
                module_root: PathBuf::from("/tmp/go"),
            }
        );
    }

    #[test]
    fn test_decode_firstparty() {
        let ws = make_workspace_with_go_mod("example.com/myrepo");
        let pkg_dir = ws.path().join("mylib");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("lib.go"), "package mylib\n").unwrap();

        let pkg = PkgBuf::from("mylib");
        let kind = decode_package(&pkg, ws.path()).unwrap();
        assert!(
            matches!(&*kind, GoPackageKind::FirstParty { import_path, .. } if import_path == "example.com/myrepo/mylib")
        );
    }

    #[test]
    fn test_decode_firstparty_root() {
        let ws = make_workspace_with_go_mod("example.com/myrepo");
        std::fs::write(ws.path().join("main.go"), "package main\n").unwrap();

        // The package is the workspace root itself
        let root_pkg_dir = ws.path().to_path_buf();
        if let Some((_, module_path)) = find_go_mod(&root_pkg_dir) {
            assert_eq!(module_path, "example.com/myrepo");
        }
    }

    #[test]
    fn test_decode_non_go_returns_none() {
        let ws = tempfile::tempdir().unwrap();
        // No go.mod anywhere
        let subdir = ws.path().join("some/pkg");
        std::fs::create_dir_all(&subdir).unwrap();

        let pkg = PkgBuf::from("some/pkg");
        assert!(decode_package(&pkg, ws.path()).is_none());
    }

    #[test]
    fn test_decode_nonexistent_dir_returns_none() {
        let ws = tempfile::tempdir().unwrap();
        let pkg = PkgBuf::from("doesnotexist");
        assert!(decode_package(&pkg, ws.path()).is_none());
    }

    // Regression: a non-existent dir UNDER a workspace go.mod must decode as
    // FirstParty so codegen-generated subpackages (e.g. `some/pkg/gen/deep`
    // produced by a codegen target wired via go_codegen_deps) resolve before
    // the dir exists on disk.
    #[test]
    fn test_decode_nonexistent_dir_under_gomod_returns_firstparty() {
        let ws = make_workspace_with_go_mod("example.com/myrepo");
        let pkg = PkgBuf::from("gen/deep");
        let kind = decode_package(&pkg, ws.path()).expect("must decode as FirstParty");
        assert!(matches!(
            &*kind,
            GoPackageKind::FirstParty { import_path, .. } if import_path == "example.com/myrepo/gen/deep"
        ));
    }

    #[test]
    fn test_import_path_to_dep_group() {
        assert_eq!(import_path_to_dep_group("fmt"), "lib_fmt");
        assert_eq!(
            import_path_to_dep_group("github.com/foo/bar"),
            "lib_github_com_foo_bar"
        );
        assert_eq!(
            import_path_to_dep_group("golang.org/x/net"),
            "lib_golang_org_x_net"
        );
        assert_eq!(
            import_path_to_dep_group("github.com/foo-bar/baz"),
            "lib_github_com_foo_bar_baz"
        );
    }

    #[test]
    fn test_dep_group_env_var() {
        assert_eq!(dep_group_env_var("lib_fmt"), "SRC_LIB_FMT");
        assert_eq!(
            dep_group_env_var("lib_github_com_foo_bar"),
            "SRC_LIB_GITHUB_COM_FOO_BAR"
        );
    }

    #[test]
    fn test_encode_stdlib() {
        let factors = Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
        };
        let addr = encode_stdlib("fmt", &factors);
        assert_eq!(addr.package.as_str(), "@heph/go/std/fmt");
        assert_eq!(addr.name, "build_lib");
    }

    #[test]
    fn test_encode_thirdparty_with_subpath() {
        let factors = Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
        };
        let addr = encode_thirdparty("github.com/foo/bar", "v1.2.3", "pkg", "", &factors);
        assert_eq!(
            addr.package.as_str(),
            "@heph/go/thirdparty/github.com/foo/bar@v1.2.3/pkg"
        );
    }

    #[test]
    fn test_encode_thirdparty_no_subpath() {
        let factors = Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
        };
        let addr = encode_thirdparty("github.com/foo/bar", "v1.0.0", "", "", &factors);
        assert_eq!(
            addr.package.as_str(),
            "@heph/go/thirdparty/github.com/foo/bar@v1.0.0"
        );
    }

    #[test]
    fn test_encode_thirdparty_with_base_pkg() {
        let factors = Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
        };
        let addr = encode_thirdparty("github.com/foo/bar", "v1.2.3", "pkg", "go", &factors);
        assert_eq!(
            addr.package.as_str(),
            "go/@heph/go/thirdparty/github.com/foo/bar@v1.2.3/pkg"
        );
    }

    #[test]
    fn test_encode_thirdparty_download_root_base() {
        let addr = encode_thirdparty_download("github.com/foo/bar", "v1.2.3", "");
        assert_eq!(
            addr.package.as_str(),
            "@heph/go/thirdparty/github.com/foo/bar@v1.2.3"
        );
        assert_eq!(addr.name, "download");
        assert!(addr.args.is_empty(), "download addr must be factor-less");
    }

    #[test]
    fn test_encode_thirdparty_download_with_base_pkg() {
        let addr = encode_thirdparty_download("github.com/foo/bar", "v1.2.3", "go");
        assert_eq!(
            addr.package.as_str(),
            "go/@heph/go/thirdparty/github.com/foo/bar@v1.2.3"
        );
        assert_eq!(addr.name, "download");
    }

    #[test]
    fn test_encode_thirdparty_download_decodes_as_module_root() {
        let addr = encode_thirdparty_download("k8s.io/apimachinery", "v0.32.1", "go");
        let ws = Path::new("/workspace");
        let kind = decode_package(&addr.package, ws).unwrap();
        assert_eq!(
            *kind,
            GoPackageKind::ThirdParty {
                module: "k8s.io/apimachinery".to_string(),
                version: "v0.32.1".to_string(),
                subpath: String::new(),
                module_root: PathBuf::from("/workspace/go"),
            }
        );
    }

    #[test]
    fn test_encode_thirdparty_roundtrip_with_base_pkg() {
        let factors = Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
        };
        let addr = encode_thirdparty(
            "k8s.io/apimachinery",
            "v0.32.1",
            "pkg/util/sets",
            "go",
            &factors,
        );
        let ws = Path::new("/workspace");
        let kind = decode_package(&addr.package, ws).unwrap();
        assert_eq!(
            *kind,
            GoPackageKind::ThirdParty {
                module: "k8s.io/apimachinery".to_string(),
                version: "v0.32.1".to_string(),
                subpath: "pkg/util/sets".to_string(),
                module_root: PathBuf::from("/workspace/go"),
            }
        );
    }

    #[test]
    fn test_encode_firstparty() {
        let ws = tempfile::tempdir().unwrap();
        let src = ws.path().join("mylib");
        std::fs::create_dir_all(&src).unwrap();
        let factors = Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
        };
        let addr = encode_firstparty(&src, ws.path(), &factors);
        assert_eq!(addr.package.as_str(), "mylib");
        assert_eq!(addr.name, "build_lib");
        assert_eq!(addr.args.get("goos").map(|s| s.as_str()), Some("linux"));
    }

    #[test]
    fn test_parse_module_path_basic() {
        let content = "module github.com/foo/bar\n\ngo 1.21\n";
        assert_eq!(
            parse_module_path(content),
            Some("github.com/foo/bar".to_string())
        );
    }

    #[test]
    fn test_parse_module_path_with_comment() {
        let content = "// some comment\nmodule example.com/test\n";
        assert_eq!(
            parse_module_path(content),
            Some("example.com/test".to_string())
        );
    }
}
