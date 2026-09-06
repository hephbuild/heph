# Target-scoped credentials: one declaration that works on a laptop and in CI.
#
# A credential is a **target that describes how to obtain a value, never the
# value**. Its output is a `secret.json` carrying only the *identity* half —
# safe to cache, safe to push to a shared remote cache, safe to print. That
# artifact becomes a hashed input on every consumer.
#
# The whole design turns on one split:
#
#   identity     role, scope, params, registry, machine, profile, shape, env
#                → HASHED. Change one and every consumer legitimately re-keys.
#
#   acquisition  provider, var/vars, file/files, helper, protocol, runner,
#                sign_in, audience, exchange, ttl, acquire
#                → NOT HASHED. Swap freely per environment.
#
# That is what lets the same address be reached one way on a laptop and another
# way in CI while both share a cache entry. `acquire` is how you spell it, and
# it is what most of this file demonstrates.
#
# Run the demos:
#
#   heph run //secrets:whoami                 # the local route
#   DEMO_CI=1 heph run --force //secrets:whoami   # the "CI" route
#   heph inspect hashin //secrets:whoami      # identical either way
#
# See docs/SECRETS.md for the full reference.

# ═══════════════════════════════════════════════════════════════════════════
# 1. The headline: one identity, two routes
# ═══════════════════════════════════════════════════════════════════════════
#
# `acquire` is an ordered list. The first entry whose `when_env` guard matches
# is used; an entry with no guard is the catch-all and must come last.
#
# **Selection, not fallback.** A chosen entry that fails, fails the build —
# heph does not try the next one. Falling through would mean a broken CI route
# quietly reaching for a laptop helper and either failing somewhere far less
# legible or, worse, succeeding as a different identity under a cache key that
# claims the first one.
#
# `DEMO_CI` stands in for `GITHUB_ACTIONS` so you can flip between the two
# routes on your own machine. In a real workspace the guard names whatever
# variable your CI already sets — heph knows about none of them.
target(
    name = "api",
    driver = "secret",

    # ── identity: hashed, one for every entry below ─────────────────────────
    machine = "api.example.com",   # slot key: which netrc entry this owns
    shape = ["file", "netrc"],     # `file` is the default; see §3

    # ── acquisition: not hashed, one entry per environment ──────────────────
    acquire = [
        # In "CI": the token is in the environment, the way every CI system
        # hands one over. `static_env` names a VARIABLE, never a literal —
        # the schema has no free-form value field at all, so a token cannot be
        # written into a BUILD file and pushed to a shared cache.
        {"when_env": "DEMO_CI", "provider": "static_env", "var": "DEMO_API_TOKEN"},

        # On a laptop: whatever CLI or secret manager you are already signed
        # into. Standing in for `op read`, `vault read`, `gh auth token`,
        # `aws configure export-credentials`, `security find-generic-password`.
        {"provider": "exec", "protocol": "raw",
         "helper": ["/bin/sh", "-c", "printf 'local-developer-token'"]},
    ],
)

# The consumer is IDENTICAL for both routes. That is the payoff — nothing here
# knows or cares which entry ran.
#
# `shape = ["file"]` (the default) writes a 0600 file OUTSIDE the sandbox's
# `ws/` and puts its path in `$SECRET_<NAME>`, upper-cased from the key in
# `secrets = {…}`. A path, not a value: pointer variables do not leak through
# `/proc/<pid>/cmdline` the way a value variable does.
target(
    name = "whoami",
    driver = "bash",
    out = "who.txt",
    secrets = {"api": "//secrets:api"},
    run = [
        # $SECRET_API is a path. Read it; never echo it.
        'echo "authenticated as: $(cat $SECRET_API)" > $OUT',
    ],
)

# ── prove the cache key does not move ────────────────────────────────────────
#
#   heph inspect hashin //secrets:whoami
#   DEMO_CI=1 heph inspect hashin //secrets:whoami
#
# Same hash. Only the acquisition half differed, and none of it is hashed — so
# a build on a laptop and a build in CI share the same cache entry for every
# consumer downstream. Get that wrong and the feature loses its whole point:
# it becomes `pass_env`'s disease one level up.

# ═══════════════════════════════════════════════════════════════════════════
# 2. Every source, with the fields that belong to each
# ═══════════════════════════════════════════════════════════════════════════
#
# A provider is a DISCRIMINANT, not one field among many. Each owns its fields,
# so the wrong combinations are unwritable rather than merely rejected:
# `helper` on a `static_env` is an unknown key, an `exec` with no `protocol`
# fails at parse, a `timeout` on an `oidc` has nowhere to go.

