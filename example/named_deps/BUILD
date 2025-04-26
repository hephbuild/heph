foo = target(
    name = "foo",
    run = [
        "echo o1 > $OUT_O1",
        "echo o2 > $OUT_O2",
    ],
    driver = "bash",
    out = {
        "o1": "oa",
        "o2": "ob",
    },
)

bar = target(
    name = "bar",
    run = [
        "echo o1 > $OUT_O1",
        "echo o2 > $OUT_O2",
    ],
    driver = "bash",
    out = {
        "o1": "oc",
        "o2": "od",
    },
)

target(
    name = "check",
    run = [
        'echo a: $SRC_A',
        'echo b: $SRC_B',
        'echo o1', 'cat $SRC_A',
        'echo o2', 'cat $SRC_B',
    ],
    driver = "bash",
    cache = False,
    deps = {
        "a": [foo+"|o1", bar+"|o1"],
        "b": [foo+"|o2", bar+"|o2"],
    },
)