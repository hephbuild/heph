//! Turning a minted credential into files a tool will actually find.
//!
//! A shape declares *where* it writes ([`crate::shape`]); this writes it. The
//! two are separate because the slot keys have to be known from declarations
//! alone — that is what lets a collision fail before anything is minted — while
//! the contents only exist after.
//!
//! # Everything lands outside `ws/`
//!
//! Two directories, both siblings of the target's workspace rather than inside
//! it, so an `out = ["**"]` can never sweep a credential into an artifact:
//!
//! - `<sandbox>/secrets/` — one 0600 file per `file`-shaped credential, its path
//!   in `$SECRET_<NAME>`.
//! - `<sandbox>/home/` — the **synthetic `$HOME`**, holding the well-known files
//!   (`.netrc`, `.aws/credentials`, `.docker/config.json`, …) and nothing else.
//!
//! The synthetic home is not a convenience. A great many tools look *only* at
//! `$HOME` and cannot be redirected by any variable — `git` reads
//! `$HOME/.netrc` through libcurl and honours no `NETRC` override, so a pointer
//! variable alone would leave every private clone unauthenticated. Pointing
//! `HOME` at a directory heph owns gets that without giving anything back: it
//! contains exactly what the declared shapes rendered, it is cleaned with the
//! sandbox, and the host's real `HOME` is still never passed through.
//!
//! # Merging is by slot, and the keys were checked already
//!
//! Two credentials both rendering `netrc` write into one file. Distinct machines
//! merge; the same machine with different values was rejected at spec time, so
//! by the time anything reaches here the set is known to be coherent and a
//! renderer can append without arbitrating.

use crate::shape::Shape;
use crate::value::{Credential, SecretValue};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where a credential's files go, relative to the sandbox.
pub const SECRETS_DIR: &str = "secrets";
/// The synthetic `$HOME`, relative to the sandbox.
pub const HOME_DIR: &str = "home";

/// One credential to render: its consumer-facing name, what it is, and its value.
pub struct Rendering<'a> {
    pub name: &'a str,
    pub identity: &'a crate::descriptor::Identity,
    pub cred: &'a Credential,
}

/// What a render produced: files on disk, and the environment that points at them.
#[derive(Debug, Default)]
pub struct Rendered {
    /// Variables to set for the target's command.
    pub env: BTreeMap<String, String>,
    /// Every file written, for the scrubber. Absolute paths.
    pub files: Vec<PathBuf>,
    /// `(name, value)` for every field of every credential rendered, for the
    /// redactor.
    ///
    /// Every field, not just the primary: a `credential_process` credential has
    /// three, and a tool that echoes the session token has leaked just as
    /// surely as one that echoes the access key.
    pub values: Vec<(String, String)>,
}

/// Accumulates the well-known files before any of them is written.
///
/// Buffered rather than written as it goes, because several credentials
/// contribute to one file and a half-written `.netrc` after a mid-loop failure
/// is worse than none: a tool would read it and authenticate as whoever
/// happened to be first.
#[derive(Default)]
struct Files {
    netrc: Vec<String>,
    git_credentials: Vec<String>,
    git_config: Vec<(String, String)>,
    docker_auths: BTreeMap<String, String>,
    aws_credentials: Vec<String>,
    aws_config: Vec<String>,
    gcloud_adc: Option<String>,
    env: BTreeMap<String, String>,
}

fn field<'a>(cred: &'a Credential, name: &str) -> Option<&'a SecretValue> {
    cred.get(name)
}

fn primary(cred: &Credential) -> anyhow::Result<&SecretValue> {
    cred.resolve_pointer("$.")
}

