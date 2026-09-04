#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction lint scoped to production code; tests are exempt"
)]

//! The examples in `docs/SECRETS.md` and the design proposal, executed.
//!
//! Every declaration here is copied verbatim from documentation. A published
//! example that does not parse is worse than no example: it is read as a
//! promise, tried once, and then the whole document is distrusted. So each one
//! goes through the real BUILD-file evaluator and the real declaration parser,
//! and the ones whose credentials can actually be minted today go through the
//! real broker with a real helper subprocess.
//!
//! This file is also the honest boundary of what is wired. `static_env` and
//! `exec` mint; `oidc` parses and validates but has no provider registered, and
//! the test at the bottom pins the error you get for asking — because "it is in
//! the docs" and "it runs" are different claims and the difference belongs in a
//! test rather than in a reader's afternoon.

mod common;

use common::Workspace;
use hsecrets::broker::{Broker, BrokerCtx};
use hsecrets::descriptor::{Descriptor, Source};
use hsecrets::provider::ProviderRegistry;
use std::sync::Arc;
use std::time::SystemTime;

/// Evaluate one BUILD declaration and parse it the way the broker would.
async fn declare(body: &str) -> anyhow::Result<Descriptor> {
    let ws = Workspace::new();
    ws.write_build_file("creds", body);
    let spec = ws.get_spec("//creds:c").await?;
    heph::pluginsecret::parse_declaration(&spec)
}

/// Mint a descriptor through the real broker and real providers.
async fn mint(desc: &Descriptor, env: &[(&str, &str)]) -> anyhow::Result<String> {
    let owned: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let lookup = move |k: &str| {
        owned
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.to_string())
    };
    let token = hcore::hasync::StdCancellationToken::new();
    let broker = Broker::new(Arc::new(ProviderRegistry::with_builtins()?));
    let ctx = BrokerCtx {
        now: SystemTime::now(),
        env: &lookup,
        ctoken: &token,
        request_id: "test",
        runner: None,
        cwd: std::path::Path::new("."),
        auth: None,
    };
    let cred = broker.mint(desc, "c", &ctx).await?;
    Ok(cred.resolve_pointer("$.")?.expose().to_string())
}

// ---------------------------------------------------------------- works today

/// Migrating one variable off `pass_env`. The smallest useful declaration, and
/// the one every workspace starts with.
#[tokio::test]
async fn static_env_declares_parses_and_mints() -> anyhow::Result<()> {
    let desc = declare(
        r#"
target(
    name = "c",
    driver = "secret",
    machine = "registry.npmjs.org",
    shape = ["netrc"],
    provider = "static_env",
    var = "NPM_TOKEN",
)
"#,
    )
    .await?;

    assert_eq!(desc.identity.machine.as_deref(), Some("registry.npmjs.org"));
    assert_eq!(
        mint(&desc, &[("NPM_TOKEN", "npm_a_token_value_here")]).await?,
        "npm_a_token_value_here"
    );
    Ok(())
}

/// A laptop credential from a CLI the developer is already signed into. This is
/// the interim path the adoption section recommends, and it needs no IAM ask.
#[tokio::test]
async fn an_exec_raw_helper_declares_parses_and_mints() -> anyhow::Result<()> {
    let desc = declare(
        r#"
target(
    name = "c",
    driver = "secret",
    machine = "github.com",
    shape = ["netrc", "git_credential"],
    provider = "exec",
    protocol = "raw",
    helper = ["/bin/echo", "ghs_a_token_from_the_helper"],
    ttl = "1h",
)
"#,
    )
    .await?;

    assert_eq!(desc.identity.shape, vec!["git_credential", "netrc"]);
    assert_eq!(mint(&desc, &[]).await?, "ghs_a_token_from_the_helper");
    Ok(())
}

/// The AWS laptop path: an SSO'd CLI exporting `credential_process` JSON. The
/// helper here stands in for `aws configure export-credentials`, which cannot
/// run in a test, but the protocol parsing is the real thing.
#[tokio::test]
async fn an_exec_credential_process_helper_mints_aws_fields() -> anyhow::Result<()> {
    let desc = declare(
        r#"
target(
    name = "c",
    driver = "secret",
    role = "arn:aws:iam::4711:role/heph-read",
    params = {"region": "eu-west-1"},
    shape = ["aws_profile"],
    profile = "artifacts",
    provider = "exec",
    protocol = "credential_process",
    helper = ["/bin/sh", "-c",
              "printf '{\"Version\":1,\"AccessKeyId\":\"ASIAEXAMPLE\",\"SecretAccessKey\":\"s3cret\",\"SessionToken\":\"tok\"}'"],
)
"#,
    )
    .await?;

    let token = hcore::hasync::StdCancellationToken::new();
    let broker = Broker::new(Arc::new(ProviderRegistry::with_builtins()?));
    let lookup = |_: &str| None;
    let cred = broker
        .mint(
            &desc,
            "c",
            &BrokerCtx {
                now: SystemTime::now(),
                env: &lookup,
                ctoken: &token,
                request_id: "test",
                runner: None,
                cwd: std::path::Path::new("."),
                auth: None,
            },
        )
        .await?;

    assert_eq!(
        cred.get("AccessKeyId").expect("key").expose(),
        "ASIAEXAMPLE"
    );
    assert_eq!(cred.get("SessionToken").expect("tok").expose(), "tok");
    Ok(())
}

