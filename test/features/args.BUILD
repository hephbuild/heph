load("//test", "e2e_test")

echo = target(
    name = "_goecho",
    run = "go build -o $OUT",
    deps = "echo.go",
    out = "goecho",
    tools = "//:go",
    env = {
        "GOOS": get_os(),
        "GOARCH": get_arch(),
    },
)

target(
    name = "exec_echo",
    run = ["$TOOL_GOECHO"],
    tools = echo,
    entrypoint = "exec",
    pass_args = True,
    cache = False,
)

target(
    name = "bash_echo",
    run = ['$TOOL_GOECHO "$@"'],
    tools = echo,
    entrypoint = "bash",
    pass_args = True,
    cache = False,
)

e2e_test(
    name = "e2e_exec_echo",
    cmd = "heph run //test/features:exec_echo -- 1 2 3",
    expected_output = "[1 2 3]",
)

e2e_test(
    name = "e2e_bash_echo",
    cmd = "heph run //test/features:bash_echo -- 1 2 3",
    expected_output = "[1 2 3]",
)