# ── static_env: one variable ────────────────────────────────────────────────
#
# The honest escape hatch and the migration path off `pass_env`. Even with no
# federation anywhere, moving a token here already buys the hash boundary, the
# declaration under review, the redaction, and the shapes.
target(
    name = "one_var",
    driver = "secret",
    provider = "static_env",
    var = "DEMO_API_TOKEN",       # sugar for vars = {"token": "DEMO_API_TOKEN"}
)

# ── static_env: several variables, named fields ─────────────────────────────
#
# A credential is a field map, not a string. `aws_profile` and the other
# structured shapes read named fields out of it.
target(
    name = "keypair",
    driver = "secret",
    provider = "static_env",
    vars = {
        "aws_access_key_id": "DEMO_AWS_ACCESS_KEY_ID",
        "aws_secret_access_key": "DEMO_AWS_SECRET_ACCESS_KEY",
    },
    shape = ["aws_profile"],
    profile = "demo",             # slot key: names a section, not an identity
    params = {"region": "eu-west-1"},
)

# ── static_env: a token projected into a FILE ───────────────────────────────
#
# Half the CI systems worth federating hand a workload its identity as a file
# rather than as a variable: a Kubernetes projected service account token, and
# Azure Pipelines' `$AZURE_FEDERATED_TOKEN_FILE`. Same rule as `vars` — it
# names a LOCATION, never a literal. Read once per mint, never watched.
target(
    name = "projected",
    driver = "secret",
    provider = "static_env",
    file = "/var/run/secrets/kubernetes.io/serviceaccount/token",
    # `file` and `var` may not name the same field: one field, one location.
)

# ── exec: the four helper protocols ─────────────────────────────────────────
#
# "Run a helper and read a credential" sounds like one thing and is four. The
# protocol is explicit rather than guessed, because the three stdin encodings
# differ — by the time a response exists, the request was already sent in one.
#
#   raw                  stdin: —          stdout IS the value, verbatim
#   credential_process   stdin: —          {"Version":1,"AccessKeyId":…}
#   credential_helper    stdin: {"uri":…}  {"headers":{…},"expires":…}
#   docker_credential    stdin: a BARE URL {"ServerURL","Username","Secret"}
#
# A helper runs UNSANDBOXED, as you, with heph's own environment — that is the
# point (it needs the real $HOME to find a keychain or an SSO cache) and it is
# also the sharpest edge in the feature. Prefer an absolute path, or name a
# `runner` so the binary comes from a described environment instead of an
# ambient $PATH lookup.

# `raw` — stdout is the value. `gh auth token`, `gcloud auth
# print-access-token`, `op read`. Sharp edge worth stating: a helper that
# prints a warning to stdout has just made it part of your credential.
target(
    name = "raw_helper",
    driver = "secret",
    provider = "exec",
    protocol = "raw",
    helper = ["/bin/sh", "-c", "printf 'value-from-a-raw-helper'"],
    ttl = "1h",                   # `raw` carries no expiry of its own
)

# `credential_process` — the AWS CLI's own schema. `aws configure
# export-credentials --format process` speaks it, and SSO comes along free.
target(
    name = "credential_process_helper",
    driver = "secret",
    provider = "exec",
    protocol = "credential_process",
    helper = ["/bin/sh", "-c",
              'printf \'{"Version":1,"AccessKeyId":"AKIAEXAMPLE","SecretAccessKey":"s3cr3t","SessionToken":"tok"}\''],
    shape = ["aws_profile"],
    profile = "from_helper",
)

# `credential_helper` — the Bazel `--credential_helper` / EngFlow spec. The
# only protocol carrying expiry natively, which is why the broker's TTL cache
# prefers it. It returns HEADERS rather than a credential.
target(
    name = "credential_helper_helper",
    driver = "secret",
    provider = "exec",
    protocol = "credential_helper",
    helper = ["/bin/sh", "-c",
              'printf \'{"headers":{"Authorization":["Bearer header-token"]}}\''],
    timeout = "30s",              # default is 60s; a deadline belongs to exec
)

# `docker_credential` — `docker-credential-osxkeychain`, `-ecr-login`, `-gcr`.
# Its stdin is a BARE URL, not JSON: the single most commonly mis-implemented
# detail of this protocol, and why it shares only a name with the spec above.
target(
    name = "docker_credential_helper",
    driver = "secret",
    provider = "exec",
    protocol = "docker_credential",
    helper = ["/bin/sh", "-c",
              'printf \'{"ServerURL":"registry.example.com","Username":"u","Secret":"p"}\''],
    shape = ["docker_config"],
    registry = "registry.example.com",   # slot key for docker_config
)

