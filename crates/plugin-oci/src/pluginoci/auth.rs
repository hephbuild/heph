//! Registry credentials for the in-process drivers (`oci_pull`, `oci_push`).
//!
//! Three modes, and which one applies is decided by the BUILD file, never by
//! what happens to be configured on the host:
//!
//! - **Neither attribute** (the default): anonymous. A public pull needs
//!   nothing, and an identity nobody declared is not one heph picks up on its
//!   own — the same build on a laptop and on a runner then makes the same
//!   request.
//! - **`credentials = [...]`**: those, and only those. A registry none of them
//!   covers is an error rather than an anonymous request. Falling back would
//!   turn a typo in a declaration into a build that quietly runs as someone
//!   else — the exact accident a credential exists to prevent (see
//!   `docs/CREDENTIALS.md`).
//! - **`ambient_credentials = True`**: the developer's own Docker config, found
//!   where the docker CLI finds it, anonymous for a registry it has nothing for.
//!   An explicit opt-in to whatever identity the host happens to have, visible
//!   in the BUILD file where a reviewer can see it.
//!
//! A declared credential reaches this driver the way it reaches any other: as a
//! [`CredentialMount`] the host already acquired. The presentation read here is
//! the Docker helper one (`present = heph.auth.docker([...])`) — a generated
//! `DOCKER_CONFIG` whose `credHelpers` name `heph`, and the
//! `docker-credential-heph` shim on the mount's `path_prefix`. The shim is
//! called exactly as the docker CLI would call it, so the pinned-source and
//! refresh rules the host enforces for every other consumer hold here too.

use anyhow::Context as _;
use hcore::hasync::Cancellable;
use hplugin::driver::CredentialMount;
use hplugin::driver::targetdef::TargetDef;
use oci_client::secrets::RegistryAuth;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Refuse `credentials` and `ambient_credentials` together, at parse, where the
/// BUILD author sees it. Two sources for one request is two identities with
/// nothing to choose between them.
pub(crate) fn check_exclusive(credentials: &[String], ambient: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        !ambient || credentials.is_empty(),
        "`credentials` and `ambient_credentials = True` are exclusive: a target authenticates \
         either with its declared credentials or with the host's own docker login. Drop one"
    );
    Ok(())
}

/// Where a registry operation's credentials come from.
pub(crate) enum RegistryCredentials<'a> {
    /// The target declared nothing: anonymous.
    None,
    /// `ambient_credentials = True`: the host's own Docker config.
    Ambient,
    /// The target declared `credentials`: these mounts, exclusively.
    Declared {
        mounts: &'a [CredentialMount],
        /// Working directory for the helper — the run's sandbox, never the
        /// plugin process's cwd, which a plugin must not assume anything about.
        cwd: &'a Path,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    },
}

impl<'a> RegistryCredentials<'a> {
    /// The source for a run of `target` that received `mounts`, where
    /// `ambient` is the target's `ambient_credentials`.
    ///
    /// Decided by what the target *declared* — its credential edges and its
    /// opt-in — and checked against what arrived. A target that declared
    /// credentials and received none is refused rather than run anonymously: a
    /// mount lost anywhere between host and driver would otherwise turn a
    /// declared identity into none, and the 401 that follows would point at
    /// the registry rather than at the lost mount.
    pub(crate) fn for_run(
        target: &TargetDef,
        ambient: bool,
        mounts: &'a [CredentialMount],
        cwd: &'a Path,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<Self> {
        let declared = target
            .inputs
            .iter()
            .filter(|i| hdriver_support::credential::is_credential(&i.annotations))
            .count();
        if declared == 0 && mounts.is_empty() {
            return Ok(if ambient { Self::Ambient } else { Self::None });
        }
        // Parse refuses both at once; a def that has both was not made by it.
        anyhow::ensure!(
            !ambient,
            "{} sets both `credentials` and `ambient_credentials`",
            target.addr.format()
        );
        // The host presents exactly one mount per declared credential, or
        // fails the run itself.
        anyhow::ensure!(
            mounts.len() >= declared,
            "{} declares {declared} credential(s), but the host presented {}. The host is likely \
             older than the plugin; upgrade heph",
            target.addr.format(),
            mounts.len()
        );
        Ok(Self::Declared {
            mounts,
            cwd,
            ctoken,
        })
    }

