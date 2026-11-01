//! What a `go` process that can fetch modules takes from the host.
//!
//! `go mod download` and a module-reading `go list` both fetch: through a proxy, or
//! directly with `git` for `GOPRIVATE` and `GOPROXY=direct`. See `docs/GO_MODULES.md`.

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
    // `git`'s config, and `go env -w`'s on Linux.
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
