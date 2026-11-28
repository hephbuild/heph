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
//!
//! Only over `https`, as Go's module fetch does with the same file: basic auth
//! over plain `http` is the credential in clear text. A loopback host is the
//! one exemption, since nothing leaves the machine.

use anyhow::Context as _;
use hfetch::BasicAuth;
use hplugin::driver::CredentialMount;

/// The variable `heph.auth.netrc` points at the file it presents.
const NETRC_ENV: &str = "NETRC";

/// The basic auth a run sends, and where it came from.
#[derive(Debug)]
pub(crate) struct Selected {
    pub(crate) auth: BasicAuth,
    /// The credential's address, for error context.
    pub(crate) credential: String,
    /// The `machine` entry that matched.
    pub(crate) host: String,
}

/// Refuse, at parse, a `url` that `credentials` cannot be sent to safely: one
/// that does not parse, names its own user (two identities for one request,
/// with the client choosing), or is not `https` to a non-loopback host.
///
/// Quotes `url` as every message does, without its userinfo, query or
/// fragment (`hfetch::redact_url`): the password it is refused for is
/// credential material (I7).
pub(crate) fn check_url(url: &str) -> anyhow::Result<()> {
    let parsed =
        url::Url::parse(url).with_context(|| format!("parse url {:?}", hfetch::redact_url(url)))?;
    anyhow::ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "the url carries a username or password and the target names `credentials`; drop the \
         one from the url"
    );
    let loopback = match parsed.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => anyhow::bail!(
            "url {:?} has no host to authenticate to",
            hfetch::redact_url(url)
        ),
    };
    anyhow::ensure!(
        parsed.scheme() == "https" || loopback,
        "the target names `credentials` but its url is {}://, which would send them in clear \
         text; use https (only a loopback host is exempt)",
        parsed.scheme()
    );
    Ok(())
}

