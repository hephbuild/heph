load("//test", "e2e_test")

target(
    name = "timeout",
    run = "sleep 2",
    timeout = "1s",
    cache = False,
)

e2e_test(
    name = "e2e_timeout",
    cmd = "heph run //test/features:timeout",
    expected_failure = True,
)
