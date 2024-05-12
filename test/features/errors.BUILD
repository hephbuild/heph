load("//test", "e2e_test")

will_err = target(
    name = "_will_err",
    run = "echo failed; exit 1",
    cache = False,
)

target(
    name = "err1",
    deps = will_err,
    cache = False,
    labels = 'will-err',
)

target(
    name = "err2",
    deps = will_err,
    cache = False,
    labels = 'will-err',
)

e2e_test(
    name = "e2e_errors",
    cmd = "HEPH_SKIP_PATH_LOG=1 heph run '//test/features && will-err'",
    expected_failure = True,
    expected_output = """
ERR| //test/features:_will_err failed: exec: exit status 1

failed

ERR| exec: exit status 1
""".strip(),
)
