#![expect(
    clippy::panic_in_result_fn,
    clippy::panic,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! End-to-end coverage for credentials as targets.
//!
//! These go through the real `Engine` — provider, BUILD-file evaluation, driver
//! registry, `get_def`, `execute` — rather than calling the driver directly,
//! because what is being tested is the *wiring*: that a declaration resolves,
//! that a reference is an edge the cache key cannot see, that the chosen source's
//! material reaches the target's environment, and that a mistake fails where a
//! BUILD author will see it.
//!
//! The chain is exercised through the `env` and `file` source kinds, which need
//! no network and no vendor CLI. `oidc` is covered by its probe (there is no
//! endpoint here) and `exec` by a command the test writes itself.

mod common;

use common::Workspace;
use std::sync::Arc;

/// `EResult` has no `Debug`, so `expect_err` will not compile. Unwrap the error
/// side explicitly instead.
fn expect_err<T>(r: anyhow::Result<T>, what: &str) -> anyhow::Error {
    match r {
        Ok(_) => panic!("{what}"),
        Err(e) => e,
    }
}

/// Set a host variable for the duration of one test.
///
/// The chain reads the *process* environment, which is what makes an `env` source
/// mean anything — so a test that exercises one has to touch it. Restored on drop
/// so a failure does not leak into another test.
struct EnvVar(&'static str);

impl EnvVar {
    fn set(name: &'static str, value: &str) -> Self {
        // SAFETY: these tests run on their own variables, and each guard restores
        // its own on drop.
        unsafe { std::env::set_var(name, value) };
        Self(name)
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        // SAFETY: see `set`.
        unsafe { std::env::remove_var(self.0) };
    }
}

async fn def_hash(engine: &Arc<heph::engine::Engine>, addr: &str) -> anyhow::Result<Vec<u8>> {
    let addr = heph::htaddr::parse_addr(addr)?;
    let def = Arc::clone(engine)
        .get_def(engine.new_state(), &addr)
        .await?;
    Ok(def.target_def.hash.clone())
}

async fn hashin(engine: &Arc<heph::engine::Engine>, addr: &str) -> anyhow::Result<String> {
    let addr = heph::htaddr::parse_addr(addr)?;
    Ok(Arc::clone(engine)
        .meta(engine.new_state(), &addr)
        .await?
        .hashin)
}

// ---------------------------------------------------------------------------
// The declaration
// ---------------------------------------------------------------------------

/// The driver is registered and a declaration resolves. Without this, everything
/// downstream fails with "driver not found" and no test says why.
#[tokio::test]
async fn a_credential_declaration_resolves_through_the_engine() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"
target(
    name    = "token",
    driver  = "credential",
    sources = [heph.auth.env(["MY_TOKEN"])],
    present = {"env": {"MY_TOKEN": "${my_token}"}},
)
"#,
    );
    let spec = ws.get_spec("//auth:token").await?;
    assert_eq!(spec.driver, "credential");
    heph::plugincredential::parse_declaration(&spec)?;
    Ok(())
}

/// A declaration produces no artifacts, so resolving one directly yields an empty
/// result rather than an error — and, critically, acquires nothing. Running
/// `heph run //auth:aws` must not sign anyone in.
#[tokio::test]
async fn resolving_a_declaration_directly_acquires_nothing() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["ABSENT_EVERYWHERE"])],
       present = {"env": {"T": "${absent_everywhere}"}})"#,
    );
    // The chain cannot be satisfied here, and that is precisely the point: a
    // declaration is inert, so this succeeds anyway.
    let result = ws.run("//auth:t").await?;
    assert!(result.artifacts.is_empty());
    Ok(())
}

/// An empty chain can never be acquired anywhere, so it fails at the declaration
/// rather than at the first consumer.
#[tokio::test]
async fn an_empty_chain_fails_at_the_declaration() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential", sources = [])"#,
    );
    let err = expect_err(ws.run("//auth:t").await, "an empty chain must not resolve");
    assert!(format!("{err:#}").contains("at least one"), "{err:#}");
    Ok(())
}

/// The sharpest rule in the design, enforced by the vocabulary rather than by a
/// check: a presentation carries material and handles only, so a region — which
/// *selects bytes* — has nowhere to go.
#[tokio::test]
async fn configuration_has_nowhere_to_hide_in_a_presentation() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["T"])],
       present = {"env": {"T": "${t}"}, "region": "eu-west-1"})"#,
    );
    let err = expect_err(
        ws.run("//auth:t").await,
        "a region must not be a presentation",
    );
    let msg = format!("{err:#}");
    assert!(msg.contains("unknown key"), "{msg}");
    assert!(
        msg.contains("hashed input on the consumer"),
        "the message must say where it does belong: {msg}"
    );
    Ok(())
}

/// **The limit of the structural guard, pinned deliberately.**
///
/// The vocabulary stops a *key* like `region` (the test above). It does not stop
/// a content-selecting *value* — `present = {"env": {"AWS_REGION": "eu-west-1"}}`
/// is accepted, and by design: the same shape is how `heph.auth.aws_web_identity`
/// carries a role ARN, which is a handle rather than a selector, and heph cannot
/// tell those apart from the outside.
///
/// What keeps that sound is the contract, not the parser: a target whose *output*
/// depends on which identity ran it is not cacheable and says so with
/// `cache = False`. This test exists so the boundary between "enforced" and
/// "conventional" is written down where someone reading the parser will find it,
/// rather than implied by a doc sentence.
#[tokio::test]
async fn a_literal_value_in_a_presentation_is_accepted_and_is_a_convention_not_a_check()
-> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_CONV_TOKEN", "v");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["HEPH_E2E_CONV_TOKEN"])],
       present = {"env": {"TOKEN": "${heph_e2e_conv_token}",
                          "AWS_ROLE_ARN": "arn:aws:iam::123456789012:role/deployer"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$AWS_ROLE_ARN\" > o.txt"])"#,
    );
    assert_eq!(
        common::artifact_string(&*ws.run("//app:a").await?),
        "arn:aws:iam::123456789012:role/deployer"
    );
    Ok(())
}

