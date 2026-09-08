# Credentials

A **credential** is a declared target naming an identity a build needs, how it
may be obtained in whatever environment the build happens to be running in, and
the shape it is presented in. The consumer says one word; it never knows which
identity it got, and the cache key never knows there was one.

Before this, heph had no concept of a credential, so eight places each invented a
partial one: `pass_env` values hashed into a def, an OCI registry client silently
degrading to anonymous, `docker_build` secrets wired to an `env=` source that
resolves against a cleared environment, `http_fetch` with no auth at all, private
Go modules with no credential path anywhere, and a devenv runner capturing its
whole environment into a cached, remotely-shippable artifact.

## The contract

> A credential grants **access**; it is not an **input**. A target's outputs must
> be identical whichever identity satisfied its credential requirement. A target
> whose *output* depends on *who* ran it is not cacheable, and says so with
> `cache = False`.

Deliberately the same shape as the [scratch contract](SCRATCH.md), and for the
same reason: it turns a fuzzy question ("does auth affect the build?") into a
rule an author can check and a reviewer can enforce.

- **Nothing about a credential enters `hashin`** — not the material, not the
  chosen source, not the declaration, not even the names of the variables it
  presents.
- **A credential is acquired only on a miss.** A cache hit probes nothing, spawns
  nothing and prompts nobody.
- **The invariant is not checkable in general**, so the system owes an audit
  rather than a proof: `heph auth explain` plus the `cache = False` convention.

### The rule that is easiest to get wrong

A presentation may carry only **material** and the handles needed to use it —
keys, tokens, a token-file path, a helper argv. Anything that **selects content**
is an input, and belongs on the consumer as ordinary hashed `env`.

That is narrower than it first looks. A region, an account or project id, a
subscription, an endpoint override, a profile name: every one of those selects
bytes. A laptop resolving through one profile and a runner resolving through a
federated role in another region would otherwise compute the same input hash and
serve each other's artifacts.

**How far the vocabulary enforces it.** A presentation accepts exactly three
keys, so `present = {"region": …}` is rejected outright. A content-selecting
*value* is not caught: `present = {"env": {"AWS_REGION": "eu-west-1"}}` parses,
because it is the same shape `heph.auth.aws_web_identity` uses to carry a role
ARN — and a role ARN is a handle, not a selector. Nothing from the outside tells
those apart.

So the second half is a **convention**, and deliberately so: the alternative —
folding presentation text into every consumer's key — would cost CI its remote
sharing on every private fetch, and would make a role rename rebuild a workspace
that does not depend on the role. The residual risk, stated plainly: a
credential-gated read of a *mutable* reference, on a **cacheable** target, pushed
to a shared remote. All three have to hold, and when they do the contract already
names the fix — a content-addressed reference, or `cache = False`. What the system
owes in exchange is an audit rather than a proof: `heph auth explain`, and the
rule written here.

### Two more places the naive reading breaks

**A gated fetch is cacheable only if the reference is content-addressed.** Pulling
an image by digest or a file by checksum is fine. Pulling `corp/base:latest` is
not: the tag resolves inside the server, against the caller's principal, and two
tenants behind one tag produce one hash and two different artifacts.

**Some outputs simply contain the identity.** A `terraform plan` naming the
account; an output embedding a presigned URL. Those were never cacheable and must
carry `cache = False` whether or not they use this feature.

## Model

| | |
|---|---|
| **credential** | a target with `driver = "credential"`. Declares what is needed and how it may be obtained. Builds nothing, executes nothing, is never cached. |
| **source** | one way to obtain material. Has a *probe* (is this applicable here?) and an *acquire*; optionally a *login* and a *hint*. |
| **chain** | the credential's ordered `sources`. The environment picks the winner, not the author. |
| **material** | named string fields and files, plus an optional expiry. Never hashed, never in an artifact, never in an event, never in a def. |
| **presentation** | how material reaches the sandbox: environment variables, files, or a callback helper. |
| **reference** | `credentials = ["//auth:aws"]` on a consumer. An `Input` with `hashed: false, runtime: false` plus an annotation. |

Why a target rather than a field on the consumer: identical reasoning to
`scratch`. Settings live in exactly one place, so two consumers cannot disagree
about which role to assume; the address gives packages, visibility and
`heph query revdeps` for free; and `heph auth status` has something to enumerate.