    /// What to add to a failed registry operation, so a 401 on an anonymous
    /// request says how to authenticate rather than only that it did not.
    pub(crate) fn failure_hint(&self) -> &'static str {
        match self {
            Self::None => {
                "the request was anonymous: this target names no `credentials` and does not set \
                 `ambient_credentials = True`. A private image needs one of them"
            }
            Self::Ambient => "authenticated with the host's own docker config",
            Self::Declared { .. } => "authenticated with the target's declared credentials",
        }
    }

    /// The auth to present to `registry` (as `Reference::resolve_registry`
    /// spells it).
    pub(crate) async fn resolve(&self, registry: &str) -> anyhow::Result<RegistryAuth> {
        match self {
            Self::None => Ok(RegistryAuth::Anonymous),
            // Off the async worker: a `credsStore` of `osxkeychain` or `desktop`
            // makes this spawn and wait on a helper.
            Self::Ambient => {
                let registry = registry.to_string();
                Ok(hcore::blocking::run(move || ambient(&registry)).await)
            }
            Self::Declared {
                mounts,
                cwd,
                ctoken,
            } => {
                let (mount, helper) = select(mounts, registry)?;
                call_helper(mount, &helper, cwd, *ctoken).await
            }
        }
    }
}

/// Credentials from the same places the docker CLI looks:
/// `$DOCKER_CONFIG`/`$HOME/.docker/config.json`, podman's `auth.json`, and the
/// `docker-credential-*` helper named by `credsStore` / `credHelpers`.
fn ambient(server: &str) -> RegistryAuth {
    match docker_credential::get_credential(server) {
        Ok(docker_credential::DockerCredential::UsernamePassword(user, pass)) => {
            RegistryAuth::Basic(user, pass)
        }
        Ok(docker_credential::DockerCredential::IdentityToken(token)) => {
            RegistryAuth::Bearer(token)
        }
        Err(e) => {
            // Not an error: an unconfigured registry is the normal case for a
            // public pull. Logged so an unexpected 401 has something to point at.
            tracing::debug!(server, error = %e, "no docker credentials; continuing anonymously");
            RegistryAuth::Anonymous
        }
    }
}

/// The subset of a Docker `config.json` a presented credential can carry.
#[derive(serde::Deserialize)]
struct DockerConfig {
    #[serde(default, rename = "credHelpers")]
    cred_helpers: BTreeMap<String, String>,
}

/// A helper to call: which config key matched (sent on stdin, as docker does)
/// and the helper's name.
#[derive(Debug, PartialEq, Eq)]
struct Helper {
    server: String,
    name: String,
}