/// A reference to something that is not a credential is a BUILD-file mistake that
/// would otherwise surface much later as a target with no identity. Name both
/// ends.
#[tokio::test]
async fn referencing_a_non_credential_target_names_both_ends() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"
target(name = "notacred", driver = "bash", run = "true", out = [])
target(name = "a", driver = "bash", run = "true", credentials = ["//app:notacred"], out = [])
"#,
    );
    let err = expect_err(
        ws.run("//app:a").await,
        "referencing a bash target as a credential must fail",
    );
    let msg = format!("{err:#}");
    assert!(msg.contains("//app:notacred"), "{msg}");
    assert!(msg.contains("`deps`"), "must suggest the fix: {msg}");
    Ok(())
}

/// A BUILD-file mistake surfaces at `get_def`, so `heph query` and
/// `heph inspect def` report it — where the author is still looking at the file
/// — rather than only when something eventually executes.
#[tokio::test]
async fn a_bad_reference_is_caught_without_executing_anything() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"
target(name = "notacred", driver = "bash", run = "true", out = [])
target(name = "a", driver = "bash", run = "true", credentials = ["//app:notacred"], out = [])
"#,
    );
    let addr = heph::htaddr::parse_addr("//app:a")?;
    let err = expect_err(
        Arc::clone(&ws.engine)
            .get_def(ws.engine.new_state(), &addr)
            .await,
        "resolution must fail at get_def",
    );
    assert!(format!("{err:#}").contains("//app:notacred"), "{err:#}");
    Ok(())
}

// ---------------------------------------------------------------------------
// The cache-key boundary — the load-bearing claim
// ---------------------------------------------------------------------------

/// **Nothing about a credential enters the consumer's key.** Not the reference,
/// not the declaration, not the names of the variables it presents.
///
/// Structural rather than conventional: `hashin` is computed over `hashed: true`
/// inputs, and a credential reference sets both flags false. This is the test a
/// reviewer should attack first.
#[tokio::test]
async fn referencing_a_credential_does_not_change_the_consumer_hash() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["MY_TOKEN"])],
       present = {"env": {"MY_TOKEN": "${my_token}"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", run = "true", out = [])"#,
    );
    let before_def = def_hash(&ws.engine, "//app:a").await?;
    let before_hashin = hashin(&ws.engine, "//app:a").await?;

    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", run = "true", out = [],
       credentials = ["//auth:t"])"#,
    );
    let engine = ws.reopen()?;
    let after_def = def_hash(&engine, "//app:a").await?;
    let after_hashin = hashin(&engine, "//app:a").await?;

    assert_eq!(
        before_def, after_def,
        "a credential reference must not move the def hash"
    );
    assert_eq!(
        before_hashin, after_hashin,
        "a credential reference must not move the input hash — this is the whole contract"
    );
    Ok(())
}

/// The declaration's *contents* are equally invisible. Changing which variable a
/// credential presents, or which sources it has, must not rebuild its consumers:
/// a target's outputs are required to be identical whichever identity satisfied
/// its requirement.
#[tokio::test]
async fn changing_the_declaration_does_not_rebuild_consumers() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["A"])],
       present = {"env": {"A": "${a}"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", run = "true", out = [],
       credentials = ["//auth:t"])"#,
    );
    let before = hashin(&ws.engine, "//app:a").await?;

    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["B"]), heph.auth.env(["C"])],
       ttl     = "10m",
       present = {"env": {"TOTALLY_DIFFERENT": "${b}"}})"#,
    );
    let engine = ws.reopen()?;
    let after = hashin(&engine, "//app:a").await?;
    assert_eq!(
        before, after,
        "a declaration change must not reach a consumer"
    );
    Ok(())
}

/// The edge exists even though it is invisible to the key — which is what makes
/// `heph query revdeps` answer "who needs this identity?" and turns a bad addr
/// into an ordinary resolution failure rather than a 403 much later.
#[tokio::test]
async fn a_credential_reference_is_an_edge_the_graph_can_see() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["A"])], present = {"env": {"A": "${a}"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", run = "true", out = [],
       credentials = ["//auth:t"])"#,
    );
    let addr = heph::htaddr::parse_addr("//app:a")?;
    let def = Arc::clone(&ws.engine)
        .get_def(ws.engine.new_state(), &addr)
        .await?;
    let input = def
        .target_def
        .inputs
        .iter()
        .find(|i| i.origin_id.starts_with("credential|"))
        .expect("the reference is an input");
    assert!(
        !input.hashed,
        "a credential must not feed the consumer's key"
    );
    assert!(
        !input.runtime,
        "a credential materializes nothing through the input path"
    );
    assert_eq!(input.r#ref.r#ref.format(), "//auth:t");
    Ok(())
}

/// A duplicate reference presents one set of variables twice, which does nothing.
/// Naming it beats quietly collapsing it.
#[tokio::test]
async fn a_duplicate_reference_is_rejected() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["A"])], present = {"env": {"A": "${a}"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", run = "true", out = [],
       credentials = ["//auth:t", "//auth:t"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "a duplicate must not resolve");
    assert!(format!("{err:#}").contains("twice"), "{err:#}");
    Ok(())
}

/// Two credentials on one target claiming one variable leaves one of them
/// inoperative with nothing to see. Refuse rather than resolve.
#[tokio::test]
async fn two_credentials_claiming_one_variable_are_refused() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"
target(name = "a", driver = "credential",
       sources = [heph.auth.env(["A"])], present = {"env": {"TOKEN": "${a}"}})
target(name = "b", driver = "credential",
       sources = [heph.auth.env(["B"])], present = {"env": {"TOKEN": "${b}"}})
"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "x", driver = "bash", run = "true", out = [],
       credentials = ["//auth:a", "//auth:b"])"#,
    );
    let err = expect_err(ws.run("//app:x").await, "a collision must not resolve");
    let msg = format!("{err:#}");
    assert!(msg.contains("TOKEN"), "{msg}");
    assert!(msg.contains("shadow"), "{msg}");
    Ok(())
}

