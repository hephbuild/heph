#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction lint scoped to production code; tests are exempt"
)]

//! A target actually receiving a credential, end to end through the real engine.
//!
//! `secret.rs` covers the cache-key contract and `secret_examples.rs` covers the
//! declarations; this covers the part a user experiences — a command that reads
//! a credential and succeeds, and, just as importantly, a build that leaves
//! nothing behind when it does not.

mod common;

use common::Workspace;

/// The whole feature in one target: declare a credential, name it, read it.
#[tokio::test]
async fn a_target_reads_a_credential_from_its_file() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"target(name = "tok", driver = "secret", provider = "exec", protocol = "raw",
       helper = ["/bin/echo", "the_secret_value"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "use", driver = "bash", out = "o.txt",
       secrets = {"tok": "//creds:tok"},
       run = ["cat $SECRET_TOK > o.txt"])"#,
    );

    let out = common::artifact_string(&*ws.run("//app:use").await?);
    assert_eq!(out.trim(), "the_secret_value");
    Ok(())
}

/// The value must not reach anything durable — the log above all, which is
/// packed into the local cache and pushed to the *shared remote* one.
///
/// A target that prints its credential is exactly the accident redaction exists
/// for, so this prints one and then looks for it everywhere heph wrote.
#[tokio::test]
async fn a_printed_credential_is_masked_everywhere_durable() -> anyhow::Result<()> {
    let ws = Workspace::new();
    // Assembled at run time, so the value appears nowhere in the declaration and
    // a hit is a real leak rather than the BUILD file matching itself.
    ws.write_build_file(
        "creds",
        r#"target(name = "tok", driver = "secret", provider = "exec", protocol = "raw",
       helper = ["/bin/sh", "-c", "printf 'leaky_%s_value' secret"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "loud", driver = "bash", out = "o.txt",
       secrets = {"tok": "//creds:tok"},
       run = ["cat $SECRET_TOK", "echo done > o.txt"])"#,
    );

    ws.run("//app:loud").await?;

    let (mut leaked, mut masked) = (Vec::new(), false);
    for entry in walk(common::root(&ws)) {
        let Ok(body) = std::fs::read(&entry) else {
            continue;
        };
        let text = String::from_utf8_lossy(&body);
        if text.contains("leaky_secret_value") {
            leaked.push(entry);
        } else if text.contains("«redacted:tok»") {
            masked = true;
        }
    }

    assert!(
        leaked.is_empty(),
        "a credential reached something durable: {leaked:?}"
    );
    // Without this the test would pass just as happily if the target had never
    // printed anything at all.
    assert!(
        masked,
        "the value was never printed, so the absence of a leak proves nothing"
    );
    Ok(())
}

/// The `env` shape is an explicit opt-in to putting a value in the environment.
#[tokio::test]
async fn an_env_shaped_credential_reaches_the_command() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"target(name = "gh", driver = "secret", shape = ["env"],
       env = {"GH_TOKEN": "$."},
       provider = "exec", protocol = "raw",
       helper = ["/bin/echo", "ghs_env_shaped_value"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "use", driver = "bash", out = "o.txt",
       secrets = {"gh": "//creds:gh"},
       run = ["printf '%s' \"$GH_TOKEN\" > o.txt"])"#,
    );

    let out = common::artifact_string(&*ws.run("//app:use").await?);
    assert_eq!(out, "ghs_env_shaped_value");
    Ok(())
}

/// A well-known shape renders a real file into a synthetic `$HOME`, and the
/// host's own `HOME` is never passed through.
#[tokio::test]
async fn a_netrc_shape_renders_into_a_synthetic_home() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"target(name = "gh", driver = "secret", shape = ["netrc"],
       machine = "github.com",
       provider = "exec", protocol = "raw",
       helper = ["/bin/echo", "ghs_netrc_value"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "use", driver = "bash", out = "o.txt",
       secrets = {"gh": "//creds:gh"},
       run = ["cat \"$HOME/.netrc\" > o.txt"])"#,
    );

    let out = common::artifact_string(&*ws.run("//app:use").await?);
    assert!(out.contains("machine github.com"), "{out}");
    assert!(out.contains("ghs_netrc_value"), "{out}");
    Ok(())
}

