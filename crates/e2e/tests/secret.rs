#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction lint scoped to production code; tests are exempt"
)]

//! End-to-end coverage for the `secret` declaration driver.
//!
//! These go through the real `Engine` — provider, BUILD-file evaluation, driver
//! registry, `get_def`, `hashin` — rather than calling the driver directly,
//! because what is being tested is the *contract*, and the contract is about
//! cache keys. The one property everything else rests on:
//!
//! > Swapping how a credential is obtained must not move any consumer's
//! > `hashin`. Changing what identity it names must.
//!
//! Get that wrong in the first direction and CI and a laptop never share a cache
//! entry, which is `pass_env`'s disease moved one level up. Get it wrong in the
//! second and two identities share artifacts.
//!
//! Minting, shapes and delivery are not here — a declaration is inert on its
//! own, exactly like a `scratch`.

mod common;

use common::Workspace;
use hsecrets::descriptor::{Descriptor, Source};

/// The resolved `runner` of an acquire entry, which now lives inside the
/// `exec` variant rather than beside it.
fn runner_of(d: &Descriptor, index: usize) -> Option<String> {
    match d.acquire.get(index).map(|a| &a.source) {
        Some(Source::Exec { runner, .. }) => runner.clone(),
        _ => None,
    }
}

/// The driver is registered and a declaration resolves. Without this, everything
/// downstream fails with "driver not found" and no test says why.
#[tokio::test]
async fn a_secret_declaration_resolves_through_the_engine() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"
target(
    name = "ecr",
    driver = "secret",
    role = "arn:aws:iam::4711:role/heph-ci-push",
    region = "eu-west-1",
    shape = ["aws_profile"],
    profile = "ecr",
    provider = "oidc",
    exchange = {"kind": "aws_sts"},
)
"#,
    );

    let spec = ws.get_spec("//creds:ecr").await?;
    assert_eq!(spec.driver, "secret");

    let desc = heph::pluginsecret::parse_declaration(&spec)?;
    assert_eq!(
        desc.identity.role.as_deref(),
        Some("arn:aws:iam::4711:role/heph-ci-push")
    );
    assert_eq!(desc.acquire.len(), 1);
    Ok(())
}

/// The descriptor builds, and what it produces is the identity half only.
#[tokio::test]
async fn a_secret_target_emits_an_identity_only_secret_json() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"
target(
    name = "github",
    driver = "secret",
    machine = "github.com",
    shape = ["netrc"],
    provider = "exec",
    protocol = "raw",
    helper = ["gh", "auth", "token"],
    ttl = "1h",
)
"#,
    );

    let out = common::artifact_string(&*ws.run("//creds:github").await?);
    assert!(out.contains("github.com"), "{out}");
    assert!(out.contains("\"version\": 1"), "{out}");

    // The acquisition half must not be in the artifact: it is what a consumer's
    // hashout is computed over, and hashing it would partition the cache by
    // *how* a credential was fetched.
    for leaked in ["helper", "gh", "protocol", "raw", "ttl", "1h", "exec"] {
        assert!(
            !out.contains(leaked),
            "acquisition field {leaked:?} reached the artifact:\n{out}"
        );
    }
    Ok(())
}