// ---------------------------------------------------------------------------
// The chain, and what reaches the target
// ---------------------------------------------------------------------------

/// The acceptance scenario, minus the clouds: a chain whose first source does not
/// apply here and whose second does, presenting material the target can read.
#[tokio::test]
async fn the_first_applicable_source_wins_and_its_material_reaches_the_target() -> anyhow::Result<()>
{
    let _guard = EnvVar::set("HEPH_E2E_CRED_TOKEN", "sekrit-token-value");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"
target(
    name    = "t",
    driver  = "credential",
    sources = [
        # Not here: there is no OIDC endpoint in a test process.
        heph.auth.oidc("github_actions", audience = "x",
                       present = {"env": {"TOKEN": "${id_token}"}}),
        # Here.
        heph.auth.env(["HEPH_E2E_CRED_TOKEN"]),
    ],
    present = {"env": {"TOKEN": "${heph_e2e_cred_token}"}},
)
"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$TOKEN\" > o.txt"])"#,
    );
    let out = common::artifact_string(&*ws.run("//app:a").await?);
    assert_eq!(out, "sekrit-token-value");
    Ok(())
}

/// The commonest failure in CI, and today an unreadable one. The error prints the
/// walk, with the fix attached to the line that failed.
#[tokio::test]
async fn a_chain_that_applies_nowhere_prints_the_walk_and_the_fix() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"
target(
    name    = "t",
    driver  = "credential",
    sources = [
        heph.auth.oidc("github_actions", audience = "sts.amazonaws.com",
                       present = {"env": {"T": "${id_token}"}}),
        heph.auth.env(["HEPH_E2E_SURELY_UNSET"]),
    ],
    present = {"env": {"T": "${heph_e2e_surely_unset}"}},
)
"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", run = "true", out = [], cache = False,
       credentials = ["//auth:t"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "no source applies");
    let msg = format!("{err:#}");
    assert!(msg.contains("no source applies here"), "{msg}");
    // Every source, in order, with why it was skipped …
    assert!(msg.contains("oidc(github_actions)"), "{msg}");
    assert!(msg.contains("ACTIONS_ID_TOKEN_REQUEST_URL"), "{msg}");
    assert!(msg.contains("HEPH_E2E_SURELY_UNSET"), "{msg}");
    // … and the fix, on the line that failed.
    assert!(
        msg.contains("id-token: write"),
        "the hint must be there: {msg}"
    );
    Ok(())
}

/// An acquire failure is **terminal, not a fallthrough**. If a chain fell through
/// on failure, a misconfigured role in CI would silently fall back to whatever
/// ambient identity the runner happened to have — precisely the accident this
/// feature exists to prevent.
#[tokio::test]
async fn an_acquire_failure_does_not_fall_through_to_the_next_source() -> anyhow::Result<()> {
    let _fallback = EnvVar::set("HEPH_E2E_FALLBACK_TOKEN", "the-wrong-identity");
    let ws = Workspace::new();
    // A `file` source whose path exists — so it is applicable — but whose
    // contents are not the JSON its `fields` map promises. The probe passes, the
    // acquire fails, and the chain must stop.
    ws.write_file("secrets/creds.json", "this is not json");
    let root = ws.dir.path().display().to_string();
    ws.write_build_file(
        "auth",
        &format!(
            r#"
target(
    name    = "t",
    driver  = "credential",
    sources = [
        heph.auth.file("{root}/secrets/creds.json", fields = {{"token": "access_token"}}),
        heph.auth.env(["HEPH_E2E_FALLBACK_TOKEN"]),
    ],
    present = {{"env": {{"T": "${{token}}"}}}},
)
"#
        ),
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$T\" > o.txt"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "an acquire failure is terminal");
    let msg = format!("{err:#}");
    assert!(msg.contains("not JSON"), "{msg}");
    assert!(
        !msg.contains("the-wrong-identity"),
        "the chain must not have reached the next source"
    );
    Ok(())
}

/// `when` is policy, not logic: a fixed vocabulary heph evaluates. A source whose
/// `when` does not hold here is skipped before its probe runs.
#[tokio::test]
async fn when_selects_a_source_before_any_probe() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_LAPTOP_TOKEN", "laptop");
    let other_os = if cfg!(target_os = "macos") {
        "linux"
    } else {
        "darwin"
    };
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        &format!(
            r#"
target(
    name    = "t",
    driver  = "credential",
    sources = [
        heph.auth.env(["HEPH_E2E_LAPTOP_TOKEN"], when = "os:{other_os}",
                      present = {{"env": {{"T": "never"}}}}),
        heph.auth.env(["HEPH_E2E_LAPTOP_TOKEN"]),
    ],
    present = {{"env": {{"T": "${{heph_e2e_laptop_token}}"}}}},
)
"#
        ),
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$T\" > o.txt"])"#,
    );
    assert_eq!(
        common::artifact_string(&*ws.run("//app:a").await?),
        "laptop"
    );
    Ok(())
}

/// A `files` presentation lands **beside** the workspace directory, never inside
/// it — because output collection is rooted at the workspace directory and packs
/// every regular file it walks. A token file inside it would land in the local
/// cache and be pushed to the shared remote automatically.
#[tokio::test]
async fn a_presented_file_is_never_collected_as_an_output() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_FILE_TOKEN", "file-material");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["HEPH_E2E_FILE_TOKEN"])],
       present = {"files": {"tok": "${heph_e2e_file_token}"},
                  "env":   {"TOKFILE": "${file:tok}"}})"#,
    );
    // A `**/*` output glob: the greediest collection there is. If the presented
    // file were inside the workspace directory, this is what would pack it.
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "**/*", cache = False,
       credentials = ["//auth:t"],
       run = ["cat \"$TOKFILE\" > seen.txt"])"#,
    );
    let result = ws.run("//app:a").await?;
    let paths = common::artifact_paths(&result);
    // The target really did read the file …
    let seen = paths
        .iter()
        .any(|p| p.to_string_lossy().ends_with("seen.txt"));
    assert!(
        seen,
        "the target must have read the presented file: {paths:?}"
    );
    // … and no artifact is the token file itself.
    for p in &paths {
        let s = p.to_string_lossy();
        assert!(
            !s.contains(".heph/auth"),
            "a presented credential file must never be collected: {s}"
        );
        assert!(!s.ends_with("/tok"), "{s}");
    }
    Ok(())
}