## What an author writes

```python
# //auth/BUILD
aws = target(
    name    = "aws",
    driver  = "credential",
    sources = [
        # CI. Probe: the runner's OIDC endpoint is in the environment. heph mints
        # the token and writes it to a file; the AWS SDK performs the exchange
        # itself and re-reads that file on every refresh, so heph contains no AWS
        # code at all — not even the exchange.
        heph.auth.oidc("github_actions", audience = "sts.amazonaws.com",
                       present = heph.auth.aws_web_identity(
                           role = "arn:aws:iam::123456789012:role/deployer")),

        # Laptop. Probe: `aws` is on PATH. Login: the vendor's own SSO flow,
        # which is where Okta actually happens.
        heph.auth.exec(["aws", "configure", "export-credentials",
                        "--profile", "acme", "--format", "process"],
                       fields  = {"access_key_id": "AccessKeyId",
                                  "secret_access_key": "SecretAccessKey",
                                  "session_token": "SessionToken"},
                       expires = "Expiration",
                       login   = [["aws", "sso", "login", "--profile", "acme"]],
                       present = heph.auth.aws_process()),
    ],
)

# //svc/BUILD
target(
    name        = "deploy",
    driver      = "bash",
    credentials = [aws],
    tools       = [terraform],
    env         = {"AWS_REGION": "eu-west-1"},   # configuration: hashed, on the consumer
    run         = "terraform apply -auto-approve",
    cache       = False,
    approval    = True,
)
```

Four things worth noticing, because they are the design decisions rather than the
syntax:

- **The consumer says one word.** No branching on CI, no wrapper script, no
  environment names.
- **The declaration is environment-independent; the chain is not.** The same two
  entries serve a runner with workload federation and a laptop with an expired
  SSO session.
- **The region is on the consumer.** It selects bytes, so it is an ordinary
  hashed input.
- **heph does not reimplement Okta.** On the laptop it drives `aws sso login`,
  whose browser hop already lands on the corporate identity provider.

## The chain

> A probe answers **"is this source applicable here?"**, not **"will it
> succeed?"**. The first applicable source *is* the source; an acquire failure is
> terminal, not a fallthrough.

Every reader's first instinct is the opposite. It is what makes the ordering
meaningful — `oidc` sits above `exec` because on a laptop its probe fails cleanly
and on a runner it wins — and it is why `exec`'s probe being merely "the program
exists" is sufficient. If a chain fell through on failure, a misconfigured role in
CI would silently fall back to whatever ambient identity the runner happened to
have, which is precisely the class of accident this feature exists to prevent.

| Kind | Probe | Acquire | Login |
|---|---|---|---|
| `env` | named variables are set | read them | — |
| `file` | path exists | read raw, or pick fields out of JSON | — |
| `exec` | program resolves (inside `runner`) | run argv; parse stdout through a field map | a *list* of argvs |
| `passthrough` | host paths exist | expose them in place | optional |
| `oidc` | the named CI provider is detected | mint a token for `audience` | — |
| *an address* | selected by `when` | build it, or delegate to it | delegates |

A bare address in `sources` is a reference; there is no `ref` kind to learn. Every
source additionally accepts `when`, `credentials`, `present` and `hint`.

`when` is a **fixed vocabulary** evaluated by heph, deliberately not an expression
language: `ci`, `ci:<provider>`, `interactive`, `env:NAME`, `os:linux`,
`os:darwin`. The moment it becomes a language, BUILD files start containing
credential-selection programs that nobody can audit.

### Why `exec` has exactly one output format

The temptation is `format = "aws_process" | "gcp_executable" | "az_token"`. That
is cloud-specific parsing code living in the engine. One generic JSON field map
expresses all three, and it is deliberately spellable — a tool heph has never
heard of is a BUILD file away rather than a pull request away.

### Which presets exist, and which deliberately do not

**Presentation presets exist**, because a presentation is a third-party wire
format with exactly one correct encoding — the AWS credential-process JSON, the
client-go `ExecCredential`, the Docker helper's three keys. Hand-rolling those is
how you get a subtly wrong one, and the set is closed because the number of such
formats is small and known.

