# A small service, built in one environment and checked in another.
#
# The shape is the point: the *build* toolchain and the *runtime* image are
# different environments, both named as targets, both in the cache key of what
# they produce.

# ── built under devenv ───────────────────────────────────────────────────────
#
# `jq` and `yq-go` are not installed on anyone's laptop, and `$APP_CHANNEL` is
# not in anyone's shell — all three come from //execrunner:devenv. Delete the
# `runner =` line and this target fails on the first line with
# `APP_CHANNEL: unbound variable`, which is the demonstration: without the
# runner, none of the environment is there.
#
# The env var is hashed along with the rest of the captured environment, so
# editing `env` in devenv.nix rebuilds this target.
target(
    name = "config_wrap",
    driver = "bash",
    runner = "//execrunner:devenv_wrap",
    deps = [glob("service.json")],
    run = [
        "jq --arg channel \"$APP_CHANNEL\" '. + {channel: $channel}' service.json > config.json",
        "yq -P '.' config.json > $OUT",
    ],
    out = "config.yaml",
)
target(
    name = "config_session",
    driver = "bash",
    runner = "//execrunner:devenv_session",
    deps = [glob("service.json")],
    run = [
        "jq --arg channel \"$APP_CHANNEL\" '. + {channel: $channel}' service.json > config.json",
        "yq -P '.' config.json > $OUT",
    ],
    out = "config.yaml",
)

# ── packaged ─────────────────────────────────────────────────────────────────

target(
    name = "dockerfile",
    driver = "textfile",
    # `debian:stable-slim` rather than `scratch`: the runtime check below runs a
    # real process in this image, so it needs a loader.
    #
    # `SRC_CFG` is injected by `docker_build` for the `cfg` context group below
    # — it is the context-relative path that group produced, so the Dockerfile
    # never has to know where heph staged it.
    text = """FROM debian:stable-slim
ARG SRC_CFG
COPY ${SRC_CFG} /etc/billing/config.yaml
""",
    out = "Dockerfile",
)

target(
    name = "image",
    driver = "docker_build",
    dockerfile = ":dockerfile",
    context = {"cfg": [":config_wrap"]},
    format = "docker",
)

# Puts the image in the local daemon under a fixed tag, which is what
# //execrunner:container resolves to a digest.
target(
    name = "load",
    driver = "oci_load",
    image = ":image",
    tag = "heph-example-billing:dev",
)

# ── checked inside the image it ships in ─────────────────────────────────────
#
# Runs in the container, not on the host: it reads the config from the path the
# service will read it from, in the filesystem the service will see.
#
# NOTE: this one needs a Linux host. The agent that runs a target inside a
# container is the heph binary itself, mounted in from the host — so a Darwin
# binary cannot execute in a Linux image. On macOS it fails in about a second
# with a message saying exactly that; everything above works anywhere.
target(
    name = "smoke",
    driver = "bash",
    runner = "//execrunner:container",
    run = [
        "test -f /etc/billing/config.yaml",
        "grep -q 'channel: stable' /etc/billing/config.yaml",
        "echo ok > $OUT",
    ],
    out = "smoke.txt",
)