/// **A failing target's sandbox is kept as the diagnostic**, so the credentials
/// in it have to go. Without the scrub a failed build is how credentials end up
/// on disk — and a crash leaves them there for good.
#[tokio::test]
async fn a_failed_target_leaves_no_credential_on_disk() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"target(name = "tok", driver = "secret", shape = ["file", "netrc"],
       machine = "example.com",
       provider = "exec", protocol = "raw",
       helper = ["/bin/sh", "-c", "printf 'must_not_%s_a_failure' survive"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "boom", driver = "bash", out = "o.txt",
       secrets = {"tok": "//creds:tok"},
       run = ["cat $SECRET_TOK > /dev/null", "exit 3"])"#,
    );

    assert!(ws.run("//app:boom").await.is_err(), "the target must fail");

    // Walk the whole engine home: the sandbox is deliberately retained, so if
    // the scrub did not run the value is sitting in it. The helper assembles the
    // string at run time so that it appears nowhere in the BUILD file — which
    // would otherwise be a false positive against the declaration itself.
    let mut found = Vec::new();
    let root = common::root(&ws);
    for entry in walk(root) {
        let Ok(body) = std::fs::read(&entry) else {
            continue;
        };
        if String::from_utf8_lossy(&body).contains("must_not_survive_a_failure") {
            found.push(entry);
        }
    }
    assert!(
        found.is_empty(),
        "a credential survived a failed target: {found:?}"
    );
    Ok(())
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(t) if t.is_file() => out.push(p),
                _ => {}
            }
        }
    }
    out
}

/// Two credentials fighting over one file fail from declarations alone, before
/// anything is minted — which is what keeps the check alive on a warm build.
#[tokio::test]
async fn a_slot_collision_fails_before_anything_is_minted() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"
target(name = "a", driver = "secret", shape = ["aws_profile"],
       provider = "exec", protocol = "raw", helper = ["/bin/false"])
target(name = "b", driver = "secret", shape = ["aws_profile"],
       provider = "exec", protocol = "raw", helper = ["/bin/false"])
"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "clash", driver = "bash", out = "o.txt",
       secrets = {"a": "//creds:a", "b": "//creds:b"},
       run = ["echo hi > o.txt"])"#,
    );

    let err = match ws.run("//app:clash").await {
        Ok(_) => panic!("a slot collision must fail the build"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("//creds:a"), "{err}");
    assert!(err.contains("//creds:b"), "{err}");
    // `/bin/false` would have failed had anything tried to mint, and the error
    // would say so instead.
    assert!(err.contains("profile"), "{err}");
    Ok(())
}

/// Naming a target that is not a `secret` is a BUILD mistake that would
/// otherwise surface as a credential that never arrives.
#[tokio::test]
async fn naming_a_non_secret_target_says_so() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"
target(name = "notasecret", driver = "bash", out = "o.txt", run = ["echo hi > o.txt"])
target(name = "use", driver = "bash", out = "o.txt",
       secrets = {"x": "//app:notasecret"},
       run = ["echo hi > o.txt"])
"#,
    );

    let err = match ws.run("//app:use").await {
        Ok(_) => panic!("must reject"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("is a `bash` target"), "{err}");
    assert!(err.contains("Did you mean"), "{err}");
    Ok(())
}

/// A warm build must not touch a provider at all: a cache hit skips `run`, and
/// the mint lives inside it.
#[tokio::test]
async fn a_cache_hit_mints_nothing() -> anyhow::Result<()> {
    let ws = Workspace::new();
    // A helper that works once and then refuses: a second mint fails the build.
    let marker = common::root(&ws).join("minted");
    ws.write_build_file(
        "creds",
        &format!(
            r#"target(name = "tok", driver = "secret", provider = "exec", protocol = "raw",
       helper = ["/bin/sh", "-c",
                 "if [ -e {m} ]; then echo minted twice >&2; exit 9; fi; touch {m}; echo once_only_value"])"#,
            m = marker.display()
        ),
    );
    ws.write_build_file(
        "app",
        r#"target(name = "use", driver = "bash", out = "o.txt",
       secrets = {"tok": "//creds:tok"},
       run = ["cat $SECRET_TOK > o.txt"])"#,
    );

    let first = common::artifact_string(&*ws.run("//app:use").await?);
    assert_eq!(first.trim(), "once_only_value");
    assert!(marker.exists(), "the first run should have minted");

    // Second run, same engine: a hit, so `run` never happens and neither does a
    // mint. If it did, the helper exits 9 and this fails.
    let second = common::artifact_string(&*ws.run("//app:use").await?);
    assert_eq!(second.trim(), "once_only_value");
    Ok(())
}
