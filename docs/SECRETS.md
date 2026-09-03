# Target-scoped credentials

A target names the credential it needs by **address**. The *recipe* is built,
hashed and shared like any other dependency. The *value* is minted at run time,
delivered as a file, redacted on the way out, and never persisted anywhere.

```
target        secrets = {"ecr": "//infra/creds:ecr"}   ← an ADDRESS
   ↓          a hashed dependency, built before hashin exists
secret.json   { "version": 1, "identity": { "role": … } }   ← IDENTITY ONLY
   ↓          the acquisition half is read from the spec, never from here
broker        mint · TTL cache · redact
```

The rule that belongs in your head, and nowhere else in this document says it
better:

> **A credential grants read access, through the shared cache, to whatever its
> consumers produced.**

## Declaring one

```python
# //infra/creds/BUILD — CODEOWNERS on this package is the access control
target(
    name     = "ecr",
    driver   = "secret",

    # identity — hashed. Consumers re-key when these move.
    role     = "arn:aws:iam::4711:role/heph-ci-push",
    region   = "eu-west-1",
    shape    = ["aws_profile"],
    profile  = "ecr",

    # acquisition — NOT hashed. Swap freely per environment.
    provider = "oidc",
    exchange = "aws",
)
```

There is deliberately no new ACL system. Which credentials exist, and what
identity each names, is a line in a BUILD file under review — not whatever
happened to be exported in the shell that ran the build.

## The one thing to understand: which half is hashed

A descriptor has two halves that behave completely differently.

| | Fields | In the cache key? |
|---|---|---|
| **Identity** | `role`, `audience`, `scope`, `impersonate`, `app_id`, `install`, `account`, `region`, `bucket`, `endpoint`, `registry`, `machine`, `profile`, `shape`, `env` | **Yes** |
| **Acquisition** | `provider`, `var`, `vars`, `helper`, `protocol`, `runner`, `exchange`, `timeout`, `ttl`, `acquire` | **No** |

Getting this wrong costs the feature its main promise. If `provider` were
hashed, CI saying `oidc` and a laptop saying `exec` would produce different
hashouts, and the two would never share a cache entry for *any* consumer —
which is `pass_env`'s disease moved one level up.

### The address is in the key

`secret.json` carries the descriptor's canonical address alongside its identity,
and that is load-bearing rather than decorative. Every `Identity` field is
optional, so two descriptors distinguished *only* by their acquisition halves —
`//creds:prod` reading `PROD_API_KEY`, `//creds:staging` reading
`STAGING_API_KEY` — would otherwise emit byte-identical artifacts, match on
hashout, and let a consumer of one be served the other's cached output. Silently,
with the winner decided by scheduling order.

The `runner.json` precedent does not transfer here, and the difference is the
whole point: a runner deliberately keeps its address *out* of the key, because
its config fully describes the environment and two addresses emitting identical
config really do describe the same thing. A `secret.json` deliberately omits the
half that decides which real-world principal you get, so identical bytes imply
nothing.

The cost, stated so it is a decision rather than a surprise: **renaming or moving
a descriptor re-keys every consumer.**

**The split itself is enforced structurally, not by a flag anyone has to
remember.** A
consumer's `hashin` folds in the *hashouts of its hashed inputs*, and a hashout
is the digest of the artifacts an input target produced. So the `secret` driver
writes only the identity half into `secret.json`, and the acquisition half never
becomes an artifact at all — the broker reads it from the target's *spec*
instead. A field with no artifact has no hashout to contribute.

Editing a helper path therefore re-runs the descriptor target and produces
byte-identical bytes, so nothing downstream moves. `crates/e2e/tests/secret.rs`
pins both directions.

This is the same discipline `docs/EXEC_RUNNERS.md` spends its most important
section on, arriving at the opposite answer for the opposite reason: a runner
needs a derived `fingerprint` because its config names a reference to something
that *moves*, while a credential recipe names an identity that does not.

