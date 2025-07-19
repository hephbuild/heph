print("in simple_deps")

target(
    name = "a",
    driver = "bash",
    deps = "//cyclic_deps:b",
    labels = "cyclic_codegen",
)

target(
    name = "b",
    driver = "bash",
    deps = "//cyclic_deps:a",
    labels = "cyclic_codegen",
)

target(
    name = "c",
    driver = "bash",
    deps = "//@heph/query:query@label=cyclic_codegen",
)