**Source presets do not.** There is no `heph.auth.vault()`, no `heph.auth.op()`,
no `heph.auth.aws_sso()`. That list has no end — every secret manager, every cloud
CLI, every internal tool at every company — and each entry would be a workflow
frozen into a plugin, ageing badly, for no gain over four lines of `exec` with a
field map. **A source is `exec` with a field map, or it is a target.**

## Sources with dependencies

### A source can be a target

A source may be a plain target reference. The target produces the material as its
outputs; the credential presents them. Whether a referenced target is a producer
or another credential to delegate to is decided by its driver, so this needs no
syntax of its own.

```python
cf_fetch = target(
    name        = "cf-token",
    driver      = "bash",
    credentials = ["//auth:vault"],          # the fetch needs its own identity
    tools       = ["//tools:vault"],         # declared, not hoped for
    runner      = "//tools/devenv:runner",   # and it lives in the devenv shell
    cache       = False,                     # required of every credential source
    out         = {"credential": "cred.json"},
    run         = "vault kv get -format=json -field=data secret/cloudflare > $OUT",
)

cf_root = target(
    name    = "cf-root",
    driver  = "credential",
    sources = [cf_fetch],
    present = {"env": {"CLOUDFLARE_API_TOKEN": "${token}"}},
)
```

Everything on that target comes free, because it is a target: the runner, tools,
deps, output collection, the sandbox, cancellation, process supervision, and the
project's rule that every subprocess goes through one exec seam.

Two constraints the form carries:

- **It has no cheap probe.** You cannot ask "is this applicable here?" of a target
  without running it, so a target source is selected by `when` rather than by
  probing. That sorts correctly: "am I in CI" is a `when`; "is the AWS CLI
  installed" is a probe.
- **Its outputs must not become cached artifacts.** A target used as a credential
  source is required to be `cache = False`, enforced at resolution, and it is run
  through `execute` directly with its output bytes taken into the credential store
  at `0600` — `cache_locally` is never reached and no artifact for it ever exists.

**How a target's output becomes material:** by one convention, not by
configuration. An output group named `credential` is read as JSON and its
top-level keys become fields; every other group becomes a named file reachable as
`${file:<group>}`. Expiry comes from `expires_at`/`expires_in` in that same JSON,
or from the credential's `ttl`.

An inline `exec` source needs a `fields` map instead, and the asymmetry is
deliberate: a target's `run` can pipe through `jq` and shape its own output, so it
does not need one; a vendor's stdout is whatever the vendor decided, so it does.

### A source can need a credential

A source that shells out to a secret manager has to authenticate to the secret
manager. The wrong answer is a second, private notion of "how the credential tool
logs in". The right one is that a source declares `credentials`, presented to its
subprocess exactly as they would be to any consumer.

```python
heph.auth.exec(["vault", "kv", "get", "-field=token", "secret/cloudflare"],
               credentials = [vault_login])
```

Secret managers therefore need no source kind of their own and no preset: they are
`exec` with a field map, or a target when the tool lives in an environment.

### Does the produced file hold a secret?

That question decides whether a credential is involved at all.

- A file **with a secret in it** is material, and belongs to a credential —
  presented at `0600`, kept out of the build cache, deleted at run end. A Rancher
  kubeconfig, which embeds a bearer token, is this.
- A file that merely **says how to obtain a secret** is an artifact, and belongs
  to a target, with `transitive` carrying the credential it will need. A GKE or
  EKS kubeconfig — an endpoint, a CA, and a stanza naming an auth plugin — is
  this.

## Presentation

Three shapes, in preference order. What separates them is one thing: what happens
when a token expires while a target is still running.

| | Survives expiry | |
|---|---|---|
| `helper` | **yes** | heph writes a config file naming `heph __auth-helper` as the tool's own credential process. The tool calls back whenever it needs a fresh credential. No loopback server, no file rewriting, no background refresh task. |
| `files` | sometimes | Written by the host at `0600`, destroyed at run end. `${file:name}` resolves to the absolute path. Whether it survives expiry depends on whether the consuming SDK re-reads the file on refresh. |
| `env` | **no** | Static material handed over once. If it expires mid-target, the target fails. |

