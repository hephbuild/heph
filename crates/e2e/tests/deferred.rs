#![expect(
    clippy::panic_in_result_fn,
    clippy::panic,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! End-to-end coverage for deferred values: `${read://pkg:name}` in a driver
//! option.
//!
//! These go through the real `Engine`, because what is being tested is the
//! *wiring* — that the host's walk finds a reference wherever an author put it,
//! that the edge it synthesizes is the right cell, that the producer's content
//! reaches the consumer's key while the def stays pure, and that the value the
//! driver ends up with is the bytes rather than the reference text.
//!
//! The property that distinguishes this from every other build system's answer is
//! in [`the_consumers_key_derives_from_the_producers_inputs`]: a change to the
//! Terraform that *defines* a value re-runs the producer, and the consumer misses
//! only if the value actually moved.

mod common;

use common::Workspace;
use std::sync::Arc;

fn expect_err<T>(r: anyhow::Result<T>, what: &str) -> anyhow::Error {
    match r {
        Ok(_) => panic!("{what}"),
        Err(e) => e,
    }
}

async fn hashin(engine: &Arc<heph::engine::Engine>, addr: &str) -> anyhow::Result<String> {
    let addr = heph::htaddr::parse_addr(addr)?;
    Ok(Arc::clone(engine)
        .meta(engine.new_state(), &addr)
        .await?
        .hashin)
}

async fn def_hash(engine: &Arc<heph::engine::Engine>, addr: &str) -> anyhow::Result<Vec<u8>> {
    let addr = heph::htaddr::parse_addr(addr)?;
    Ok(Arc::clone(engine)
        .get_def(engine.new_state(), &addr)
        .await?
        .target_def
        .hash
        .clone())
}

/// A producer whose value comes from a file it declares as a dep — the shape the
/// documentation leads with, because it is the one that re-runs when the thing
/// that *defines* the value changes and not otherwise.
fn producer(name: &str, src: &str) -> String {
    format!(
        r#"target(name = "{name}-value", driver = "bash",
       deps = [file("{src}")], out = "value.txt",
       run = ["cat $SRC > value.txt"])
"#
    )
}

// ---------------------------------------------------------------------------
// The value arrives
// ---------------------------------------------------------------------------

/// The case with no workaround today: `exec` mode has no shell, so a computed
/// value cannot reach an argv through `$SRC_<GROUP>`.
#[tokio::test]
async fn a_reference_in_exec_argv_becomes_the_producers_bytes() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("infra/registry", "registry.example.com\n");
    ws.write_build_file("infra", &producer("registry", "registry"));
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "exec", out = "o.txt", cache = False,
       run = ["sh", "-c", "printf '%s' \"${read://infra:registry-value}\" > o.txt"])"#,
    );
    assert_eq!(
        common::artifact_string(&*ws.run("//app:a").await?),
        "registry.example.com"
    );
    Ok(())
}

/// A template may mix literals and references, and may name more than one
/// producer.
#[tokio::test]
async fn a_template_may_mix_literals_and_several_producers() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("infra/registry", "reg.example\n");
    ws.write_file("infra/version", "1.4.2\n");
    ws.write_build_file(
        "infra",
        &format!(
            "{}{}",
            producer("registry", "registry"),
            producer("version", "version")
        ),
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "exec", out = "o.txt", cache = False,
       run = ["sh", "-c",
              "printf '%s' '${read://infra:registry-value}/app:${read://infra:version-value}' > o.txt"])"#,
    );
    assert_eq!(
        common::artifact_string(&*ws.run("//app:a").await?),
        "reg.example/app:1.4.2"
    );
    Ok(())
}

/// Surrounding whitespace is trimmed; interior whitespace is the value.
///
/// `echo`, `printf '%s\n'` and `terraform output` all end with a newline, a file
/// committed on Windows ends with `\r\n`, a `yq`/`jq` pipeline can leave a
/// trailing space, and an indented heredoc leaves leading ones. None of that is
/// a role ARN, an image tag or a registry host.
#[tokio::test]
async fn surrounding_whitespace_is_trimmed_and_interior_whitespace_is_not() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "infra",
        r#"target(name = "v", driver = "bash", out = "value.txt", cache = False,
       run = ["printf '  \r\n\t a b  \n\n' > value.txt"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "exec", out = "o.txt", cache = False,
       run = ["sh", "-c", "printf '[%s]' '${read://infra:v}' > o.txt"])"#,
    );
    assert_eq!(common::artifact_string(&*ws.run("//app:a").await?), "[a b]");
    Ok(())
}

