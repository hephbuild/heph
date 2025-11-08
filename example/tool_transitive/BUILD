print("in simple_deps")

tool1 = target(
    name = "mytool1",
    run = [
        "echo 'echo hello1' > $OUT",
        "chmod +x $OUT",
    ],
    driver = "bash",
    out = "mytool1",
)

tool2 = target(
    name = "mytool2",
    run = [
        "echo 'echo hello2' > $OUT",
        "chmod +x $OUT",
    ],
    driver = "bash",
    out = "mytool2",
    transitive = struct(
        tools = tool1,
    ),
)

target(
    name = "result",
    run = [
        "which mytool1",
        "which mytool2",
    ],
    driver = "bash",
    tools = tool2,
)
