load("//test", "e2e_test")

deep_gen_1_build = """
hello_build_src = '''
target(
    name='hello-deep-gen',
    run='echo Hello deep gen',
    cache=False,
)
'''

hello = text_file(name="hello_build", text=hello_build_src)

target(
    name="deep_gen_1",
    run="mv $SRC $OUT",
    deps=hello,
    out="deep-gen-1.BUILD",
    gen=":hello-deep-gen",
)
"""

deep_gen_1 = text_file(
    name = "deep_gen_1_build",
    text = deep_gen_1_build,
)

target(
    name = "deep_gen_0",
    run = "mv $SRC $OUT",
    deps = deep_gen_1,
    out = "deep-gen-0.BUILD",
    gen = [
        ":deep_gen_1",
        ":hello_build",
        # deep:
        ":hello-deep-gen",
    ],
)

e2e_test(
    name = "e2e_echo",
    cmd = "heph run //test/features:hello-deep-gen",
    expected_output = "Hello deep gen",
)
