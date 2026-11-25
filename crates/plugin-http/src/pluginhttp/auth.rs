//! Credentials for `http_fetch`.
//!
//! The driver speaks HTTP in-process rather than spawning a tool, so it cannot
//! simply inherit a presented environment. It reads the one presentation an
//! HTTP client already understands, a netrc file: `present =
//! heph.auth.netrc(["host"])` writes one at `0600` and points `NETRC` at it,
//! the same file curl, wget and Go's module fetch read.
//!
//! The mode is decided by the BUILD file, never by the host:
//!
//! - **No `credentials`** (the default): anonymous. The developer's own
//!   `~/.netrc` is not read: an identity nobody declared is not one heph picks
//!   up, and the same BUILD file must make the same request on a laptop and on
//!   a runner.
//! - **`credentials = [...]`**: those, and only those. A host none of them
//!   covers is an error rather than an anonymous request, and two covering one
//!   host are refused, for the reasons `oci.pull` gives (see
//!   `docs/CREDENTIALS.md`).
//!
//! Only `machine` entries count. A `default` entry would cover every host, so
//! a declaration written for one server would authenticate a request to any
//! other; the error for an uncovered host says it was skipped.

use anyhow::Context as _;
use hfetch::BasicAuth;
use hplugin::driver::CredentialMount;
use hplugin::driver::targetdef::TargetDef;

/// The variable `heph.auth.netrc` points at the file it presents.
const NETRC_ENV: &str = "NETRC";

/// The basic auth a run sends, and the credential it came from.
#[derive(Debug)]
pub(crate) struct Selected {
    pub(crate) auth: BasicAuth,
    /// The credential's address, for error context.
    pub(crate) credential: String,
}

/// How many credentials `target` declared: its credential edges.
pub(crate) fn declared(target: &TargetDef) -> usize {
    target
        .inputs
        .iter()
        .filter(|i| hdriver_support::credential::is_credential(&i.annotations))
        .count()
}

/// The auth for a run of `target`, which [`declared`] `declared` credentials,
/// fetching `url`, given the `mounts` the host presented: `None` when the
/// target declared none.
///
/// Decided by what the target declared and checked against what arrived. A
/// target that declared credentials and received none is refused rather than
/// fetched anonymously: a mount lost between host and driver would otherwise
/// turn a declared identity into none, and the 401 that follows would point at
/// the server rather than at the lost mount.
///
/// Reads the presented files, so call it off the async workers.
pub(crate) fn resolve(
    target: &str,
    declared: usize,
    mounts: &[CredentialMount],
    url: &str,
) -> anyhow::Result<Option<Selected>> {
    if declared == 0 && mounts.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        mounts.len() >= declared,
        "{target} declares {declared} credential(s), but the host presented {}. The host is \
         likely older than the driver; upgrade heph",
        mounts.len()
    );
    let host = host_of(url)?;
    select(mounts, &host).map(Some)
}

/// `url`'s host, the way a netrc `machine` names it: lowercase, and an IPv6
/// literal without its brackets.
fn host_of(url: &str) -> anyhow::Result<String> {
    let parsed = url::Url::parse(url).with_context(|| format!("parse url {url:?}"))?;
    let host = parsed
        .host_str()
        .with_context(|| format!("url {url:?} has no host to authenticate to"))?;
    Ok(normalize_host(host))
}

