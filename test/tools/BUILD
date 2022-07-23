load("//test", "e2e_test")

target(
    name="echoer",
    run="./echo.sh",
    deps=["echo.sh"],
    sandbox=True,
)

e2e_test(
    name="basic_run_sanity",
    cmd="heph run //test/tools:echoer",
    expect_output_contains="Hello, world",
)

e2e_test(
    name="basic_query_sanity",
    cmd="heph query alltargets",
    expect_output_contains="""
//test/tools:basic_query_sanity
//test/tools:basic_run_sanity
//test/tools:echoer
""".strip(),
)
