# Exec runners: two environments this package's builds can run in.
#
# A runner is a target whose output is a `runner.json`. Other targets point at
# one with `runner = "//execrunner:devenv"` and their command runs there
# instead of on the bare host. See docs/EXEC_RUNNERS.md.
#
# Both runners are hashed dependencies of whatever uses them, so changing the
# environment re-keys every target that runs in it — a devenv.lock bump or a
# rebuilt image invalidates the builds that happened inside them, which is the
# whole reason to name the environment as a target rather than assume it.

# ── the build environment ────────────────────────────────────────────────────
#
# `jq` and `yq-go`, from nixpkgs, for anyone who runs a build here. Needs
# `devenv` on PATH; the first evaluation is slow (nix), every later one is not.
#
# `mode = "wrap"` captures the environment once and runs targets locally in it:
# no devenv process per target, no shell evaluation on the hot path. Switch to
# `mode = "session"` when the environment is more than variables — services it
# starts, state under .devenv/ — and one shell is held open for the build.
target(
    name = "devenv",
    driver = "devenv_runner",
    mode = "wrap",
    # What the environment is made of. The fingerprint is NOT derived from
    # these — devenv.nix can import files nobody declared — but they are what
    # makes the runner rebuild when you edit them.
    deps = [glob("devenv.*")],
)

# ── the runtime environment ──────────────────────────────────────────────────
#
# The image the service actually ships in, so a check can run against the real
# runtime rather than against the developer's machine. Needs a docker daemon.
#
# `image` must already be in the daemon, which is what the `deps` on the
# `oci_load` target below arranges — and, because that load is a hashed dep, a
# rebuilt image re-derives this runner and re-keys everything that ran in it.
target(
    name = "container",
    driver = "oci_runner",
    image = "heph-example-billing:dev",
    deps = ["//execrunner/app:load"],
)