### Where the files go, and why the path is load-bearing

Presentation files live at `<sandbox_dir>/.heph/auth/<addr>/<name>` — a sibling of
the workspace directory, **never inside it**. This is not tidiness. Output
collection is rooted at the workspace directory and packs every regular file it
walks, and unlike a scratch mount (a symlink to an absolute path, which the
artifact packer refuses) a token file is an ordinary file that would pack
silently, land in the local cache, and be pushed to the shared remote
automatically.

Two more rules fall out of the same reasoning. The directory is named by the full
sanitized *address* rather than the target name, so two credentials called `aws`
in different packages cannot collide. And it is deleted at run end regardless of
outcome — a failed target's sandbox is deliberately kept for diagnostics, and the
log tail is what makes it useful, not the token.

### The callback is pinned, not re-derived

A helper runs **inside the target's sandbox**, where the environment is cleared,
`PATH` is the sandbox's, and there is no `HOME`. A callback that re-walked the
chain would therefore probe a different environment and could pick a *different
source* — on a GitHub Actions runner, concretely: the host wins on
`oidc(github_actions)`, the callback finds no OIDC endpoint, falls through to
`exec(aws)`, and the build runs against whatever ambient identity the runner
happened to have. That is the exact accident the chain exists to prevent, arriving
by the back door.

So the host writes a **pin** next to the presented files — the material, the
workspace, and the index of the source it chose — and every generated config
points the callback at it:

```
heph __auth-helper <dialect> --pin <sandbox>/.heph/auth/<addr>/pin.json
```

Two things follow. The common path is a file read: no engine, no workspace parse,
no probe, and nothing that depends on this process's environment — which matters
because a tool re-invokes its helper on every refresh, every fetch, every registry
operation. And a refresh, when the pinned material has genuinely lapsed, may use
**only that source**; if it cannot run there, heph says so, naming the sandbox as
the reason, rather than quietly acquiring something else.

The pin is `0600`, lives inside the run's sandbox, and is deleted with it.

### The dialects the helper speaks

One hidden subcommand wearing five protocol hats. Each is a third-party grammar
heph must produce byte-exactly, so each has a pinned conformance test.

| Dialect | How it is presented | Wire shape |
|---|---|---|
| `aws` | a config file naming the helper as the profile's `credential_process`, plus `AWS_CONFIG_FILE` | JSON on stdout: `Version` (must be 1), `AccessKeyId`, `SecretAccessKey`, `SessionToken`, RFC 3339 `Expiration` |
| `gcp` | an `external_account` file whose `credential_source.executable` names the helper, plus `GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES=1` | JSON on stdout: `version`, `success`, `token_type`, the token, `expiration_time` |
| `docker` | a generated `DOCKER_CONFIG` directory pointing per-registry at a `docker-credential-heph` shim | the bare registry URL on stdin; `{"ServerURL","Username","Secret"}` on stdout, `Username` = `<token>` for an identity token |
| `git` | `GIT_CONFIG_COUNT` + `GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n`, so no gitconfig is written and the developer's own is never touched | `key=value` lines terminated by a blank line, including `password_expiry_utc` |
| `kubernetes` | heph writes no document: the author templates the kubeconfig and places the argv with `${helper:command}` and `${helper:args}` | the client-go `ExecCredential` object |

Two wrinkles worth knowing before they surprise someone: the **Docker** dialect is
the only presentation that touches `PATH`, because Docker resolves a helper by
executable name; and the **git** dialect deliberately writes no file.

The `gcp` variable is not optional decoration — no Google SDK will run an
executable credential source without it, and forgetting it by hand produces an
error from inside the SDK rather than from heph.

### Template vocabulary

| Token | Resolves to |
|---|---|
| `${<field>}` | a material field, by name |
| `${file:<name>}` | the absolute path of a presented file |
| `${helper:command}` | absolute path of the heph binary |
| `${helper:args}` | the argv tail selecting a dialect and this credential |
| `$$` | a literal `$` |

Substitution is single-pass, so material containing `${` is never re-interpreted.
An unknown name is a loud error naming the credential and the field.

## Where it enters a run