/// Material never reaches `log.txt`, the TUI, the failure event or the cache —
/// because it is scrubbed at the tee, before any of them.
///
/// Asserted on a **failing** target deliberately. That is the case where the
/// captured log matters most (it is lifted into the failure event, the JSON
/// output and the CI report) and it is also the one where the sandbox survives,
/// so `log.txt` can be read back.
#[tokio::test]
async fn credential_material_is_scrubbed_from_a_targets_output() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_LOUD_TOKEN", "extremely-secret-value");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["HEPH_E2E_LOUD_TOKEN"])],
       present = {"env": {"TOKEN": "${heph_e2e_loud_token}"}})"#,
    );
    // A build step that echoes its own credential — the motivating accident —
    // and then fails, which is when the log is lifted into an event.
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = [], cache = False,
       credentials = ["//auth:t"],
       run = ["echo \"using $TOKEN\"", "exit 3"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "the target exits 3");
    let msg = format!("{err:#}");
    assert!(
        !msg.contains("extremely-secret-value"),
        "a token echoed by a build step must not reach the failure: {msg}"
    );

    let mut found = false;
    for entry in walkdir(&ws.dir.path().join(".heph3").join("sandbox")) {
        if entry.file_name().is_some_and(|n| n == "log.txt") {
            let body = std::fs::read_to_string(&entry).unwrap_or_default();
            if !body.contains("using") {
                continue;
            }
            assert!(
                !body.contains("extremely-secret-value"),
                "a token echoed by a build step must not reach log.txt: {body}"
            );
            assert!(
                body.contains("[redacted]"),
                "the log must show that something was scrubbed: {body}"
            );
            found = true;
        }
    }
    assert!(found, "no log.txt was written, so nothing was asserted");
    Ok(())
}

/// The floor is a stated limit, not a hidden one: material shorter than
/// `REDACT_MIN_LEN` is **not** scrubbed, because a substring replacement over a
/// byte stream cannot remove `"1"` without corrupting every number a build
/// prints.
#[tokio::test]
async fn a_secret_below_the_length_floor_is_documented_as_not_scrubbed() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_TINY_TOKEN", "abc");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["HEPH_E2E_TINY_TOKEN"])],
       present = {"env": {"TOKEN": "${heph_e2e_tiny_token}"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$TOKEN\" > o.txt"])"#,
    );
    // The material still reaches the target — only the scrubbing declines.
    assert_eq!(common::artifact_string(&*ws.run("//app:a").await?), "abc");
    assert_eq!(heph::engine::driver::REDACT_MIN_LEN, 8);
    Ok(())
}

fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// A presented file may name another presented file — Google's
/// `external_account` document points at the token file beside it — so the
/// rendering pass retries until it makes no progress.
#[tokio::test]
async fn a_presented_file_can_name_another_presented_file() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_CHAIN_TOKEN", "chained");
    let ws = Workspace::new();
    // `adc` sorts before `token`, so a single pass in map order would fail.
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["HEPH_E2E_CHAIN_TOKEN"])],
       present = {"files": {"adc": "points at ${file:token}",
                            "token": "${heph_e2e_chain_token}"},
                  "env":   {"ADC": "${file:adc}"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["cat \"$ADC\" > o.txt"])"#,
    );
    let out = common::artifact_string(&*ws.run("//app:a").await?);
    assert!(out.starts_with("points at /"), "{out:?}");
    assert!(out.trim().ends_with("/token"), "{out:?}");
    Ok(())
}

/// A typo in a presented file's template reports *that*, not a generic "does not
/// resolve" — the retry loop must carry the real error out.
#[tokio::test]
async fn a_bad_field_in_a_presented_file_names_the_field() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_TYPO_TOKEN", "v");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["HEPH_E2E_TYPO_TOKEN"])],
       present = {"files": {"f": "${heph_e2e_typo_tokn}"},
                  "env":   {"F": "${file:f}"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", run = "true", out = [], cache = False,
       credentials = ["//auth:t"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "a typo must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("heph_e2e_typo_tokn"), "{msg}");
    assert!(
        msg.contains("heph_e2e_typo_token"),
        "must name what the source did yield: {msg}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// The chain refuses to eat itself
// ---------------------------------------------------------------------------

/// A credential that names itself must **fail**, not hang.
///
/// The guard has to run before anything takes the per-address cell, or the
/// recursion parks on a lock it is itself holding — a one-line BUILD typo turning
/// into a wedged build with no diagnostic and no timeout. The `timeout` here is
/// what makes a regression fail rather than hang the whole suite.
#[tokio::test]
async fn a_credential_that_is_its_own_source_fails_rather_than_hanging() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential", sources = ["//auth:t"],
       present = {"env": {"T": "${token}"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", run = "true", out = [], cache = False,
       credentials = ["//auth:t"])"#,
    );
    let res = tokio::time::timeout(std::time::Duration::from_secs(20), ws.run("//app:a"))
        .await
        .expect("a self-referencing credential must fail, not hang");
    let err = expect_err(res, "a cycle must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("its own source"), "{msg}");
    assert!(
        msg.contains("//auth:t → //auth:t"),
        "must name the loop: {msg}"
    );
    Ok(())
}

/// The same, one hop further out: `a → b → a`.
#[tokio::test]
async fn a_two_credential_cycle_is_named_not_hung() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"
target(name = "a", driver = "credential", sources = ["//auth:b"],
       present = {"env": {"A": "${token}"}})
target(name = "b", driver = "credential", sources = ["//auth:a"],
       present = {"env": {"B": "${token}"}})
"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "x", driver = "bash", run = "true", out = [], cache = False,
       credentials = ["//auth:a"])"#,
    );
    let res = tokio::time::timeout(std::time::Duration::from_secs(20), ws.run("//app:x"))
        .await
        .expect("a two-credential cycle must fail, not hang");
    let err = expect_err(res, "a cycle must fail");
    assert!(
        format!("{err:#}").contains("//auth:a → //auth:b → //auth:a"),
        "{err:#}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// The rest of the source vocabulary
// ---------------------------------------------------------------------------

/// `passthrough` exposes host files **in place** — nothing is copied into the
/// credential store — and the presentation points a variable at the path.
#[tokio::test]
async fn a_passthrough_source_exposes_a_host_path_in_place() -> anyhow::Result<()> {
    let host = tempfile::tempdir().expect("tempdir");
    let cfg = host.path().join("config");
    std::fs::write(&cfg, "vendor session state").expect("write");

    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        &format!(
            r#"target(name = "t", driver = "credential",
       sources = [heph.auth.passthrough(
           paths = {{"config": "{}"}},
           env   = {{"VENDOR_CONFIG": "${{file:config}}"}})],
       present = {{"env": {{"UNUSED": "${{file:config}}"}}}})"#,
            cfg.display()
        ),
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["cat \"$VENDOR_CONFIG\" > o.txt"])"#,
    );
    assert_eq!(
        common::artifact_string(&*ws.run("//app:a").await?),
        "vendor session state"
    );
    // Exposed, not copied: the host file is still the one the target read.
    assert!(cfg.exists());
    Ok(())
}