/// The load-bearing property, measured where it actually matters: on a
/// *consumer's* `hashin`, through the real engine.
///
/// CI federates; a laptop shells out to a vendor CLI under a runner, with a
/// different TTL. Same identity. Every consumer must keep its cache entry.
#[tokio::test]
async fn swapping_the_acquisition_half_does_not_move_a_consumers_hashin() -> anyhow::Result<()> {
    let ci = r#"
target(
    name = "artifacts",
    driver = "secret",
    role = "arn:aws:iam::4711:role/heph-read",
    region = "eu-west-1",
    shape = ["aws_profile"],
    profile = "artifacts",
    provider = "oidc",
    exchange = {"kind": "aws_sts"},
)
"#;
    let laptop = r#"
target(
    name = "artifacts",
    driver = "secret",
    role = "arn:aws:iam::4711:role/heph-read",
    region = "eu-west-1",
    shape = ["aws_profile"],
    profile = "artifacts",
    provider = "exec",
    protocol = "credential_process",
    helper = ["aws", "configure", "export-credentials", "--format", "process"],
    runner = "//tools/devenv:runner",
    ttl = "30m",
)
"#;

    let hashin_of = async |decl: &str| -> anyhow::Result<String> {
        let ws = Workspace::new();
        ws.write_build_file("creds", decl);
        ws.write_build_file(
            "app",
            r#"target(name = "build", driver = "bash", out = "o.txt",
       hash_deps = ["//creds:artifacts"], run = ["echo hi > o.txt"])"#,
        );
        // The consumer's own hashin is what a cache entry is keyed by.
        ws.hashin("//app:build").await
    };

    assert_eq!(
        hashin_of(ci).await?,
        hashin_of(laptop).await?,
        "swapping only the acquisition half moved a consumer's cache key — CI and laptops can \
         no longer share a cache entry, which is the bug this design exists to prevent"
    );
    Ok(())
}

/// The other half of the same contract: an identity change *must* re-key, or two
/// identities silently share artifacts.
#[tokio::test]
async fn changing_the_identity_half_moves_a_consumers_hashin() -> anyhow::Result<()> {
    let decl = |role: &str| {
        format!(
            r#"
target(
    name = "artifacts",
    driver = "secret",
    role = "{role}",
    provider = "static_env",
    var = "TOKEN",
)
"#
        )
    };

    let hashin_of = async |role: &str| -> anyhow::Result<String> {
        let ws = Workspace::new();
        ws.write_build_file("creds", &decl(role));
        ws.write_build_file(
            "app",
            r#"target(name = "build", driver = "bash", out = "o.txt",
       hash_deps = ["//creds:artifacts"], run = ["echo hi > o.txt"])"#,
        );
        ws.hashin("//app:build").await
    };

    assert_ne!(
        hashin_of("arn:aws:iam::4711:role/read").await?,
        hashin_of("arn:aws:iam::4711:role/write").await?,
        "two identities produced one cache key"
    );
    Ok(())
}

/// A shape is identity, so it re-keys. Stated as a test because it is the one
/// field whose side of the line is genuinely arguable: it is here because a
/// shape decides which files and variables exist in the sandbox, and that is
/// part of what the target reads.
#[tokio::test]
async fn changing_a_shape_moves_a_consumers_hashin() -> anyhow::Result<()> {
    let decl = |shape: &str| {
        format!(
            r#"
target(
    name = "c",
    driver = "secret",
    machine = "github.com",
    shape = ["{shape}"],
    env = {{"GH_TOKEN": "$."}},
    provider = "static_env",
    var = "TOKEN",
)
"#
        )
    };

    let hashin_of = async |shape: &str| -> anyhow::Result<String> {
        let ws = Workspace::new();
        ws.write_build_file("creds", &decl(shape));
        ws.write_build_file(
            "app",
            r#"target(name = "build", driver = "bash", out = "o.txt",
       hash_deps = ["//creds:c"], run = ["echo hi > o.txt"])"#,
        );
        ws.hashin("//app:build").await
    };

    assert_ne!(hashin_of("netrc").await?, hashin_of("env").await?);
    Ok(())
}