```
result(addr)
  → cache probe            ← a HIT leaves the timeline here, having acquired nothing
  → execute
      → dep resolution
      → scratch acquire
      → credential resolve  (which declaration, is the set coherent?)
      → sandbox claim
      → credential acquire + present  ← files written 0600, beside the workspace dir
      → driver run
      → teardown            ← the auth directory is deleted whatever the outcome
```

Two orderings are load-bearing. **Acquisition is after the cache decision**, which
is what makes "zero cost on a hit" structural rather than aspirational. And it is
**strictly downstream of runner preparation**: the devenv `wrap` runner captures
its entire resolved environment into a `runner.json`, which is a cached,
remotely-shippable artifact, so a runner capture must never see material.

## Caching acquired material

- **Per process**, single-flighted per address and expiry-aware, so two hundred
  consumers cause one acquisition. Not the engine's memoizer: a memoized cell is
  computed once and kept forever, and material expires — a three-hour build with a
  one-hour token would hand a target starting at hour two material that lapsed an
  hour earlier.
- **Across processes**, under `<home>/auth/` at mode `0700` with entries at
  `0600`, keyed by a resolution key over the address, the chosen source's whole
  declaration (`when` included), and the resolution keys of the credentials that
  led there. Mechanically this is what `~/.aws/cli/cache` already is — heph is not
  inventing a mechanism, only owning one — with one difference worth stating:
  `<home>` is the *workspace's* `.heph3`, not a user-level directory, so
  credentials are per checkout and `rm -rf .heph3` is a logout.
- **No daemon.** Same reasoning as the local build cache.

The key separates *declarations*, not identities. A parent's key is a function of
what it declares, so material derived under two different tokens that arrived
through the same `env` source shares a key. That is sound only because unbounded
material is never persisted, which is the next rule.

**Material with no expiry is never written to disk.** A one-hour session token
cached for an hour is a convenience; a long-lived API token written to
`<home>/auth/` is a durable secret at rest that nothing will ever clean up.
Declare a `ttl` to make such material cacheable — set it *under* the true
lifetime, never at it.

That covers *files* too, which a producer target produces before anything knows
whether they have an expiry. They are staged under `<home>/auth/files/live/<pid>/`
— swept of dead pids by the next `heph` — and promoted into their durable home
only once the material has earned the right to outlive the process. An unbounded
producer credential works for the whole run; what it does not do is leave a
`0600` secret behind that only `heph auth logout` would collect.

## The CLI

| Command | Does |
|---|---|
| `heph auth status` | one row per declared credential. Exits non-zero unless every row is `ok`. `--json` carries `addr`, `state`, `source`, `expires_at`, `login`. |
| `heph auth explain <addr>` | the whole chain walk: every source, why each was skipped, which won. |
| `heph auth login [addr]` | runs whichever login commands the probe found stale. Never requires a terminal. |
| `heph auth logout` | clears the disk tier. |
| `heph __auth-helper` | hidden. Speaks one callback dialect; parsed before the argument parser, since its argv and stdin belong to the calling tool. |

**A build never signs anyone in.** It fails with the exact command to run, and a
human runs `heph auth login`, where heph owns the terminal because it is the only
thing using it. That removes a seam (nothing has to hand a child process the
terminal mid-build) and an event (no login URL can reach the hook stream and end
up in a pull-request comment).

Even `login` does not require a tty. An agent's stderr is not a terminal; it runs
the command, relays the URL and code a human needs, and the human finishes
elsewhere.

## Redaction

Declared credential values are scrubbed from a target's output **at the tee**,
before any byte reaches `log.txt`. Redacting at render would be too late: the
captured log is packed into the cache as an artifact and lifted into the failure
event, the JSON output and the CI report.

The scrubber holds back `len(longest secret) - 1` bytes across every read
boundary, because a token can arrive split across two chunks with nothing wrong
with either half — and per-chunk scanning would let exactly that one through,
*intermittently*.

It is best-effort by construction, and that is stated rather than hidden:

- Material shorter than **8 bytes** is not scrubbed. A substring replacement over
  a byte stream cannot remove `"1"` without corrupting every number a build
  prints.
- A secret the target *transformed* — base64, URL-encoded, split across lines —
  passes through.
