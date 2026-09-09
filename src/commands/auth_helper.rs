//! `heph __auth-helper` — one hidden subcommand wearing five protocol hats.
//!
//! A tool that supports a *credential helper* asks for a credential whenever it
//! needs one, instead of being handed a snapshot at spawn. That is the only
//! presentation that survives an expiry mid-run, and it is why it is the preferred
//! one: a three-hour `terraform apply` under a one-hour token simply works.
//!
//! Each hat is a **third-party grammar heph must produce byte-exactly**, and a
//! wrong field name is a silent failure inside somebody else's SDK. They are not
//! internal interfaces that can be adjusted later, so each one has a pinned
//! conformance test below rather than a best effort.
//!
//! # Why it is parsed before the argument parser
//!
//! Its argv and its stdin belong to the *calling tool*. Docker appends `get`,
//! git appends `get`/`store`/`erase`, and both write a request on stdin — none of
//! which is heph's CLI grammar. Parsing it as flags would misread the caller's
//! own protocol.

use anyhow::Context as _;
use std::io::Read as _;

/// A parsed `__auth-helper` invocation.
pub struct Invocation {
    pub dialect: String,
    /// The pin the host wrote beside the presented files. It carries the material,
    /// the workspace, and which source the host chose.
    pub pin: std::path::PathBuf,
    /// Whatever the calling tool appended — `get`, `store`, `erase`.
    pub operation: Option<String>,
}

/// Recognize `heph __auth-helper <dialect> --pin <path> [op]`.
///
/// A pure prefix match on `args_os`, like the exec-runner subcommands: nothing
/// here may pull in logging, a runtime, clap or the TUI, because this process is
/// a callback inside somebody else's tool and must stay small and predictable.
pub fn parse(args: impl IntoIterator<Item = std::ffi::OsString>) -> Option<Invocation> {
    let mut it = args.into_iter().skip(1);
    if it.next()?.to_str()? != crate::engine::credential::AUTH_HELPER_SUBCOMMAND {
        return None;
    }
    let dialect = it.next()?.to_str()?.to_string();
    let mut pin = None;
    let mut operation = None;
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--pin") => pin = it.next().map(std::path::PathBuf::from),
            Some(other) if !other.starts_with("--") => operation = Some(other.to_string()),
            _ => {}
        }
    }
    Some(Invocation {
        dialect,
        pin: pin?,
        operation,
    })
}

/// Answer the calling tool, then exit.
///
/// Never returns: a helper process does one thing.
pub fn run(inv: Invocation) -> ! {
    let code = match answer(&inv) {
        Ok(out) => {
            print!("{out}");
            0
        }
        Err(e) => {
            // stderr, always: stdout is the protocol. Every one of these dialects
            // reads stdout as a document, so a diagnostic there is a parse error
            // inside the calling tool rather than a message anyone sees.
            eprintln!("heph auth helper ({}): {e:#}", inv.dialect);
            1
        }
    };
    std::process::exit(code)
}

fn answer(inv: &Invocation) -> anyhow::Result<String> {
    // `store` and `erase` are the write half of the Docker and git protocols.
    // heph's credential store is not a place a tool may write to — the material
    // comes from a declared chain, and accepting a write would let a tool
    // silently substitute an identity — so they succeed and do nothing, which is
    // what the protocols specify for a read-only helper.
    if matches!(inv.operation.as_deref(), Some("store" | "erase")) {
        return Ok(String::new());
    }

    let material = material_for(inv)?;
    match inv.dialect.as_str() {
        "aws" => aws(&material),
        "gcp" => gcp(&material),
        "docker" => docker(&material, &read_stdin()?),
        "git" => git(&material, &read_stdin()?),
        "kubernetes" => kubernetes(&material),
        other => anyhow::bail!("unknown credential helper dialect {other:?}"),
    }
}