/// A registry credential from a Docker credential helper — the protocol whose
/// stdin is a bare URL rather than JSON, which is the detail most often got
/// wrong.
#[tokio::test]
async fn an_exec_docker_credential_helper_mints() -> anyhow::Result<()> {
    let desc = declare(
        r#"
target(
    name = "c",
    driver = "secret",
    registry = "ghcr.io",
    shape = ["docker_config"],
    provider = "exec",
    protocol = "docker_credential",
    helper = ["/bin/sh", "-c",
              "printf '{\"ServerURL\":\"%s\",\"Username\":\"<token>\",\"Secret\":\"ghs_registry\"}' \"$(cat)\""],
)
"#,
    )
    .await?;

    assert_eq!(mint(&desc, &[]).await?, "ghs_registry");
    // The helper echoed back what arrived on stdin, so this also proves the
    // registry host crossed as a bare URL.
    let token = hcore::hasync::StdCancellationToken::new();
    let broker = Broker::new(Arc::new(ProviderRegistry::with_builtins()?));
    let lookup = |_: &str| None;
    let cred = broker
        .mint(
            &desc,
            "c",
            &BrokerCtx {
                now: SystemTime::now(),
                env: &lookup,
                ctoken: &token,
                request_id: "test",
                runner: None,
                cwd: std::path::Path::new("."),
                auth: None,
            },
        )
        .await?;
    assert_eq!(cred.get("ServerURL").expect("url").expose(), "ghcr.io");
    Ok(())
}

/// Two routes for one identity, selected by an environment variable. The route
/// taken must be the laptop one off CI, and the CI one on it.
#[tokio::test]
async fn a_two_route_descriptor_selects_by_environment() -> anyhow::Result<()> {
    let desc = declare(
        r#"
target(
    name = "c",
    driver = "secret",
    role = "arn:aws:iam::4711:role/heph-read",
    params = {"region": "eu-west-1"},
    shape = ["aws_profile"],
    profile = "artifacts",
    acquire = [
        {"when_env": "GITHUB_ACTIONS", "provider": "oidc",
         "exchange": {"kind": "aws_sts"}},
        {"provider": "exec", "protocol": "credential_process",
         "helper": ["aws", "configure", "export-credentials",
                    "--profile", "sso", "--format", "process"]},
    ],
)
"#,
    )
    .await?;

    let ci = desc.select(&|k: &str| (k == "GITHUB_ACTIONS").then(|| "true".to_string()))?;
    assert_eq!(ci.index, 0);
    assert!(matches!(ci.entry.source, Source::Oidc {}));

    let laptop = desc.select(&|_: &str| None)?;
    assert_eq!(laptop.index, 1);
    assert!(matches!(laptop.entry.source, Source::Exec { .. }));
    Ok(())
}

/// The vendor-REST exchange, which is what a GitHub App installation token is.
/// It parses and validates with no vendor support in heph at all — that is the
/// whole claim of the standards-first model.
#[tokio::test]
async fn a_vendor_rest_exchange_parses_with_no_vendor_support() -> anyhow::Result<()> {
    let desc = declare(
        r#"
target(
    name = "c",
    driver = "secret",
    machine = "github.com",
    params = {"app_id": "1180022", "install": "org/heph"},
    shape = ["git_credential", "netrc"],
    provider = "oidc",
    exchange = {
        "kind": "http",
        "url": "https://api.github.com/app/installations/42/access_tokens",
        "fields": {"token": "/token"},
    },
)
"#,
    )
    .await?;

    assert_eq!(
        desc.identity.params.get("app_id").map(String::as_str),
        Some("1180022")
    );
    assert_eq!(desc.acquire.first().map(|a| a.exchange.len()), Some(1));
    Ok(())
}

