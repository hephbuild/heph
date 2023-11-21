def e2e_test(
    name,
    cmd,
    expected_failure = False,
    expected_output = None,
    expect_output_contains = None,
    expect_output_doesnt_contain = None,
    expected_exit_code = None,
    deps = None,
):
    cmd = cmd.replace("heph ", "heph --porcelain ")
    cmd = "(" + cmd + ")"

    if expected_failure:
        test_cmd = '%s | tee output && RC=$? || RC=$?; if [ $RC -eq 0 ]; then exit 1; fi; echo "Command exited with code $RC, as expected"; ' % cmd
    else:
        test_cmd = "%s | tee output && RC=$? " % cmd

    if expected_output:
        test_cmd += "echo '%s' > expected_output && diff -Nau output expected_output" % expected_output
    elif expect_output_contains:
        test_cmd += '_STR="$(cat output)" _SUBSTR="{}" && if [ "${{_STR##*$_SUBSTR*}}" ]; then echo "FAILED, expected: $_SUBSTR"; exit 1; fi'.format(expect_output_contains)
    elif expect_output_doesnt_contain:
        test_cmd += '_STR="$(cat output)" _SUBSTR="{}" && if [ -z "${{_STR##*$_SUBSTR*}}" ]; then echo "FAILED, did not expected: $_SUBSTR"; exit 1; fi'.format(expect_output_doesnt_contain)
    elif expected_exit_code:
        test_cmd += 'if [ $RC -ne {0} ]; then echo "expected exit code {0}, got $RC"; exit 1; fi;'.format(expected_exit_code)
    else:
        test_cmd += "true"

    test_cmd = "({}) && echo E2E PASSED".format(test_cmd)

    return target(
        name = name,
        run = test_cmd,
        labels = ["light_e2e"],
        tools = ["heph", "diff"],
        cache = False,
        pass_env = ["PATH", "TERM"],
        deps = deps,
    )