### Why `shape` is identity

It is the one field whose side of the line is genuinely arguable. It is identity
because a shape decides which *files and variables exist* in the sandbox, and
that is part of what the target reads.

It costs nothing in cache sharing, because a shape's paths and variable *names*
are fixed by the shape and its slot key while only the contents vary. A
federated and a non-federated GCP credential both render `gcloud_adc` at the
same path under the same variables; what differs is whether the ADC inside says
`external_account` or carries a token directly — and contents are exactly what
is never hashed.

## More than one way in: `acquire`

One identity often has to be obtained differently depending on where the build
is running — ambiently in CI, from a stored session on a laptop. Rather than
duplicating the descriptor, the acquisition half becomes an ordered list:

```python
target(
    name = "artifacts",
    driver = "secret",
    role = "arn:aws:iam::4711:role/heph-read",   # identity — one for all entries

    acquire = [
        {"when_env": "GITHUB_ACTIONS", "provider": "oidc", "exchange": "aws"},
        {"provider": "exec", "protocol": "credential_process",   # no guard: the catch-all
         "helper": ["aws", "configure", "export-credentials", "--format", "process"],
         "runner": "//tools/devenv:runner"},
    ],
)
```

The flat form above is sugar for a single-entry list. The two do not compose;
mixing them is an error.

### Selection, not fallback

**The first entry whose guard matches is used, and a chosen entry that fails,
fails the build.** heph does not try the next entry when one errors.

Falling through on failure would mean a broken OIDC exchange in CI quietly
reaching for a laptop helper, and then either failing somewhere far less legible
or — worse — succeeding as a *different identity*, under a cache key that claims
the first one.

An entry with no `when_env` always matches, so it is the catch-all and must come
last. Entries after it are rejected at the declaration rather than silently
never running.

### `when_env`

```python
{"when_env": "GITHUB_ACTIONS"}                        # set and non-empty
{"when_env": {"CI": "true", "AWS_REGION": "eu-west-1"}}  # every one exact
```

*Set but empty counts as unset*, because CI systems routinely blank a variable
to mean "off" and treating that as true is a reliable way to select the wrong
route. Exact string comparison, no globs: a file this close to cache keys is the
wrong place for an expression language.

An environment variable rather than a named list of CI systems, because every CI
system already announces itself that way — so heph needs to know about none of
them, and a team running something bespoke sets a marker of their own. There is
no enum to extend and no release to wait for.

This is the one place heph reads the ambient environment by design. It is safe
here for a precise reason: the guard selects only an acquisition route, and
nothing in the acquisition half is hashed, so no ambient value can reach a cache
key.

### The one risk it introduces

**heph cannot verify that two routes yield the same identity; the descriptor
asserts it.** The identity half is shared by every entry, so the cache key is
the same whichever route ran — but nothing checks that an `exec` helper actually
returns credentials for the role named above it. Point one entry at a different
account and you get artifacts from two identities sharing one key. Prefer a
route that *derives* its credential from the declared identity over one that is
merely believed to match it.

## Providers

| `provider` | What it does |
|---|---|
| `static_env` | Reads a named host variable. The honest escape hatch and the migration path off `pass_env`. |
| `exec` | Runs a helper subprocess speaking one of four protocols. |
| `oidc` | Acquires a workload identity token and exchanges it for a scoped short-lived credential. |

`static_env` names a **variable, never a literal**, and the descriptor schema has
no free-form value field at all — otherwise someone writes a token into a
`text_file` target and it is pushed to the shared remote cache.

### The four helper protocols

"Run a helper and read a credential" sounds like one thing and is four. The
protocol is an explicit closed field, never guessed from output — by the time a
response exists the request has already been sent in some encoding.

