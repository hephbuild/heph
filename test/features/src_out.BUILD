load("//test", "e2e_test")

f1 = text_file(
    name = "f1",
    text = "a",
)

f2 = text_file(
    name = "f2",
    text = "a",
)

f3 = text_file(
    name = "f3",
    text = "a",
)

grep_env = [
    "env > e",
    "cat e | grep -E 'SRC|OUT' | sort || true",
]

target(
    name = "simple_src_out",
    deps = f1,
    out = "out",
    run = grep_env + ["touch $OUT"],
    cache = False,
)

e2e_test(
    name = "e2e_simple_src_out",
    cmd = "heph run //test/features:simple_src_out",
    expected_output = """
OUT=out
SRC=f1
""".strip(),
)

named_src_out = target(
    name = "named_src_out",
    deps = {
        "d1": f1,
        "d2": f2,
    },
    out = {
        "out1": "out1file",
        "out2": "out2file",
    },
    run = grep_env + ["touch $OUT_OUT1 $OUT_OUT2"],
    cache = False,
)

e2e_test(
    name = "e2e_named_src_out",
    cmd = "heph run //test/features:named_src_out",
    expected_output = """
OUT_OUT1=out1file
OUT_OUT2=out2file
SRC_D1=f1
SRC_D2=f2
""".strip(),
)

target(
    name = "named_src_named_out",
    deps = {
        "d1": f1,
        "d2": ":named_src_out",
        "d3": ":named_src_out|out1",
    },
    run = grep_env,
    cache = False,
)

e2e_test(
    name = "e2e_named_src_named_out",
    cmd = "heph run //test/features:named_src_named_out",
    expected_output = """
SRC_D1=f1
SRC_D2_OUT1=out1file
SRC_D2_OUT2=out2file
SRC_D3=out1file
""".strip(),
)

target(
    name = "unamed_src",
    deps = named_src_out,
    run = grep_env,
    cache = False,
)

e2e_test(
    name = "e2e_unamed_src",
    cmd = "heph run //test/features:unamed_src",
    expected_output = """
SRC_OUT1=out1file
SRC_OUT2=out2file
""".strip(),
)