/// The one mount covering `registry`, and the helper it names for it.
///
/// Exactly one: none is a declaration that does not cover this registry, and
/// two is two identities for one request with nothing to choose between them —
/// the same reason two credentials may not present one variable.
fn select<'m>(
    mounts: &'m [CredentialMount],
    registry: &str,
) -> anyhow::Result<(&'m CredentialMount, Helper)> {
    let wanted = normalize_registry(registry);
    let mut found: Vec<(&CredentialMount, Helper)> = Vec::new();
    let mut without_config: Vec<String> = Vec::new();
    for m in mounts {
        let Some(dir) = m.env.get("DOCKER_CONFIG") else {
            without_config.push(m.addr.format());
            continue;
        };
        let path = Path::new(dir).join("config.json");
        let raw = std::fs::read(&path).with_context(|| {
            format!(
                "read the docker config credential {} presented at {path:?}",
                m.addr.format()
            )
        })?;
        let config: DockerConfig = serde_json::from_slice(&raw).with_context(|| {
            format!(
                "parse the docker config credential {} presented at {path:?}",
                m.addr.format()
            )
        })?;
        if let Some((server, name)) = config
            .cred_helpers
            .into_iter()
            .find(|(k, _)| normalize_registry(k) == wanted)
        {
            found.push((m, Helper { server, name }));
        }
    }

    match found.len() {
        1 => Ok(found.remove(0)),
        0 => {
            let mut msg = format!(
                "no declared credential covers registry {registry:?}. A target that names \
                 `credentials` authenticates with those only — never with the host's own docker \
                 login — so list {registry:?} in a credential's `heph.auth.docker([...])` \
                 presentation, or add a credential that does"
            );
            if !without_config.is_empty() {
                msg.push_str(&format!(
                    ". Not a docker presentation, so not consulted: {}",
                    without_config.join(", ")
                ));
            }
            anyhow::bail!(msg)
        }
        _ => anyhow::bail!(
            "registry {registry:?} is covered by more than one declared credential ({}); a \
             request carries one identity, so drop it from all but one",
            found
                .iter()
                .map(|(m, _)| m.addr.format())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// A registry host as a Docker config keys it, reduced to one spelling.
///
/// A config key may carry a scheme and a path (`https://index.docker.io/v1/`),
/// and Docker Hub answers to three hostnames. Without this, `heph.auth.docker(
/// ["docker.io"])` would not cover `alpine`, which `oci_client` resolves to
/// `index.docker.io`.
fn normalize_registry(s: &str) -> String {
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let host = s.split_once('/').map_or(s, |(h, _)| h).to_ascii_lowercase();
    match host.as_str() {
        "index.docker.io" | "registry-1.docker.io" | "registry.hub.docker.com" => {
            "docker.io".to_string()
        }
        _ => host,
    }
}

/// Run `docker-credential-<name> get` from the mount, the way the docker CLI
/// would, and turn its answer into the auth `oci_client` takes.
async fn call_helper(
    mount: &CredentialMount,
    helper: &Helper,
    cwd: &Path,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<RegistryAuth> {
    let addr = mount.addr.format();
    let exe = format!("docker-credential-{}", helper.name);
    // Resolved against the mount's own `path_prefix`, not a `PATH` lookup: the
    // helper a credential presents is the one it put there, and the plugin
    // process's `PATH` is not something it may assume anything about.
    let program = find_executable(&mount.path_prefix, &exe).with_context(|| {
        format!(
            "credential {addr} names docker helper {:?} for {}, but presents no `{exe}`",
            helper.name, helper.server
        )
    })?;

    let stdin = stdin_file(cwd, &helper.server)?;

    let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> = mount
        .env
        .iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
    if let Some(path) = hexecrunner::join_path(mount.path_prefix.iter().map(|p| p.into())) {
        env.push(("PATH".into(), path));
    }
    let spec = hproc::proc_exec::Spec {
        program,
        args: vec!["get".into()],
        env,
        cwd: cwd.to_path_buf(),
        stdin: hproc::proc_exec::StdioSpec::Fd(stdin.into()),
        stdout: hproc::proc_exec::StdioSpec::Piped,
        stderr: hproc::proc_exec::StdioSpec::Piped,
        setsid: false,
        ctty: false,
    };
    let out = hexecrunner::output(hexecrunner::RunnerRef::local(), spec, ctoken)
        .await
        .with_context(|| format!("run {exe} for credential {addr}"))?;
    anyhow::ensure!(
        out.status.success(),
        "{exe} (credential {addr}) failed for {}: {}",
        helper.server,
        redact(&String::from_utf8_lossy(&out.stderr), &mount.redact).trim()
    );
    parse_helper_answer(&out.stdout).with_context(|| {
        format!(
            "read {exe}'s answer for {} (credential {addr})",
            helper.server
        )
    })
}

/// The helper's stdin: `server`, as an unlinked regular file opened for reading.
///
/// Docker writes the server on stdin and closes it. A file rather than a pipe,
/// because a pipe's EOF depends on every write end being closed: on macOS
/// `pipe()` sets close-on-exec in a second step, so a sibling fork landing in
/// between keeps the write end open in some unrelated long-lived process and
/// the helper waits for EOF forever. A file's EOF is its size, whoever else
/// holds it; std opens it close-on-exec atomically on every supported target.
/// Unlinked once open, so nothing is left behind whatever happens next.
fn stdin_file(dir: &Path, server: &str) -> anyhow::Result<std::fs::File> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = dir.join(format!(
        ".heph-docker-helper-stdin-{}-{n}",
        std::process::id()
    ));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, server.as_bytes()))
        .with_context(|| format!("write the helper's stdin to {path:?}"))?;
    let file =
        std::fs::File::open(&path).with_context(|| format!("open the helper's stdin {path:?}"));
    // Removed whether or not the open worked: an open fd reads it regardless.
    let removed = std::fs::remove_file(&path);
    let file = file?;
    removed.with_context(|| format!("remove the helper's stdin {path:?}"))?;
    Ok(file)
}

fn find_executable(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| {
        use std::os::unix::fs::PermissionsExt as _;
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// Scrub `needles` out of `text` before it reaches an error — and from there a
/// log, an event and a CI report. A helper's stderr is written by a tool heph
/// does not control, so it may echo the material it was about to answer with.
///
/// Same floor as the output tee: a needle shorter than
/// [`REDACT_MIN_LEN`](hplugin::driver::REDACT_MIN_LEN) cannot be scrubbed
/// without corrupting ordinary text. Longest first, so a secret that contains
/// another is replaced whole.
fn redact(text: &str, needles: &[String]) -> String {
    let mut needles: Vec<&str> = needles
        .iter()
        .map(String::as_str)
        .filter(|n| n.len() >= hplugin::driver::REDACT_MIN_LEN)
        .collect();
    needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
    needles
        .into_iter()
        .fold(text.to_string(), |t, n| t.replace(n, "[redacted]"))
}

/// The Docker credential-helper answer: `{"ServerURL", "Username", "Secret"}`.
///
/// A `Username` of `<token>` is refused. To the docker CLI it marks an OAuth
/// refresh token to exchange at the registry's token endpoint; sent as-is it
/// would be a raw `Authorization: Bearer`, which skips that exchange and which
/// registries reject for a personal access token. The fix is one field on the
/// credential, and the error says so — better than a 401 from the registry.
fn parse_helper_answer(stdout: &[u8]) -> anyhow::Result<RegistryAuth> {
    #[derive(serde::Deserialize)]
    struct Answer {
        #[serde(rename = "Username")]
        username: String,
        #[serde(rename = "Secret")]
        secret: String,
    }
    let a: Answer = serde_json::from_slice(stdout)
        .context("expected a JSON object with `Username` and `Secret`")?;
    anyhow::ensure!(
        !a.secret.is_empty(),
        "the helper returned an empty `Secret`"
    );
    anyhow::ensure!(
        a.username != "<token>" && !a.username.is_empty(),
        "the credential's material has a token but no username. A registry takes a token as \
         the password of basic auth, under a username it chooses — give the material a \
         `username` field (ghcr.io and GitLab accept any name, Google Artifact Registry wants \
         `oauth2accesstoken`, ECR `AWS`, Docker Hub your account name)"
    );
    Ok(RegistryAuth::Basic(a.username, a.secret))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hmodel::htaddr::parse_addr;

    /// A presented credential the way the host writes one for
    /// `heph.auth.docker(registries)`: a `DOCKER_CONFIG` naming `heph`, and a
    /// `docker-credential-heph` on the mount's `path_prefix`. The shim here is a
    /// script answering with `answer` and recording what it was asked.
    struct Presented {
        dir: tempfile::TempDir,
        mount: CredentialMount,
    }

    impl Presented {
        fn new(addr: &str, registries: &[&str], answer: &str) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let cfg = dir.path().join("docker");
            std::fs::create_dir_all(&cfg).expect("mkdir");
            let helpers: serde_json::Map<String, serde_json::Value> = registries
                .iter()
                .map(|r| ((*r).to_string(), serde_json::json!("heph")))
                .collect();
            std::fs::write(
                cfg.join("config.json"),
                serde_json::json!({ "credHelpers": helpers }).to_string(),
            )
            .expect("write config");
            let bin = dir.path().join("bin");
            std::fs::create_dir_all(&bin).expect("mkdir");
            let asked = dir.path().join("asked");
            // Builtins only: the helper's `PATH` is the mount's `path_prefix`
            // and nothing else, so there is no `cat` to call.
            hcore::fsutil::write_executable(
                &bin.join("docker-credential-heph"),
                format!(
                    "#!/bin/sh\nIFS= read -r server\nprintf '%s|%s' \"$1\" \"$server\" > \
                     {asked:?}\nprintf '%s' '{answer}'\n"
                )
                .as_bytes(),
            )
            .expect("write shim");
            let mount = CredentialMount {
                addr: parse_addr(addr).expect("addr"),
                env: BTreeMap::from([(
                    "DOCKER_CONFIG".to_string(),
                    cfg.to_string_lossy().into_owned(),
                )]),
                path_prefix: vec![bin],
                redact: vec![],
            };
            Presented { dir, mount }
        }

        fn asked(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("asked")).unwrap_or_default()
        }
    }

    /// A def declaring `n` credentials, the way `oci_pull`/`oci_push` parse one.
    fn def(n: usize) -> TargetDef {
        let refs: Vec<String> = (0..n).map(|i| format!("//auth:c{i}")).collect();
        let addr = parse_addr("//img:app").expect("addr");
        TargetDef {
            inputs: hdriver_support::credential::inputs(&refs, &addr.package).expect("inputs"),
            addr,
            labels: vec![],
            raw_def: std::sync::Arc::new(()),
            outputs: vec![],
            support_files: vec![],
            cache: hplugin::driver::targetdef::CacheConfig::off(),
            pty: false,
            hash: vec![],
            transparent: false,
        }
    }

    async fn resolve(mounts: &[CredentialMount], registry: &str) -> anyhow::Result<RegistryAuth> {
        let cwd = tempfile::tempdir().expect("tempdir");
        let ctoken = StdCancellationToken::new();
        RegistryCredentials::for_run(&def(mounts.len()), false, mounts, cwd.path(), &ctoken)?
            .resolve(registry)
            .await
    }

    /// The whole round trip: the shim is called with `get`, is told the
    /// registry on stdin, and its username/secret become basic auth.
    #[tokio::test]
    async fn a_declared_credential_answers_through_its_helper() {
        let p = Presented::new(
            "//auth:ghcr",
            &["ghcr.io"],
            r#"{"ServerURL":"ghcr.io","Username":"bot","Secret":"s3cret-material"}"#,
        );
        let auth = resolve(std::slice::from_ref(&p.mount), "ghcr.io")
            .await
            .expect("resolve");
        assert_eq!(
            auth,
            RegistryAuth::Basic("bot".into(), "s3cret-material".into())
        );
        assert_eq!(p.asked(), "get|ghcr.io");
    }

    /// `<token>` is what the host's helper answers for material with no
    /// username. Sent as a raw bearer it would skip the registry's token
    /// exchange, so it is refused with the fix spelled out.
    #[tokio::test]
    async fn a_token_without_a_username_is_refused() {
        let p = Presented::new(
            "//auth:t",
            &["reg.example"],
            r#"{"ServerURL":"reg.example","Username":"<token>","Secret":"tok-material"}"#,
        );
        let err = resolve(std::slice::from_ref(&p.mount), "reg.example")
            .await
            .expect_err("a bare token must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("`username` field"), "{msg}");
        assert!(msg.contains("//auth:t"), "{msg}");
        assert!(!msg.contains("tok-material"), "material leaked: {msg}");
    }

    /// Malformed answers fail naming the credential, never as auth.
    #[test]
    fn malformed_helper_answers_are_errors() {
        for (answer, needle) in [
            ("not json", "`Username` and `Secret`"),
            (r#"{"Secret":"x"}"#, "`Username` and `Secret`"),
            (r#"{"Username":"u","Secret":""}"#, "empty `Secret`"),
            (r#"{"Username":"","Secret":"s"}"#, "`username` field"),
        ] {
            let err = parse_helper_answer(answer.as_bytes()).expect_err(answer);
            assert!(format!("{err:#}").contains(needle), "{answer}: {err:#}");
        }
        // The real helper ends its answer with a newline.
        assert_eq!(
            parse_helper_answer(b"{\"Username\":\"u\",\"Secret\":\"s\"}\n").expect("parse"),
            RegistryAuth::Basic("u".into(), "s".into())
        );
    }

    #[test]
    fn registry_spellings_normalize_to_one_key() {
        for (a, b) in [
            ("localhost:5000", "localhost:5000"),
            ("LOCALHOST:5000", "localhost:5000"),
            ("http://localhost:5000/v2/", "localhost:5000"),
            ("ghcr.io/", "ghcr.io"),
            ("https://ghcr.io", "ghcr.io"),
            ("registry-1.docker.io", "docker.io"),
        ] {
            assert_eq!(normalize_registry(a), normalize_registry(b), "{a} vs {b}");
        }
        // A port is part of the registry's identity: not the same key.
        assert_ne!(
            normalize_registry("ghcr.io:443"),
            normalize_registry("ghcr.io")
        );
    }

    /// A target that declared credentials and received none must not run on
    /// the host's own login — the mount was lost somewhere, and that is a bug
    /// to surface, not an identity to substitute.
    #[test]
    fn declared_credentials_without_mounts_are_refused() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let ctoken = StdCancellationToken::new();
        let err = match RegistryCredentials::for_run(&def(1), false, &[], cwd.path(), &ctoken) {
            Ok(_) => panic!("must refuse"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("//img:app"), "{err}");
        assert!(err.contains("host presented 0"), "{err}");
    }

    /// Material a helper echoes to stderr is scrubbed before it becomes an
    /// error — which is a log line, an event and a CI report.
    #[tokio::test]
    async fn a_helper_error_is_redacted() {
        let mut p = Presented::new("//auth:t", &["ghcr.io"], "{}");
        p.mount.redact = vec!["leaky-secret-material".to_string()];
        hcore::fsutil::write_executable(
            &p.mount.path_prefix[0].join("docker-credential-heph"),
            b"#!/bin/sh\necho 'bad token leaky-secret-material' >&2\nexit 1\n",
        )
        .expect("rewrite shim");
        let err = resolve(std::slice::from_ref(&p.mount), "ghcr.io")
            .await
            .expect_err("helper failed");
        let msg = format!("{err:#}");
        assert!(msg.contains("bad token [redacted]"), "{msg}");
        assert!(!msg.contains("leaky-secret-material"), "{msg}");
    }

    #[tokio::test]
    async fn an_unparseable_docker_config_names_the_credential() {
        let p = Presented::new("//auth:t", &["ghcr.io"], "{}");
        let cfg = std::path::PathBuf::from(&p.mount.env["DOCKER_CONFIG"]).join("config.json");
        std::fs::write(&cfg, "{not json").expect("write");
        let err = resolve(std::slice::from_ref(&p.mount), "ghcr.io")
            .await
            .expect_err("bad config");
        assert!(format!("{err:#}").contains("//auth:t"), "{err:#}");
    }

    /// A helper that is present but not executable is not a helper.
    #[tokio::test]
    async fn a_non_executable_helper_is_not_found() {
        use std::os::unix::fs::PermissionsExt as _;
        let p = Presented::new("//auth:t", &["ghcr.io"], "{}");
        let shim = p.mount.path_prefix[0].join("docker-credential-heph");
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let err = resolve(std::slice::from_ref(&p.mount), "ghcr.io")
            .await
            .expect_err("no helper");
        assert!(
            format!("{err:#}").contains("presents no `docker-credential-heph`"),
            "{err:#}"
        );
    }

    /// Docker Hub has three hostnames and config keys may carry a scheme and a
    /// path; a declaration naming `docker.io` must cover what `oci_client`
    /// resolves `alpine` to.
    #[tokio::test]
    async fn docker_hub_spellings_match() {
        for key in [
            "docker.io",
            "https://index.docker.io/v1/",
            "index.docker.io",
        ] {
            let p = Presented::new(
                "//auth:hub",
                &[key],
                r#"{"ServerURL":"x","Username":"u","Secret":"hub-material"}"#,
            );
            let auth = resolve(std::slice::from_ref(&p.mount), "index.docker.io")
                .await
                .unwrap_or_else(|e| panic!("{key}: {e:#}"));
            assert_eq!(auth, RegistryAuth::Basic("u".into(), "hub-material".into()));
            // The key as written is what the helper is asked about.
            assert_eq!(p.asked(), format!("get|{key}"));
        }
    }

    /// Declared credentials that do not cover the registry are an error, not
    /// an anonymous request — and not a fall back to the host's own login.
    #[tokio::test]
    async fn an_uncovered_registry_fails_rather_than_going_anonymous() {
        let p = Presented::new("//auth:ghcr", &["ghcr.io"], "{}");
        let err = resolve(std::slice::from_ref(&p.mount), "quay.io")
            .await
            .expect_err("uncovered");
        let msg = format!("{err:#}");
        assert!(msg.contains("no declared credential covers"), "{msg}");
        assert!(msg.contains("quay.io"), "{msg}");
        assert_eq!(p.asked(), "", "the helper must not be called");
    }

    /// A credential presented as plain env has no docker config to read. Named
    /// in the error, because "not covered" alone would send the reader looking
    /// at the wrong declaration.
    #[tokio::test]
    async fn a_non_docker_presentation_is_named_in_the_error() {
        let mount = CredentialMount {
            addr: parse_addr("//auth:envonly").expect("addr"),
            env: BTreeMap::from([("TOKEN".to_string(), "v".to_string())]),
            ..Default::default()
        };
        let err = resolve(&[mount], "ghcr.io").await.expect_err("uncovered");
        assert!(format!("{err:#}").contains("//auth:envonly"), "{err:#}");
    }

    #[tokio::test]
    async fn two_credentials_for_one_registry_are_refused() {
        let a = Presented::new("//auth:a", &["ghcr.io"], "{}");
        let b = Presented::new("//auth:b", &["https://ghcr.io"], "{}");
        let err = resolve(&[a.mount.clone(), b.mount.clone()], "ghcr.io")
            .await
            .expect_err("ambiguous");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("//auth:a") && msg.contains("//auth:b"),
            "{msg}"
        );
    }

    /// Each registry goes to the credential that covers it.
    #[tokio::test]
    async fn the_covering_credential_is_the_one_asked() {
        let a = Presented::new(
            "//auth:a",
            &["ghcr.io"],
            r#"{"Username":"a","Secret":"a-material"}"#,
        );
        let b = Presented::new(
            "//auth:b",
            &["quay.io"],
            r#"{"Username":"b","Secret":"b-material"}"#,
        );
        let mounts = [a.mount.clone(), b.mount.clone()];
        assert_eq!(
            resolve(&mounts, "quay.io").await.expect("resolve"),
            RegistryAuth::Basic("b".into(), "b-material".into())
        );
        assert_eq!(a.asked(), "");
    }

    #[tokio::test]
    async fn a_failing_helper_surfaces_its_stderr() {
        let p = Presented::new("//auth:t", &["ghcr.io"], "{}");
        hcore::fsutil::write_executable(
            &p.mount.path_prefix[0].join("docker-credential-heph"),
            b"#!/bin/sh\necho 'run heph auth login //auth:t' >&2\nexit 1\n",
        )
        .expect("rewrite shim");
        let err = resolve(std::slice::from_ref(&p.mount), "ghcr.io")
            .await
            .expect_err("helper failed");
        assert!(format!("{err:#}").contains("heph auth login"), "{err:#}");
    }

    #[tokio::test]
    async fn a_missing_helper_names_the_credential() {
        let p = Presented::new("//auth:t", &["ghcr.io"], "{}");
        std::fs::remove_file(p.mount.path_prefix[0].join("docker-credential-heph")).expect("rm");
        let err = resolve(std::slice::from_ref(&p.mount), "ghcr.io")
            .await
            .expect_err("no helper");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("//auth:t") && msg.contains("docker-credential-heph"),
            "{msg}"
        );
    }

    /// A registry that demands basic auth, serving one image whose manifest
    /// names a single `{}` config blob. Every `Authorization` header it sees is
    /// recorded, so a test asserts on what actually went over the wire rather
    /// than on the `RegistryAuth` value handed to `oci_client`.
    struct StubRegistry {
        port: u16,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    const CONFIG_DIGEST: &str =
        "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";

    impl StubRegistry {
        fn start(expected_basic: &'static str) -> Self {
            use std::io::{BufRead as _, Write as _};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let log = std::sync::Arc::clone(&seen);
            let manifest = serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {
                    "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": CONFIG_DIGEST,
                    "size": 2,
                },
                "layers": [],
            })
            .to_string();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).is_err() {
                        continue;
                    }
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    let mut auth = None;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some((k, v)) = line.split_once(':')
                            && k.eq_ignore_ascii_case("authorization")
                        {
                            auth = Some(v.trim().to_string());
                        }
                    }
                    if let Some(a) = &auth {
                        log.lock().expect("lock").push(a.clone());
                    }
                    let (status, ctype, body) = if auth.as_deref() != Some(expected_basic) {
                        ("401 Unauthorized", "application/json", String::new())
                    } else if path.contains("/manifests/") {
                        (
                            "200 OK",
                            "application/vnd.oci.image.manifest.v1+json",
                            manifest.clone(),
                        )
                    } else if path.contains("/blobs/") {
                        ("200 OK", "application/octet-stream", "{}".to_string())
                    } else {
                        ("200 OK", "application/json", "{}".to_string())
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
                         WWW-Authenticate: Basic realm=\"stub\"\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            });
            StubRegistry { port, seen }
        }

        fn host(&self) -> String {
            format!("127.0.0.1:{}", self.port)
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().expect("lock").clone()
        }
    }

    async fn pull(stub: &StubRegistry, mount: &CredentialMount) -> anyhow::Result<()> {
        let cwd = tempfile::tempdir().expect("tempdir");
        let blobs = tempfile::tempdir().expect("tempdir");
        let ctoken = StdCancellationToken::new();
        let creds = RegistryCredentials::for_run(
            &def(1),
            false,
            std::slice::from_ref(mount),
            cwd.path(),
            &ctoken,
        )?;
        super::super::registry::pull_layout(
            &format!("{}/acme/app:1", stub.host()),
            &super::super::pull::PlatformSelect::Only(vec!["linux/amd64".to_string()]),
            true,
            blobs.path(),
            &creds,
        )
        .await
        .map(|_| ())
    }

    /// End to end over the wire: the helper's username and secret reach the
    /// registry as basic auth, and the image is pulled with it.
    #[tokio::test]
    async fn a_declared_credential_authenticates_a_pull_over_the_wire() {
        let stub = StubRegistry::start("Basic Ym90OnMzY3JldC1tYXRlcmlhbA==");
        let p = Presented::new(
            "//auth:reg",
            &[&stub.host()],
            r#"{"Username":"bot","Secret":"s3cret-material"}"#,
        );
        pull(&stub, &p.mount).await.expect("pull");
        assert!(
            stub.seen()
                .iter()
                .any(|a| a == "Basic Ym90OnMzY3JldC1tYXRlcmlhbA=="),
            "{:?}",
            stub.seen()
        );
        assert_eq!(p.asked(), format!("get|{}", stub.host()));
    }

    /// Credentials the registry rejects fail the pull, naming the registry —
    /// and do not retry anonymously.
    #[tokio::test]
    async fn rejected_credentials_fail_the_pull() {
        let stub = StubRegistry::start("Basic Ym90OnMzY3JldC1tYXRlcmlhbA==");
        let p = Presented::new(
            "//auth:reg",
            &[&stub.host()],
            r#"{"Username":"bot","Secret":"wrong-material"}"#,
        );
        let err = pull(&stub, &p.mount).await.expect_err("rejected");
        assert!(format!("{err:#}").contains(&stub.host()), "{err:#}");
    }

    #[test]
    fn no_declaration_is_anonymous_and_ambient_is_opt_in() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let ctoken = StdCancellationToken::new();
        assert!(matches!(
            RegistryCredentials::for_run(&def(0), false, &[], cwd.path(), &ctoken),
            Ok(RegistryCredentials::None)
        ));
        assert!(matches!(
            RegistryCredentials::for_run(&def(0), true, &[], cwd.path(), &ctoken),
            Ok(RegistryCredentials::Ambient)
        ));
    }

    /// An anonymous request never consults the host: not even a configured
    /// `DOCKER_CONFIG` reaches it.
    #[tokio::test]
    async fn no_declaration_sends_nothing_over_the_wire() {
        let stub = StubRegistry::start("Basic Ym90OnMzY3JldC1tYXRlcmlhbA==");
        let cwd = tempfile::tempdir().expect("tempdir");
        let blobs = tempfile::tempdir().expect("tempdir");
        let ctoken = StdCancellationToken::new();
        let creds =
            RegistryCredentials::for_run(&def(0), false, &[], cwd.path(), &ctoken).expect("none");
        let err = super::super::registry::pull_layout(
            &format!("{}/acme/app:1", stub.host()),
            &super::super::pull::PlatformSelect::Only(vec!["linux/amd64".to_string()]),
            true,
            blobs.path(),
            &creds,
        )
        .await
        .err()
        .expect("an authenticated registry refuses an anonymous pull");
        assert!(stub.seen().is_empty(), "{:?}", stub.seen());
        assert!(format!("{err:#}").contains(&stub.host()), "{err:#}");
    }

    #[test]
    fn credentials_and_ambient_together_are_refused() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let ctoken = StdCancellationToken::new();
        let mount = CredentialMount::default();
        assert!(
            RegistryCredentials::for_run(
                &def(1),
                true,
                std::slice::from_ref(&mount),
                cwd.path(),
                &ctoken
            )
            .is_err()
        );
    }
}
