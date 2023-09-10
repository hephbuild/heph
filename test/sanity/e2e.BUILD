load("//test", "e2e_test")

e2e_test(
    name = "sanity_echo",
    cmd = "heph run //test/sanity:echo",
    expected_output = "Hello world",
)

e2e_test(
    name = "sanity_echo-hash-input",
    cmd = "heph run //test/sanity:echo-hash-input",
    expected_output = "106c6f40671ba69f23bf11f1e40198f8",
)

e2e_test(
    name = "sanity_count-lorem",
    cmd = "heph run //test/sanity:count-lorem",
    expected_output = "69",
)

e2e_test(
    name = "basic_query_sanity",
    cmd = "heph query -a | grep '//test/sanity'",
    expected_output = """
//test/sanity:basic_query_sanity
//test/sanity:codegen-copy
//test/sanity:codegen-link
//test/sanity:count-lorem
//test/sanity:custom-exit-code
//test/sanity:echo
//test/sanity:echo-hash-input
//test/sanity:sanity_codegen_copy
//test/sanity:sanity_codegen_link
//test/sanity:sanity_count-lorem
//test/sanity:sanity_echo
//test/sanity:sanity_echo-hash-input
//test/sanity:sanity_exit-code
""".strip(),
)

e2e_test(
    name = "sanity_exit-code",
    cmd = "heph run //test/sanity:custom-exit-code",
    expected_failure = True,
    expected_exit_code = 42,
)

e2e_test(
    name = "sanity_codegen_link",
    cmd = "heph run //test/sanity:codegen-link; cd $(heph query root)/$PACKAGE; ls codegen_link; rm codegen_link",
    expected_output = "codegen_link",
)

e2e_test(
    name = "sanity_codegen_copy",
    cmd = "heph run //test/sanity:codegen-copy; cd $(heph query root)/$PACKAGE; ls codegen_copy; rm codegen_copy",
    expected_output = "codegen_copy",
)