/// A producer whose output is nothing but whitespace is empty, not a value —
/// the trim must not turn "  \n" into a silently-accepted empty string.
#[tokio::test]
async fn an_all_whitespace_output_is_empty_not_a_value() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "infra",
        r#"target(name = "v", driver = "bash", out = "value.txt", cache = False,
       run = ["printf '  \n\t\n' > value.txt"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "exec", out = [], cache = False,
       run = ["true", "${read://infra:v}"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "whitespace is not a value");
    assert!(format!("{err:#}").contains("empty"), "{err:#}");
    Ok(())
}

// ---------------------------------------------------------------------------
// The edge, and the two hashes
// ---------------------------------------------------------------------------

/// The edge is `(hashed, not staged)`.
///
/// `runtime: true` would do two unwanted things: merge the *producer's*
/// transitive tools, deps and env into every consumer, and stage its file into
/// the consumer's sandbox. A credential target must not inherit Terraform's
/// environment.
#[tokio::test]
async fn a_deferred_edge_is_hashed_and_not_staged() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("infra/registry", "r\n");
    ws.write_build_file("infra", &producer("registry", "registry"));
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "exec", out = [], cache = False,
       run = ["true", "${read://infra:registry-value}"])"#,
    );
    let addr = heph::htaddr::parse_addr("//app:a")?;
    let def = Arc::clone(&ws.engine)
        .get_def(ws.engine.new_state(), &addr)
        .await?;
    let input = def
        .target_def
        .inputs
        .iter()
        .find(|i| i.origin_id.starts_with("option|"))
        .expect("the reference is an input");
    assert!(input.hashed, "the producer's content must reach hashin");
    assert!(
        !input.runtime,
        "the producer's environment must not reach the consumer"
    );
    assert_eq!(input.r#ref.r#ref.format(), "//infra:registry-value");
    // The path through the config rides on the origin id, which is what lets
    // `heph inspect deps` say *which field* wanted the edge.
    assert!(input.origin_id.contains("run"), "{}", input.origin_id);
    Ok(())
}

/// **The def stays pure.** It holds the unresolved reference, which is what keeps
/// `heph query` and `heph inspect def` from triggering a build — and it is why
/// evaluation never blocks on one, the failure mode that made Nix forbid
/// import-from-derivation in nixpkgs.
#[tokio::test]
async fn the_def_holds_the_reference_and_resolving_it_builds_nothing() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let counter = ws.dir.path().join("producer-runs");
    ws.write_build_file(
        "infra",
        &format!(
            r#"target(name = "v", driver = "bash", out = "value.txt", cache = False,
       run = ["echo x >> {c}", "printf 'the-value' > value.txt"])"#,
            c = counter.display()
        ),
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "exec", out = [], cache = False,
       run = ["true", "${read://infra:v}"])"#,
    );

    let addr = heph::htaddr::parse_addr("//app:a")?;
    let def = Arc::clone(&ws.engine)
        .get_def(ws.engine.new_state(), &addr)
        .await?;
    // The def is opaque to the host by contract, so read it the way `heph inspect
    // def` does: through its serialization.
    let rendered = serde_json::to_string(&def.target_def.raw_def).expect("a def serializes");
    assert!(
        rendered.contains("${read://infra:v}"),
        "the def must hold the reference, not the value: {rendered}"
    );
    assert!(
        !counter.exists(),
        "resolving a def must not build the producer"
    );
    // The positive control: without it this assertion cannot tell "the def did
    // not build the producer" from "the producer could never have written there".
    ws.run("//app:a").await?;
    assert!(
        counter.exists(),
        "the producer does build when something actually needs the value"
    );
    Ok(())
}

