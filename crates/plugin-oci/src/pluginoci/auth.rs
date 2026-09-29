//! Registry credentials for the in-process drivers (`oci_pull`, `oci_push`).
//!
//! Two sources, and which one applies is decided by the BUILD file, never by
//! what happens to be configured:
//!
//! - **No `credentials`**: the developer's own Docker config, found where the
//!   docker CLI finds it, and anonymous when there is none — a public pull
//!   needs nothing, and failing there would break the common case to serve the
//!   rare one.
//! - **`credentials = [...]`**: those, and only those. The ambient config is not
//!   consulted, and a registry none of them covers is an error rather than an
//!   anonymous request. Falling back would turn a typo in a declaration into a
//!   build that quietly runs as whoever the host happens to be logged in as —
//!   the exact accident a credential exists to prevent (see
//!   `docs/CREDENTIALS.md`).
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
use oci_client::secrets::RegistryAuth;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where a registry operation's credentials come from.
pub(crate) enum RegistryCredentials<'a> {
    /// The target declared no `credentials`: the ambient Docker config.
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
    /// The source for a run that received `mounts`.
    ///
    /// Keyed on the mounts rather than on the spec, because the host is what
    /// decides which of a target's references arrive: a target that declared
    /// credentials always receives them, and one that did not never does.
    pub(crate) fn for_run(
        mounts: &'a [CredentialMount],
        cwd: &'a Path,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> Self {
        if mounts.is_empty() {
            Self::Ambient
        } else {
            Self::Declared {
                mounts,
                cwd,
                ctoken,
            }
        }
    }

    /// The auth to present to `registry` (as `Reference::resolve_registry`
    /// spells it).
    pub(crate) async fn resolve(&self, registry: &str) -> anyhow::Result<RegistryAuth> {
        match self {
            Self::Ambient => Ok(ambient(registry)),
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
    let host = s.split('/').next().unwrap_or(s).to_ascii_lowercase();
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

    // Docker writes the server on stdin and closes it. A registry host is a few
    // bytes, far under a pipe's buffer, so it is written before the spawn and
    // no pump is needed.
    let (reader, mut writer) = std::io::pipe().context("create the helper's stdin pipe")?;
    std::io::Write::write_all(&mut writer, helper.server.as_bytes())
        .context("write the registry to the helper's stdin")?;
    drop(writer);

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
        stdin: hproc::proc_exec::StdioSpec::Fd(reader.into()),
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
        String::from_utf8_lossy(&out.stderr).trim()
    );
    parse_helper_answer(&out.stdout).with_context(|| {
        format!(
            "read {exe}'s answer for {} (credential {addr})",
            helper.server
        )
    })
}

fn find_executable(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| {
        use std::os::unix::fs::PermissionsExt as _;
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// The Docker credential-helper answer: `{"ServerURL", "Username", "Secret"}`.
///
/// `Username` of `<token>` is the protocol's marker for an identity token,
/// presented as a bearer token — the same reading the ambient path gives it.
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
    Ok(if a.username == "<token>" {
        RegistryAuth::Bearer(a.secret)
    } else {
        RegistryAuth::Basic(a.username, a.secret)
    })
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

    async fn resolve(mounts: &[CredentialMount], registry: &str) -> anyhow::Result<RegistryAuth> {
        let cwd = tempfile::tempdir().expect("tempdir");
        let ctoken = StdCancellationToken::new();
        RegistryCredentials::for_run(mounts, cwd.path(), &ctoken)
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

    /// `<token>` is the protocol's identity-token marker.
    #[tokio::test]
    async fn a_token_username_is_a_bearer_token() {
        let p = Presented::new(
            "//auth:t",
            &["reg.example"],
            r#"{"ServerURL":"reg.example","Username":"<token>","Secret":"tok-material"}"#,
        );
        let auth = resolve(std::slice::from_ref(&p.mount), "reg.example")
            .await
            .expect("resolve");
        assert_eq!(auth, RegistryAuth::Bearer("tok-material".into()));
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

    #[test]
    fn no_mounts_is_the_ambient_config() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let ctoken = StdCancellationToken::new();
        assert!(matches!(
            RegistryCredentials::for_run(&[], cwd.path(), &ctoken),
            RegistryCredentials::Ambient
        ));
    }
}
