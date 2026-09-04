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

// ---------------------------------------------------------------- transitive

/// A credential is usually a property of a *dependency*. Whatever pulls a
/// private module needs the credential because of what it depends on — and for
/// generated targets, which nobody authors, a transitive contribution is the
/// only way it can arrive at all.
#[tokio::test]
async fn a_dependency_contributes_a_credential_to_its_consumers() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"target(name = "tok", driver = "secret", provider = "exec", protocol = "raw",
       helper = ["/bin/echo", "inherited_token_value"])"#,
    );
    ws.write_build_file(
        "lib",
        r#"target(name = "lib", driver = "bash", out = "l.txt", run = ["echo lib > l.txt"],
       transitive = {"secrets": {"tok": "//creds:tok"}})"#,
    );
    // Declares no secret of its own; gets one because of what it depends on.
    ws.write_build_file(
        "app",
        r#"target(name = "use", driver = "bash", out = "o.txt",
       deps = ["//lib:lib"],
       run = ["cat $SECRET_TOK > o.txt"])"#,
    );

    let out = common::artifact_string(&*ws.run("//app:use").await?);
    assert!(out.contains("inherited_token_value"), "{out}");
    Ok(())
}

/// An inherited credential reaches the consumer's cache key, exactly like a
/// declared one — otherwise the identity a target built under is not in its key.
#[tokio::test]
async fn an_inherited_credential_reaches_the_consumers_hashin() -> anyhow::Result<()> {
    let hashin_of = async |role: &str| -> anyhow::Result<String> {
        let ws = Workspace::new();
        ws.write_build_file(
            "creds",
            &format!(
                r#"target(name = "tok", driver = "secret", role = "{role}",
       provider = "static_env", var = "TOKEN")"#
            ),
        );
        ws.write_build_file(
            "lib",
            r#"target(name = "lib", driver = "bash", out = "l.txt", run = ["echo lib > l.txt"],
       transitive = {"secrets": {"tok": "//creds:tok"}})"#,
        );
        ws.write_build_file(
            "app",
            r#"target(name = "use", driver = "bash", out = "o.txt",
       deps = ["//lib:lib"], run = ["echo hi > o.txt"])"#,
        );
        ws.hashin("//app:use").await
    };

    assert_ne!(
        hashin_of("arn:aws:iam::4711:role/read").await?,
        hashin_of("arn:aws:iam::4711:role/write").await?,
        "an inherited identity did not reach the consumer's cache key"
    );
    Ok(())
}

/// The consumer's own declaration wins. That is the escape hatch when a
/// dependency's choice is wrong for one consumer — and unlike a slot collision,
/// one of the two parties is written in the target itself.
#[tokio::test]
async fn a_targets_own_declaration_overrides_an_inherited_one() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"
target(name = "inherited", driver = "secret", provider = "exec", protocol = "raw",
       helper = ["/bin/echo", "from_the_dependency"])
target(name = "own", driver = "secret", provider = "exec", protocol = "raw",
       helper = ["/bin/echo", "from_the_target_itself"])
"#,
    );
    ws.write_build_file(
        "lib",
        r#"target(name = "lib", driver = "bash", out = "l.txt", run = ["echo lib > l.txt"],
       transitive = {"secrets": {"tok": "//creds:inherited"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "use", driver = "bash", out = "o.txt",
       deps = ["//lib:lib"],
       secrets = {"tok": "//creds:own"},
       run = ["cat $SECRET_TOK > o.txt"])"#,
    );

    let out = common::artifact_string(&*ws.run("//app:use").await?);
    assert!(out.contains("from_the_target_itself"), "{out}");
    assert!(!out.contains("from_the_dependency"), "{out}");
    Ok(())
}