# ── exec under a runner ─────────────────────────────────────────────────────
#
# The structural answer to the $PATH hole above: the helper binary comes from a
# described environment rather than from whatever `PATH` heph started with.
#
# **A helper inherits NO workspace-wide `runner:` default** — the one place in
# heph where that option deliberately does not apply. That option exists to
# move *targets* into hermetic environments, and those are precisely the
# environments a helper cannot work in: `aws configure export-credentials`
# needs `~/.aws/sso/cache`, `gh auth token` needs the login keychain, `op`
# needs a desktop-app session. All of that lives in the real $HOME a hermetic
# runner exists to hide. So a helper opts in by address, or runs where heph
# runs.
#
# The runner is UNHASHED here, unlike on a target — it affects only how a value
# was fetched, and the value is not in the key at all.
#
# Commented out because it needs `example/execrunner`'s devenv plugin
# installed; uncomment after `gen-example`.
#
# target(
#     name = "helper_in_an_environment",
#     driver = "secret",
#     provider = "exec",
#     protocol = "raw",
#     helper = ["op", "read", "op://build/api/token"],
#     runner = "//execrunner:devenv_wrap",   # pinned binary, host $HOME intact
# )

# ═══════════════════════════════════════════════════════════════════════════
# 3. Shapes: what a target actually sees
# ═══════════════════════════════════════════════════════════════════════════
#
# A shape decides which files and variables exist in the sandbox, so it is part
# of the HASHED identity — it is part of what the target reads. Everything
# materializes outside `ws/`, mode 0600, and is scrubbed on success and on
# failure alike.
#
#   file             a 0600 file; its path in $SECRET_<NAME>        (default)
#   env              named variables holding the VALUE              (see below)
#   netrc            <home>/.netrc, one entry per `machine`
#   git_credential   <home>/.git-credentials + GIT_CONFIG_* wiring
#   docker_config    <home>/.docker/config.json, one entry per `registry`
#   aws_profile      <home>/.aws/{credentials,config}, one section per `profile`
#   gcloud_adc       application_default_credentials.json (a singleton)
#
# Shapes compose: one credential can render several at once.
target(
    name = "many_shapes",
    driver = "secret",
    machine = "git.example.com",
    registry = "registry.example.com",
    shape = ["file", "netrc", "git_credential", "docker_config"],
    provider = "exec",
    protocol = "raw",
    helper = ["/bin/sh", "-c", "printf 'shaped-token'"],
)

target(
    name = "show_shapes",
    driver = "bash",
    out = "env.txt",
    secrets = {"tok": "//secrets:many_shapes"},
    run = [
        # None of these print the credential — they print where it landed and
        # how the toolchain was pointed at it.
        'echo "pointer:  $SECRET_TOK" > $OUT',
        'echo "home:     $HOME" >> $OUT',
        'echo "netrc:    $(test -f "$HOME/.netrc" && echo yes || echo no)" >> $OUT',
        'echo "docker:   $DOCKER_CONFIG" >> $OUT',
        'echo "gitcount: $GIT_CONFIG_COUNT" >> $OUT',
    ],
)

# ── the `env` shape opts into an exposure ───────────────────────────────────
#
# `env` puts the VALUE in a variable, which many tools require (`GH_TOKEN`,
# `NPM_TOKEN`). It is a per-secret opt-in rather than the default because of a
# real leak: the `oci` runner passes a target's whole environment to the docker
# CLI as `docker exec -e KEY=VALUE` **argv**, and on Linux `/proc/<pid>/cmdline`
# is world-readable. A pointer variable is harmless — it is a path. A value
# variable sits on a command line any local uid can read.
#
# The map is variable name → JSON pointer into the credential. `$.` is the
# primary field; `$.token` names one explicitly.
target(
    name = "as_env",
    driver = "secret",
    shape = ["env"],
    env = {"DEMO_TOKEN": "$."},
    provider = "exec",
    protocol = "raw",
    helper = ["/bin/sh", "-c", "printf 'env-shaped-token'"],
)

target(
    name = "reads_env",
    driver = "bash",
    out = "len.txt",
    secrets = {"t": "//secrets:as_env"},
    run = ['printf "%s" "${#DEMO_TOKEN}" > $OUT'],   # its length, not its value
)