/// A `passthrough` whose path is absent is skipped, not failed — that is what
/// makes it usable as the laptop half of a chain whose other half is CI.
#[tokio::test]
async fn a_passthrough_with_a_missing_path_is_skipped() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_PT_FALLBACK", "fallback");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [
           heph.auth.passthrough(paths = {"c": "/nonexistent/heph/e2e"},
                                 env = {"T": "never"}),
           heph.auth.env(["HEPH_E2E_PT_FALLBACK"]),
       ],
       present = {"env": {"T": "${heph_e2e_pt_fallback}"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$T\" > o.txt"])"#,
    );
    assert_eq!(
        common::artifact_string(&*ws.run("//app:a").await?),
        "fallback"
    );
    Ok(())
}

/// The `generic` OIDC provider: a token some other CI system already put where
/// heph can find it. Deliberately generic — a provider list has no end.
#[tokio::test]
async fn a_generic_oidc_token_is_read_from_its_file_and_trimmed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let token = dir.path().join("token");
    // A trailing newline is what every CI system writes.
    std::fs::write(&token, "jwt-value\n").expect("write");
    let _guard = EnvVar::set("HEPH_OIDC_TOKEN_FILE", &token.to_string_lossy());

    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.oidc("generic")],
       present = {"env": {"TOK": "${id_token}"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$TOK\" > o.txt"])"#,
    );
    assert_eq!(
        common::artifact_string(&*ws.run("//app:a").await?),
        "jwt-value"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// The helper presentations
// ---------------------------------------------------------------------------

/// Read a file the credential presented, from inside the target, by copying it
/// to an output. The sandbox is gone by the time a test could look.
fn cat_target(var: &str) -> String {
    format!(
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["cat \"${var}\" > o.txt"])"#
    )
}

/// The AWS credential-process presentation writes a config file naming the helper
/// and points `AWS_CONFIG_FILE` at it.
#[tokio::test]
async fn the_aws_helper_writes_a_config_naming_the_callback() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_AWS_TOKEN", "aws-material");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["HEPH_E2E_AWS_TOKEN"])],
       present = heph.auth.aws_process())"#,
    );
    ws.write_build_file("app", &cat_target("AWS_CONFIG_FILE"));
    let cfg = common::artifact_string(&*ws.run("//app:a").await?);
    assert!(cfg.contains("[default]"), "{cfg}");
    assert!(cfg.contains("credential_process = "), "{cfg}");
    assert!(cfg.contains("__auth-helper aws"), "{cfg}");
    assert!(cfg.contains("--pin "), "the callback must be pinned: {cfg}");
    Ok(())
}

/// The GCP helper writes an `external_account` document whose
/// `credential_source.executable` names the callback — and sets the opt-in
/// without which no Google SDK will run an executable credential source at all.
#[tokio::test]
async fn the_gcp_helper_writes_an_executable_sourced_external_account() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_GCP_TOKEN", "gcp-material");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["HEPH_E2E_GCP_TOKEN"])],
       present = {"helper": {"dialect": "gcp",
                             "audience": "//iam.googleapis.com/projects/1/x",
                             "impersonate": "d@p.iam.gserviceaccount.com"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"],
       run = ["cat \"$GOOGLE_APPLICATION_CREDENTIALS\" > o.txt",
              "test \"$GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES\" = 1",
              "test -n \"$CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE\""])"#,
    );
    let doc: serde_json::Value =
        serde_json::from_str(&common::artifact_string(&*ws.run("//app:a").await?))
            .expect("valid json");
    assert_eq!(doc["type"], "external_account");
    assert_eq!(doc["audience"], "//iam.googleapis.com/projects/1/x");
    let cmd = doc["credential_source"]["executable"]["command"]
        .as_str()
        .expect("a command");
    assert!(cmd.contains("__auth-helper gcp"), "{cmd}");
    assert!(
        doc["service_account_impersonation_url"]
            .as_str()
            .is_some_and(|u| u.contains("d@p.iam.gserviceaccount.com")),
        "{doc}"
    );
    Ok(())
}

/// The Docker helper writes a `credHelpers` config, a `docker-credential-heph`
/// shim, and puts the shim's directory on `PATH` — which is the only presentation
/// that touches `PATH`, because Docker resolves a helper by executable name.
#[tokio::test]
async fn the_docker_helper_writes_a_shim_and_puts_it_on_path() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_REG_TOKEN", "registry-material");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["HEPH_E2E_REG_TOKEN"])],
       present = heph.auth.docker(["ghcr.io"]))"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"],
       run = ["cat \"$DOCKER_CONFIG/config.json\" > o.txt",
              # resolvable by name, and executable
              "command -v docker-credential-heph > /dev/null",
              "test -x \"$(command -v docker-credential-heph)\""])"#,
    );
    let doc: serde_json::Value =
        serde_json::from_str(&common::artifact_string(&*ws.run("//app:a").await?))
            .expect("valid json");
    assert_eq!(doc["credHelpers"]["ghcr.io"], "heph");
    Ok(())
}

