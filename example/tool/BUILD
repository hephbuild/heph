print("in simple_deps")

tool = target(
    name = "mytool",
    run = [
        "echo 'echo hello' > $OUT",
        "chmod +x $OUT",
    ],
    driver = "bash",
    out = "mytool",
)

target(
    name = "result",
    run = [
        "which mytool",
        "mytool > $OUT",
    ],
    driver = "bash",
    tools = tool,
    out = "result",
)