/// GCP federation is two hops, so an exchange is a pipeline.
#[tokio::test]
async fn an_exchange_pipeline_parses_in_order() -> anyhow::Result<()> {
    let desc = declare(
        r#"
target(
    name = "c",
    driver = "secret",
    audience = "//iam.googleapis.com/projects/8801/locations/global/workloadIdentityPools/ci/providers/gh",
    params = {"impersonate": "heph-push@proj.iam.gserviceaccount.com"},
    scope = ["https://www.googleapis.com/auth/cloud-platform"],
    registry = "europe-west1-docker.pkg.dev",
    shape = ["gcloud_adc", "docker_config"],
    provider = "oidc",
    exchange = [
        {"kind": "token_exchange", "issuer": "https://sts.googleapis.com"},
        {"kind": "http",
         "url": "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/x:generateAccessToken"},
    ],
)
"#,
    )
    .await?;

    assert_eq!(desc.acquire.first().map(|a| a.exchange.len()), Some(2));
    Ok(())
}

/// An OAuth grant names an **issuer**, and the token endpoint is discovered
/// from the metadata document every IdP publishes. That is the standard, and
/// it is what `heph auth login` already does for its own endpoints — an
/// exchange had no reason to work differently.
#[tokio::test]
async fn a_grant_discovers_its_endpoint_from_the_issuer() -> anyhow::Result<()> {
    let desc = declare(
        r#"
target(
    name = "c",
    driver = "secret",
    audience = "heph.org.example",
    provider = "oidc",
    exchange = {"kind": "token_exchange", "issuer": "https://org.okta.com/oauth2/default"},
)
"#,
    )
    .await?;

    let step = desc
        .acquire
        .first()
        .and_then(|a| a.exchange.first())
        .expect("one step");
    assert_eq!(
        step.endpoint()?.and_then(|e| e.discovery_url()).as_deref(),
        Some("https://org.okta.com/oauth2/default/.well-known/openid-configuration")
    );
    Ok(())
}

/// A grant naming neither an issuer nor an endpoint — or both — fails at the
/// declaration, where a BUILD author sees it.
#[tokio::test]
async fn a_grant_with_no_destination_is_refused_at_the_declaration() -> anyhow::Result<()> {
    let err = declare(
        r#"
target(
    name = "c",
    driver = "secret",
    provider = "oidc",
    exchange = {"kind": "token_exchange"},
)
"#,
    )
    .await
    .expect_err("no destination");
    let msg = format!("{err:#}");
    assert!(msg.contains("issuer"), "{msg}");
    assert!(msg.contains(".well-known/openid-configuration"), "{msg}");
    Ok(())
}

// ------------------------------------------------------- not wired yet, and
// ------------------------------------------------------- the error says so

/// `oidc` is registered now, so the failure a laptop gets is "no ambient
/// workload identity" rather than "no provider" — and it names the two ways
/// out, because a build that cannot federate needs to know which it is.
#[tokio::test]
async fn oidc_without_an_ambient_identity_says_what_to_do() -> anyhow::Result<()> {
    let desc = declare(
        r#"
target(
    name = "c",
    driver = "secret",
    role = "arn:aws:iam::4711:role/heph-ci-push",
    audience = "sts.amazonaws.com",
    provider = "oidc",
    exchange = {"kind": "aws_sts"},
)
"#,
    )
    .await?;
    desc.validate()?;

    let err = mint(&desc, &[])
        .await
        .expect_err("no ambient identity in a test");
    let msg = format!("{err:#}");
    assert!(msg.contains("no ambient workload identity"), "{msg}");
    // The GitHub Actions case is the one people hit, and its cause is a missing
    // permissions block rather than an authorization failure.
    assert!(msg.contains("id-token: write"), "{msg}");
    // And the laptop case, which is what the adoption path recommends.
    assert!(msg.contains("acquire"), "{msg}");
    Ok(())
}

/// A shape collision is caught from declarations alone — before any build, any
/// mint, and any network call. This is the check that keeps working on a fully
/// warm build where nothing executes.
#[tokio::test]
async fn two_aws_profiles_collide_from_declarations_alone() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "creds",
        r#"
target(name = "ecr", driver = "secret", shape = ["aws_profile"],
       provider = "static_env", var = "A")
target(name = "r2", driver = "secret", shape = ["aws_profile"],
       provider = "static_env", var = "B")
"#,
    );

    let mut claims = Vec::new();
    for name in ["ecr", "r2"] {
        let spec = ws.get_spec(&format!("//creds:{name}")).await?;
        let d = heph::pluginsecret::parse_declaration(&spec)?;
        claims.push(hsecrets::shape::Claim {
            name: name.to_string(),
            addr: d.addr.clone(),
            via: Vec::new(),
            identity: d.identity,
        });
    }

    let err = hsecrets::shape::check_collisions("//svc:release", &claims)
        .expect_err("both default to profile `default`");
    let msg = format!("{err:#}");
    assert!(msg.contains("//creds:ecr"), "{msg}");
    assert!(msg.contains("//creds:r2"), "{msg}");
    assert!(msg.contains("profile"), "{msg}");
    Ok(())
}