/// The git helper is injected entirely through `GIT_CONFIG_*`, so no gitconfig is
/// written and the developer's own is never touched.
#[tokio::test]
async fn the_git_helper_is_environment_only() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_GIT_TOKEN", "git-material");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["HEPH_E2E_GIT_TOKEN"])],
       present = heph.auth.git(["git.corp.example"]))"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"],
       run = ["printf '%s|%s|%s' \"$GIT_CONFIG_COUNT\" \"$GIT_CONFIG_KEY_0\" \"$GIT_CONFIG_VALUE_0\" > o.txt"])"#,
    );
    let out = common::artifact_string(&*ws.run("//app:a").await?);
    let parts: Vec<&str> = out.split('|').collect();
    assert_eq!(parts.first().copied(), Some("1"), "{out}");
    assert_eq!(
        parts.get(1).copied(),
        Some("credential.https://git.corp.example.helper"),
        "{out}"
    );
    let value = parts.get(2).copied().unwrap_or_default();
    assert!(
        value.starts_with('!'),
        "a git helper argv is `!<cmd>`: {out}"
    );
    assert!(value.contains("__auth-helper git"), "{out}");
    Ok(())
}

/// The kubernetes dialect writes no document of its own: the author templates the
/// kubeconfig and places the callback with `${helper:command}` / `${helper:args}`.
#[tokio::test]
async fn the_kubernetes_helper_places_the_callback_in_the_authors_document() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_K8S_TOKEN", "k8s-material");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["HEPH_E2E_K8S_TOKEN"])],
       present = {"helper": "kubernetes",
                  "files":  {"kubeconfig": "command: ${helper:command}\nargs: ${helper:args}\n"},
                  "env":    {"KUBECONFIG": "${file:kubeconfig}"}})"#,
    );
    ws.write_build_file("app", &cat_target("KUBECONFIG"));
    let out = common::artifact_string(&*ws.run("//app:a").await?);
    assert!(out.starts_with("command: /"), "an absolute path: {out}");
    assert!(out.contains("__auth-helper"), "{out}");
    assert!(out.contains("kubernetes"), "{out}");
    assert!(out.contains("--pin"), "{out}");
    Ok(())
}

/// A credential whose material has no expiry and no `ttl` is never written to
/// disk: a long-lived API token under `<home>/auth/` is a durable secret at rest
/// that nothing will ever clean up.
#[tokio::test]
async fn material_with_no_expiry_is_never_left_at_rest() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_UNBOUNDED", "no-expiry-token");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"target(name = "t", driver = "credential",
       sources = [heph.auth.env(["HEPH_E2E_UNBOUNDED"])],
       present = {"env": {"T": "${heph_e2e_unbounded}"}})"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$T\" > o.txt"])"#,
    );
    assert_eq!(
        common::artifact_string(&*ws.run("//app:a").await?),
        "no-expiry-token"
    );

    let auth_dir = ws.dir.path().join(".heph3").join("auth");
    for f in walkdir(&auth_dir) {
        let body = std::fs::read(&f).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&body).contains("no-expiry-token"),
            "unbounded material must not be written to {}",
            f.display()
        );
    }
    Ok(())
}

/// A target used as a credential source must be `cache = False`: its outputs are
/// material, and a cacheable target's outputs are written to the local cache and
/// pushed to the shared remote automatically. Enforced at resolution rather than
/// documented.
#[tokio::test]
async fn a_cacheable_producer_is_refused_as_a_credential_source() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"
target(name = "fetch", driver = "bash", out = {"credential": "cred.json"},
       run = ["printf '{\"token\":\"abc\"}' > cred.json"])
target(name = "t", driver = "credential", sources = ["//auth:fetch"],
       present = {"env": {"T": "${token}"}})
"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$T\" > o.txt"])"#,
    );
    let err = expect_err(
        ws.run("//app:a").await,
        "a cacheable credential source must be refused",
    );
    let msg = format!("{err:#}");
    assert!(msg.contains("cache = False"), "{msg}");
    assert!(msg.contains("shared remote"), "must say why: {msg}");
    Ok(())
}

/// A source can be a target, and everything about how and where it runs is the
/// exec driver's existing surface. Its `credential` output group is read as JSON
/// into fields — one convention, not configuration.
#[tokio::test]
async fn a_target_source_yields_its_credential_group_as_fields() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"
target(name = "fetch", driver = "bash", cache = False,
       out = {"credential": "cred.json"},
       run = ["printf '{\"token\":\"from-a-target\",\"expires_in\":600}' > cred.json"])
target(name = "t", driver = "credential", sources = ["//auth:fetch"],
       present = {"env": {"T": "${token}"}})
"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$T\" > o.txt"])"#,
    );
    assert_eq!(
        common::artifact_string(&*ws.run("//app:a").await?),
        "from-a-target"
    );
    Ok(())
}

/// A credential naming another credential is a delegation: take that one's
/// material, apply this one's presentation. No syntax of its own — the driver of
/// the referenced target decides.
#[tokio::test]
async fn a_credential_can_delegate_to_another() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_ROOT_TOKEN", "root-material");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"
target(name = "root", driver = "credential",
       sources = [heph.auth.env(["HEPH_E2E_ROOT_TOKEN"])],
       present = {"env": {"ROOT": "${heph_e2e_root_token}"}})
target(name = "derived", driver = "credential", sources = ["//auth:root"],
       present = {"env": {"DERIVED": "${heph_e2e_root_token}"}})