| `protocol` | stdin | stdout | Expiry | Speakers |
|---|---|---|---|---|
| `engflow` | `{"uri": …}` | `{"headers": {…}, "expires": …}` | Native, RFC 3339 | Bazel `--credential_helper` helpers |
| `credential_process` | — | `{"Version":1,"AccessKeyId":…}` | `Expiration`, optional | `aws configure export-credentials`, aws-vault, Granted |
| `docker_credential` | bare URL, **not JSON** | `{"ServerURL","Username","Secret"}` | None | `docker-credential-osxkeychain`, `-ecr-login`, `-gcr` |
| `raw` | — | the value, verbatim | None | `gh auth token`, `gcloud auth print-access-token`, `op read` |

- `engflow` is the only one carrying expiry natively. It returns *headers*
  rather than a credential, so a bare token is recovered from `Authorization`.
- `credential_process` is the one heph both reads and writes. `Version` must be
  `1`; an absent `Expiration` means "treat as static", not "expired".
- `docker_credential` shares a name with the Bazel spec and nothing else. heph
  only ever calls `get`. A `Username` of `<token>` is the convention for "the
  secret is an identity token, not a password".
- `raw` is a concession rather than a protocol. **A helper that prints a warning
  to stdout has just made it part of your credential.** Helpers here must be
  silent, or wrapped until they are.

### A helper has a deadline

60 seconds by default, overridable per entry:

```python
{"provider": "exec", "protocol": "raw",
 "helper": ["op", "read", "op://build/r2/parent"],
 "timeout": "120s"}
```

**This is what makes "never interactive during a build" true.** Closing stdin
enforces only half of it: it stops a helper prompting on stdin, but a macOS
keychain dialog, a Touch ID prompt from `op`, and a helper blocked on an
unreachable endpoint all read no stdin at all. Under the broker's per-descriptor
lock, every consumer of that credential then queues behind the hang and the
build simply looks stuck.

```
error: secret //infra/creds:r2: credential helper "op read op://build/r2/parent"
       did not finish within 1m.
  It may be waiting on a desktop approval or a biometric prompt, which a build
  cannot answer. Run it once by hand to prime the session, or raise `timeout`
  on the acquire entry if it is legitimately slow.
```

The deadline arrives as a *cancellation*, not as a dropped future, so the helper
is killed and reaped rather than orphaned. And it sits in the unhashed half, so
changing it moves no cache key.

### Where a helper runs: `runner`

An `exec` entry may name an exec runner by address, exactly as a target does:

```python
{"provider": "exec", "protocol": "raw",
 "helper": ["op", "read", "op://build/r2/parent"],
 "runner": "//tools/devenv:runner"}
```

A helper is otherwise resolved from whatever `PATH` heph started with —
unpinned, unhashed, outside any sandbox. A `PATH`-hijacked `op` exfiltrates
every credential in the workspace. Under a runner the helper comes from a
described environment instead of an ambient lookup.

**The default is `local`, and a helper inherits no workspace default.** This is
the one place in heph where the workspace-wide `runner:` option deliberately
does not apply. That option exists to move *targets* into a described
environment, and the environments people put targets in are precisely the ones a
helper cannot work in: `aws configure export-credentials` needs
`~/.aws/sso/cache`, `gh auth token` needs the login keychain, `op` needs a
desktop-app session — all of it in the real `$HOME` a hermetic runner exists to
hide. Inheriting the default would mean that the day someone sets a workspace
runner, every laptop credential stops resolving.

**It is unhashed, unlike a target's runner.** On a target the runner is part of
what produced the output, so its fingerprint belongs in the key. Here it only
affects *how a value was fetched*, and the value is not in the key at all.

It narrows the hole rather than closing it: the helper still runs **as you**,
outside any sandbox, and is trusted code either way.

**Known gap, until the consumer wiring lands:** the `oci` runner rewrites a
helper's argv to `docker exec …` with no `-i`, and `SpecRewrite` carries no
stdin — so a `docker_credential` or `engflow` helper (the two protocols with a
request payload) would read EOF inside the container and either fail obscurely
or return the wrong registry's credential. `wrap`-style runners inherit stdin and
are unaffected, so the same descriptor behaves differently per runner. Either
reject that combination or make stdin a declared property of the runner seam
before the wiring ships.