- Only material **fields** are scrubbed, not the contents of a presented *file*.
  A path is not a secret and redacting it would mangle ordinary output. So a
  target that `cat`s its own kubeconfig does put that token in `log.txt`; the
  answer there is not to.

Ordinary output is never held back: the scrubber retains only a tail that is
genuinely a partial match, so a `--shell` prompt still appears immediately and a
streaming `terraform apply` still streams.

## The security boundary

**What this does.** Material is never hashed, never in the build cache, never in a
target definition, never in an event, never printed by `inspect`. Presented files
are `0600` inside the sandbox and destroyed with it; environment values are set
only on the declaring consumer. A credential reaches only the targets that named
it, instead of every target that happens to run.

**What it does not do.** It does not create an isolation boundary — heph's sandbox
is a logical scope, not an OS one, and a BUILD file that wanted the whole
environment could already ask for it. It does not protect against a malicious
target in the same workspace. It does not remove long-lived secrets where a
provider offers no alternative.

Three limits worth naming, because each is a place someone will otherwise assume
more than is true:

- **Do not name a credential-source target in `deps`, and do not build it
  directly.** The credential path runs it through `execute` and takes its bytes
  into the store, so no artifact exists. The *ordinary* path does not: an
  uncacheable target's outputs still transit the in-memory tmp store, which spills
  to the durable local cache above its per-entry cap. `cache = False` keeps them
  off the shared remote — the exposure that crosses machines — but not off this
  disk. heph cannot detect the mistake.
- **A helper refresh runs inside the target's sandbox.** The host pins which
  source it chose, so a callback can never *silently* pick a different identity —
  but a source that needs `$HOME`, a vendor session or a CI OIDC endpoint cannot
  re-acquire there, and says so rather than guessing. A credential that must
  survive expiry mid-run needs a source that can be re-run from nothing.
- **An acquire subprocess gets a fixed environment**: `PATH`, `HOME`, `TMPDIR`,
  `LANG`, `LC_ALL`, `TERM`, plus whatever its own `credentials` present. Not the
  host's whole environment — that is how a source quietly picks up an identity
  the declaration never mentioned — and not an empty one, which is how a vendor
  CLI fails to find the session the probe just decided was there.

This narrows exposure substantially and makes it auditable; it does not turn heph
into a secrets manager.

## The audit question

`scratch` has `--no-scratch`, which withholds cache contents and proves outputs do
not depend on them. There is no equivalent here, and there deliberately is no
`--no-credentials`: withholding an identity produces a permission error, not an
audit. The real audit is **two identities, one output hash** — run the target
forced under each and compare — which is a short CI script rather than a flag.

## Interactions to know about

- **The argument-size limiter never evicts a credential.** Its eviction order is
  longest-first, which is precisely a session token, and `ARG_MAX` differs by
  operating system — so without the exemption the same BUILD file would keep the
  credential on Linux and drop it on macOS, then run unauthenticated. If the
  protected set alone does not fit, the target fails rather than spawning.

  That trades a per-platform *identity* divergence for a per-platform *failure*
  one: `ARG_MAX` is roughly 2 MiB on Linux and 256 KiB on macOS, so a very large
  credential set could spawn on one and fail on the other. Failing loudly is the
  better half of that trade, and the ceiling is far out of reach — each presented
  value is capped at 32 KiB, so it needs eight to thirty maximum-size credentials
  on one target.
- **A credential's variables are set last**, and a collision with the target's own
  `env` is refused rather than resolved.
- **A duplicate reference** is refused: a credential presents one set of variables,
  so referencing it twice does nothing.
- **Two credentials claiming one variable** on the same consumer are refused, for
  the same reason.

## ABI

`ABI_SEMVER` 0.8.0 → 0.9.0. `RunRequest`/`ManagedRunRequest` gained
`repeated CredentialMount credentials` — additive and cold-path, so an old plugin
still loads. It should nonetheless be rebuilt: an old driver drops the mounts and
runs its target with no identity, which either fails much later with a message
from someone else's SDK or succeeds against whatever ambient identity the host
had. Nothing is silently wrong in the *cache* — a credential contributes nothing
to any hash in either direction — what is lost is the identity itself.

`example/credential/` is a worked package: a two-source chain, a credential whose
source is a target, and a build step that leaks its own token so the redaction is
visible.