/// The material to answer with.
///
/// **The common path reads the pin and stops.** No engine, no workspace parse, no
/// probe, and — critically — no dependence on this process's environment, which
/// is the target's sandbox rather than the host's. A tool re-invokes its helper
/// on every refresh, every fetch, every registry operation; each of those has to
/// be a file read.
///
/// Only when the pinned material has actually lapsed does this reach for the
/// engine, and then it re-acquires from **the source the host chose** and no
/// other — see `Engine::refresh_credential_source`.
fn material_for(inv: &Invocation) -> anyhow::Result<crate::engine::Material> {
    let raw = std::fs::read(&inv.pin).with_context(|| {
        format!(
            "read the credential pin at {} — a helper is only ever invoked from a config heph \
             wrote, and that file lives inside the run's sandbox, so this usually means the run \
             has already finished",
            inv.pin.display()
        )
    })?;
    let pin: crate::engine::credential::HelperPin =
        serde_json::from_slice(&raw).context("parse the credential pin")?;

    let acquired_at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(pin.acquired_at);
    if pin
        .material
        .usable_at(std::time::SystemTime::now(), acquired_at)
    {
        return Ok(pin.material);
    }

    // The sandbox has its own cwd, so the workspace is named by the pin.
    std::env::set_current_dir(&pin.root)
        .with_context(|| format!("enter workspace {}", pin.root.display()))?;
    crate::commands::bootstrap::block_on(refresh(&pin))?
}

async fn refresh(
    pin: &crate::engine::credential::HelperPin,
) -> anyhow::Result<crate::engine::Material> {
    let (engine, _shutdown) = crate::commands::bootstrap::new_engine()?;
    let rs = engine.new_state();
    let addr = hmodel::htaddr::parse_addr(&pin.addr)?;
    engine
        .refresh_credential_source(&rs, &addr, pin.source_index)
        .await
}

fn read_stdin() -> anyhow::Result<String> {
    let mut s = String::new();
    std::io::stdin()
        .read_to_string(&mut s)
        .context("read the calling tool's request from stdin")?;
    Ok(s)
}

// ---------------------------------------------------------------------------
// The dialects
// ---------------------------------------------------------------------------

/// The first field present under any of `names`.
///
/// Vendors disagree about spelling for the same value — `AccessKeyId` from the
/// AWS CLI, `access_key_id` from a `fields` map an author wrote — and a
/// presentation should not have to normalize by hand.
fn field<'a>(m: &'a crate::engine::Material, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|n| m.fields.get(*n))
        .map(String::as_str)
}

fn require<'a>(
    m: &'a crate::engine::Material,
    names: &[&str],
    what: &str,
) -> anyhow::Result<&'a str> {
    field(m, names).ok_or_else(|| {
        anyhow::anyhow!(
            "this credential has no {what} — the {} dialect needs a field named one of {}; this \
             source yields {}",
            what,
            names.join(", "),
            if m.fields.is_empty() {
                "nothing".to_string()
            } else {
                m.fields.keys().cloned().collect::<Vec<_>>().join(", ")
            }
        )
    })
}

fn rfc3339(secs: u64) -> Option<String> {
    chrono::DateTime::from_timestamp(secs as i64, 0).map(|d| d.to_rfc3339())
}

/// The AWS credential-process contract.
///
/// `Version` must be `1` — the SDKs reject anything else outright — and
/// `Expiration` is RFC 3339, which is what the SDK re-invokes against.
fn aws(m: &crate::engine::Material) -> anyhow::Result<String> {
    let mut doc = serde_json::json!({
        "Version": 1,
        "AccessKeyId": require(m, &["access_key_id", "AccessKeyId"], "access key id")?,
        "SecretAccessKey": require(
            m,
            &["secret_access_key", "SecretAccessKey"],
            "secret access key",
        )?,
    });
    if let Some(t) = field(m, &["session_token", "SessionToken"])
        && let Some(o) = doc.as_object_mut()
    {
        o.insert(
            "SessionToken".to_string(),
            serde_json::Value::String(t.to_string()),
        );
    }
    if let Some(at) = m.expires_at.and_then(rfc3339)
        && let Some(o) = doc.as_object_mut()
    {
        o.insert("Expiration".to_string(), serde_json::Value::String(at));
    }
    Ok(format!("{doc}\n"))
}

/// GCP executable-sourced external-account credentials.
///
/// `success` is a field rather than an exit status, and `expiration_time` is a
/// Unix timestamp. A subject token is a JWT, which is what an OIDC source yields.
fn gcp(m: &crate::engine::Material) -> anyhow::Result<String> {
    let token = require(m, &["id_token", "token", "value"], "token")?;
    let mut doc = serde_json::json!({
        "version": 1,
        "success": true,
        "token_type": "urn:ietf:params:oauth:token-type:jwt",
        "id_token": token,
    });
    if let Some(at) = m.expires_at
        && let Some(o) = doc.as_object_mut()
    {
        o.insert("expiration_time".to_string(), serde_json::json!(at));
    }
    Ok(format!("{doc}\n"))
}

