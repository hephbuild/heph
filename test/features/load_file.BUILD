load("//test/features/load_file_src.BUILD", "load_file_source")
load("//test", "e2e_test")

src = json_file(
    name = "_load_file_source",
    data = load_file_source(),
)

target(
    name = "cat_load_file_source",
    deps = src,
    run = ["echo >> $SRC", "cat $SRC"],
    cache = False,
)

e2e_test(
    name = "e2e_load_file_source",
    cmd = "heph run //test/features:cat_load_file_source",
    expected_output = "[1,2,3]",
)