/// Two dependencies supplying one name from different declarations is worse
/// than a slot collision: the name is what the command references, and neither
/// party appears at the failing target's call site. The chains are what make
/// the message actionable.
#[tokio::test]
async fn two_dependencies_supplying_one_name_fail_with_both_chains() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"
target(name = "a", driver = "secret", provider = "static_env", var = "A")
target(name = "b", driver = "secret", provider = "static_env", var = "B")
"#,
    );
    ws.write_build_file(
        "l1",
        r#"target(name = "l", driver = "bash", out = "l.txt", run = ["echo l > l.txt"],
       transitive = {"secrets": {"tok": "//creds:a"}})"#,
    );
    ws.write_build_file(
        "l2",
        r#"target(name = "l", driver = "bash", out = "l.txt", run = ["echo l > l.txt"],
       transitive = {"secrets": {"tok": "//creds:b"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "use", driver = "bash", out = "o.txt",
       deps = ["//l1:l", "//l2:l"], run = ["echo hi > o.txt"])"#,
    );

    let err = match ws.run("//app:use").await {
        Ok(_) => panic!("two declarations under one name must fail"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("//creds:a"), "{err}");
    assert!(err.contains("//creds:b"), "{err}");
    assert!(err.contains("$SECRET_<NAME>"), "{err}");
    Ok(())
}

/// Two dependencies needing *the same* credential is the common case, not a
/// conflict.
#[tokio::test]
async fn two_dependencies_needing_the_same_credential_dedupe() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"target(name = "tok", driver = "secret", provider = "exec", protocol = "raw",
       helper = ["/bin/echo", "shared_token_value"])"#,
    );
    for pkg in ["l1", "l2"] {
        ws.write_build_file(
            pkg,
            r#"target(name = "l", driver = "bash", out = "l.txt", run = ["echo l > l.txt"],
       transitive = {"secrets": {"tok": "//creds:tok"}})"#,
        );
    }
    ws.write_build_file(
        "app",
        r#"target(name = "use", driver = "bash", out = "o.txt",
       deps = ["//l1:l", "//l2:l"],
       run = ["cat $SECRET_TOK > o.txt"])"#,
    );

    let out = common::artifact_string(&*ws.run("//app:use").await?);
    assert!(out.contains("shared_token_value"), "{out}");
    Ok(())
}

// ------------------------------------------------------------------- policy

/// `allow` is access control without a new ACL system: which credentials exist
/// is CODEOWNERS on the declaring package, and which targets may *use* one is a
/// line in the same reviewed file.
#[tokio::test]
async fn allow_permits_a_matching_target_and_refuses_others() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"target(name = "tok", driver = "secret", allow = "//svc/...",
       provider = "exec", protocol = "raw", helper = ["/bin/echo", "allowed_value"])"#,
    );
    ws.write_build_file(
        "svc/api",
        r#"target(name = "ok", driver = "bash", out = "o.txt",
       secrets = {"tok": "//creds:tok"}, run = ["cat $SECRET_TOK > o.txt"])"#,
    );
    ws.write_build_file(
        "other",
        r#"target(name = "nope", driver = "bash", out = "o.txt",
       secrets = {"tok": "//creds:tok"}, run = ["cat $SECRET_TOK > o.txt"])"#,
    );

    let out = common::artifact_string(&*ws.run("//svc/api:ok").await?);
    assert!(out.contains("allowed_value"), "{out}");

    let err = match ws.run("//other:nope").await {
        Ok(_) => panic!("a target outside `allow` must be refused"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("not permitted"), "{err}");
    assert!(err.contains("//svc/..."), "{err}");
    Ok(())
}

/// **Evaluated on the effective set.** A dependency must not be able to launder
/// a credential past its own policy onto a consumer that names nothing — and
/// the message has to carry the chain, or the reader is told their target may
/// not hold a credential they have never heard of.
#[tokio::test]
async fn allow_is_checked_on_inherited_credentials_and_names_the_chain() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"target(name = "tok", driver = "secret", allow = "//lib/...",
       provider = "static_env", var = "TOKEN")"#,
    );
    ws.write_build_file(
        "lib",
        r#"target(name = "lib", driver = "bash", out = "l.txt", run = ["echo l > l.txt"],
       transitive = {"secrets": {"tok": "//creds:tok"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "use", driver = "bash", out = "o.txt",
       deps = ["//lib:lib"], run = ["echo hi > o.txt"])"#,
    );

    let err = match ws.run("//app:use").await {
        Ok(_) => panic!("a laundered credential must still be refused"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("not permitted"), "{err}");
    assert!(err.contains("//creds:tok"), "{err}");
    assert!(err.contains("reached this target through"), "{err}");
    Ok(())
}

/// A policy edit must not invalidate a cache: `allow` decides whether a build is
/// permitted, not what it computes.
#[tokio::test]
async fn allow_does_not_reach_the_cache_key() -> anyhow::Result<()> {
    let hashin_of = async |allow: &str| -> anyhow::Result<String> {
        let ws = Workspace::new();
        ws.write_build_file(
            "creds",
            &format!(
                r#"target(name = "tok", driver = "secret", allow = "{allow}",
       provider = "static_env", var = "TOKEN")"#
            ),
        );
        ws.write_build_file(
            "app",
            r#"target(name = "use", driver = "bash", out = "o.txt",
       secrets = {"tok": "//creds:tok"}, run = ["echo hi > o.txt"])"#,
        );
        ws.hashin("//app:use").await
    };

    assert_eq!(
        hashin_of("//...").await?,
        hashin_of("//app/... + //svc/...").await?,
        "editing a policy re-keyed every consumer"
    );
    Ok(())
}

