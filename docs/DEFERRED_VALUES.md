# Deferred values

A driver option's value has to be a literal in the BUILD file. But the value's
owner is usually somewhere else — a role ARN in the Terraform that created the
role, a registry per environment in a platform team's repo, a version in a
release process. Copying it in makes a second source of truth that someone has to
keep agreeing with the first, purely to satisfy heph.

```python
# //infra/aws/BUILD — the value's owner
target(
    name   = "role-arn",
    driver = "bash",
    deps   = {"tf": ["//infra/aws:*.tf", "//infra/aws:terraform.tfstate"]},
    run    = "terraform -chdir=$(dirname $SRC_TF) output -raw deployer_role_arn > $OUT",
    out    = ["role_arn.txt"],
)

# //auth/BUILD — where the literal used to be
target(
    name    = "aws",
    driver  = "credential",
    sources = [heph.auth.oidc("github_actions", audience = WIF,
                              present = heph.auth.aws_web_identity(
                                  role = "${read://infra/aws:role-arn}"))],
)
```

Read the two together and the property that matters is visible: the ARN appears
exactly once in the repository, in the Terraform that defines it. heph reads it;
it does not hold a copy.

## Why this is not what every other build system does

Gradle, Bazel, Buck2, Pants and Nix each shipped an answer. They differ in surface
and agree in structure, because they all made the same choice: **the value crosses
the boundary as a bare string, and the thing that produced it never becomes a node
in the graph.** Three consequences follow mechanically.

**Change can only be detected by re-running the producer, eagerly, every
invocation.** With no edge to invalidate, the only way to notice a new value is to
go and look — before the graph exists, on every build, including the ones that hit
every cache. Bazel's `--workspace_status_command` is "a binary that Bazel runs
before each build". Gradle's `ValueSource` documents that `obtain()` "will be
called during each build … only fast-running commands should be used". That
sentence is the tell: it is an apology for a design in which the producer can
never be cached.