/// **The property no other build system has.** The consumer's key derives from
/// what the value was *derived from*, not from the string — so a change to the
/// producer's inputs that leaves the value alone is a hit, and one that moves it
/// is a miss.
#[tokio::test]
async fn the_consumers_key_derives_from_the_producers_inputs() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("infra/registry", "reg-a\n");
    ws.write_build_file("infra", &producer("registry", "registry"));
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "exec", out = [], cache = False,
       run = ["true", "${read://infra:registry-value}"])"#,
    );
    let before_def = def_hash(&ws.engine, "//app:a").await?;
    let before = hashin(&ws.engine, "//app:a").await?;

    // The value moves.
    ws.write_file("infra/registry", "reg-b\n");
    let engine = ws.reopen()?;
    let after_def = def_hash(&engine, "//app:a").await?;
    let after = hashin(&engine, "//app:a").await?;

    assert_eq!(
        before_def, after_def,
        "the def covers the reference, which did not change"
    );
    assert_ne!(
        before, after,
        "the input hash covers the producer's content, which did"
    );
    Ok(())
}

/// And the other half: a producer that re-runs but produces the same bytes leaves
/// every consumer hitting. That is the difference between an edge and a string —
/// Gradle's `ValueSource` re-running `git describe` invalidates the configuration
/// cache every build; here it invalidates nothing unless the value moved.
#[tokio::test]
async fn an_unchanged_value_leaves_the_consumer_hitting() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("infra/registry", "reg\n");
    ws.write_file("infra/unrelated", "a\n");
    ws.write_build_file(
        "infra",
        r#"target(name = "registry-value", driver = "bash", out = "value.txt",
       deps = {"reg": [file("registry")], "other": [file("unrelated")]},
       run = ["cat $SRC_REG > value.txt"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "exec", out = [], cache = False,
       run = ["true", "${read://infra:registry-value}"])"#,
    );
    let before = hashin(&ws.engine, "//app:a").await?;

    // An unrelated file the producer declares changes: it re-runs, and produces
    // the same value.
    ws.write_file("infra/unrelated", "b\n");
    let engine = ws.reopen()?;
    assert_eq!(
        hashin(&engine, "//app:a").await?,
        before,
        "a producer re-run that changes nothing must invalidate nothing"
    );
    Ok(())
}

/// The shape the feature invites: one value, many consumers. The producer builds
/// once, and its bytes are read once.
#[tokio::test]
async fn many_consumers_of_one_producer_build_and_read_it_once() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let counter = ws.dir.path().join("producer-runs");
    ws.write_build_file(
        "infra",
        &format!(
            r#"target(name = "v", driver = "bash", out = "value.txt",
       run = ["echo x >> {c}", "printf 'shared' > value.txt"])"#,
            c = counter.display()
        ),
    );
    let consumers: String = (0..8)
        .map(|i| {
            format!(
                r#"target(name = "c{i}", driver = "exec", out = "o.txt", cache = False,
       run = ["sh", "-c", "printf '%s' '${{read://infra:v}}' > o.txt"])
"#
            )
        })
        .collect();
    ws.write_build_file("app", &consumers);

    // One request over all eight, which is what a build actually looks like.
    let matcher = heph::htmatcher::Matcher::PackagePrefix("app".into());
    let results = ws.run_matcher(&matcher).await?;
    assert_eq!(results.len(), 8, "every consumer resolved");
    for r in &results {
        assert_eq!(common::artifact_string(r), "shared");
    }
    let ran = std::fs::read_to_string(&counter).unwrap_or_default();
    assert_eq!(
        ran.lines().count(),
        1,
        "eight consumers must build the producer once, got {ran:?}"
    );
    Ok(())
}

/// Substitution is single-pass, over pieces of the *input*: a producer whose
/// output happens to contain `${…}` is never re-interpreted.
#[tokio::test]
async fn a_value_containing_a_reference_is_not_re_interpreted() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "infra",
        r#"target(name = "v", driver = "bash", out = "value.txt", cache = False,
       # Assembled from parts: a bash `run` may not itself hold a reference, and
       # the point of the test is what happens when the *output* contains one.
       run = ["printf '%s%s' '$' '{read://infra:other}' > value.txt"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "exec", out = "o.txt", cache = False,
       run = ["sh", "-c", "printf '%s' '${read://infra:v}' > o.txt"])"#,
    );
    assert_eq!(
        common::artifact_string(&*ws.run("//app:a").await?),
        "${read://infra:other}"
    );
    Ok(())
}