# ═══════════════════════════════════════════════════════════════════════════
# 4. Exchanges: turning what you have into what you need
# ═══════════════════════════════════════════════════════════════════════════
#
# An exchange is named for the GRANT, not for the vendor, and the grants have
# RFC numbers:
#
#   token_exchange       RFC 8693. What GCP's own STS endpoint speaks.
#   jwt_bearer           RFC 7523 §2.1. What a service-account key actually is.
#   client_credentials   RFC 6749 §4.4.
#   aws_sts              AssumeRoleWithWebIdentity. Not IETF; kept because it
#                        is how one of the three clouds federates.
#   http                 A vendor REST call. The escape hatch that keeps heph
#                        out of the business of knowing vendors.
#
# The three OAuth grants take an `issuer` and DISCOVER their endpoint from
# `{issuer}/.well-known/openid-configuration`. Exactly one of `issuer` and
# `endpoint`: silently preferring one would make which server was contacted
# depend on a precedence nobody can see at the call site.
#
# **An exchange runs after EVERY source, not only after `oidc`.** An assertion
# is not the only thing worth trading — which is the entire basis of §5 below.

# ── the pipeline is a list, because two hops are normal ─────────────────────
#
# GCP federation is RFC 8693 against the workload identity pool, then a
# service-account impersonation call. Two steps, not a special case. Each step
# consumes what the last produced.
# The workload identity pool provider: what a GCP-side trust is registered as.
POOL = "//iam.googleapis.com/projects/8801/locations/global/workloadIdentityPools/heph-ci/providers/github"

target(
    name = "gcp",
    driver = "secret",
    # identity — hashed
    params = {"impersonate": "heph-push@proj.iam.gserviceaccount.com"},
    scope = ["https://www.googleapis.com/auth/cloud-platform"],
    registry = "europe-west1-docker.pkg.dev",
    shape = ["gcloud_adc", "docker_config"],
    # acquisition — not hashed
    provider = "oidc",
    # Two `audience` fields, one string, two different questions. The one on the
    # SOURCE is what the assertion must claim — GitHub Actions is asked to stamp
    # it. The one on the STEP is what that hop is addressing. They coincide for
    # GCP because the pool provider validates the `aud` it is named by; they do
    # not coincide for AWS, where the assertion says `sts.amazonaws.com` and no
    # step takes an audience at all. Neither is hashed.
    audience = POOL,
    exchange = [
        {"kind": "token_exchange", "issuer": "https://sts.googleapis.com",
         "audience": POOL},
        {"kind": "http",
         "url": "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/{impersonate}:generateAccessToken",
         "fields": {"token": "/accessToken"}},
    ],
)

# ═══════════════════════════════════════════════════════════════════════════
# 5. `exec` + `exchange`: mint something short-lived from something static
# ═══════════════════════════════════════════════════════════════════════════
#
# Cloudflare R2 exposes no OIDC federation — there is nothing to federate. It
# is still not the static-key dead end it looks like: one API call trades a
# static parent token for credentials scoped to a bucket, with a session token
# and a TTL. So the parent lives in one place and NEVER reaches a target, and
# what a target holds is bucket-scoped and expires.
#
# This shape — a helper, then an exchange — is why the pipeline belongs to the
# broker rather than to the `oidc` provider. While it lived there, this
# declaration parsed, validated, and then silently handed the target the
# 1Password PARENT token. Nothing failed. The BUILD file said one thing and the
# build authenticated as another.
#
# `{account}` and `{token}` interpolate: named identity fields, every `params`
# key, and the previous step's value.
target(
    name = "r2",
    driver = "secret",
    # identity — hashed
    params = {
        "account": "0123456789abcdef",
        "bucket": "heph-cache",
        "endpoint": "https://0123456789abcdef.r2.cloudflarestorage.com",
    },
    shape = ["aws_profile"],
    profile = "r2",
    # acquisition — not hashed
    provider = "exec",
    protocol = "raw",
    helper = ["/opt/homebrew/bin/op", "read", "op://build/r2/parent"],
    exchange = {
        "kind": "http",
        "url": "https://api.cloudflare.com/client/v4/accounts/{account}/r2/temp-access-credentials",
        "fields": {
            "aws_access_key_id": "/result/accessKeyId",
            "aws_secret_access_key": "/result/secretAccessKey",
            "aws_session_token": "/result/sessionToken",
        },
    },
    ttl = "12h",
)

# ═══════════════════════════════════════════════════════════════════════════
# 6. Federation: the same identity from CI, from a laptop, from anywhere
# ═══════════════════════════════════════════════════════════════════════════

