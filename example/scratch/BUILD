# Scratch caches: a directory a target keeps between runs.
#
# Everything else heph gives a target is hermetic — declared inputs in, declared
# outputs out, and a sandbox thrown away afterwards. A scratch is the deliberate
# exception: a named directory that survives, is shared by every target that
# references it, and is never hashed.
#
# The rule that makes it safe is one sentence: **a target's outputs must be
# identical whether its scratch is warm, cold, or absent.** A scratch may only
# make a target faster. Check it with `heph run --no-scratch //scratch:...` —
# same outputs, or the target is depending on carried-over state and is broken.
#
# Because it is never hashed, a scratch never invalidates anything. Changing one
# rebuilds nothing; deleting one costs time and nothing else.

# ── 1. a download cache ──────────────────────────────────────────────────────
#
# The simplest useful shape, and the one to reach for first: **no `path`**. The
# directory is never placed in the sandbox at all — the target finds it through
# the environment variable, which is how most tools want to be told anyway.
#
# Nothing is mounted, so nothing can go wrong with a mount: no output glob can
# reach it, and no dependency can be shadowed by it.
#
# No `version`, which is what declares a cache portable: these are downloaded
# bytes, identical on every machine. `remote = True` lets CI publish one and a
# laptop pick it up.
target(
    name = "downloads",
    driver = "scratch",
    env = "DOWNLOAD_CACHE",   # the variable consumers read the path from
    access = "shared",        # concurrent readers/writers are fine here
    remote = True,            # may travel; see the README
    max_size = "2GiB",        # over this, the cache is dropped whole and refills
)

# A target that "fetches" something. The second run finds it already there.
#
# Note what it does *not* do: it never fails when the cache is empty, and it
# produces the same `payload.txt` either way. That is the whole contract.
target(
    name = "fetch",
    driver = "bash",
    scratch = ["//scratch:downloads"],
    out = "payload.txt",
    run = [
        'if [ -f "$DOWNLOAD_CACHE/payload" ]; then',
        '  echo "cache hit"',
        'else',
        '  echo "downloading..."',
        '  echo "the payload" > "$DOWNLOAD_CACHE/payload"',
        'fi',
        # Same bytes on a hit or a miss. This is the line that makes the target
        # honest, and `--no-scratch` is what proves it.
        'cp "$DOWNLOAD_CACHE/payload" $OUT',
    ],
)

# ── 2. a compiler cache, shared ──────────────────────────────────────────────
#
# `version` is the *whole* of a cache's identity beyond its address. heph does
# not guess: if the contents depend on the machine, say so. Here they are
# compiled objects, so the host os/arch belongs in the key — otherwise a Linux
# CI runner and a macOS laptop would share one directory and each would find
# nothing but the other's misses.
#
# `access = "shared"` says concurrent use is safe *by construction* — the way a
# Go build cache or ccache is. It is an assertion about the tool, not a wish:
# get it wrong and two targets corrupt the cache.
objcache = target(
    name = "objcache",
    driver = "scratch",
    path = ".cache/obj",      # mounted here, for a tool that wants a location
    env = "OBJCACHE",
    access = "shared",
    version = heph.core.os() + "/" + heph.core.arch(),
)

# Two targets sharing one cache. Build them together — `heph run //scratch:...`
# — and they run concurrently, because `shared` says that is safe.
def compile(name, src):
    target(
        name = name,
        driver = "bash",
        scratch = [objcache],
        out = name + ".o",
        run = [
            'key=$(echo "%s" | cksum | cut -d" " -f1)' % src,
            'if [ -f "$OBJCACHE/$key" ]; then',
            '  echo "%s: cached"' % name,
            'else',
            '  echo "%s: compiling"' % name,
            '  echo "compiled %s" > "$OBJCACHE/$key"' % src,
            'fi',
            'cp "$OBJCACHE/$key" $OUT',
        ],
    )

compile("lib_a", "a.src")
compile("lib_b", "b.src")

# ── 3. a cache for a tool that is not concurrency-safe ───────────────────────
#
# `exclusive` is the default, and the reason it is: most tools assume they own
# their cache directory. heph serializes every target referencing this one — a
# real lock, held across the whole run, and across separate `heph` processes on
# the same machine.
#
# The cost is real: those targets no longer run in parallel with each other.
# Reach for `shared` only when the tool documents that it is safe.
target(
    name = "index",
    driver = "scratch",
    env = "INDEX_DIR",
    access = "exclusive",
)

def indexed(name):
    target(
        name = name,
        driver = "bash",
        scratch = ["//scratch:index"],
        out = "n.txt",
        run = [
            # `mkdir` is an atomic test-and-set: if another target were inside
            # the cache right now, this would fail. Under `exclusive` it cannot.
            'mkdir "$INDEX_DIR/lock" || { echo "OVERLAPPED" >&2; exit 1; }',
            'echo "%s" >> "$INDEX_DIR/log"' % name,
            'rmdir "$INDEX_DIR/lock"',
            'wc -l < "$INDEX_DIR/log" | tr -d " " > $OUT',
        ],
    )

indexed("index_one")
indexed("index_two")