/// A producer that fails takes its consumer down, naming the producer and
/// showing its log — the same shape as any failing dependency, because that is
/// what it is.
///
/// It is reached while computing the consumer's `hashin`, before anything
/// substitutes, so the *field* that wanted it is not in this message. That is
/// what `heph inspect deps` is for: the synthesized edge carries an `origin_id`
/// naming the path through the config.
#[tokio::test]
async fn a_failing_producer_names_itself_and_shows_its_log() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "infra",
        r#"target(name = "v", driver = "bash", out = "value.txt", cache = False,
       run = ["echo 'backend initialization required' >&2", "exit 1"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "exec", out = [], cache = False,
       run = ["true", "${read://infra:v}"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "a failing producer must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("//infra:v"), "{msg}");
    assert!(
        msg.contains("backend initialization required"),
        "the producer's own output is the diagnostic: {msg}"
    );

    // And the edge says which field wanted it.
    let addr = heph::htaddr::parse_addr("//app:a")?;
    let def = Arc::clone(&ws.engine)
        .get_def(ws.engine.new_state(), &addr)
        .await?;
    let origin = def
        .target_def
        .inputs
        .iter()
        .find(|i| i.r#ref.r#ref.format() == "//infra:v")
        .map(|i| i.origin_id.clone())
        .expect("the edge exists");
    assert_eq!(origin, "option|run[1]", "{origin}");
    Ok(())
}

/// A reference pointed at an ordinary build target must not buffer its whole
/// output before deciding there is no value in it.
#[tokio::test]
async fn an_output_too_large_to_be_a_value_is_refused() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "infra",
        r#"target(name = "big", driver = "bash", out = "value.txt", cache = False,
       run = ["awk 'BEGIN{for(i=0;i<200000;i++)printf \"x\"}' > value.txt"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "exec", out = [], cache = False,
       run = ["true", "${read://infra:big}"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "an oversized value must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("not a value"), "{msg}");
    Ok(())
}

// ---------------------------------------------------------------------------
// The refusals
// ---------------------------------------------------------------------------

/// A reference in a field that shapes the **graph** is rejected at parse, in
/// every driver at once, because the reservation lives in the shared decoder
/// rather than in a per-field flag.
#[tokio::test]
async fn a_reference_in_a_graph_shaping_field_is_rejected() -> anyhow::Result<()> {
    let ws = Workspace::new();
    for (field, decl) in [
        (
            "deps",
            r#"deps = ["${read://infra:v}"], run = "true", out = []"#,
        ),
        ("out", r#"run = "true", out = "${read://infra:v}""#),
        (
            "tools",
            r#"tools = ["${read://infra:v}"], run = "true", out = []"#,
        ),
    ] {
        ws.write_build_file(
            "app",
            &format!(r#"target(name = "a", driver = "bash", {decl})"#),
        );
        let engine = ws.reopen()?;
        let addr = heph::htaddr::parse_addr("//app:a")?;
        let err = expect_err(
            Arc::clone(&engine)
                .get_def(engine.new_state(), &addr)
                .await
                .map(|_d| ()),
            &format!("a reference in `{field}` must be rejected"),
        );
        let msg = format!("{err:#}");
        assert!(msg.contains("deferred value reference"), "{field}: {msg}");
    }
    Ok(())
}

/// `bash` mode is not deferrable, and that is a removal of a collision class
/// rather than an omission: `${src:0:3}` is valid bash — substring expansion on a
/// lowercase variable named `src`. Bash already has `$SRC_<GROUP>`.
#[tokio::test]
async fn a_reference_in_a_bash_run_is_refused_with_the_reason() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = [], cache = False,
       run = ["echo ${read://infra:v}"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "bash `run` is not deferrable");
    let msg = format!("{err:#}");
    assert!(msg.contains("does not accept one"), "{msg}");
    assert!(msg.contains("bash"), "must name the driver: {msg}");
    Ok(())
}

/// Every shell construct has to survive being written in a deferrable field:
/// heph expands nothing there, and a reservation that swallowed them would break
/// BUILD files with nothing to do with this feature.
#[tokio::test]
async fn an_unrelated_dollar_brace_still_works_everywhere() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"
target(name = "b", driver = "bash", out = "o.txt", cache = False,
       run = ["printf '%s' \"${HEPH_E2E_UNSET_XYZ:-fallback}\" > $OUT"])
target(name = "e", driver = "exec", out = "o.txt", cache = False,
       run = ["sh", "-c", "printf '%s' \"${HEPH_E2E_UNSET_XYZ:-fallback}\" > o.txt"])
"#,
    );
    assert_eq!(
        common::artifact_string(&*ws.run("//app:b").await?),
        "fallback"
    );
    assert_eq!(
        common::artifact_string(&*ws.run("//app:e").await?),
        "fallback"
    );
    Ok(())
}

/// A silently-empty value would reach the tool as an empty role, tag or registry.
/// A stopped build is cheaper.
#[tokio::test]
async fn an_empty_producer_output_fails_rather_than_substituting_nothing() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "infra",
        r#"target(name = "v", driver = "bash", out = "value.txt", cache = False,
       run = [": > value.txt"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "exec", out = [], cache = False,
       run = ["true", "${read://infra:v}"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "an empty value must fail");
    assert!(format!("{err:#}").contains("empty"), "{err:#}");
    Ok(())
}

/// Multi-line output is rejected rather than having its first line taken: a
/// rejected surprise is cheaper than a silently wrong ARN.
#[tokio::test]
async fn a_multi_line_producer_output_is_rejected_not_truncated() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "infra",
        r#"target(name = "v", driver = "bash", out = "value.txt", cache = False,
       run = ["printf 'one\ntwo\n' > value.txt"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "exec", out = [], cache = False,
       run = ["true", "${read://infra:v}"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "multi-line must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("more than one line"), "{msg}");
    assert!(msg.contains("will not guess"), "{msg}");
    Ok(())
}

/// Several outputs is ambiguous, and guessing is how a value silently becomes the
/// wrong one. Name the groups and point at the fix.
#[tokio::test]
async fn an_ambiguous_producer_names_its_groups() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "infra",
        r#"target(name = "v", driver = "bash", cache = False,
       out = {"a": "a.txt", "b": "b.txt"},
       run = ["printf 'x' > a.txt", "printf 'y' > b.txt"])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "exec", out = [], cache = False,
       run = ["true", "${read://infra:v}"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "ambiguity must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("no single value to read"), "{msg}");
    assert!(msg.contains("a.txt") && msg.contains("b.txt"), "{msg}");
    Ok(())
}

/// The bootstrap cycle a first-time author will hit: the producer needs the
/// consumer. It must be named, not hung.
#[tokio::test]
async fn a_producer_that_needs_its_consumer_is_a_cycle_and_says_so() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"
target(name = "value", driver = "bash", out = "value.txt", cache = False,
       deps = ["//app:consumer"], run = ["printf 'v' > value.txt"])
target(name = "consumer", driver = "exec", out = [], cache = False,
       run = ["true", "${read://app:value}"])
"#,
    );
    let res = tokio::time::timeout(std::time::Duration::from_secs(30), ws.run("//app:consumer"))
        .await
        .expect("a cycle must fail, not hang");
    let err = expect_err(res, "a cycle must fail");
    let msg = format!("{err:#}").to_lowercase();
    assert!(msg.contains("cyclic") || msg.contains("cycle"), "{msg}");
    Ok(())
}