# ── AWS: the two-audience case, which is why `audience` is unhashed ─────────
#
# The `aud` an assertion can carry is decided by WHICHEVER ISSUER MINTED IT,
# not by who you are:
#
#   GitHub Actions will stamp any audience a job asks for, so the CI route asks
#   for `sts.amazonaws.com` and the IAM OIDC provider is registered against it.
#
#   An OIDC ID token's `aud` IS the client id — OIDC Core §2 requires it — so
#   the `heph auth login` route cannot ask for that, and the cloud-side trust
#   policy names the client id instead.
#
# One principal, one `role`, two audiences. `audience` lives in the acquisition
# half for exactly this reason: while it was hashed, declaring both routes
# split every consumer's cache key by environment.
OKTA = "https://org.okta.com"   # the org authorization server: licence-free

target(
    name = "ecr",
    driver = "secret",
    # identity — hashed. The role is the principal; that is what re-keys.
    role = "arn:aws:iam::4711:role/heph-ci-push",
    params = {"region": "eu-west-1"},
    shape = ["aws_profile"],
    profile = "ecr",
    # acquisition — not hashed
    acquire = [
        # CI: an ambient workload identity, scoped to the job. Strictly better
        # than a session when both exist, so it goes first.
        {"when_env": "GITHUB_ACTIONS", "provider": "oidc",
         "audience": "sts.amazonaws.com",
         "exchange": {"kind": "aws_sts"}},

        # A laptop, direct federation: `heph auth login` establishes the
        # session. One OIDC application per integration, which is why the
        # client id is declared HERE and not in a config file — an org with one
        # Okta tenant still has a separate app for AWS, for GCP, for each SaaS.
        {"provider": "oidc",
         "sign_in": {"issuer": OKTA, "client_id": "0oa_aws_app"},
         "exchange": {"kind": "aws_sts"}},
    ],
)

# ── the same, for an org that does per-cloud SSO instead ────────────────────
#
# This is the common case and it needs no IdP work at all. Where `aws sso
# login` / `gcloud auth login` already hold a session, the vendor CLI has
# already done the OIDC flow — so `exec` consumes the result and heph signs in
# to nothing. `sign_in` is present exactly when a credential is reached by
# federating to YOUR OWN IdP, and absent otherwise, which is most secrets.
target(
    name = "ecr_via_cli",
    driver = "secret",
    role = "arn:aws:iam::4711:role/heph-ci-push",   # same identity as above
    params = {"region": "eu-west-1"},
    shape = ["aws_profile"],
    profile = "ecr",
    acquire = [
        {"when_env": "GITHUB_ACTIONS", "provider": "oidc",
         "audience": "sts.amazonaws.com",
         "exchange": {"kind": "aws_sts"}},
        {"provider": "exec", "protocol": "credential_process",
         "helper": ["aws", "configure", "export-credentials",
                    "--profile", "sso", "--format", "process"]},
    ],
)

# ── every CI system, not just GitHub Actions ────────────────────────────────
#
# heph detects exactly ONE CI system natively, and that is the correct number.
# GitHub Actions is special only because its ID token sits behind an
# authenticated HTTP call — a job is handed an endpoint and a bearer and has to
# go and ask. Everywhere else the token is simply THERE, in a variable or a
# file, so it needs no detection, no enum entry and no heph release.
#
# Adding a CI system is a BUILD-file edit by whoever needs it.
target(
    name = "ecr_anywhere",
    driver = "secret",
    role = "arn:aws:iam::4711:role/heph-ci-push",
    params = {"region": "eu-west-1"},
    shape = ["aws_profile"],
    profile = "ecr",
    acquire = [
        {"when_env": "GITHUB_ACTIONS", "provider": "oidc",
         "audience": "sts.amazonaws.com", "exchange": {"kind": "aws_sts"}},

        # GitLab: the variable named by the job's `id_tokens:` block.
        {"when_env": "GITLAB_CI", "provider": "static_env",
         "var": "GITLAB_OIDC_TOKEN", "exchange": {"kind": "aws_sts"}},

        {"when_env": "CIRCLECI", "provider": "static_env",
         "var": "CIRCLE_OIDC_TOKEN_V2", "exchange": {"kind": "aws_sts"}},

        # Kubernetes and Azure Pipelines project the token into a FILE.
        {"when_env": "KUBERNETES_SERVICE_HOST", "provider": "static_env",
         "file": "/var/run/secrets/kubernetes.io/serviceaccount/token",
         "exchange": {"kind": "aws_sts"}},

        # Buildkite: the agent mints one on demand.
        {"when_env": "BUILDKITE", "provider": "exec", "protocol": "raw",
         "helper": ["buildkite-agent", "oidc", "request-token",
                    "--audience", "sts.amazonaws.com"],
         "exchange": {"kind": "aws_sts"}},

        # The catch-all: laptops. No guard, and it must come last.
        {"provider": "exec", "protocol": "credential_process",
         "helper": ["aws", "configure", "export-credentials", "--format", "process"]},
    ],
)

