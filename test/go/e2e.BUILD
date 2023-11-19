load("//test", "e2e_test")

e2e_test(
    name = "sanity_go_version",
    cmd = "heph run //test/go:version",
    expect_output_contains = "go version go1.21.4",
)

e2e_test(
    name = "sanity_run_bin",
    cmd = "heph run //test/go/mod-simple:run",
    expect_output_contains = "Hello from mod-simple/hello",
)

e2e_test(
    name = "sanity_count_tests",
    cmd = "heph query -i //test/go/... | heph query -i test - | wc -l | xargs",
    expected_output = "25",
)

e2e_test(
    name = "sanity_count_tests2",
    cmd = "heph query '//test/go/... && test' | wc -l | xargs",
    expected_output = "25",
)

e2e_test(
    name = "sanity_ldflags_default",
    cmd = "heph run //test/go/mod-ldflags:run-default",
    expected_output = "default",
)

e2e_test(
    name = "sanity_ldflags_withflags",
    cmd = "heph run //test/go/mod-ldflags:run-withflags",
    expected_output = "overriden",
)

e2e_test(
    name = "sanity_lddeps",
    cmd = "heph run //test/go/mod-lddeps:run",
    expected_output = "coolversion",
)

e2e_test(
    name = "sanity_buildconstraint",
    cmd = "heph run //test/go/buildconstraint:build_all",
)