/// A reference to a target that does not exist is an ordinary resolution failure,
/// which is one of the things making the producer a real node buys.
#[tokio::test]
async fn a_reference_to_a_missing_target_is_an_ordinary_resolution_failure() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "exec", out = [], cache = False,
       run = ["true", "${read://infra:nope}"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "a missing producer must fail");
    assert!(format!("{err:#}").contains("//infra:nope"), "{err:#}");
    Ok(())
}

// ---------------------------------------------------------------------------
// The consumer the design was written for
// ---------------------------------------------------------------------------

/// The motivating case: a credential's role ARN, which is not a constant — it
/// differs per account, per environment, per developer sandbox, and it was
/// already written down once in the Terraform that created the role.
///
/// This is also the shape a flat per-field schema could not describe: the
/// reference sits three levels inside a list of maps.
#[tokio::test]
async fn a_credential_presentation_reads_its_role_from_terraform() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("infra/state", "arn:aws:iam::123456789012:role/deployer\n");
    ws.write_build_file(
        "infra",
        r#"target(name = "role-arn", driver = "bash", deps = [file("state")],
       out = "role_arn.txt", run = ["cat $SRC > role_arn.txt"])"#,
    );
    ws.write_build_file(
        "auth",
        r#"
target(
    name    = "aws",
    driver  = "credential",
    sources = [heph.auth.env(["HEPH_E2E_DEFERRED_TOKEN"],
                             present = {"env": {
                                 "TOKEN": "${heph_e2e_deferred_token}",
                                 "AWS_ROLE_ARN": "${read://infra:role-arn}",
                             }})],
)
"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:aws"], run = ["printf '%s' \"$AWS_ROLE_ARN\" > o.txt"])"#,
    );

    // SAFETY: single-threaded test on a variable unique to it.
    unsafe { std::env::set_var("HEPH_E2E_DEFERRED_TOKEN", "t") };
    let out = common::artifact_string(&*ws.run("//app:a").await?);
    // SAFETY: as above.
    unsafe { std::env::remove_var("HEPH_E2E_DEFERRED_TOKEN") };

    assert_eq!(out, "arn:aws:iam::123456789012:role/deployer");
    Ok(())
}