**The cache key can only be all-or-nothing.** Hash the string into every
consumer's key and over-invalidate (`--define` discarding the whole analysis
cache), or declare it invisible and sanction staleness (*"Bazel pretends that the
volatile file never changes"*). None of them can express the key that is actually
correct: *this action depends on the Terraform state's serial, not on the ARN
string.*

**Provenance ends at the string.** "Why did this rebuild?" bottoms out at
`environment variable 'X' has changed`. Nothing can say *"because `role_arn` moved
when state went from serial 41 to 42"*, because the graph never knew there was a
Terraform.

Each ecosystem then converged on the same workaround — commit a snapshot of the
computed value, to escape the per-build tax — and every one of those snapshots is
a second source of truth that drifts, undetectably, for exactly the reason the
snapshot was needed.

Here the producer **is** a node. It declares its own inputs, so it re-runs when the
thing that defines the value changes and not otherwise; a consumer's key derives
from the Terraform state rather than from the ARN string; and `heph inspect deps`
can say which field wanted it.

## The mechanism

Two moments. At **parse** the reference becomes an edge; at **run** the bytes
arrive.

```
LOAD                      PARSE                        RUN
the author writes         an edge is synthesized       the bytes are substituted
role = "${read://…}"      Input{hashed:true,           role = "arn:aws:iam::…"
                                runtime:false}

WHAT EACH HASH SEES
                          def hash                     hashin
                          the reference, verbatim      the producer's hashout
                          never the resolved value     via the ordinary dep path
```

The def hash covers the **unresolved** reference, which is what keeps
`heph query` and `heph inspect def` from triggering a build. The producer's
content still reaches `hashin`, because `hashin` is the def hash plus the hashouts
of hashed inputs. **No new hashing rule is introduced at any point** — the correct
behaviour is a structural consequence of where the work happens rather than a rule
anyone follows.

Each piece already existed: a driver synthesizing an edge that is not in the BUILD
`deps` list is what `runner` does; a producer's content reaching a consumer's key
is what `inputs_result_meta` does for every hashed input.

## Three layers, and a driver touches only the third

An earlier shape left three obligations on the plugin author: type the field,
collect its edges at parse, substitute at run. Both "remembers" fail **silently** —
a missed edge means the producer never builds and the value never enters the key; a
missed substitution means `"${read://infra:role-arn}"` is used as the ARN. That is
the exact bug class this exists to remove, reintroduced one layer down.

| | |
|---|---|
| **host** | walks `TargetSpec.config` for references at any nesting depth, appends `Input`s to `def.inputs` after `parse`, reads the producers, fills `RunRequest.deferred`. Cannot be forgotten; the driver never participates. |
| **shared decoder** | `String::from_spec_value` rejects `${read:…}` — so a reference in `out`, `deps`, `name` or a glob is a parse error in **every driver at once**, with no driver code and no schema flag. |
| **driver** | one field type changes: `role: String` → `role: Deferred<String>`. Reading it requires a `&RunRequest`, so forgetting to resolve is a compile error. |

The walk is over the raw `Value` tree, which is what makes it work where a flat
per-field schema could not: a credential's `sources = [heph.auth.oidc(…)]` is a
list of maps of maps, and the reference sits three levels inside it.

**The entire diff to make a field deferrable:**

```diff
 #[derive(Spec)]
 struct MySpec {
-    role: String,
+    role: Deferred<String>,
 }
```

No parsing, no edge collection, no substitution call, no schema annotation. The
`accepts_deferred` flag on the driver's schema is derived from the field types, so
there is nothing to remember there either.

## Grammar

| Form | Resolves to | Edge |
|---|---|---|
| `${read://pkg:name}` | contents of the producer's single output, surrounding whitespace trimmed | `(hashed, not staged)` |

An **unknown kind is left untouched**, so `${FOO:-default}`, `${OUT}` and every
other shell construct survives being written in a deferrable field.

A template may mix literals and references, and may name more than one producer:
`"${read://infra:registry}/app:${read://infra:version}"`.

**A driver option has no escape, and needs none.** `$$` is a piece of the grammar
the shared tokenizer recognizes, but in a driver option it is *reproduced
verbatim* — `echo tmp.$$` is the shell's PID idiom and heph reproduces it byte for
byte, whether or not a `${read:…}` appears elsewhere in the same string. `$$`
unescapes to a literal `$` only in a **credential presentation**, which is a
document heph itself writes and therefore owns the escape for; see
[credentials](CREDENTIALS.md). One tokenizer, two consumers that disagree about
one piece, and both are right — a field whose meaning changed depending on whether
a reference happened to be elsewhere in it is the bug that split them.

**A substituted value is not quoted, and in bash that is a code boundary.** In an
exec-mode `run` a reference fills one argv element, and argv needs no quoting. In
a bash `run` — and in `sh -c "…"` written from exec mode — the bytes are *spliced
into a shell program*, so a producer emitting `1.2.3; rm -rf /` runs it.

Two things make that worth stating rather than assuming. The bytes come from a
target in your own repository, which is the same trust boundary as any `run`
line — but a producer's output may have been **pulled from the shared remote
cache**, written by another machine, another user, another CI job. And splicing
is not passing: `["docker", "push", "${read://infra:registry}/app"]` in exec mode
passes one argument, while the same text in a bash `run` is shell source. heph
substitutes; it does not quote.

**A reference names an absolute address, and that is the whole discriminator.**
The kind name alone is not enough, because the shells and tools whose syntax
shares this shape are not going away — so `${read:X}` is heph's only when `X`
starts with `//`.

| written | what it is |
|---|---|
| `${read://infra:v}` | heph. In bash, a `src`-style expansion of an unset lowercase variable — an arithmetic error, or `""` |
| `${src::3}` | bash: the first three characters of `$src` |
| `${src:0:3}` | bash: the same, spelled with an explicit offset |
| `${FOO:-default}` | bash: `$FOO`, or `default` |

The `:name` and `./` relative forms an address elsewhere accepts are deliberately
**not** claimed here: `${src::3}` is the exact text of a real bash idiom, and no
diagnostic is worth taking it over. One predicate (`template::claims`) answers
"is this heph's?" for the spec decoder, the engine walk and the substitution
alike — two answers would be the silent misclassification this exists to prevent.

**Reserved, and only where it matters.** `${src://…}` — the sandbox *path* of an
artifact — is named by this design and not implemented. It is refused with "not
yet" rather than "unknown", but **only inside a driver that accepts deferred
values**, where an author writing one plainly meant it to resolve.

**A `${` inside a `${…}` is refused.** `${FOO:-${read://a:b}}` closes on the
inner `}`, so the outer form would tokenize as an unknown kind, be reproduced
verbatim, and the inner reference would silently never resolve. `${VAR:-default}`
is the commonest bash brace form there is, so this one is loud.

## The refusal

> A deferred value changes what a target **does**. It never changes which targets
> **exist**.
>
> The reference is static and written in the BUILD file. Only the bytes arrive
> late.

A reference is rejected in `deps`, `tools`, `runner`, `scratch`, `out`, `name` and
`labels` — everything that decides graph shape or identity. That line buys three
things: cycle detection still works at parse, because every edge is known before
anything runs; `heph query` and `heph inspect def` never trigger a build; and
evaluation never blocks on one.

That last is the whole of Nix's import-from-derivation problem. IFD forces a build
inside single-threaded evaluation, so `nix eval` and `nix flake show` stop working
without a builder, network and credentials, and evaluations serialize. nixpkgs
forbids it outright. What is here is IFD with the recursion cut off at one level: a
leaf value, resolved after the graph is known, in a phase that was already going to
run the producer.

**Watch for selectors dressed as values.** Some fields look like values and are
really selectors — an address filter, a glob in `out`, a `runner`. None may be
deferrable, and the enforcement is simply that no driver types them as accepting
one. A **runner spec is never deferrable**, as a standing rule: a runner target's
fingerprint is what moves every consumer's cache key when the environment moves,
and deferring it would make the identity of every target in the workspace depend on
a build output.

## There is no `${env:NAME}`

An earlier draft had one: a host environment variable, snapshotted at parse and
hashed. It is not here, and the reason is worth writing down, because it will be
proposed again.

It fails on its own terms. `${env:NAME}` is *legal bash* — `${var:offset}`, where
a bare identifier is a valid arithmetic expression — so in a `run` it silently
means something else already. Getting the value into the def hash requires
substituting it into the spec **before** `parse`, which is also before the shared
decoder that refuses a reference in `out`, `deps`, `runner` and `name`; a value
from the ambient environment could then decide graph shape, and `inspect spec`
would no longer show what the author wrote. And it reaches a credential
declaration, where an `Input` is `hashed: false` by design, so nothing would fold
it into any key at all.

Every one of those disappears if the value enters through a node, which costs one
target and needs no new mechanism:

```python
target(
    name     = "role",
    driver   = "bash",
    pass_env = ["AWS_ROLE_ARN"],
    cache    = False,
    out      = "role.txt",
    run      = 'printf %s "$AWS_ROLE_ARN" > $OUT',
)

role = "${read://:role}"
```

That is strictly better than the reference would have been. `pass_env` hashes the
variable's **name** as well as its value, so "why did this rebuild" has an
answer; `cache = False` states the freshness choice out loud instead of implying
it; `heph inspect deps` shows `//:role` as an edge; and the refusals above apply
to it like any other producer.

## Config or credential

A `${read:}` producer is an ordinary target, so its output is written to the cache
— local and, if configured, remote and shared. That single fact decides the
boundary between this and [credentials](CREDENTIALS.md):

> **Would you commit it?**
>
> Yes → it is configuration. `${read:}`, hashed, cached, an ordinary target.
> No → it is a credential, and it belongs to the other mechanism: never hashed,
> never cached, acquired at run.

An ARN, a project id, a registry host, a cluster endpoint: all public names. A
token, a private key, a Rancher kubeconfig: not.

The corollary matters too: **there is no unhashed value reference.** A value that
shapes a command and is not hashed is the silently-wrong-build direction
`docs/EXEC_RUNNERS.md` warns about. If something must stay out of the key, that is
a statement that it is a credential.

## Freshness, and the bootstrap cycle

A target running `terraform output` with no declared inputs never re-runs. Three
answers, best first:

| Approach | Re-runs when | Cost |
|---|---|---|
| declare the source as `deps` — `*.tf`, `terraform.tfstate` | the thing that defines the value changes | none on a warm cache |
| `cache = False` | every invocation | the command, every build — the Gradle tax, taken deliberately |
| a refresh target run by hand | when someone runs it | none, but drift is possible |

Lead with the first. It is available here and not in any of the prior art,
precisely because the producer is a node that can declare inputs.

**The cycle that will actually happen:** `//infra:role-arn` runs Terraform, which
needs AWS credentials to read remote state; the credential needs the ARN. That is a
real cycle and the first thing anyone will hit. It is caught at parse and named,
because every edge is known before anything runs. The answer is two credentials —
a bootstrap identity that reads state, and a derived identity that does the work —
which is the shape the credentials design already uses for delegation.

## Failures

| When | Behaviour |
|---|---|
| the producer exits non-zero | fail, naming the producer and showing its log — the same shape as any failing dependency, because that is what it is. The consuming *field* is not in this message: the producer is reached while computing the consumer's `hashin`, before anything substitutes. The synthesized edge carries it as an `origin_id` (`option\|run[1]`), which is what `heph inspect deps` reads |
| the output is zero bytes | fail. A silently-empty ARN is worse than a stopped build |
| the output has more than one line | fail; do not take the first line |
| the producer emits several files | fail, listing them, pointing at `\|<group>` |
| a reference in a field that does not accept one | fail at parse, in every driver at once |
| the driver's plugin predates this feature | fail at parse, naming the driver |

## What ships, and what does not

**Ships:** `${read://…}`, the host-side walk and edge append, resolution from the
store, `RunRequest.deferred`, `Deferred<String>`, the `//`-address claiming rule,
the whole-driver schema gate. Consumers: the credential driver's presentation
templates, and the `exec` driver's `run` in **both exec and bash mode**.

An earlier draft of this design excluded bash, on the grounds that `${src:0:3}`
is valid bash and accepting references there would create a collision an escape
rule could only document. The claiming rule removes the collision instead: heph
takes a `${…}` only when its argument is an absolute address, and `${src:0:3}`,
`${src::3}` and `${FOO:-d}` are not. `${read://a:b}` is meanwhile an arithmetic
error for a set `read` and `""` for an unset one, so claiming it takes nothing
from anybody. `$$` is untouched, because it is not `${` — `echo tmp.$$` still
prints a PID.

`$SRC_<GROUP>` is still there and is still the right tool for a *declared dep
group*: it names a group, and its value is the space-joined list of that group's
paths. `${read://x:y}` declares the edge inline and is a single value. They are
different things that happen to overlap.

**Does not ship yet:** `${src://…}` (named, refused with "not yet" inside a
driver that takes references, left alone everywhere else); `heph.core.read()` as a Starlark function, which is discoverability
rather than capability; `inspect deps` "via" lines and `inspect def --resolved`;
and the OCI family's `build_args`, `dest`, `labels`, `cache_from` — which is where
the largest number of genuinely deferrable fields live, and is the cheapest to add
because none of them sits in a `Hash` impl shared with a hot path.

## ABI

`ABI_SEMVER` 0.9.0 → 0.10.0. `RunRequest`/`ManagedRunRequest` gained
`map<string, string> deferred`; `Schema` gained `bool accepts_deferred`. Both
additive and cold-path.

The bump exists for the schema field. An old plugin's `String` decoder rejects
nothing, so it would read `"${read://infra:role-arn}"` as a literal and run the
target with the reference text as the value. `accepts_deferred` decodes as `false`
for anything that has not said otherwise, so the host refuses the reference at
parse instead — loudly, naming the driver.

The reservation also closes the reverse direction: if a driver ever changes a field
back from `Deferred<String>` to `String`, a BUILD file containing a reference fails
loudly rather than silently using the literal. And `Deferred<T>`'s `Hash` delegates
to the raw text, so retyping an existing field leaves its def hash byte-identical —
without that, every target of that driver invalidates on upgrade and a mixed fleet
double-populates the shared remote.

`example/deferred/` is a worked package: two producers, one `exec` consumer, and
the two `heph inspect hashin` runs that show the key deriving from what the value
was derived from rather than from the string.