## Shapes

`$SECRET_<NAME>` holds a *path*, mode 0600, under `<sandbox>/secrets/` — outside
`ws/`, so an `out = ["**"]` can never sweep it into an artifact. `environ` is
readable through `/proc/<pid>/environ` by any same-uid process and is inherited
by every descendant; systemd's `$CREDENTIALS_DIRECTORY` and BuildKit's
`--mount=type=secret` both landed on files for this reason.

Most tools want something else, so a shape renders a well-known file *plus* the
pointer variable that aims the tool at it.

| Shape | Merge key | Two secrets, one target |
|---|---|---|
| `file` | per-secret path | Never collides; each gets its own `$SECRET_<NAME>` |
| `env` | variable name | Merge — same variable with a different value is a collision |
| `netrc` | `machine` | Merge — one entry per machine |
| `docker_config` | `registry` | Merge — one entry per registry under `auths` |
| `git_credential` | `machine` | Merge — one helper per prefix |
| `aws_profile` | `profile` | Merge — one section each. *Both defaulting to `default` is a collision.* |
| `gcloud_adc` | *(singleton)* | Collision on the second |

Which gives one rule for the whole feature:

> A shape contributes entries to keyed files and variables to the environment.
> Both are namespaces. **Distinct keys merge; the same key with differing values
> is an error naming both descriptors.**

Two deps needing *the same descriptor* is the common case, not a conflict — that
merges silently.

**The check runs from the secret targets' specs, not their built output.** Every
merge key is an attribute of the declaration, so it can be read without building
or minting anything — which is what keeps the check alive on a fully warm build
where every consumer is a cache hit.

There is deliberately **no priority or ordering field** to pick a winner. It
would make the outcome invisible at the call site and turn a declaration-time
error into a silent runtime one.

## Expiry

Four sources, in precedence order:

1. **The protocol's own field** — `expires`, `Expiration`. Authoritative.
2. **A parsed `exp` claim**, when the value is a JWT.
3. **The descriptor's `ttl`** — a declaration, not an observation.
4. **A conservative default** (5 minutes).

Two of the four protocols carry no expiry at all, so for `raw` and
`docker_credential` the fallback is a hand-written `ttl` — and **the dangerous
direction is a `ttl` longer than the truth**. Too short merely re-mints more
than it needs to; too long means holding a dead credential and discovering it
mid-target.

Re-minting happens at a margin *before* the stated expiry, because `exp` is
absolute and the host's clock may not agree with the issuer's.

### The JWT reader reads claims; it does not verify them

No signature check. That is fine for scheduling — a lie about `exp` costs at
worst an extra mint — and it draws one hard line:

- **Expiry may be read from any source.** It is a hint, and a wrong hint is not
  a security event.
- **The subject may only be read from a token heph obtained itself**, from a
  known issuer over TLS. Never from helper output. A `sub` reaches the cache key
  under `cache.subject_scoped`, so a helper free to claim any subject is a
  helper free to have its artifacts served as somebody else's.

Detection is deliberately dull — three base64url segments, the middle decoding
to JSON with a numeric `exp` — and anything else falls straight through to `ttl`
without an error. It never fails a build; it only ever improves a number.

RFC 3339 timestamps go through `chrono`, not a hand-rolled parser. The
hand-rolled one accepted `2026-02-30T00:00:00Z` (rolling it forward to March
2nd), a missing offset (silently reinterpreting a local time as UTC, up to 14
hours out) and unchecked separators — each an expiry wrong in the direction that
costs a build mid-target, and each *succeeding*, so the fallback warning never
fired either.

## Redaction is best-effort

The broker knows every live value, so it owns a multi-pattern Aho–Corasick
automaton replacing matches with `«redacted:NAME»` before bytes reach `log.txt`,
the TUI and the event stream. Each value is registered raw, base64-encoded (both
alphabets) and percent-encoded.