"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:derived"],
       run = ["printf '%s|%s' \"$DERIVED\" \"${ROOT:-unset}\" > o.txt"])"#,
    );
    // The consumer gets the *derived* presentation and only that: delegation
    // takes the material, not the parent's variable names.
    assert_eq!(
        common::artifact_string(&*ws.run("//app:a").await?),
        "root-material|unset"
    );
    Ok(())
}

/// A source that shells out to a secret manager has to authenticate to the secret
/// manager. That is not a second, private notion of "how the credential tool logs
/// in" — it is the same presentation machinery, with the acquire subprocess as
/// just another consumer.
#[tokio::test]
async fn a_source_can_declare_its_own_credentials() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_VAULT_TOKEN", "vault-login");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"
target(name = "vault", driver = "credential",
       sources = [heph.auth.env(["HEPH_E2E_VAULT_TOKEN"])],
       present = {"env": {"VAULT_TOKEN": "${heph_e2e_vault_token}"}})

# The acquire command prints whatever identity it was handed, which is exactly
# what a real `vault read` does with the token it was given.
target(name = "cf", driver = "credential",
       sources = [heph.auth.exec(
           ["sh", "-c", "printf '{\"token\":\"scoped-for-%s\"}' \"$VAULT_TOKEN\""],
           fields = {"token": "token"},
           credentials = ["//auth:vault"])],
       present = {"env": {"CF": "${token}"}})
"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:cf"], run = ["printf '%s' \"$CF\" > o.txt"])"#,
    );
    assert_eq!(
        common::artifact_string(&*ws.run("//app:a").await?),
        "scoped-for-vault-login"
    );
    Ok(())
}

/// One acquisition serves every **concurrent** consumer. This is a
/// single-flighting property, not a memoization one — running the consumers one
/// after another would prove only that the second read a cache the first filled,
/// which is the easy half.
///
/// The command sleeps, so without single-flighting all five would be in flight at
/// once and the counter would show five.
#[tokio::test]
async fn one_acquisition_serves_every_concurrent_consumer() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let counter = ws.dir.path().join("concurrent-acquisitions");
    ws.write_build_file(
        "auth",
        &format!(
            r#"
target(name = "t", driver = "credential",
       sources = [heph.auth.exec(["sh", "-c",
           "sleep 0.3; echo x >> {c}; printf '{{\"token\":\"once\"}}'"],
           fields = {{"token": "token"}})],
       present = {{"env": {{"T": "${{token}}"}}}})
"#,
            c = counter.display()
        ),
    );
    let consumers: String = (0..5)
        .map(|i| {
            format!(
                r#"target(name = "c{i}", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$T\" > o.txt"])
"#
            )
        })
        .collect();
    ws.write_build_file("app", &consumers);

    let runs = (0..5).map(|i| {
        let ws = &ws;
        async move { ws.run(&format!("//app:c{i}")).await }
    });
    for r in futures::future::try_join_all(runs).await? {
        assert_eq!(common::artifact_string(&r), "once");
    }
    let acquisitions = std::fs::read_to_string(&counter).unwrap_or_default();
    assert_eq!(
        acquisitions.lines().count(),
        1,
        "five concurrent consumers must cause one acquisition, got {acquisitions:?}"
    );
    Ok(())
}

/// Material that has lapsed is re-acquired rather than handed on.
///
/// This is the whole reason the process cache is not the engine's `Memoizer`: a
/// memoized cell is computed once and kept forever, so a long build with a
/// short-lived token would hand a target starting later material that expired
/// before it did.
#[tokio::test]
async fn lapsed_material_is_re_acquired_rather_than_handed_on() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let counter = ws.dir.path().join("refreshes");
    // A one-second lifetime. `usable_at`'s margin is `min(60s, lifetime/2)`, so
    // this is usable for about half a second and then is not.
    ws.write_build_file(
        "auth",
        &format!(
            r#"
target(name = "t", driver = "credential",
       sources = [heph.auth.exec(["sh", "-c",
           "echo x >> {c}; printf '{{\"token\":\"v\",\"expires_in\":1}}'"],
           fields = {{"token": "token"}}, expires = "expires_in")],
       present = {{"env": {{"T": "${{token}}"}}}})
"#,
            c = counter.display()
        ),
    );
    ws.write_build_file(
        "app",
        r#"
target(name = "first", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$T\" > o.txt"])
target(name = "second", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$T\" > o.txt"])
"#,
    );
    ws.run("//app:first").await?;
    assert_eq!(
        std::fs::read_to_string(&counter)
            .unwrap_or_default()
            .lines()
            .count(),
        1
    );
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    ws.run("//app:second").await?;
    assert_eq!(
        std::fs::read_to_string(&counter)
            .unwrap_or_default()
            .lines()
            .count(),
        2,
        "a target starting after the material lapsed must not be handed it"
    );
    Ok(())
}

/// A second `heph` process reuses the first's material rather than re-acquiring
/// it — which is what "sign in once a day" actually means.
#[tokio::test]
async fn a_second_process_reuses_the_disk_tier() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let counter = ws.dir.path().join("cross-process");
    ws.write_build_file(
        "auth",
        &format!(
            r#"
target(name = "t", driver = "credential",
       sources = [heph.auth.exec(["sh", "-c",
           "echo x >> {c}; printf '{{\"token\":\"v\",\"expires_in\":3600}}'"],
           fields = {{"token": "token"}}, expires = "expires_in")],
       present = {{"env": {{"T": "${{token}}"}}}})
"#,
            c = counter.display()
        ),
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$T\" > o.txt"])"#,
    );
    ws.run("//app:a").await?;

    // A second engine over the same on-disk store: what the next `heph`
    // invocation sees. Its process tier starts empty, so a hit here can only
    // have come from disk.
    let engine = ws.reopen()?;
    let addr = heph::htaddr::parse_addr("//app:a")?;
    Arc::clone(&engine)
        .result_addr(
            engine.new_state(),
            &addr,
            heph::engine::OutputMatcher::All,
            &heph::engine::ResultOptions::default(),
        )
        .await?;
    let acquisitions = std::fs::read_to_string(&counter).unwrap_or_default();
    assert_eq!(
        acquisitions.lines().count(),
        1,
        "a second process must reuse the disk tier, got {acquisitions:?}"
    );
    Ok(())
}