/// Render every credential a target holds into `sandbox_dir`.
///
/// Returns the environment the target's command needs. Files are written 0600
/// and their directories 0700; the caller is responsible for scrubbing them, and
/// [`Rendered::files`] is what it scrubs.
pub fn render_all(sandbox_dir: &Path, items: &[Rendering<'_>]) -> anyhow::Result<Rendered> {
    let mut files = Files::default();
    let secrets_dir = sandbox_dir.join(SECRETS_DIR);
    let home = sandbox_dir.join(HOME_DIR);
    let mut out = Rendered::default();

    for item in items {
        for value in item.cred.values() {
            out.values
                .push((item.name.to_string(), value.expose().to_string()));
        }
        for shape_name in &item.identity.shape {
            let shape = Shape::parse(shape_name)?;
            render_one(shape, item, &secrets_dir, &mut files, &mut out)?;
        }
    }

    // Nothing was written until here, so a failure above leaves no partial
    // credential file behind for a tool to half-read.
    write_files(&home, &files, &mut out)?;
    out.env.extend(files.env);
    Ok(out)
}

fn render_one(
    shape: Shape,
    item: &Rendering<'_>,
    secrets_dir: &Path,
    files: &mut Files,
    out: &mut Rendered,
) -> anyhow::Result<()> {
    let id = item.identity;
    match shape {
        Shape::File => {
            // The default, and the only shape with a per-secret path: the value
            // goes in its own 0600 file and the command is handed the path.
            std::fs::create_dir_all(secrets_dir)?;
            let path = secrets_dir.join(item.name);
            write_secret_file(&path, primary(item.cred)?.expose().as_bytes())?;
            out.files.push(path.clone());
            out.env.insert(
                hdriver_support::secret::default_env_name(item.name),
                path.to_string_lossy().into_owned(),
            );
        }
        Shape::Env => {
            // The one shape that puts a *value* in the environment. An explicit,
            // per-secret opt-in to the exposure — see `Shape::leaks_via_argv`.
            for (var, pointer) in &id.env {
                let v = item.cred.resolve_pointer(pointer)?;
                files.env.insert(var.clone(), v.expose().to_string());
            }
        }
        Shape::Netrc => {
            let machine = need(&id.machine, "machine", shape)?;
            let login = field(item.cred, "Username")
                .map(|u| u.expose().to_string())
                .unwrap_or_else(|| "x-access-token".to_string());
            files.netrc.push(format!(
                "machine {machine} login {login} password {}",
                primary(item.cred)?.expose()
            ));
            // `cmd/go` has honoured NETRC since 1.13; `GOAUTH=netrc` since 1.24.
            // git is *not* covered here — see `git_credential`.
            files.env.insert("GOAUTH".to_string(), "netrc".to_string());
        }
        Shape::GitCredential => {
            let machine = need(&id.machine, "machine", shape)?;
            let login = field(item.cred, "Username")
                .map(|u| u.expose().to_string())
                .unwrap_or_else(|| "x-access-token".to_string());
            files.git_credentials.push(format!(
                "https://{login}:{}@{machine}",
                primary(item.cred)?.expose()
            ));
            // Configured through `GIT_CONFIG_*` rather than a file: git delegates
            // netrc entirely to libcurl, which only learned the `NETRC` variable
            // in curl 8.16.0 (Sept 2025), so that route depends on the host's
            // libcurl and is unusable as a contract. `GIT_CONFIG_COUNT` is
            // documented, stable since git 2.31, and needs no home directory.
            files.git_config.push((
                format!("credential.https://{machine}.helper"),
                String::new(), // filled in once the path is known
            ));
        }
        Shape::DockerConfig => {
            let registry = need(&id.registry, "registry", shape)?;
            let user = field(item.cred, "Username")
                .map(|u| u.expose().to_string())
                .unwrap_or_else(|| "oauth2accesstoken".to_string());
            let blob = base64_std(format!("{user}:{}", primary(item.cred)?.expose()).as_bytes());
            files.docker_auths.insert(registry, blob);
        }
        Shape::AwsProfile => {
            let profile = id.profile.clone().unwrap_or_else(|| "default".to_string());
            let mut creds = format!("[{profile}]\n");
            for (key, field_name) in [
                ("aws_access_key_id", "AccessKeyId"),
                ("aws_secret_access_key", "SecretAccessKey"),
                ("aws_session_token", "SessionToken"),
            ] {
                // Accept both the protocol's spelling and the descriptor's, so a
                // `static_env` naming `aws_access_key_id` and a
                // `credential_process` returning `AccessKeyId` render the same.
                if let Some(v) = field(item.cred, field_name).or_else(|| field(item.cred, key)) {
                    creds.push_str(&format!("{key} = {}\n", v.expose()));
                }
            }
            files.aws_credentials.push(creds);

            // Region and endpoint are *profile keys*, never scalar variables: no
            // single `AWS_REGION`-shaped variable satisfies boto3, the JS SDK and
            // the Java SDK at once, and two credentials setting one would collide
            // where two profile sections do not.
            let mut cfg = format!("[profile {profile}]\n");
            if let Some(r) = &id.region {
                cfg.push_str(&format!("region = {r}\n"));
            }
            if let Some(e) = &id.endpoint {
                cfg.push_str(&format!("endpoint_url = {e}\n"));
                // Since the Jan 2025 default-checksum change, uploads without
                // this fail against R2 with `NotImplemented: x-amz-checksum-crc32`.
                cfg.push_str("request_checksum_calculation = when_required\n");
            }
            files.aws_config.push(cfg);
            files.env.insert("AWS_PROFILE".to_string(), profile);
        }
        Shape::GcloudAdc => {
            // A singleton: `GOOGLE_APPLICATION_CREDENTIALS` names one identity,
            // and the collision check already rejected a second.
            let json = match field(item.cred, "adc") {
                // A provider that produced a whole ADC document uses it verbatim.
                Some(doc) => doc.expose().to_string(),
                None => format!(
                    "{{\n  \"type\": \"authorized_user\",\n  \"access_token\": {}\n}}\n",
                    json_string(primary(item.cred)?.expose())
                ),
            };
            files.gcloud_adc = Some(json);
        }
    }
    Ok(())
}

fn need(v: &Option<String>, field: &str, shape: Shape) -> anyhow::Result<String> {
    v.clone().ok_or_else(|| {
        anyhow::anyhow!("shape {shape} needs `{field}` on the descriptor to know what to write")
    })
}

fn json_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

fn base64_std(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn write_files(home: &Path, files: &Files, out: &mut Rendered) -> anyhow::Result<()> {
    let mut wrote_home = false;

    if !files.netrc.is_empty() {
        let mut body = files.netrc.clone();
        // Sorted, so a merged file reads the same twice. Free here: it is never
        // hashed and never an artifact.
        body.sort();
        let p = home.join(".netrc");
        write_secret_file(&p, format!("{}\n", body.join("\n")).as_bytes())?;
        out.files.push(p);
        wrote_home = true;
    }

    if !files.git_credentials.is_empty() {
        let mut body = files.git_credentials.clone();
        body.sort();
        let p = home.join(".git-credentials");
        write_secret_file(&p, format!("{}\n", body.join("\n")).as_bytes())?;
        out.files.push(p.clone());
        wrote_home = true;

        // git ≥ 2.31, no config file needed.
        let mut i = 0usize;
        out.env.insert(
            "GIT_CONFIG_COUNT".to_string(),
            files.git_config.len().to_string(),
        );
        for (key, _) in &files.git_config {
            out.env.insert(format!("GIT_CONFIG_KEY_{i}"), key.clone());
            out.env.insert(
                format!("GIT_CONFIG_VALUE_{i}"),
                format!("store --file={}", p.to_string_lossy()),
            );
            i = i.saturating_add(1);
        }
    }

    if !files.docker_auths.is_empty() {
        let auths = files
            .docker_auths
            .iter()
            .map(|(host, blob)| {
                format!(
                    "    {}: {{ \"auth\": {} }}",
                    json_string(host),
                    json_string(blob)
                )
            })
            .collect::<Vec<_>>()
            .join(",\n");
        let p = home.join(".docker/config.json");
        write_secret_file(
            &p,
            format!("{{\n  \"auths\": {{\n{auths}\n  }}\n}}\n").as_bytes(),
        )?;
        out.files.push(p.clone());
        wrote_home = true;
        // docker and crane take a *directory*; skopeo wants the file. Set both.
        out.env.insert(
            "DOCKER_CONFIG".to_string(),
            home.join(".docker").to_string_lossy().into_owned(),
        );
        out.env.insert(
            "REGISTRY_AUTH_FILE".to_string(),
            p.to_string_lossy().into_owned(),
        );
    }

    if !files.aws_credentials.is_empty() {
        let mut creds = files.aws_credentials.clone();
        creds.sort();
        let cp = home.join(".aws/credentials");
        write_secret_file(&cp, creds.join("\n").as_bytes())?;
        out.files.push(cp.clone());

        let mut cfg = files.aws_config.clone();
        cfg.sort();
        let gp = home.join(".aws/config");
        write_secret_file(&gp, cfg.join("\n").as_bytes())?;
        out.files.push(gp.clone());
        wrote_home = true;

        out.env.insert(
            "AWS_SHARED_CREDENTIALS_FILE".to_string(),
            cp.to_string_lossy().into_owned(),
        );
        out.env.insert(
            "AWS_CONFIG_FILE".to_string(),
            gp.to_string_lossy().into_owned(),
        );
    }

    if let Some(adc) = &files.gcloud_adc {
        let p = home.join(".config/gcloud/application_default_credentials.json");
        write_secret_file(&p, adc.as_bytes())?;
        out.files.push(p.clone());
        wrote_home = true;
        let s = p.to_string_lossy().into_owned();
        // The SDK variable. gcloud itself does *not* read it — ADC is documented
        // as "used by Google client libraries" only — so the gcloud-native
        // override goes alongside, or gcloud silently falls back to whatever
        // identity the machine has, or none.
        out.env
            .insert("GOOGLE_APPLICATION_CREDENTIALS".to_string(), s.clone());
        out.env
            .insert("CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE".to_string(), s);
        out.env.insert(
            "CLOUDSDK_CONFIG".to_string(),
            home.join(".config/gcloud").to_string_lossy().into_owned(),
        );
    }

    if wrote_home {
        out.env
            .insert("HOME".to_string(), home.to_string_lossy().into_owned());
    }
    Ok(())
}

/// Write one credential-bearing file: 0700 directories, 0600 file.
fn write_secret_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_mode(parent, 0o700)?;
    }
    let mut f = std::fs::File::create(path)?;
    set_mode(path, 0o600)?;
    f.write_all(contents)?;
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Overwrite and remove every rendered file, leaving a marker in its place.
///
/// **A failing target's sandbox is deliberately retained** — it *is* the
/// diagnostic — so without this a failed run leaves credentials on disk until
/// that target's next run, and a crash or SIGKILL leaves them indefinitely. The
/// marker is what keeps the retained tree self-explanatory: a reader finds a
/// note rather than a missing file and a mystery.
///
/// Best-effort by construction: it runs on the failure path, where there is
/// often nothing useful to report an error to, and a scrub that refused to
/// finish because one file was already gone would leave the rest behind.
pub fn scrub(files: &[PathBuf]) -> usize {
    let mut removed = 0usize;
    for path in files {
        // Truncate before unlinking: the inode may still be open elsewhere, and
        // a zero-length file is a weaker leak than an unlinked-but-readable one.
        //
        // Every step is best-effort and the results are deliberately discarded.
        // This runs where there is often nothing to report to — a failure path,
        // or a teardown — and refusing to continue because one file was already
        // gone would leave the rest of them behind, which is the outcome that
        // actually matters.
        if std::fs::write(path, b"").is_ok() && std::fs::remove_file(path).is_ok() {
            removed = removed.saturating_add(1);
        }
        if std::fs::write(
            path,
            b"heph removed a credential file here before leaving this sandbox behind.\n",
        )
        .is_ok()
        {
            drop(set_mode(path, 0o600));
        }
    }
    removed
}

/// Scrub every credential-bearing directory of a sandbox.
///
/// Takes the sandbox rather than a list of files, and that is deliberate: it
/// catches whatever is actually there, including anything a driver wrote into
/// the synthetic home itself, rather than only what this module remembers
/// writing. It also needs no state carried across the run, which is what lets
/// the failure path call it without threading a value through every layer
/// between the render and the failure.
///
/// Returns how many files were scrubbed.
pub fn scrub_sandbox(sandbox_dir: &Path) -> usize {
    let mut files = Vec::new();
    for dir in [sandbox_dir.join(SECRETS_DIR), sandbox_dir.join(HOME_DIR)] {
        collect_files(&dir, &mut files);
    }
    scrub(&files)
}

/// Depth-first, ignoring anything unreadable: a scrub runs on the failure path,
/// where there is nowhere useful to report an error to, and one unreadable
/// entry must not stop the rest from being cleaned.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect_files(&path, out),
            // Symlinks are not followed: a link out of the sandbox would make a
            // scrub delete something it does not own.
            Ok(t) if t.is_file() => out.push(path),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::Identity;
    use crate::expiry::{Expiry, ExpirySource};
    use std::time::SystemTime;

    fn exp() -> Expiry {
        Expiry {
            at: SystemTime::UNIX_EPOCH,
            source: ExpirySource::Default,
            issued_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn id(shape: &[&str]) -> Identity {
        Identity {
            shape: shape.iter().map(|s| (*s).to_string()).collect(),
            ..Identity::default()
        }
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).expect("read rendered file")
    }

    #[test]
    fn a_file_shape_writes_0600_outside_the_workspace_and_points_at_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cred = Credential::single("ghs_the_token_value", exp());
        let out = render_all(
            dir.path(),
            &[Rendering {
                name: "github",
                identity: &id(&["file"]),
                cred: &cred,
            }],
        )
        .expect("render");

        let path = out.env.get("SECRET_GITHUB").expect("pointer variable");
        assert_eq!(read(Path::new(path)), "ghs_the_token_value");

        // Outside `ws/`, so no output glob can collect it.
        assert!(path.contains("/secrets/"), "{path}");
        assert!(!path.contains("/ws/"), "{path}");

        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "rendered {mode:o}");
    }

    /// Two credentials for different machines share one `.netrc`; the collision
    /// check already rejected two for the same one.
    #[test]
    fn two_netrc_credentials_merge_into_one_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gh = Credential::single("gh_token_value", exp());
        let gl = Credential::single("gl_token_value", exp());
        let out = render_all(
            dir.path(),
            &[
                Rendering {
                    name: "gh",
                    identity: &Identity {
                        machine: Some("github.com".into()),
                        ..id(&["netrc"])
                    },
                    cred: &gh,
                },
                Rendering {
                    name: "gl",
                    identity: &Identity {
                        machine: Some("gitlab.com".into()),
                        ..id(&["netrc"])
                    },
                    cred: &gl,
                },
            ],
        )
        .expect("render");

        let home = out.env.get("HOME").expect("synthetic home");
        let netrc = read(&Path::new(home).join(".netrc"));
        assert!(netrc.contains("machine github.com"), "{netrc}");
        assert!(netrc.contains("machine gitlab.com"), "{netrc}");
        assert_eq!(netrc.lines().count(), 2);
    }

    /// The synthetic `$HOME` is the point: `git` reads `$HOME/.netrc` through
    /// libcurl and honours no override, so a pointer variable alone would leave
    /// every private clone unauthenticated.
    #[test]
    fn home_points_into_the_sandbox_and_not_at_the_host() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cred = Credential::single("a_token_value_here", exp());
        let out = render_all(
            dir.path(),
            &[Rendering {
                name: "gh",
                identity: &Identity {
                    machine: Some("github.com".into()),
                    ..id(&["netrc"])
                },
                cred: &cred,
            }],
        )
        .expect("render");
        let home = out.env.get("HOME").expect("HOME");
        assert!(
            home.starts_with(&dir.path().to_string_lossy().to_string()),
            "{home}"
        );
    }

    /// Region and endpoint are profile keys, not scalar variables: no single
    /// `AWS_REGION`-shaped variable satisfies boto3, the JS SDK and the Java SDK
    /// at once, and two credentials setting one would collide.
    #[test]
    fn aws_region_and_endpoint_are_profile_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut fields = BTreeMap::new();
        fields.insert("AccessKeyId".to_string(), SecretValue::new("ASIAEXAMPLE"));
        fields.insert("SecretAccessKey".to_string(), SecretValue::new("s3cret"));
        fields.insert("SessionToken".to_string(), SecretValue::new("tok"));
        let cred = Credential {
            fields,
            expiry: exp(),
        };

        let out = render_all(
            dir.path(),
            &[Rendering {
                name: "r2",
                identity: &Identity {
                    profile: Some("r2".into()),
                    region: Some("auto".into()),
                    endpoint: Some("https://acct.r2.cloudflarestorage.com".into()),
                    ..id(&["aws_profile"])
                },
                cred: &cred,
            }],
        )
        .expect("render");

        let cfg = read(Path::new(out.env.get("AWS_CONFIG_FILE").expect("config")));
        assert!(cfg.contains("[profile r2]"), "{cfg}");
        assert!(cfg.contains("region = auto"), "{cfg}");
        assert!(cfg.contains("endpoint_url = https://acct"), "{cfg}");
        // The R2 checksum workaround rides with the endpoint.
        assert!(cfg.contains("request_checksum_calculation"), "{cfg}");
        assert!(
            !out.env.contains_key("AWS_REGION"),
            "region leaked to a scalar"
        );
        assert!(!out.env.contains_key("AWS_ENDPOINT_URL"), "endpoint leaked");

        let creds = read(Path::new(
            out.env.get("AWS_SHARED_CREDENTIALS_FILE").expect("creds"),
        ));
        assert!(creds.contains("aws_session_token = tok"), "{creds}");
        assert_eq!(out.env.get("AWS_PROFILE").map(String::as_str), Some("r2"));
    }

    /// gcloud does not read `GOOGLE_APPLICATION_CREDENTIALS`; without the
    /// native override it silently falls back to whatever identity the machine
    /// has, or none.
    #[test]
    fn gcloud_adc_sets_both_the_sdk_and_the_gcloud_variable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cred = Credential::single("ya29.an_access_token", exp());
        let out = render_all(
            dir.path(),
            &[Rendering {
                name: "gar",
                identity: &id(&["gcloud_adc"]),
                cred: &cred,
            }],
        )
        .expect("render");
        assert!(out.env.contains_key("GOOGLE_APPLICATION_CREDENTIALS"));
        assert!(
            out.env
                .contains_key("CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE")
        );
        assert!(out.env.contains_key("CLOUDSDK_CONFIG"));
    }

    /// git is configured through `GIT_CONFIG_*`, not `NETRC`: git delegates
    /// netrc to libcurl, which only learned that variable in curl 8.16.0.
    #[test]
    fn git_credential_wires_a_helper_rather_than_relying_on_netrc() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cred = Credential::single("ghs_token_for_git", exp());
        let out = render_all(
            dir.path(),
            &[Rendering {
                name: "gh",
                identity: &Identity {
                    machine: Some("github.com".into()),
                    ..id(&["git_credential"])
                },
                cred: &cred,
            }],
        )
        .expect("render");

        assert_eq!(
            out.env.get("GIT_CONFIG_COUNT").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            out.env.get("GIT_CONFIG_KEY_0").map(String::as_str),
            Some("credential.https://github.com.helper")
        );
        assert!(
            out.env
                .get("GIT_CONFIG_VALUE_0")
                .is_some_and(|v| v.starts_with("store --file=")),
            "{:?}",
            out.env.get("GIT_CONFIG_VALUE_0")
        );
    }

    #[test]
    fn an_env_shape_resolves_its_pointers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cred = Credential::single("ghs_env_shaped", exp());
        let out = render_all(
            dir.path(),
            &[Rendering {
                name: "gh",
                identity: &Identity {
                    env: BTreeMap::from([("GH_TOKEN".to_string(), "$.token".to_string())]),
                    ..id(&["env"])
                },
                cred: &cred,
            }],
        )
        .expect("render");
        assert_eq!(
            out.env.get("GH_TOKEN").map(String::as_str),
            Some("ghs_env_shaped")
        );
    }

    /// A failing target's sandbox is kept as the diagnostic, so the credentials
    /// in it have to go — and the reader has to be told why the file is gone.
    #[test]
    fn scrub_removes_every_value_and_leaves_a_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cred = Credential::single("ghs_should_not_survive", exp());
        let out = render_all(
            dir.path(),
            &[Rendering {
                name: "gh",
                identity: &Identity {
                    machine: Some("github.com".into()),
                    ..id(&["file", "netrc"])
                },
                cred: &cred,
            }],
        )
        .expect("render");

        assert!(!out.files.is_empty());
        for f in &out.files {
            assert!(read(f).contains("ghs_should_not_survive"), "{f:?}");
        }

        scrub(&out.files);

        for f in &out.files {
            let after = read(f);
            assert!(
                !after.contains("ghs_should_not_survive"),
                "credential survived the scrub in {f:?}: {after}"
            );
            assert!(
                after.contains("heph removed a credential file here"),
                "{after}"
            );
        }
    }

    /// The failure path has no captured file list to work from, so the scrub
    /// works off the sandbox — which also catches anything a driver wrote into
    /// the synthetic home itself.
    #[test]
    fn scrubbing_a_sandbox_finds_files_no_render_recorded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cred = Credential::single("ghs_should_not_survive", exp());
        render_all(
            dir.path(),
            &[Rendering {
                name: "gh",
                identity: &Identity {
                    machine: Some("github.com".into()),
                    ..id(&["file", "netrc"])
                },
                cred: &cred,
            }],
        )
        .expect("render");

        // Something a tool wrote into the synthetic home on its own.
        let stray = dir.path().join(HOME_DIR).join(".config/gh/hosts.yml");
        std::fs::create_dir_all(stray.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &stray,
            "oauth_token: ghs_should_not_survive
",
        )
        .expect("write");

        let n = scrub_sandbox(dir.path());
        assert!(n >= 3, "scrubbed only {n} files");
        assert!(
            !read(&stray).contains("ghs_should_not_survive"),
            "a file the renderer never recorded survived"
        );
    }

    /// A sandbox with no credentials in it is not an error, and costs a
    /// failed `read_dir` rather than a walk.
    #[test]
    fn scrubbing_a_sandbox_without_credentials_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(scrub_sandbox(dir.path()), 0);
    }

    /// A missing `machine` cannot render a netrc entry, and failing before
    /// anything is written is what keeps a half-written file from existing.
    #[test]
    fn a_shape_missing_its_key_writes_nothing_at_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cred = Credential::single("a_token_value_here", exp());
        let err = render_all(
            dir.path(),
            &[Rendering {
                name: "gh",
                identity: &id(&["netrc"]),
                cred: &cred,
            }],
        )
        .expect_err("no machine");
        assert!(err.to_string().contains("`machine`"), "{err}");
        assert!(!dir.path().join(HOME_DIR).join(".netrc").exists());
    }
}
