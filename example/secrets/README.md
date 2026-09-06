# Target-scoped credentials

A credential is a **target that describes how to obtain a value, never the
value**. Its output is a `secret.json` carrying only the *identity* half — safe
to cache, safe to push to a shared remote cache, safe to print — and that
artifact becomes a hashed input on every consumer.

The whole design turns on one split:

| | Fields | In the cache key? |
|---|---|---|
| **Identity** | `role`, `scope`, `params`, `registry`, `machine`, `profile`, `shape`, `env` | **Yes** |
| **Acquisition** | `provider`, `var`/`vars`, `file`/`files`, `helper`, `protocol`, `runner`, `sign_in`, `audience`, `exchange`, `ttl`, `acquire` | **No** |

That is what lets one address be reached one way on a laptop and another way in
CI while both share a cache entry. Get it wrong and the feature becomes
`pass_env`'s disease one level up: if `provider` were hashed, CI saying `oidc`
and a laptop saying `exec` would never share a cache entry for *any* consumer.

## Run it

```sh
heph run //secrets:whoami                      # the local route
DEMO_CI=1 heph run --force //secrets:whoami    # the "CI" route
```

`--force` because the second run is otherwise an ordinary cache hit — which is
the point, and the next command is how you see it:

```sh
heph inspect hashin //secrets:whoami
DEMO_CI=1 heph inspect hashin //secrets:whoami
```

**Same hash.** Two different credentials arrived, from two different mechanisms,
and every consumer downstream keys identically. That is the claim the whole
feature rests on.

`DEMO_CI` stands in for `GITHUB_ACTIONS` so you can flip between the routes on
your own machine. In a real workspace the guard names whatever variable your CI
already sets — heph knows about none of them, and there is no enum to extend.

Other targets worth running:

```sh
heph run //secrets:show_shapes   # what a target actually sees in its sandbox
heph run //secrets:app           # a credential arriving through a dependency
heph run //secrets:permitted     # an `allow` policy admitting a target
```

## What is runnable here, and what is reference

Being blunt about this, because a published example that does not work is read
as a promise and then distrusted:

- **`static_env` and `exec` routes mint for real.** Everything under "Run it"
  above works on a laptop with no cloud account, no network and no IdP.
- **The federated declarations parse and validate, and need a cloud to run.**
  `//secrets:ecr`, `//secrets:gcp`, `//secrets:github`, `//secrets:r2` describe
  real exchanges against real endpoints. They are here so the shapes are
  copyable, not so they execute in a demo.

Both halves are covered by `crates/e2e/tests/secret_examples.rs`, which reads
*this file's* `BUILD` from the repository and parses every declaration in it. A
field renamed in the schema breaks that test rather than rotting the example.

## What each target demonstrates

| target | what it shows |
|---|---|
| `api` + `whoami` | **the headline** — one identity, two routes, one cache key |
| `one_var`, `keypair` | `static_env`: naming a variable, never a literal |
| `projected` | `file`: a Kubernetes / Azure Pipelines token projected to a path |
| `raw_helper` … `docker_credential_helper` | the four helper wire protocols |
| `many_shapes` + `show_shapes` | shapes rendering into the sandbox |
| `as_env` + `reads_env` | the `env` shape, and why it is opt-in |
| `gcp` | a two-hop exchange pipeline |
| `r2` | **`exec` + `exchange`** — mint something short-lived from something static |
| `ecr` | `audience` per route, and why it is *not* hashed |
| `ecr_via_cli` | the common case: an org that already has per-cloud SSO |
| `ecr_anywhere` | GitLab, CircleCI, Kubernetes, Buildkite — no heph release needed |
| `github` | a vendor REST call, because GitHub's IdP path is closed |
| `gcp_sa_key` | RFC 7523: a service-account key *is* an assertion |
| `restricted` + `permitted` | `allow`, evaluated on the effective set |
| `lib` + `app` | credentials travelling with dependencies |

## Three things worth taking away

**Selection, not fallback.** The first `acquire` entry whose `when_env` guard
matches is used, and a chosen entry that fails *fails the build*. heph does not
try the next one. Falling through would mean a broken CI route quietly reaching
for a laptop helper and either failing somewhere far less legible or — worse —
succeeding as a different identity under a cache key claiming the first one.

**The value never crosses back.** It is not in the artifact, the cache key, the
log, the event stream or any target output. Redaction is a backstop for the
accident where a target prints its own credential; it is not the containment
boundary. The boundary is that the value only ever exists in a sandbox that is
scrubbed on success and on failure alike.

**heph cannot verify that two routes yield the same identity — the descriptor
asserts it.** The identity half is written once and shared by every entry, so
the cache key is the same whichever route ran, and nothing checks that an `exec`
helper really returns credentials for the role named above it. Point one entry
at a different account and you get artifacts from two identities under one key.
`heph auth show //...` reports which entry was selected and what chose it.

## See also

- `docs/SECRETS.md` — the full reference.
- `heph auth show //...` — every credential-bearing target, and whether it is
  also remotely cached.
- `heph auth check //...` — mint every credential a pattern touches, then
  drop it. On a warm workspace it is the only thing that validates the
  credential path at all, since a cache hit mints nothing.
- `heph auth login` — establish a laptop session, for the direct-federation
  path. Most workspaces never need it: where per-cloud SSO already exists, the
  vendor CLI holds the session and `provider = "exec"` consumes it.
