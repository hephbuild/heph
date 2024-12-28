time = target(
    name = "time",
    run = [
        "echo $(date) > $OUT",
    ],
    driver = "bash",
    cache = False,
    out = "time",
)

def genDeps(n, prefix, deep):
    deps = []
    for i in range(0, n):
        deep_deps = []
        if deep > 0:
            deep_deps = genDeps(n, prefix+"nested_", deep-1)
        deep_deps.append(time)

        t = target(
            name = "{}{}".format(prefix, i),
            run = [
                "sleep 0.1",
                "echo running {} {}".format(prefix, i),
                "echo hello > $OUT",
            ],
            driver = "bash",
            out = "{}d{}".format(prefix, i),
            deps = deep_deps,
        )
        deps.append(t)

    return deps

target(
    name = "result",
    run = [
        "echo run result",
    ],
    driver = "bash",
    deps = genDeps(10, "", 3),
)

