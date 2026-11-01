//! What a `go` process that can fetch modules takes from the host.
//!
//! `go mod download` and a module-reading `go list` both fetch: through a proxy, or
//! directly with `git` for `GOPRIVATE` and `GOPROXY=direct`. See `docs/GO_MODULES.md`.

use std::collections::HashMap;
use std::path::PathBuf;

/// Host variables that say where module bytes come from and how to reach them.
///
/// None of these reach a hash, so nothing may go on this list unless it only
/// answers that question. `GOFLAGS` does not: it changes what `go list` selects.
pub(crate) const FETCH_ACCESS_ENV: &[&str] = &[
    "HOME",
    "GOPATH",
    "GOPROXY",
    "GONOPROXY",
    "GOSUMDB",
    "GONOSUMDB",
    "GOPRIVATE",
    "GOINSECURE",
    "GOVCS",
    "GOAUTH",
    "NETRC",
    // `git`'s config. `go`'s own file can't be reached through it: see `add_fetch_env`.
    "XDG_CONFIG_HOME",
    "GIT_CONFIG_GLOBAL",
    "GIT_ASKPASS",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "SSH_AUTH_SOCK",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NIX_SSL_CERT_FILE",
    "GIT_SSL_CAINFO",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
];

/// The variables `go env -w` may contribute to a fetch.
const GO_ENV_FILE_FETCH_VARS: &[&str] = &[
    "GOPROXY",
    "GONOPROXY",
    "GOSUMDB",
    "GONOSUMDB",
    "GOPRIVATE",
    "GOINSECURE",
    "GOVCS",
    "GOAUTH",
];

/// Where `go env -w` writes, as `go env GOENV` would answer it from `host`.
fn go_env_file(host: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    match host("GOENV").as_deref() {
        Some("off") => return None,
        Some(path) if !path.is_empty() => return Some(PathBuf::from(path)),
        _ => {}
    }
    let home = || host("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    let config = if cfg!(target_os = "macos") {
        home()?.join("Library/Application Support")
    } else {
        host("XDG_CONFIG_HOME")
            .filter(|dir| dir.starts_with('/'))
            .map(PathBuf::from)
            .or_else(|| home().map(|h| h.join(".config")))?
    };
    Some(config.join("go/env"))
}

/// Add what a fetch takes from the host to `env`.
///
/// `go` reads its `go env -w` file through `HOME`, and that file can hold
/// `GOFLAGS=-tags=…`, which changes what `go list` selects while reaching no hash.
/// An empty `GOFLAGS` in the environment does not override it. So the file is off,
/// and its fetch settings are read here: `go env -w GOPRIVATE=…` is how many
/// people set that. The environment wins over the file, as it does in `go`.
pub(crate) fn add_fetch_env(
    env: &mut HashMap<String, String>,
    host: &impl Fn(&str) -> Option<String>,
) {
    let file = go_env_file(host)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    for (name, value) in file.lines().filter_map(|line| line.split_once('=')) {
        let name = name.trim();
        if GO_ENV_FILE_FETCH_VARS.contains(&name) {
            env.insert(name.to_string(), value.trim().to_string());
        }
    }
    for name in FETCH_ACCESS_ENV {
        if let Some(value) = host(name) {
            env.insert((*name).to_string(), value);
        }
    }
    env.insert("GOENV".to_string(), "off".to_string());
}

/// Whether a `go list -e` package error is an answer about the module's contents,
/// which the same inputs will always give, rather than a failure to reach it.
///
/// The provider turns an errored package with no Go files into "no such package"
/// and drops the import, and the listing is cached. A fetch failure must not take
/// that path: an unreachable module would read as an empty one, the build would
/// fail later on an unrelated "could not import", and fixing the credential would
/// not help because the answer is cached.
///
/// Matches known answers, so an error `go` words differently fails the target
/// instead of being cached as empty.
pub(crate) fn is_settled_package_error(err: &str) -> bool {
    const SETTLED: &[&str] = &[
        "no Go files in",
        "no non-test Go files in",
        "build constraints exclude all Go files",
        "does not contain package",
        "no required module provides package",
        "missing go.sum entry",
        "is not in std",
    ];
    SETTLED.iter().any(|answer| err.contains(answer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetch_env(vars: &[(&str, &str)]) -> HashMap<String, String> {
        let mut env = HashMap::new();
        add_fetch_env(&mut env, &|name: &str| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v).to_string())
        });
        env
    }

    #[test]
    fn the_go_env_file_gives_fetch_settings_and_nothing_else() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("env");
        std::fs::write(
            &file,
            "# a comment\nGOPRIVATE=a.b/*\nGOFLAGS=-tags=enterprise\nGOEXPERIMENT=x\nGOPROXY=direct\n",
        )
        .expect("write");
        let file = file.to_str().expect("utf8");

        let env = fetch_env(&[("GOENV", file), ("GOPROXY", "https://from-env")]);
        assert_eq!(env["GOPRIVATE"], "a.b/*");
        assert_eq!(env["GOPROXY"], "https://from-env", "the environment wins");
        assert_eq!(env["GOENV"], "off", "go must not read the file itself");
        for dropped in ["GOFLAGS", "GOEXPERIMENT", "# a comment"] {
            assert!(!env.contains_key(dropped), "{env:?}");
        }
    }

    #[test]
    fn the_go_env_file_is_found_where_go_env_w_writes_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().to_str().expect("utf8");
        let relative = if cfg!(target_os = "macos") {
            "Library/Application Support/go/env"
        } else {
            ".config/go/env"
        };
        let file = dir.path().join(relative);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, "GOPRIVATE=a.b/*\n").expect("write");

        assert_eq!(fetch_env(&[("HOME", home)])["GOPRIVATE"], "a.b/*");
        assert!(!fetch_env(&[("HOME", home), ("GOENV", "off")]).contains_key("GOPRIVATE"));
        assert!(!fetch_env(&[("GOENV", "/no/such/file")]).contains_key("GOPRIVATE"));
        assert_eq!(fetch_env(&[])["GOENV"], "off");
    }

    #[test]
    fn settled_package_errors() {
        for (err, settled) in [
            ("no Go files in /m/x", true),
            ("package a/b: no non-test Go files in /m/b", true),
            ("build constraints exclude all Go files in /m/x", true),
            (
                "module a.b/c@v1 found (v1), but does not contain package a.b/c/d",
                true,
            ),
            (
                "no required module provides package a.b/c; to add it:\n\tgo get a.b/c",
                true,
            ),
            (
                "missing go.sum entry for module providing package a.b/c",
                true,
            ),
            ("package nope is not in std (/goroot/src/nope)", true),
            // Reaching the module failed: the same inputs can succeed next time.
            ("a.b/c@v1: no secure protocol found for repository", false),
            (
                "a.b/c@v1: invalid version: git ls-remote -q origin: exit status 128: fatal: could not read Username: terminal prompts disabled",
                false,
            ),
            (
                "a.b/c@v1: Get \"https://proxy/a.b/c/@v/v1.info\": dial tcp: lookup proxy: no such host",
                false,
            ),
            (
                "a.b/c@v1: reading https://proxy/a.b/c/@v/v1.zip: 403 Forbidden",
                false,
            ),
            (
                "go: missing Git command. See https://go.dev/s/gogetcmd",
                false,
            ),
            ("", false),
        ] {
            assert_eq!(is_settled_package_error(err), settled, "{err}");
        }
    }
}
