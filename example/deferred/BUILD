# Deferred values: a driver option whose value is computed by another target.
#
# A driver option's value has to be a literal in the BUILD file. But the value's
# owner is usually somewhere else — a role ARN in the Terraform that created the
# role, a registry per environment, a version in a release process. Copying it in
# makes a second source of truth that someone has to keep agreeing with the
# first, purely to satisfy heph.
#
# Every other build system's answer passes the value as a bare string, so the
# thing that produced it never becomes a node. Three things follow: the producer
# has to re-run eagerly on every build (which is why they all document "only use
# fast commands here"), the cache key can only hash the string or hide it, and
# "why did this rebuild?" bottoms out at the string.
#
# Here the producer is a node. Try:
#
#   heph run //deferred:image
#   heph inspect hashin //deferred:image       # note it
#   # edit deferred/version, re-run inspect    # it moved
#   # edit deferred/notes,   re-run inspect    # it did not

# ── the value's owner ────────────────────────────────────────────────────────
#
# An ordinary target. It declares what defines the value, so it re-runs when that
# changes and not otherwise — the option no other build system has, precisely
# because the producer is a real node.
#
# `notes` is in `deps` too, and is deliberately irrelevant to the output: editing
# it re-runs this target and invalidates nothing downstream. That is the
# difference between an edge and a string.
target(
    name = "version",
    driver = "bash",
    deps = {"v": [file("version")], "notes": [file("notes")]},
    out = "version.txt",
    run = ["tr -d '\\n' < $SRC_V > $OUT"],
)

target(
    name = "registry",
    driver = "bash",
    deps = [file("registry")],
    out = "registry.txt",
    run = ["tr -d '\\n' < $SRC > $OUT"],
)

# ── the consumer ─────────────────────────────────────────────────────────────
#
# A template may mix literals and references, and may name more than one
# producer. Writing a computed string into a file is a shell's job, so this one
# is `bash` — which takes references too, because heph claims a `${...}` only
# when its argument is an absolute address. `${src:0:3}` stays bash, and `$$`
# stays a PID.
#
# Worth knowing here: heph substitutes, it does not quote. This value is spliced
# into a shell program, so a producer emitting `1.2.3; rm -rf /` would run it —
# and those bytes may have come from the shared remote cache. See the `exec`
# target below for the case where nothing is spliced into anything.
target(
    name = "image",
    driver = "bash",
    out = "image.txt",
    run = 'printf "%s" "${read://deferred:registry}/app:${read://deferred:version}" > $OUT',
)

# ── the other kind: a path, not the contents ─────────────────────────────────
#
# `${src://pkg:name}` is the sandbox path of a producer's artifact — `$SRC_<GROUP>`
# finally available in an `exec` argv, and without a `deps` entry to keep in sync.
#
# `exec`, and this time there is genuinely no shell: the path is one argv element
# handed straight to `cp`. Nothing parses it, so nothing can be spliced into it.
# That is the case `$SRC_<GROUP>` could never serve, because expanding it needs a
# shell and there isn't one.
#
# It is a `deps`-shaped edge (hashed, staged) with one deliberate difference: the
# producer's `transitive` env and tools do NOT follow. Writing a path expression
# asks for a path. Take the artifact as an ordinary `deps` entry when you do want
# the environment.
target(
    name = "version-copy",
    driver = "exec",
    out = "version.copy",
    run = ["cp", "${src://deferred:version}", "version.copy"],
)

# ── what is refused, and why ─────────────────────────────────────────────────
#
# Uncomment either of these and heph will tell you where the value belongs.
#
# A reference never changes which targets *exist* — only what one does. So it is
# rejected in everything that decides graph shape or identity: `deps`, `tools`,
# `runner`, `out`, `name`, a glob, an address filter. That is what keeps cycle
# detection working at parse, keeps `heph query` from triggering a build, and
# keeps evaluation from ever blocking on one — which is the whole of Nix's
# import-from-derivation problem.
#
# target(name = "no", driver = "bash", out = "${read://deferred:version}", run = "true")
#
# `bash` mode takes references too — heph claims a `${...}` only when its argument
# is an absolute address, so `${src:0:3}` stays bash and `$$` stays a PID:
#
# target(name = "also-ok", driver = "bash", out = "v.txt", cache = False,
#        run = 'printf "%s" "${read://deferred:version}" > $OUT')