/// The netrc entry for `url`'s host among the `mounts` a run of a target that
/// declared credentials received (see
/// [`check_presented`](hdriver_support::credential::check_presented)).
///
/// Reads the presented files, so call it off the async workers. Quotes `url`
/// as [`check_url`] does.
pub(crate) fn select_for(mounts: &[CredentialMount], url: &str) -> anyhow::Result<Selected> {
    let parsed =
        url::Url::parse(url).with_context(|| format!("parse url {:?}", hfetch::redact_url(url)))?;
    let host = parsed.host_str().with_context(|| {
        format!(
            "url {:?} has no host to authenticate to",
            hfetch::redact_url(url)
        )
    })?;
    select(mounts, &normalize_host(host))
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
    // What each consulted credential does cover: host names, not material, so
    // the error can name the near miss.
    let mut covers: Vec<String> = Vec::new();
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
        let machines: Vec<&str> = entries
            .iter()
            .filter_map(|e| e.machine.as_deref())
            .collect();
        covers.push(format!(
            "{credential} covers {}",
            if machines.is_empty() {
                "no machine".to_string()
            } else {
                machines.join(", ")
            }
        ));
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
            host: host.to_string(),
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
    if !covers.is_empty() {
        msg.push_str(&format!(" ({})", covers.join("; ")));
    }
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
/// lines, a line starting with `#` a comment, a `macdef` body running to the
/// next empty line.
///
/// A keyword waiting for its value takes the next token whatever it looks
/// like, so material that equals a keyword or starts with `#` is a value.
/// Keywords are matched case-sensitively, and curl's quoted tokens are refused
/// rather than read with their quotes: a password sent as `"s3cret"` would
/// come back as a 401 that blames the server.
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
        // A macro body runs to the next empty line (`lines` has already
        // dropped a `\r`), and is nothing heph uses.
        if in_macdef {
            in_macdef = !line.is_empty();
            continue;
        }
        if pending.is_none() && line.trim_start().starts_with('#') {
            continue;
        }
        for token in line.split_ascii_whitespace() {
            anyhow::ensure!(
                !token.starts_with('"'),
                "line {lineno}: quoted netrc tokens are not supported; write the value unquoted"
            );
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
        let why = match key {
            Pending::Login | Pending::Password => {
                ". An empty login or password cannot be written in netrc"
            }
            Pending::Machine | Pending::Account | Pending::Macdef => "",
        };
        anyhow::bail!("line {lineno}: `{}` has no value{why}", key.keyword());
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
    /// stream, a macro body runs to the empty line, and `default` is an entry
    /// with no machine.
    #[test]
    fn netrc_in_its_other_spellings() {
        let got = parse_netrc(
            "# a comment naming machine evil\n\
             machine\n  a.example\n\tlogin u1\n  account acct\n  password p#1\n\
             macdef init\ncd /pub\n  \nmachine inside-a-macro login x password y\n\n\
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
        // A macro body running to the end of the file is fine.
        assert_eq!(
            parse_netrc("machine h login u password p\nmacdef m\nls\n").expect("macdef at eof"),
            vec![entry(Some("h"), Some("u"), Some("p"))]
        );
    }

    /// Generated material is whatever bytes the source returned: a value that
    /// starts with `#`, even on the line after its keyword, or equals a
    /// keyword, is a value. CRLF line ends parse like LF.
    #[test]
    fn netrc_values_that_look_like_syntax() {
        assert_eq!(
            parse_netrc("machine h login u password #tok\n").expect("hash value"),
            vec![entry(Some("h"), Some("u"), Some("#tok"))]
        );
        assert_eq!(
            parse_netrc("machine h login u password\n#tok\n").expect("hash value next line"),
            vec![entry(Some("h"), Some("u"), Some("#tok"))]
        );
        assert_eq!(
            parse_netrc("machine h login machine password default\r\n").expect("keyword values"),
            vec![entry(Some("h"), Some("machine"), Some("default"))]
        );
        assert_eq!(
            parse_netrc("machine h password p\n").expect("no login"),
            vec![entry(Some("h"), None, Some("p"))]
        );
    }

    /// What `heph.auth.netrc` renders from material it cannot write (an empty
    /// login, a password with a space), and what it never writes (curl's
    /// quoting). All are errors, and none quotes the material.
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
            (
                "machine h login u password\n",
                "`password` has no value. An empty",
            ),
            ("login u password s3cr3t-token\n", "before any `machine`"),
            (
                "machine h login u passwd s3cr3t-token\n",
                "expected a netrc keyword",
            ),
            (
                "machine h login u password \"s3cr3t token\"\n",
                "quoted netrc tokens are not supported",
            ),
        ] {
            let err = format!("{:#}", parse_netrc(text).expect_err(text));
            assert!(err.contains(wants), "{text:?}: {err}");
            assert!(!err.contains("s3cr3t"), "an error quoted material: {err}");
        }
        let err = format!("{:#}", parse_netrc("machine").expect_err("bare machine"));
        assert!(err.ends_with("`machine` has no value"), "{err}");
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

    fn select_err(mounts: &[CredentialMount], url: &str) -> String {
        format!("{:#}", select_for(mounts, url).expect_err(url))
    }

    #[test]
    fn the_entry_for_the_urls_host_is_sent() {
        let p = Presented::netrc(
            "//auth:files",
            "machine other.example login no password no\n\
             machine FILES.example login u password the-token\n\
             machine files.example login second password entry\n",
        );
        // Host matching ignores case on both sides, port, path and query, and
        // the first entry for a host wins.
        let got = select_for(
            std::slice::from_ref(&p.mount),
            "https://Files.EXAMPLE:8443/a/b.tar.gz?x=1",
        )
        .expect("select");
        assert_eq!(got.auth.username, "u");
        assert_eq!(got.auth.password, "the-token");
        assert_eq!(got.credential, "//auth:files");
        assert_eq!(got.host, "files.example");
    }

    #[test]
    fn an_ipv6_literal_matches_without_brackets() {
        let p = Presented::netrc("//auth:v6", "machine ::1 login u password p\n");
        let got =
            select_for(std::slice::from_ref(&p.mount), "http://[::1]:8080/f").expect("select");
        assert_eq!(got.auth.username, "u");
    }

    /// The near misses: a declaration for `files.example` covers neither a
    /// longer name, nor its parent domain, nor its trailing-dot spelling, and
    /// an entry naming a port or a `default` entry is no fallback. The error
    /// lists what is covered, and never the material.
    #[test]
    fn an_uncovered_host_fails_rather_than_going_anonymous() {
        let p = Presented::netrc(
            "//auth:files",
            "machine files.example login u password s3cr3t\n\
             machine other.example:443 login u password s3cr3t\n\
             default login a password s3cr3t\n",
        );
        for url in [
            "https://files.example.evil/f",
            "https://example/f",
            "https://files.example./f",
            "https://other.example/f",
        ] {
            let err = select_err(std::slice::from_ref(&p.mount), url);
            assert!(err.contains("no declared credential covers host"), "{err}");
            assert!(
                err.contains("//auth:files covers files.example, other.example:443"),
                "{err}"
            );
            assert!(err.contains("`default` entry"), "{err}");
            assert!(!err.contains("s3cr3t"), "an error quoted material: {err}");
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
        let err = select_err(&[mount], "https://x.example/f");
        assert!(err.contains("not consulted: //auth:envonly"), "{err}");
    }

    #[test]
    fn two_credentials_for_one_host_are_refused() {
        let a = Presented::netrc("//auth:a", "machine x.example login u password p\n");
        let b = Presented::netrc("//auth:b", "machine x.example login v password q\n");
        let err = select_err(&[a.mount.clone(), b.mount.clone()], "https://x.example/f");
        assert!(err.contains("more than one declared credential"), "{err}");
        assert!(err.contains("//auth:a, //auth:b"), "{err}");
    }

    #[test]
    fn an_entry_without_both_halves_is_refused() {
        for body in [
            "machine x.example login u\n",
            "machine x.example password p\n",
        ] {
            let p = Presented::netrc("//auth:half", body);
            let err = select_err(std::slice::from_ref(&p.mount), "https://x.example/f");
            assert!(
                err.contains("without both a `login` and a `password`"),
                "{err}"
            );
        }
    }

    #[test]
    fn credentials_go_only_over_https_or_to_loopback() {
        for ok in [
            "https://files.example/f",
            "http://127.0.0.1:8080/f",
            "http://127.9.9.9/f",
            "http://[::1]/f",
            "http://LOCALHOST/f",
        ] {
            check_url(ok).unwrap_or_else(|e| panic!("{ok}: {e:#}"));
        }
        for (bad, wants) in [
            ("http://files.example/f", "clear text"),
            ("ftp://files.example/f", "clear text"),
            ("http://localhost.example/f", "clear text"),
            ("https://u:p@files.example/f", "username or password"),
            ("https://u@files.example/f", "username or password"),
            ("https://:p@files.example/f", "username or password"),
            ("not a url", "parse url"),
        ] {
            let err = format!("{:#}", check_url(bad).expect_err(bad));
            assert!(err.contains(wants), "{bad}: {err}");
        }
    }
}
