def e2e_test(name, cmd, expected_failure=False, expected_output=None, expect_output_contains=None, expect_output_doesnt_contain=None):
    cmd = cmd.replace("heph ", "$HEPH --porcelain ")

    if expected_failure:
        test_cmd = '%s 2>&1 | tee output; if [ $? -eq 0 ]; then exit 1; fi; ' % cmd
    else:
        test_cmd = '%s 2>&1 | tee output && ' % cmd

    if expected_output:
        test_cmd += 'diff -au output $(location %s)' % expected_output
    elif expect_output_contains:
        test_cmd += '_STR="$(cat output)" _SUBSTR="{}" && if [ "${{_STR##*$_SUBSTR*}}" ]; then echo "$_STR"; exit 1; fi'.format(expect_output_contains)
    elif expect_output_doesnt_contain:
        test_cmd += '_STR="$(cat output)" _SUBSTR="{}" && if [ -z "${{_STR##*$_SUBSTR*}}" ]; then echo "$_STR"; exit 1; fi'.format(expect_output_doesnt_contain)
    else:
        test_cmd += 'true'

    return target(
        name=name,
        run=test_cmd,
        labels=["e2e_test"],
        sandbox=True,
    )