/// The Docker credential-helper protocol.
///
/// The bare registry URL arrives on stdin; the answer is three keys. `Username`
/// is the literal `<token>` when the secret is an identity token rather than a
/// password — that convention is what makes a registry accept a bearer token
/// through a protocol that only speaks username/password.
///
/// The protocol carries no expiry field, so the tool re-asks only on its next
/// registry interaction.
fn docker(m: &crate::engine::Material, stdin: &str) -> anyhow::Result<String> {
    let server = stdin.trim();
    let secret = require(m, &["token", "password", "secret", "value"], "token")?;
    let username = field(m, &["username", "user"]).unwrap_or("<token>");
    let doc = serde_json::json!({
        "ServerURL": server,
        "Username": username,
        "Secret": secret,
    });
    Ok(format!("{doc}\n"))
}

/// The git credential-helper protocol.
///
/// `key=value` lines terminated by a blank line, in both directions.
/// `password_expiry_utc` is a Unix timestamp git honours by re-asking once it
/// passes — which is what makes this dialect survive an expiry mid-fetch.
fn git(m: &crate::engine::Material, stdin: &str) -> anyhow::Result<String> {
    let password = require(m, &["token", "password", "value"], "token")?;
    // Not `<token>` here: git sends the username as basic-auth, and GitHub (the
    // dominant case) accepts any non-empty username with a token as the password.
    let username = field(m, &["username", "user"]).unwrap_or("x-access-token");
    let mut out = String::new();
    // Echo back the protocol/host git asked about. Git tolerates their absence,
    // but returning them is what lets a helper be scoped to one host in a config
    // that also has others.
    for line in stdin.lines() {
        if let Some((k, _)) = line.split_once('=')
            && matches!(k, "protocol" | "host")
        {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(&format!("username={username}\n"));
    out.push_str(&format!("password={password}\n"));
    if let Some(at) = m.expires_at {
        out.push_str(&format!("password_expiry_utc={at}\n"));
    }
    out.push('\n');
    Ok(out)
}

/// The client-go `ExecCredential` object.
///
/// Pure format: heph knows nothing about Kubernetes beyond this shape and the
/// kubeconfig stanza that names it, which is why it costs the engine almost
/// nothing to speak. kubectl caches the token until `expirationTimestamp` and
/// then calls again.
fn kubernetes(m: &crate::engine::Material) -> anyhow::Result<String> {
    let token = require(m, &["token", "id_token", "value"], "token")?;
    let mut status = serde_json::json!({ "token": token });
    if let Some(at) = m.expires_at.and_then(rfc3339)
        && let Some(o) = status.as_object_mut()
    {
        o.insert(
            "expirationTimestamp".to_string(),
            serde_json::Value::String(at),
        );
    }
    let doc = serde_json::json!({
        "apiVersion": "client.authentication.k8s.io/v1beta1",
        "kind": "ExecCredential",
        "status": status,
    });
    Ok(format!("{doc}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Material;
    use std::collections::BTreeMap;

    fn material(pairs: &[(&str, &str)], expires_at: Option<u64>) -> Material {
        Material {
            fields: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            files: BTreeMap::new(),
            expires_at,
        }
    }

    fn json(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("valid json")
    }

    #[test]
    fn the_helper_is_recognized_before_the_argument_parser() {
        let inv = parse(
            [
                "heph",
                "__auth-helper",
                "aws",
                "--pin",
                "/s/.heph/auth/a/pin.json",
            ]
            .into_iter()
            .map(std::ffi::OsString::from),
        )
        .expect("parsed");
        assert_eq!(inv.dialect, "aws");
        assert_eq!(inv.pin, std::path::Path::new("/s/.heph/auth/a/pin.json"));
        assert_eq!(inv.operation, None);
        // An ordinary command is not one.
        assert!(
            parse(
                ["heph", "run", "//a:b"]
                    .into_iter()
                    .map(std::ffi::OsString::from)
            )
            .is_none()
        );
    }

    #[test]
    fn a_trailing_operation_from_the_calling_tool_is_captured() {
        let inv = parse(
            ["heph", "__auth-helper", "docker", "--pin", "/p", "get"]
                .into_iter()
                .map(std::ffi::OsString::from),
        )
        .expect("parsed");
        assert_eq!(inv.operation.as_deref(), Some("get"));
    }

    /// AWS rejects anything but `Version: 1`, and re-invokes against
    /// `Expiration`.
    #[test]
    fn aws_speaks_the_credential_process_contract() {
        let m = material(
            &[
                ("access_key_id", "AKIA"),
                ("secret_access_key", "sec"),
                ("session_token", "tok"),
            ],
            Some(1_893_456_000),
        );
        let doc = json(&aws(&m).expect("render"));
        assert_eq!(doc["Version"], 1);
        assert_eq!(doc["AccessKeyId"], "AKIA");
        assert_eq!(doc["SecretAccessKey"], "sec");
        assert_eq!(doc["SessionToken"], "tok");
        assert_eq!(doc["Expiration"], "2030-01-01T00:00:00+00:00");
    }

    #[test]
    fn aws_accepts_either_spelling_of_a_field() {
        // The AWS CLI prints `AccessKeyId`; an author's own `fields` map is more
        // likely to say `access_key_id`. Both must work.
        let m = material(&[("AccessKeyId", "A"), ("SecretAccessKey", "S")], None);
        let doc = json(&aws(&m).expect("render"));
        assert_eq!(doc["AccessKeyId"], "A");
        // No session token and no expiry: a long-lived key pair is a legal
        // credential-process answer, and inventing an `Expiration` would make the
        // SDK re-invoke forever.
        assert!(doc.get("SessionToken").is_none());
        assert!(doc.get("Expiration").is_none());
    }

    #[test]
    fn aws_names_what_is_missing_rather_than_emitting_a_half_document() {
        let err = aws(&material(&[("token", "t")], None)).expect_err("must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("access key id"), "{msg}");
        assert!(
            msg.contains("token"),
            "must name what the source did yield: {msg}"
        );
    }

    #[test]
    fn gcp_speaks_the_executable_sourced_contract() {
        let doc =
            json(&gcp(&material(&[("id_token", "jwt")], Some(1_893_456_000))).expect("render"));
        assert_eq!(doc["version"], 1);
        assert_eq!(doc["success"], true);
        assert_eq!(doc["token_type"], "urn:ietf:params:oauth:token-type:jwt");
        assert_eq!(doc["id_token"], "jwt");
        // A Unix timestamp here, unlike AWS's RFC 3339 — the two protocols
        // genuinely differ and getting it wrong fails inside the SDK.
        assert_eq!(doc["expiration_time"], 1_893_456_000u64);
    }

    #[test]
    fn docker_answers_the_registry_it_was_asked_about() {
        let out = docker(&material(&[("token", "t")], None), "ghcr.io\n").expect("render");
        let doc = json(&out);
        assert_eq!(doc["ServerURL"], "ghcr.io");
        assert_eq!(doc["Secret"], "t");
        // The identity-token convention: a registry accepts a bearer token
        // through a username/password protocol only under this literal username.
        assert_eq!(doc["Username"], "<token>");
    }

    #[test]
    fn git_frames_key_values_and_echoes_the_hosts_it_was_asked_about() {
        let out = git(
            &material(&[("token", "t")], Some(1_893_456_000)),
            "protocol=https\nhost=github.com\n\n",
        )
        .expect("render");
        assert_eq!(
            out,
            "protocol=https\nhost=github.com\nusername=x-access-token\npassword=t\npassword_expiry_utc=1893456000\n\n"
        );
    }

    #[test]
    fn git_output_always_ends_with_the_blank_line_that_terminates_it() {
        let out = git(&material(&[("token", "t")], None), "").expect("render");
        assert!(out.ends_with("\n\n"), "{out:?}");
    }

    #[test]
    fn kubernetes_speaks_the_exec_credential_object() {
        let doc =
            json(&kubernetes(&material(&[("token", "t")], Some(1_893_456_000))).expect("render"));
        assert_eq!(doc["apiVersion"], "client.authentication.k8s.io/v1beta1");
        assert_eq!(doc["kind"], "ExecCredential");
        assert_eq!(doc["status"]["token"], "t");
        assert_eq!(
            doc["status"]["expirationTimestamp"],
            "2030-01-01T00:00:00+00:00"
        );
    }

    /// A tool asking heph to *write* a credential must not be able to substitute
    /// an identity. Both protocols specify a no-op for a read-only helper.
    #[test]
    fn store_and_erase_succeed_and_change_nothing() {
        for op in ["store", "erase"] {
            let inv = Invocation {
                dialect: "git".to_string(),
                pin: std::path::PathBuf::from("/nonexistent"),
                operation: Some(op.to_string()),
            };
            // Returns before touching the pin, which is why a bogus path is
            // harmless here.
            assert_eq!(answer(&inv).expect("no-op"), "");
        }
    }
}