// --------------------------------------------------------- subject scoping

/// Where a result genuinely depends on who produced it, a target can ask to be
/// keyed by the run's identity.
#[tokio::test]
async fn subject_scoped_partitions_the_cache_by_who_ran_it() -> anyhow::Result<()> {
    let hashin_of = async |subject: &str, scoped: bool| -> anyhow::Result<String> {
        // The subject is injected rather than set in the environment. Detection
        // reads process-global state, which a test in a parallel binary cannot
        // set without racing every other test — and a cache-key input deserves
        // a test that is not a coin flip.
        let ws = Workspace::with_run_subject(subject);
        ws.write_build_file(
            "app",
            &format!(
                r#"target(name = "t", driver = "bash", out = "o.txt",
       cache = {{"subject_scoped": {}}}, run = ["echo hi > o.txt"])"#,
                if scoped { "True" } else { "False" }
            ),
        );
        ws.hashin("//app:t").await
    };

    // Scoped: two subjects, two keys.
    let a = hashin_of("org/one", true).await?;
    let b = hashin_of("org/two", true).await?;
    assert_ne!(a, b, "a subject-scoped target did not partition");

    // Unscoped: the subject is not written at all, so the key is unchanged.
    let c = hashin_of("org/one", false).await?;
    let d = hashin_of("org/two", false).await?;
    assert_eq!(c, d, "an unscoped target picked up the subject anyway");

    Ok(())
}

// ---------------------------------------------------------------- heph auth

/// `heph auth show` is the other half of the design's bargain: a target holding
/// a credential stays cacheable and *the author configures* — which is only
/// fair to ask of someone who can see what they are configuring.
#[tokio::test]
async fn auth_show_reports_what_a_target_would_hold_without_minting() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        // `/bin/false` would fail if anything tried to mint. `show` must not.
        r#"target(name = "gh", driver = "secret", shape = ["netrc"], machine = "github.com",
       provider = "exec", protocol = "raw", helper = ["/bin/false"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "use", driver = "bash", out = "o.txt",
       secrets = {"gh": "//creds:gh"}, run = ["echo hi > o.txt"])"#,
    );

    let addr = heph::htaddr::parse_addr("//app:use")?;
    let rs = ws.engine.new_state();
    let def = std::sync::Arc::clone(&ws.engine)
        .get_def(rs.clone(), &addr)
        .await?;
    let held = ws
        .engine
        .resolve_secrets_for_check(&rs, &addr, &def.target_def.inputs)
        .await?;

    assert_eq!(held.len(), 1);
    let h = held.first().expect("one");
    assert_eq!(h.name, "gh");
    assert_eq!(h.desc.addr, "//creds:gh");
    assert_eq!(h.desc.identity.shape, vec!["netrc".to_string()]);
    Ok(())
}

/// The audit trail. "This token leaked, what now" needs an answer, and the
/// event stream is where it belongs — which descriptor, for which target, by
/// which route. **Never the value, and never the subject.**
#[tokio::test]
async fn a_grant_is_recorded_on_the_event_stream_without_the_value() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"target(name = "tok", driver = "secret", provider = "exec", protocol = "raw",
       ttl = "1h", helper = ["/bin/sh", "-c", "printf 'audited_%s_value' secret"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "use", driver = "bash", out = "o.txt",
       secrets = {"tok": "//creds:tok"}, run = ["cat $SECRET_TOK > o.txt"])"#,
    );

    let events = ws.run_collecting_events("//app:use").await?;
    let granted: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            heph::engine::event::BuildEventKind::SecretGranted {
                addr,
                secret,
                name,
                provider,
                ttl_secs,
                expiry_source,
                ..
            } => Some((
                addr.clone(),
                secret.clone(),
                name.clone(),
                provider.clone(),
                *ttl_secs,
                expiry_source.clone(),
            )),
            _ => None,
        })
        .collect();

    let g = granted.first().expect("a grant was recorded");
    assert_eq!(g.0, "//app:use");
    assert_eq!(g.1, "//creds:tok");
    assert_eq!(g.2, "tok");
    assert_eq!(g.3, "exec");
    assert!(g.4 > 0, "the grant recorded no usable life");
    assert_eq!(g.5, "declared ttl");

    // The whole point: nothing on the stream carries the credential.
    let rendered = format!("{events:?}");
    assert!(
        !rendered.contains("audited_secret_value"),
        "a credential reached the event stream"
    );
    Ok(())
}
