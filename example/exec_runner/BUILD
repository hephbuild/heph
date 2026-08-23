# Exec runners: choosing the environment a target's processes run in.
#
# See docs/EXEC_RUNNERS.md. Requires `devenv` on PATH (https://devenv.sh); the
# first run evaluates the environment and caches it, so it is slow once and
# free afterwards.
#
#   heph run //exec_runner:needs_jq
#   heph inspect runners
#   heph inspect def //exec_runner:needs_jq      # shows which runner, and why

# The runner itself is an ordinary target. Its ARTIFACT is the environment's
# identity, which is why `runner =` names a target rather than a config string:
# only a target has a hashout, and that is what carries the environment into
# every consumer's cache key.
devenv_env = target(
    name = "env",
    driver = "devenv",
)

# `jq` comes from devenv.nix's `packages`, so this only builds under the runner.
target(
    name = "needs_jq",
    driver = "bash",
    run = "jq --version > $OUT",
    out = "out.txt",
    runner = devenv_env,
)

# The same command with no runner. It FAILS, and that is the point: the runner's
# PATH replaces the driver's rather than sitting on top of it, so a tool missing
# from the environment does not quietly fall through to the host.
#
#   heph run //exec_runner:no_jq_without_runner   # expected to fail
target(
    name = "no_jq_without_runner",
    driver = "bash",
    run = "jq --version > $OUT",
    out = "out.txt",
)

# A variable devenv.nix itself sets. Captured into the snapshot, so it is in the
# cache key.
target(
    name = "reads_devenv_env",
    driver = "bash",
    run = "test \"$FROM_DEVENV_NIX\" = captured && echo ok > $OUT",
    out = "out.txt",
    runner = devenv_env,
)

# ---------------------------------------------------------------------------
# A runner declaring its own environment.
#
# Four keys, the same four a target has — and the split between them is where
# the cache key is drawn:
#
#   env               literal, captured  -> in the key
#   pass_env          host VALUE captured -> in the key (changing it re-keys)
#   runtime_env       literal, at spawn  -> declaration in the key
#   runtime_pass_env  host value at SPAWN -> only the NAME is ever hashed
#
# The last row is the one that matters. `SSH_AUTH_SOCK` differs per machine and
# per login; passing it with `pass_env` would bake one machine's value into the
# environment's identity and hand every other machine a cache key that lies.
# `runtime_pass_env` reads it at spawn, so it reaches the process and never the
# key.
declared_env = target(
    name = "declared",
    driver = "devenv",
    env = {"BUILD_CHANNEL": "stable"},
    pass_env = ["LANG"],
    runtime_env = {"NOISY_DEBUG": "1"},
    runtime_pass_env = ["SSH_AUTH_SOCK"],
)

target(
    name = "uses_declared",
    driver = "bash",
    run = "echo \"channel=$BUILD_CHANNEL debug=$NOISY_DEBUG\" > $OUT",
    out = "out.txt",
    runner = declared_env,
)

# A target always wins over its runner. Everything above is applied UNDERNEATH
# what the target sets itself, so a runner can never silently change what a
# target builds — including `$OUT` and the sandbox's own routing.
target(
    name = "target_wins",
    driver = "bash",
    run = "test \"$BUILD_CHANNEL\" = mine && echo ok > $OUT",
    out = "out.txt",
    env = {"BUILD_CHANNEL": "mine"},
    runner = declared_env,
)

# ---------------------------------------------------------------------------
# Session mode: a `devenv shell` held open, with every target's process forked
# from inside it. Costs one shell per heph process; in exchange the environment's
# shell functions and `enterShell` side effects are actually present.
#
# Snapshot mode cannot provide those, and says so by name rather than reporting
# a bare "not found in PATH".
session_env = target(
    name = "session",
    driver = "devenv",
    mode = "session",
)

target(
    name = "uses_session",
    driver = "bash",
    run = "type -t _activatePkgs > $OUT",
    out = "out.txt",
    runner = session_env,
)

# The same command under the snapshot runner. It FAILS: `_activatePkgs` is a
# shell function, and a captured environment is a set of variables.
#
#   heph run //exec_runner:no_functions_in_snapshot   # expected to fail
target(
    name = "no_functions_in_snapshot",
    driver = "bash",
    run = "type -t _activatePkgs > $OUT",
    out = "out.txt",
    runner = devenv_env,
)

# `runner = None` opts a target out of a workspace `defaultRunner:`. Bootstrap
# targets need it — something has to build before the environment exists.
target(
    name = "always_host",
    driver = "bash",
    run = "echo host > $OUT",
    out = "out.txt",
    runner = None,
)
