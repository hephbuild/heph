# Targets that sleep then fail — gives time to tab to the "failed" tab while running.

a = target(
    name = "a",
    run = [
        "echo building a",
        "sleep 3",
        "echo a done > $OUT",
    ],
    driver = "bash",
    out = "out",
    cache = False,
)

b = target(
    name = "b",
    run = [
        "echo building b",
        "sleep 4",
        "echo b failing now",
        "exit 1",
    ],
    driver = "bash",
    out = "out",
    cache = False,
)

# Depends on a (succeeds) and b (fails after sleep) — failure surfaces mid-run.
target(
    name = "fan",
    run = [
        "echo fan",
        "cat $SRC_A",
        "echo fan > $OUT",
    ],
    driver = "bash",
    deps = {
        "a": a,
        "b": b,
    },
    out = "out",
    cache = False,
)
