load("//test", "e2e_test")

target(
    name="which_ls",
    run='which ls',
    tools='ls',
    cache=False,
)

e2e_test(
    name="e2e_host_tool",
    cmd="heph run //test/features:which_ls -- 1 2 3",
    expect_output_contains=".heph/sandbox/test/features/__target_which_ls/_bin/ls",
)