/// A producer's non-`credential` output groups are material too, so the
/// no-expiry-at-rest rule covers them.
#[tokio::test]
async fn a_producers_files_are_not_left_at_rest_without_an_expiry() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"
target(name = "mint", driver = "bash", cache = False,
       out = {"kubeconfig": "kc.yaml"},
       run = ["printf 'token: unbounded-producer-secret' > kc.yaml"])
target(name = "t", driver = "credential", sources = ["//auth:mint"],
       present = {"env": {"KUBECONFIG": "${file:kubeconfig}"}})
"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["cat \"$KUBECONFIG\" > o.txt"])"#,
    );
    // The material still reaches the target …
    assert_eq!(
        common::artifact_string(&*ws.run("//app:a").await?),
        "token: unbounded-producer-secret"
    );
    // … and the only copy at rest is the per-process staging directory, which
    // the next `heph` sweeps. Nothing sits in the durable half of the store.
    let auth = ws.dir.path().join(".heph3").join("auth");
    let live = auth.join("files").join("live");
    let mut staged = 0usize;
    for f in walkdir(&auth) {
        let body = String::from_utf8_lossy(&std::fs::read(&f).unwrap_or_default()).into_owned();
        if !body.contains("unbounded-producer-secret") {
            continue;
        }
        assert!(
            f.starts_with(&live),
            "unbounded material must not be promoted out of staging: {}",
            f.display()
        );
        staged += 1;
    }
    assert_eq!(staged, 1, "the staged copy should exist exactly once");
    Ok(())
}

/// The same producer with a `ttl` **is** kept: a declared lifetime is what makes
/// material cacheable at all, and is the difference between the two halves of the
/// store.
#[tokio::test]
async fn a_producers_files_are_kept_once_a_ttl_gives_them_a_lifetime() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"
target(name = "mint", driver = "bash", cache = False,
       out = {"kubeconfig": "kc.yaml"},
       run = ["printf 'token: bounded-producer-secret' > kc.yaml"])
target(name = "t", driver = "credential", sources = ["//auth:mint"],
       ttl = "6h",
       present = {"env": {"KUBECONFIG": "${file:kubeconfig}"}})
"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["cat \"$KUBECONFIG\" > o.txt"])"#,
    );
    assert_eq!(
        common::artifact_string(&*ws.run("//app:a").await?),
        "token: bounded-producer-secret"
    );
    let live = ws
        .dir
        .path()
        .join(".heph3")
        .join("auth")
        .join("files")
        .join("live");
    let durable = walkdir(&ws.dir.path().join(".heph3").join("auth"))
        .into_iter()
        .filter(|f| !f.starts_with(&live))
        .any(|f| {
            String::from_utf8_lossy(&std::fs::read(&f).unwrap_or_default())
                .contains("bounded-producer-secret")
        });
    assert!(durable, "material with a declared lifetime is kept");
    Ok(())
}

/// One acquisition serves every consumer in a run. An `exec` source shelling out
/// to a vendor CLI costs the better part of a second, and two hundred consumers
/// must not pay it two hundred times.
#[tokio::test]
async fn one_acquisition_serves_every_consumer() -> anyhow::Result<()> {
    let ws = Workspace::new();
    // The command appends to a counter file, so the number of acquisitions is
    // observable from outside.
    let counter = ws.dir.path().join("acquisitions");
    ws.write_build_file(
        "auth",
        &format!(
            r#"
target(name = "t", driver = "credential",
       sources = [heph.auth.exec(["sh", "-c",
           "echo x >> {c}; printf '{{\"token\":\"once\"}}'"],
           fields = {{"token": "token"}})],
       present = {{"env": {{"T": "${{token}}"}}}})
"#,
            c = counter.display()
        ),
    );
    let consumers: String = (0..5)
        .map(|i| {
            format!(
                r#"target(name = "a{i}", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$T\" > o.txt"])
"#
            )
        })
        .collect();
    ws.write_build_file("app", &consumers);

    for i in 0..5 {
        assert_eq!(
            common::artifact_string(&*ws.run(&format!("//app:a{i}")).await?),
            "once"
        );
    }
    let runs = std::fs::read_to_string(&counter).unwrap_or_default();
    assert_eq!(
        runs.lines().count(),
        1,
        "five consumers must cause one acquisition, got {runs:?}"
    );
    Ok(())
}

/// A cache hit acquires nothing. That placement is what makes the "zero cost on a
/// hit" claim structural rather than aspirational: a fully cached build never
/// reaches the acquisition step at all.
#[tokio::test]
async fn a_cache_hit_acquires_nothing() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let counter = ws.dir.path().join("acquisitions");
    ws.write_build_file(
        "auth",
        &format!(
            r#"
target(name = "t", driver = "credential",
       sources = [heph.auth.exec(["sh", "-c",
           "echo x >> {c}; printf '{{\"token\":\"v\"}}'"],
           fields = {{"token": "token"}})],
       present = {{"env": {{"T": "${{token}}"}}}})
"#,
            c = counter.display()
        ),
    );
    // Cacheable, deliberately: the output does not depend on the identity.
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt",
       credentials = ["//auth:t"], run = ["printf 'constant' > o.txt"])"#,
    );

    ws.run("//app:a").await?;
    let after_first = std::fs::read_to_string(&counter).unwrap_or_default();
    assert_eq!(after_first.lines().count(), 1);

    // A second engine over the same on-disk cache: the next `heph` invocation.
    let engine = ws.reopen()?;
    let addr = heph::htaddr::parse_addr("//app:a")?;
    Arc::clone(&engine)
        .result_addr(
            engine.new_state(),
            &addr,
            heph::engine::OutputMatcher::All,
            &heph::engine::ResultOptions::default(),
        )
        .await?;
    let after_second = std::fs::read_to_string(&counter).unwrap_or_default();
    assert_eq!(
        after_second.lines().count(),
        1,
        "a cache hit must not acquire, got {after_second:?}"
    );
    Ok(())
}
