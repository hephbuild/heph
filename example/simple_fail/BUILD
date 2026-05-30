# print("in simple_deps")

log = [
    "echo stuff",
    "echo stuff",
    "echo stuff",
    "echo stuff",
    "echo stuff",
    "echo stuff",
    "echo stuff",
    "echo stuff",
    "echo not gucci",
]

d1 = target(
    name = "d1",
    run = log + [
        "exit 1",
    ],
    driver = "bash",
    out = "d1",
    cache = False,
)
d2 = target(
    name = "d2",
    run = log + [
        "exit 1",
    ],
    driver = "bash",
    out = "d2",
    cache = False,
)
target(
    name = "d3",
    run = log + [
        "exit 1",
    ],
    driver = "bash",
    out = "d3",
    deps = [d1, d2],
    cache = False,
)
