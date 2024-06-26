load("//test", "e2e_test")
load("//backend/go", "go_bin_build_addr")

bin = go_bin_build_addr("//test/go/mod-transitive-gen")

e2e_test(
    name = "e2e_gen_deps",
    cmd = "heph q deps '{}'".format(bin),
    expect_output_contains = """
//:go
//go_backend:_std_lib_OS_ARCH
//test/go/mod-transitive-gen:_go_lib_
""".replace("OS", get_os()).replace("ARCH", get_arch()).strip(),
)

e2e_test(
    name = "e2e_gen_revdeps",
    cmd = "heph q revdeps '{}'".format(bin),
    expected_output = "//test/go/mod-transitive-gen:test".strip(),
)

target(
    name = "deps1",
)

target(
    name = "deps2",
    deps = ":deps1",
)

target(
    name = "deps3",
    deps = ":deps2",
)

e2e_test(
    name = "e2e_deps",
    cmd = "heph q deps '//test/features:deps2'",
    expected_output = "//test/features:deps1".strip(),
)

e2e_test(
    name = "e2e_revdeps",
    cmd = "heph q revdeps '//test/features:deps2'",
    expected_output = "//test/features:deps3".strip(),
)

some_file = text_file(
    name = "_some_file",
    text = "hello",
    out = "file.txt",
)

deps_run = "set +e; env | grep SRC; ls"

target(
    name = "deps4",
    deps = some_file,
    run = deps_run,
    cache = False,
)

e2e_test(
    name = "e2e_deps4",
    cmd = "heph r '//test/features:deps4'",
    expected_output = "SRC=file.txt\nfile.txt",
)

target(
    name = "deps5",
    hash_deps = some_file,
    run = deps_run,
    cache = False,
)

e2e_test(
    name = "e2e_deps5",
    cmd = "heph r '//test/features:deps5'",
    expected_output = "",
)

target(
    name = "deps6",
    runtime_deps = some_file,
    run = deps_run,
    cache = False,
)

e2e_test(
    name = "e2e_deps6",
    cmd = "heph r '//test/features:deps6'",
    expected_output = "SRC=file.txt\nfile.txt",
)
