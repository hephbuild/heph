# Credentials: an identity a target declares, and heph obtains.
#
# heph's sandbox clears the environment completely, so before this the only way a
# target got a credential was `pass_env` — captured at parse and *hashed into the
# def*, which puts a session token into a durable cache key.
#
# A credential is a target instead. It declares what identity is needed, an
# ordered list of ways to obtain it, and the shape it is presented in. The
# consumer says one word.
#
# The rule that makes it safe is one sentence: **a target's outputs must be
# identical whichever identity satisfied its credential requirement.** A
# credential grants *access*; it is not an *input*. Nothing about one — not the
# material, not the chosen source, not the declaration, not even the names of the
# variables it presents — reaches any cache key, in either direction.
#
# Everything here is runnable with no setup and no real secrets: the sources
# below produce fake material from `printf`. Try:
#
#   heph auth status
#   heph auth explain //example/credential:demo
#   heph run //example/credential:uses-it

# ── 1. the declaration ───────────────────────────────────────────────────────
#
# `sources` is an ordered chain, and the *environment* picks the winner, not the
# author. The rule to internalise:
#
#   A probe answers "is this source applicable here?", not "will it succeed?".
#   The first applicable source IS the source; an acquire failure is terminal,
#   not a fallthrough.
#
# That is what makes the ordering meaningful. If a chain fell through on failure,
# a misconfigured role in CI would silently fall back to whatever ambient
# identity the runner happened to have — precisely the accident this prevents.
demo = target(
    name = "demo",
    driver = "credential",
    sources = [
        # CI: mint the runner's OIDC token. On a laptop this probe fails cleanly
        # (there is no OIDC endpoint), which is what lets one declaration serve
        # both. `present` is per-source here because a federated token file and a
        # key pair are genuinely different shapes.
        heph.auth.oidc(
            "github_actions",
            audience = "example.heph.invalid",
            present = {"env": {"DEMO_TOKEN": "${id_token}"}},
        ),

        # Anywhere: a command whose stdout is the credential. This is what a
        # `vault read` or an `aws configure export-credentials` looks like —
        # there is deliberately no per-product source kind, because that list has
        # no end. A source is `exec` with a field map, or it is a target.
        heph.auth.exec(
            ["sh", "-c", 'printf \'{"token":"demo-not-a-real-secret","expires_in":600}\''],
            fields = {"token": "token"},
            expires = "expires_in",   # seconds; a string would be RFC 3339
        ),
    ],

    # How the material reaches a consumer. Three keys and nothing else: `env`,
    # `files`, `helper`. There is deliberately nowhere to put a region, an
    # account or a profile — those *select bytes*, so they are ordinary hashed
    # inputs on the consumer.
    present = {"env": {"DEMO_TOKEN": "${token}"}},
)

# ── 2. the consumer ──────────────────────────────────────────────────────────
#
# One word. No branching on CI, no wrapper script, no environment names — and the
# same file on a laptop and a runner.
#
# `cache = True` (the default) is correct here, and worth pausing on: the output
# does not depend on *who* ran the target, only on that someone did. A target
# whose output embeds the identity — a `terraform plan` naming the account, an
# output holding a presigned URL — was never cacheable and says so with
# `cache = False`.
target(
    name = "uses-it",
    driver = "bash",
    credentials = [demo],
    out = "greeting.txt",
    # The token is never printed: heph would scrub it from the log anyway (see 4)
    # but a build step should not be reaching for it in the first place.
    run = [
        'if [ -n "$DEMO_TOKEN" ]; then',
        '  echo "authenticated" > $OUT',
        'else',
        '  echo "no identity" > $OUT && exit 1',
        'fi',
    ],
)

# ── 3. a credential whose source is a target ─────────────────────────────────
#
# When the acquiring command needs tools, deps, an environment or more than one
# step, it stops being an inline `exec` and becomes an ordinary target. Then
# everything heph already does — the runner, declared tools, the sandbox, output
# collection, process supervision — applies to acquisition for free.
#
# Two rules this form carries. It has no cheap probe (you cannot ask "is this
# applicable?" of a target without running it), so it is selected by `when`. And
# it MUST be `cache = False`: its outputs are material, and a cacheable target's
# outputs are written to the local cache and pushed to the shared remote
# automatically. heph enforces that rather than documenting it.
mint = target(
    name = "mint",
    driver = "bash",
    cache = False,                            # required of every credential source
    out = {"credential": "cred.json"},        # the reserved group, read as JSON
    # A named output group is `$OUT_<GROUP>`, exactly as anywhere else — a
    # credential source is an ordinary target and gets no special vocabulary.
    run = 'printf \'{"token":"minted-not-a-real-secret","expires_in":300}\' > $OUT_CREDENTIAL',
)

derived = target(
    name = "derived",
    driver = "credential",
    sources = [mint],
    present = {"env": {"DERIVED_TOKEN": "${token}"}},
)

target(
    name = "uses-derived",
    driver = "bash",
    credentials = [derived],
    cache = False,
    out = "ok.txt",
    run = 'test -n "$DERIVED_TOKEN" && echo ok > $OUT',
)

# ── 4. what heph does when a build step leaks one ────────────────────────────
#
# Declared credential values are scrubbed from a target's output at the tee,
# before any byte reaches `log.txt` — which is packed into the cache as an
# artifact and lifted into the failure event, so redacting at render would be too
# late.
#
# Run this and look at the output: the token is `[redacted]`.
#
# It is best-effort by construction, and that is stated rather than hidden:
# material shorter than 8 bytes is not scrubbed (a substring replacement cannot
# remove "1" without corrupting every number a build prints), and a secret the
# target transformed — base64, URL-encoded — passes through.
target(
    name = "leaks-it",
    driver = "bash",
    credentials = [demo],
    cache = False,
    out = "out.txt",
    run = [
        'echo "the token is $DEMO_TOKEN"',
        'echo done > $OUT',
    ],
)
