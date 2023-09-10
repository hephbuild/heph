load("//test", "e2e_test")

toolsh = text_file(
    name = "cool_tool_sh",
    text = '#!/bin/sh\necho hello "$@"',
    mode = 777,
)

tool = tool_target(
    name = "cool_tool",
    tools = toolsh,
)

e2e_test(
    name = "e2e_tool",
    cmd = "heph run " + tool + " -- raphael",
    expected_output = "hello raphael",
)
