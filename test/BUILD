def e2e_test(name, cmd, expected_failure=False, expected_output=None, expect_output_contains=None, expect_output_doesnt_contain=None, expected_exit_code=None):
    cmd = cmd.replace("heph ", "heph --porcelain ")

    if expected_failure:
        test_cmd = '%s | tee output && RC=$? || RC=$?; if [ $RC -eq 0 ]; then exit 1; fi; ' % cmd
    else:
        test_cmd = '%s | tee output && RC=$? ' % cmd

    if expected_output:
        test_cmd += "echo '%s' > expected_output && diff -au output expected_output" % expected_output
    elif expect_output_contains:
        test_cmd += '_STR="$(cat output)" _SUBSTR="{}" && if [ "${{_STR##*$_SUBSTR*}}" ]; then echo "FAILED, got: $_STR"; exit 1; fi'.format(expect_output_contains)
    elif expect_output_doesnt_contain:
        test_cmd += '_STR="$(cat output)" _SUBSTR="{}" && if [ -z "${{_STR##*$_SUBSTR*}}" ]; then echo "FAILED, got: $_STR"; exit 1; fi'.format(expect_output_doesnt_contain)
    elif expected_exit_code:
        test_cmd += 'if [ $RC -ne {0} ]; then echo "expected exit code {0}, got $RC"; exit 1; fi;'.format(expected_exit_code)
    else:
        test_cmd += 'true'

    return target(
        name=name,
        run=test_cmd,
        labels=["e2e_test"],
        tools=['heph', 'diff'],
        cache=False,
        pass_env=["PATH"],
    )