/// **The regression test for a cache-poisoning bug.**
///
/// Two descriptors whose identities are equal — which is the *default*, since
/// every `Identity` field is optional — emitted byte-identical `secret.json`,
/// so their hashouts matched and a consumer of one was a cache hit on the
/// other's artifact. Measured before the fix: `//app:fetch_prod` and
/// `//app:fetch_stg` shared the key `90e184a271c23ab8`, so whichever ran second
/// was handed output built with the other's credential — no execution, no
/// warning, and the winner decided by scheduling order.
///
/// This is the test that belongs beside the two hashin tests above: they cover
/// "same identity, different acquisition → same key" and "different identity →
/// different key", and neither can see this.
#[tokio::test]
async fn two_descriptors_differing_only_in_acquisition_do_not_share_a_key() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"
target(name = "prod", driver = "secret", provider = "static_env", var = "PROD_API_KEY")
target(name = "staging", driver = "secret", provider = "static_env", var = "STAGING_API_KEY")
"#,
    );
    ws.write_build_file(
        "app",
        r#"
target(name = "fetch_prod", driver = "bash", out = "o.txt",
       hash_deps = ["//creds:prod"], run = ["echo hi > o.txt"])
target(name = "fetch_stg", driver = "bash", out = "o.txt",
       hash_deps = ["//creds:staging"], run = ["echo hi > o.txt"])
"#,
    );

    assert_ne!(
        ws.hashin("//app:fetch_prod").await?,
        ws.hashin("//app:fetch_stg").await?,
        "a production-credential consumer and a staging-credential consumer share one cache          key; whichever built first would serve its artifact to the other"
    );
    Ok(())
}

/// Renaming or moving a descriptor re-keys its consumers. That is the price of
/// the address being in the key, and it is the safe direction — but it is a
/// decision, so it is pinned rather than left to be discovered.
#[tokio::test]
async fn moving_a_descriptor_re_keys_its_consumers() -> anyhow::Result<()> {
    let hashin_of = async |pkg: &str| -> anyhow::Result<String> {
        let ws = Workspace::new();
        ws.write_build_file(
            pkg,
            r#"target(name = "c", driver = "secret", provider = "static_env", var = "TOKEN")"#,
        );
        ws.write_build_file(
            "app",
            &format!(
                r#"target(name = "build", driver = "bash", out = "o.txt",
       hash_deps = ["//{pkg}:c"], run = ["echo hi > o.txt"])"#
            ),
        );
        ws.hashin("//app:build").await
    };
    assert_ne!(hashin_of("creds").await?, hashin_of("infra/creds").await?);
    Ok(())
}

/// `runner` is an address like any other, so a malformed one must fail at the
/// declaration — it used to parse, validate, and survive all the way to the
/// broker. And `"local"`, documented as the explicit opt-out, must actually
/// mean it rather than being carried through as a literal nobody parses.
#[tokio::test]
async fn an_acquire_runner_is_validated_and_local_means_local() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"
target(name = "bad", driver = "secret", provider = "exec", protocol = "raw",
       helper = ["gh", "auth", "token"], runner = "not an addr!!")
target(name = "local", driver = "secret", provider = "exec", protocol = "raw",
       helper = ["gh", "auth", "token"], runner = "local")
target(name = "rel", driver = "secret", provider = "exec", protocol = "raw",
       helper = ["gh", "auth", "token"], runner = ":devenv")
"#,
    );

    let bad = ws.get_spec("//creds:bad").await?;
    let err = heph::pluginsecret::parse_declaration(&bad).expect_err("must reject");
    let msg = format!("{err:#}");
    assert!(msg.contains("not an addr!!"), "{msg}");
    assert!(msg.contains("runner.json"), "{msg}");

    let local = ws.get_spec("//creds:local").await?;
    let desc = heph::pluginsecret::parse_declaration(&local)?;
    assert!(
        runner_of(&desc, 0).is_none(),
        "`local` must resolve to no runner, not to the literal string"
    );

    // A relative address resolves against the declaring package.
    let rel = ws.get_spec("//creds:rel").await?;
    let desc = heph::pluginsecret::parse_declaration(&rel)?;
    assert_eq!(runner_of(&desc, 0).as_deref(), Some("//creds:devenv"));
    Ok(())
}

/// A bad declaration must fail where a BUILD author will see it, not at target
/// 400 of a build as a missing-credential error.
#[tokio::test]
async fn a_declaration_with_no_acquisition_fails_at_the_declaration() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"target(name = "c", driver = "secret", role = "arn:aws:iam::4711:role/x")"#,
    );
    let spec = ws.get_spec("//creds:c").await?;
    let err = heph::pluginsecret::parse_declaration(&spec).expect_err("must reject");
    let msg = format!("{err:#}");
    assert!(msg.contains("no way to acquire"), "{msg}");
    Ok(())
}