# ── GitHub: a vendor call, because the IdP path is closed ───────────────────
#
# GitHub does not accept its own OIDC token for repository access, so the
# exchange terminates in an App installation token — one hour, scoped to
# repositories and permissions. Neither a GitHub App token nor an R2 temporary
# credential needs a name in heph's schema: both are one POST and a JSON
# pointer, described in the BUILD file under review beside the identity they
# mint.
target(
    name = "github",
    driver = "secret",
    # identity — hashed
    machine = "github.com",
    params = {"app_id": "1180022", "install": "org/heph"},
    shape = ["git_credential", "netrc", "env"],
    env = {"GH_TOKEN": "$.token"},
    # acquisition — not hashed
    acquire = [
        {"when_env": "GITHUB_ACTIONS", "provider": "oidc",
         "exchange": {"kind": "http",
                      "url": "https://tokens.example.com/github/{install}",
                      "fields": {"token": "/token"}},
         "ttl": "1h"},
        # A laptop: `gh` is already keychain-backed and already installed.
        {"provider": "exec", "protocol": "raw", "helper": ["gh", "auth", "token"]},
    ],
)

# ── a service-account key is an assertion, so trade it ──────────────────────
#
# Not federated, and still better than handing the key to a target: RFC 7523
# turns it into a short-lived access token. Migrate off it first — a
# service-account key is permanent, downloadable and org-wide.
target(
    name = "gcp_sa_key",
    driver = "secret",
    scope = ["https://www.googleapis.com/auth/cloud-platform"],
    shape = ["gcloud_adc"],
    provider = "exec",
    protocol = "raw",
    helper = ["op", "read", "op://build/gar-sa/key.json"],
    exchange = {"kind": "jwt_bearer", "issuer": "https://accounts.google.com"},
)

# ═══════════════════════════════════════════════════════════════════════════
# 7. Policy: `allow`
# ═══════════════════════════════════════════════════════════════════════════
#
# Access control without a new ACL system. WHICH credentials exist is
# CODEOWNERS on this package; WHICH TARGETS may use one is this line, in the
# same reviewed file. It is an htmatcher expression, enforced at link time
# against the EFFECTIVE set — so a target that inherits this credential
# transitively through a dependency is checked too.
#
# Unhashed: it decides whether a build is permitted, not what it computes.
target(
    name = "restricted",
    driver = "secret",
    provider = "exec",
    protocol = "raw",
    helper = ["/bin/sh", "-c", "printf 'only-for-publishers'"],
    allow = "//secrets/... && label(publish)",
)

target(
    name = "permitted",
    driver = "bash",
    labels = ["publish"],
    out = "ok.txt",
    secrets = {"r": "//secrets:restricted"},
    run = ['test -s "$SECRET_R" && echo permitted > $OUT'],
)

# ═══════════════════════════════════════════════════════════════════════════
# 8. Credentials travel with dependencies
# ═══════════════════════════════════════════════════════════════════════════
#
# A credential declared `transitive` reaches every target that depends on this
# one, the same way transitive `deps`, `tools` and `env` already do. That is
# what lets a library package say "anything built against me needs the private
# module token" once, rather than every consumer repeating it.
#
# It reaches the CONSUMER's cache key, which is the point and the cost: the
# effective set is what gets hashed, so a dependency adding a credential
# re-keys everyone downstream. `heph auth show //...` names every target that
# is both credential-bearing and remotely cached — the combination worth a
# deliberate decision.
target(
    name = "lib",
    driver = "bash",
    out = "lib.txt",
    transitive = {"secrets": {"api": "//secrets:api"}},
    run = ['echo "built" > $OUT'],
)

target(
    name = "app",
    driver = "bash",
    out = "app.txt",
    deps = ["//secrets:lib"],
    # No `secrets` here: `api` arrives through //secrets:lib.
    run = ['echo "app has: $SECRET_API" > $OUT'],
)