/// …and the producer reaches the **consumer's** key.
///
/// This is the half the first implementation got wrong, and it is worth being
/// precise about why. A credential's own `get_def` is never called on a build
/// path — a reference resolves through `get_spec` + `parse_declaration` — so an
/// edge appended to the credential's def would be resolved by nothing and reach
/// no key at all. The ARN would then shape which account a target read from while
/// every consumer's `hashin` stayed byte-identical: build against dev, edit the
/// Terraform, build again, and the dev artifact is served as prod.
///
/// So the producer rides to the consumer as an ordinary hashed input. The cost is
/// over-invalidation — a role change rebuilds the targets that use it — which
/// errs toward a spurious miss rather than a wrong build. It is also exactly why
/// *material* stays on the other side of the line: material is unhashed because a
/// target's output must be identical whichever identity produced it, which is not
/// true of an ARN that selects an account.
#[tokio::test]
async fn a_credentials_deferred_value_reaches_its_consumers_key() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("infra/state", "arn:aws:iam::111:role/dev\n");
    ws.write_build_file(
        "infra",
        r#"target(name = "role-arn", driver = "bash", deps = [file("state")],
       out = "role_arn.txt", run = ["cat $SRC > role_arn.txt"])"#,
    );
    ws.write_build_file(
        "auth",
        r#"target(name = "aws", driver = "credential",
       sources = [heph.auth.env(["T"], present = {"env": {
           "AWS_ROLE_ARN": "${read://infra:role-arn}"}})])"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", run = "true", out = [],
       credentials = ["//auth:aws"])"#,
    );
    let before = hashin(&ws.engine, "//app:a").await?;

    ws.write_file("infra/state", "arn:aws:iam::222:role/prod\n");
    let engine = ws.reopen()?;
    assert_ne!(
        hashin(&engine, "//app:a").await?,
        before,
        "a value that shapes which account a target reads from must move its key"
    );
    Ok(())
}

/// …while a credential change that carries *no* deferred value still reaches no
/// consumer. The two together are the whole boundary: material is invisible to a
/// key, a computed configuration value is not.
#[tokio::test]
async fn a_credential_change_with_no_deferred_value_still_rebuilds_nothing() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "aws", driver = "credential",
       sources = [heph.auth.env(["A"])], present = {"env": {"T": "${a}"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", run = "true", out = [],
       credentials = ["//auth:aws"])"#,
    );
    let before = hashin(&ws.engine, "//app:a").await?;

    ws.write_build_file(
        "auth",
        r#"target(name = "aws", driver = "credential",
       sources = [heph.auth.env(["B"]), heph.auth.env(["C"])],
       ttl = "10m", present = {"env": {"TOTALLY_DIFFERENT": "${b}"}})"#,
    );
    let engine = ws.reopen()?;
    assert_eq!(hashin(&engine, "//app:a").await?, before);
    Ok(())
}