**A value the tool derives before printing escapes it.** This is a backstop for
accidents, not a containment boundary — which is why the log-artifact leak is
also fixed at its source.

**And it cannot mask the value a mint is currently fetching.** The redactor a
provider is handed is a snapshot, and a value is registered only once its mint
succeeds — so a helper that fetches a token and then fails while echoing it
defeats redaction on the first mint of that descriptor. There is no fix
available: heph never sees that value. The honest scope is "credentials already
live in this run", which is the case that actually recurs, since a build mints
many descriptors and a later failure can quote an earlier one.

Superseded values stay maskable for a few re-mints and then stop. Unbounded
retention would grow the automaton quadratically over a long build, hold every
dead token in memory for the run, and — worst — saturate the first-byte table
that makes the per-chunk cost negligible.

Two properties worth knowing:

- With no secrets live it is one branch per chunk and no copy, so a target that
  never touches the feature pays nothing.
- Values shorter than 8 bytes are **not** masked, and heph warns by name. A
  three-character secret cannot be masked without shredding unrelated output,
  and a redactor that mangles a build log is worse than one that misses.

## Diagnosability

The route taken is invisible in a build's output, so:

- A descriptor where no entry matches fails by listing **each entry's guard
  beside whether that variable was set, and to what** — not a missing-credential
  error three layers down.
- Every mint records a grant naming the descriptor, which `acquire` entry ran,
  what selected it, and the expiry with its source. Never the value.

## Version skew

`secret.json` carries `version: 1`, checked by exact match, following
`runner.json` rather than the `RemoteManifest.version` precedent of a field
written everywhere and read nowhere.

`Identity` is `deny_unknown_fields`. That is deliberate, and the alternative is
worse: serde's default is to *silently drop* an unrecognized field, so an older
heph reading a newer descriptor would compute a different, wrong view of what
identity it names without saying anything. With `deny_unknown_fields` it is a
loud parse error naming the target instead.

**State the consequence rather than inferring it from the `skip_serializing_if`
attributes**: those make a new `Identity` field free for descriptors that *do
not* use it, but for a descriptor that *does*, an older binary fails to parse it
under the same `version: 1`. That is an accepted loud break, not silent
extension. It is tolerable because a descriptor is local-cache-only, so the
blast radius is one machine — and in practice a new field ships with a new
Starlark kwarg, so an old binary fails earlier at spec parsing anyway. A future
field that needs old binaries to keep working must bump `SECRET_JSON_VERSION`.

Two defaults are frozen the moment this ships, since changing a default breaks
BUILD files that already exist: `shape` defaults to `["file"]`, and
`aws_profile`'s `profile` defaults to `default`.

## What is not built yet

This document describes the design in full; the tree currently implements the
declaration and the broker. Still to land: consumer wiring (`secrets = {…}` on a
target), sandbox delivery of the well-known shapes, the transitive `Sandbox`
field, `cache.subject_scoped`, the `oidc` provider and `heph auth`. See the
design proposal for the phasing.

**Do not assume that work needs an `ABI_SEMVER` bump.** `TargetDef` crosses the
plugin seam on the *cold prost path* (`plugin-abi/src/convert.rs`), where
`crates/plugin-stabby/ABI_VERSIONING.md` documents additive fields as
explicitly not requiring one — prost ignores unknowns, and an old plugin simply
does not see the new field. `ScratchMount`, the closest precedent for a per-run
resource added by an earlier feature, is cold-path too. What *would* force a
bump is delivering a secret through a **native stabby struct** in
`plugin-abi/src/abi.rs`, or changing the signature of an existing vtable symbol;
a brand-new optional load-time symbol in the `SET_LOG_SINK_SYMBOL` style — the
likely shape of the redacting sink — is additive and does not. Decide the
transport lane explicitly before writing it, and bump only if the frozen surface
is actually touched.