fn normalize_host(host: &str) -> String {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

/// The one mount whose netrc covers `host`.
///
/// Exactly one: none is a declaration that does not cover this host, and two
/// is two identities for one request with nothing to choose between them.
fn select(mounts: &[CredentialMount], host: &str) -> anyhow::Result<Selected> {
    let mut found: Vec<Selected> = Vec::new();
    let mut without_netrc: Vec<String> = Vec::new();
    let mut with_default: Vec<String> = Vec::new();
    for m in mounts {
        let credential = m.addr.format();
        let Some(path) = m.env.get(NETRC_ENV) else {
            without_netrc.push(credential);
            continue;
        };
        let text = std::fs::read_to_string(path).with_context(|| {
            format!("read the netrc credential {credential} presented at {path:?}")
        })?;
        let entries = parse_netrc(&text).with_context(|| {
            format!("parse the netrc credential {credential} presented at {path:?}")
        })?;
        if entries.iter().any(|e| e.machine.is_none()) {
            with_default.push(credential.clone());
        }
        // The first entry for a host wins, as it does for curl.
        let Some(entry) = entries
            .into_iter()
            .find(|e| e.machine.as_deref().map(normalize_host).as_deref() == Some(host))
        else {
            continue;
        };
        let (Some(username), Some(password)) = (entry.login, entry.password) else {
            anyhow::bail!(
                "the netrc credential {credential} presents machine {host:?} without both a \
                 `login` and a `password`; basic auth needs the two"
            );
        };
        found.push(Selected {
            auth: BasicAuth { username, password },
            credential,
        });
    }

    if found.len() > 1 {
        anyhow::bail!(
            "host {host:?} is covered by more than one declared credential ({}); a request \
             carries one identity, so drop it from all but one",
            found
                .iter()
                .map(|s| s.credential.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if let Some(selected) = found.pop() {
        return Ok(selected);
    }
    let mut msg = format!(
        "no declared credential covers host {host:?}. A target that names `credentials` \
         authenticates with those only, never anonymously and never with the host's own \
         ~/.netrc, so list {host:?} in a credential's `heph.auth.netrc([...])` presentation, or \
         add a credential that does"
    );
    if !with_default.is_empty() {
        msg.push_str(&format!(
            ". A netrc `default` entry covers no host, and was skipped in: {}",
            with_default.join(", ")
        ));
    }
    if !without_netrc.is_empty() {
        msg.push_str(&format!(
            ". Not a netrc presentation (no `{NETRC_ENV}`), so not consulted: {}",
            without_netrc.join(", ")
        ));
    }
    anyhow::bail!(msg)
}

/// One netrc entry. `machine` is `None` for the `default` entry.
#[derive(Debug, PartialEq, Eq)]
struct NetrcEntry {
    machine: Option<String>,
    login: Option<String>,
    password: Option<String>,
}

/// Parse a netrc document: whitespace-separated `machine`, `default`,
/// `login`, `password`, `account` and `macdef` tokens, entries free to span
/// lines, a line starting with `#` a comment.
///
/// No error quotes a token: the document holds material, and an error chain
/// ends up in the run's log and failure event.
fn parse_netrc(text: &str) -> anyhow::Result<Vec<NetrcEntry>> {
    /// A keyword still waiting for its value, possibly on a later line.
    #[derive(Clone, Copy)]
    enum Pending {
        Machine,
        Login,
        Password,
        Account,
        Macdef,
    }

    impl Pending {
        fn keyword(self) -> &'static str {
            match self {
                Self::Machine => "machine",
                Self::Login => "login",
                Self::Password => "password",
                Self::Account => "account",
                Self::Macdef => "macdef",
            }
        }
    }

    let mut entries: Vec<NetrcEntry> = Vec::new();
    let mut pending: Option<(Pending, usize)> = None;
    let mut in_macdef = false;
    for (i, line) in text.lines().enumerate() {
        let lineno = i + 1;
        // A macro body runs to the next blank line, and is nothing heph uses.
        if in_macdef {
            in_macdef = !line.trim().is_empty();
            continue;
        }
        if line.trim_start().starts_with('#') {
            continue;
        }
        for token in line.split_ascii_whitespace() {
            if let Some((key, _)) = pending.take() {
                match key {
                    Pending::Machine => entries.push(NetrcEntry {
                        machine: Some(token.to_string()),
                        login: None,
                        password: None,
                    }),
                    Pending::Login | Pending::Password => {
                        let entry = entries.last_mut().with_context(|| {
                            format!(
                                "line {lineno}: `{}` before any `machine` or `default`",
                                key.keyword()
                            )
                        })?;
                        let slot = if matches!(key, Pending::Login) {
                            &mut entry.login
                        } else {
                            &mut entry.password
                        };
                        *slot = Some(token.to_string());
                    }
                    Pending::Account => {}
                    Pending::Macdef => {
                        // The body starts on the next line; the rest of this
                        // one is ignored.
                        in_macdef = true;
                        break;
                    }
                }
                continue;
            }
            pending = Some((
                match token {
                    "machine" => Pending::Machine,
                    "login" => Pending::Login,
                    "password" => Pending::Password,
                    "account" => Pending::Account,
                    "macdef" => Pending::Macdef,
                    "default" => {
                        entries.push(NetrcEntry {
                            machine: None,
                            login: None,
                            password: None,
                        });
                        continue;
                    }
                    // A value with whitespace in it, or an empty one, shifts
                    // every later token by one, and lands here.
                    _ => anyhow::bail!(
                        "line {lineno}: expected a netrc keyword (machine, default, login, \
                         password, account, macdef). A login or password that is empty or holds \
                         whitespace cannot be written in netrc"
                    ),
                },
                lineno,
            ));
        }
    }
    if let Some((key, lineno)) = pending {
        anyhow::bail!(
            "line {lineno}: `{}` has no value. An empty login or password cannot be written in \
             netrc",
            key.keyword()
        );
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmodel::htaddr::parse_addr;
    use std::collections::BTreeMap;

    fn entry(machine: Option<&str>, login: Option<&str>, password: Option<&str>) -> NetrcEntry {
        NetrcEntry {
            machine: machine.map(str::to_string),
            login: login.map(str::to_string),
            password: password.map(str::to_string),
        }
    }

    #[test]
    fn netrc_as_the_preset_writes_it() {
        let got = parse_netrc(
            "machine a.example login u1 password p1\nmachine b.example login u2 password p2\n",
        )
        .expect("parse");
        assert_eq!(
            got,
            vec![
                entry(Some("a.example"), Some("u1"), Some("p1")),
                entry(Some("b.example"), Some("u2"), Some("p2")),
            ]
        );
    }

    /// Entries span lines, `account` is skipped, a comment line is not a token
    /// stream, a macro body runs to the blank line, and `default` is an entry
    /// with no machine.
    #[test]
    fn netrc_in_its_other_spellings() {
        let got = parse_netrc(
            "# a comment naming machine evil\n\
             machine\n  a.example\n\tlogin u1\n  account acct\n  password p#1\n\
             macdef init\ncd /pub\nmachine inside-a-macro login x password y\n\n\
             default login anon password anon@\n",
        )
        .expect("parse");
        assert_eq!(
            got,
            vec![
                entry(Some("a.example"), Some("u1"), Some("p#1")),
                entry(None, Some("anon"), Some("anon@")),
            ]
        );
        assert_eq!(parse_netrc("").expect("empty"), vec![]);
        assert_eq!(parse_netrc("\n  \n").expect("blank"), vec![]);
    }

    /// What `heph.auth.netrc` renders from material it cannot write: an empty
    /// login, or a password with a space. Both are errors, and neither error
    /// quotes the material.
    #[test]
    fn netrc_that_cannot_be_read_is_an_error_that_quotes_nothing() {
        for (text, wants) in [
            // Empty login: `password` becomes the login, `s3cr3t-token` a keyword.
            (
                "machine h login  password s3cr3t-token\n",
                "expected a netrc keyword",
            ),
            (
                "machine h login u password s3cr3t token\n",
                "expected a netrc keyword",
            ),
            ("machine h login u password\n", "`password` has no value"),
            ("login u password s3cr3t-token\n", "before any `machine`"),
            (
                "machine h login u passwd s3cr3t-token\n",
                "expected a netrc keyword",
            ),
        ] {
            let err = format!("{:#}", parse_netrc(text).expect_err(text));
            assert!(err.contains(wants), "{text:?}: {err}");
            assert!(!err.contains("s3cr3t"), "an error quoted material: {err}");
        }
    }

    /// A presented netrc credential, the way the host writes one for
    /// `heph.auth.netrc(machines)`.
    struct Presented {
        _dir: tempfile::TempDir,
        mount: CredentialMount,
    }

    impl Presented {
        fn netrc(addr: &str, body: &str) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("netrc");
            std::fs::write(&path, body).expect("write netrc");
            let mount = CredentialMount {
                addr: parse_addr(addr).expect("addr"),
                env: BTreeMap::from([(NETRC_ENV.to_string(), path.to_string_lossy().into_owned())]),
                path_prefix: vec![],
                redact: vec![],
            };
            Self { _dir: dir, mount }
        }
    }

    /// A def declaring `n` credentials, the way `http_fetch` parses one.
    fn def(n: usize) -> TargetDef {
        let refs: Vec<String> = (0..n).map(|i| format!("//auth:c{i}")).collect();
        let addr = parse_addr("//dl:file").expect("addr");
        TargetDef {
            inputs: hdriver_support::credential::inputs(&refs, &addr.package).expect("inputs"),
            addr,
            ..Default::default()
        }
    }

    fn resolve_def(
        def: &TargetDef,
        mounts: &[CredentialMount],
        url: &str,
    ) -> anyhow::Result<Option<Selected>> {
        resolve(&def.addr.format(), declared(def), mounts, url)
    }

    fn resolve_for(mounts: &[CredentialMount], url: &str) -> anyhow::Result<Option<Selected>> {
        resolve_def(&def(mounts.len()), mounts, url)
    }

    #[test]
    fn no_credentials_is_anonymous() {
        assert!(
            resolve_for(&[], "https://x.example/f")
                .expect("ok")
                .is_none()
        );
    }

    #[test]
    fn the_entry_for_the_urls_host_is_sent() {
        let p = Presented::netrc(
            "//auth:files",
            "machine other.example login no password no\n\
             machine files.example login u password the-token\n",
        );
        // Host matching ignores case, port, path and query.
        let got = resolve_for(
            std::slice::from_ref(&p.mount),
            "https://FILES.example:8443/a/b.tar.gz?x=1",
        )
        .expect("resolve")
        .expect("authenticated");
        assert_eq!(got.auth.username, "u");
        assert_eq!(got.auth.password, "the-token");
        assert_eq!(got.credential, "//auth:files");
    }

    #[test]
    fn an_ipv6_literal_matches_without_brackets() {
        let p = Presented::netrc("//auth:v6", "machine ::1 login u password p\n");
        let got = resolve_for(std::slice::from_ref(&p.mount), "http://[::1]:8080/f")
            .expect("resolve")
            .expect("authenticated");
        assert_eq!(got.auth.username, "u");
    }

    /// The near miss: a declaration for `files.example` does not cover
    /// `files.example.evil`, nor its parent domain, and a `default` entry is
    /// not a fallback.
    #[test]
    fn an_uncovered_host_fails_rather_than_going_anonymous() {
        let p = Presented::netrc(
            "//auth:files",
            "machine files.example login u password p\ndefault login a password b\n",
        );
        for url in ["https://files.example.evil/f", "https://example/f"] {
            let err = format!(
                "{:#}",
                resolve_for(std::slice::from_ref(&p.mount), url).expect_err(url)
            );
            assert!(err.contains("no declared credential covers host"), "{err}");
            assert!(err.contains("`default` entry"), "{err}");
            assert!(err.contains("//auth:files"), "{err}");
        }
    }

    #[test]
    fn a_non_netrc_presentation_is_named_in_the_error() {
        let mount = CredentialMount {
            addr: parse_addr("//auth:envonly").expect("addr"),
            env: BTreeMap::from([("TOKEN".to_string(), "t".to_string())]),
            path_prefix: vec![],
            redact: vec![],
        };
        let err = format!(
            "{:#}",
            resolve_for(&[mount], "https://x.example/f").expect_err("uncovered")
        );
        assert!(err.contains("not consulted: //auth:envonly"), "{err}");
    }

    #[test]
    fn two_credentials_for_one_host_are_refused() {
        let a = Presented::netrc("//auth:a", "machine x.example login u password p\n");
        let b = Presented::netrc("//auth:b", "machine x.example login v password q\n");
        let err = format!(
            "{:#}",
            resolve_for(&[a.mount.clone(), b.mount.clone()], "https://x.example/f")
                .expect_err("ambiguous")
        );
        assert!(err.contains("more than one declared credential"), "{err}");
        assert!(err.contains("//auth:a, //auth:b"), "{err}");
    }

    #[test]
    fn an_entry_without_a_password_is_refused() {
        let p = Presented::netrc("//auth:half", "machine x.example login u\n");
        let err = format!(
            "{:#}",
            resolve_for(std::slice::from_ref(&p.mount), "https://x.example/f")
                .expect_err("half an entry")
        );
        assert!(
            err.contains("without both a `login` and a `password`"),
            "{err}"
        );
    }

    /// A target that declared credentials and received none fails, rather
    /// than fetching anonymously.
    #[test]
    fn declared_credentials_without_mounts_are_refused() {
        let err = format!(
            "{:#}",
            resolve_def(&def(1), &[], "https://x.example/f").expect_err("lost mount")
        );
        assert!(err.contains("declares 1 credential(s)"), "{err}");
    }
}