/// An unguarded `acquire` entry always matches, so anything after it is dead.
/// Caught at the declaration rather than discovered when a route never runs.
#[tokio::test]
async fn an_unreachable_acquire_entry_is_rejected() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"
target(
    name = "c",
    driver = "secret",
    acquire = [
        {"provider": "static_env", "var": "A"},
        {"when_env": "GITHUB_ACTIONS", "provider": "static_env", "var": "B"},
    ],
)
"#,
    );
    let spec = ws.get_spec("//creds:c").await?;
    let err = heph::pluginsecret::parse_declaration(&spec).expect_err("must reject");
    let msg = format!("{err:#}");
    assert!(msg.contains("must come last"), "{msg}");
    Ok(())
}

/// The `acquire` list survives BUILD-file evaluation with both guard forms
/// intact — a dict inside a list inside a target config is the deepest shape
/// this driver's schema has, and it is the one a Starlark round-trip is most
/// likely to flatten.
#[tokio::test]
async fn an_acquire_list_round_trips_through_starlark() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"
target(
    name = "artifacts",
    driver = "secret",
    role = "arn:aws:iam::4711:role/heph-read",
    acquire = [
        {"when_env": "GITHUB_ACTIONS", "provider": "oidc",
         "exchange": {"kind": "aws_sts"}},
        {"when_env": {"CI": "true"}, "provider": "static_env", "var": "TOK"},
        {"provider": "exec", "protocol": "credential_process",
         "helper": ["aws", "configure", "export-credentials"],
         "runner": "//tools/devenv:runner"},
    ],
)
"#,
    );

    let spec = ws.get_spec("//creds:artifacts").await?;
    let desc = heph::pluginsecret::parse_declaration(&spec)?;
    assert_eq!(desc.acquire.len(), 3);

    // In CI the first entry wins; on a bare laptop the unguarded catch-all does.
    let ci = desc.select(&|k: &str| (k == "GITHUB_ACTIONS").then(|| "true".to_string()))?;
    assert_eq!(ci.index, 0);

    let laptop = desc.select(&|_: &str| None)?;
    assert_eq!(laptop.index, 2);
    assert_eq!(
        runner_of(&desc, laptop.index).as_deref(),
        Some("//tools/devenv:runner"),
        "the acquire entry's runner must survive the round trip"
    );
    Ok(())
}

/// The route taken is otherwise invisible, so a descriptor that matches nothing
/// must say what it looked for and what it found — not report a missing
/// credential three layers down.
#[tokio::test]
async fn no_matching_route_reports_every_guard_and_its_observed_state() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"
target(
    name = "c",
    driver = "secret",
    acquire = [
        {"when_env": "GITHUB_ACTIONS", "provider": "static_env", "var": "A"},
        {"when_env": "BUILDKITE", "provider": "static_env", "var": "B"},
    ],
)
"#,
    );
    let spec = ws.get_spec("//creds:c").await?;
    let desc = heph::pluginsecret::parse_declaration(&spec)?;

    // GITHUB_ACTIONS set-but-empty is how CI systems spell "off".
    let err = desc
        .select(&|k: &str| (k == "GITHUB_ACTIONS").then(String::new))
        .expect_err("no route");
    let msg = format!("{err:#}");
    assert!(msg.contains("set but empty"), "{msg}");
    assert!(msg.contains("BUILDKITE is unset"), "{msg}");
    Ok(())
}

/// A descriptor holds no credential, but it does hold role ARNs, account
/// numbers and internal endpoints — and it is trivially cheap to rebuild.
#[tokio::test]
async fn a_descriptor_is_not_shared_to_the_remote_cache() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"target(name = "c", driver = "secret", provider = "static_env", var = "TOKEN")"#,
    );
    let def = ws.get_def("//creds:c").await?;
    assert!(
        !def.target_def.cache.remote_enabled,
        "a descriptor must not default to the shared remote cache"
    );
    Ok(())
}
