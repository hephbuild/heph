# Will generate one sandbox per e2e_main*.go file

main_files = glob("roots/**/e2e_main*.go")

for file in main_files:
    if file.startswith("e2e/lib"):
        continue

    dir = heph.path.dir(file)
    filename = heph.path.base(file)

    skip = len(glob(dir + "/.skip"))

    name = "e2e_" + dir.replace("roots/", "").replace("/", "_")
    if filename != "e2e_main.go":
        name += "_" + filename.replace("e2e_main", "").replace(".go", "").strip(".")

    build = target(
        name = "_" + name + "_build",
        deps = {
            "f": file,
            "files": [file, "go.mod"] + glob("go.sum") + glob(
                dir + "/**/*",
                exclude = main_files + ["**/BUILD"],
            ) + glob("lib/**/*"),
        },
        tools = "//:go|go",
        run = ["go build -o $OUT $(dirname $SRC_F)"],
        out = "test",
        src_env = {"f": "abs"},
    )

    target(
        name = name,
        deps = {
            "cfg": "e2e.hephconfig",
            "f": glob(dir + "/**/*", exclude = main_files),
        },
        run = [
            "cd " + dir,
            "cp $SRC_CFG .hephconfig",
            "$TOOL_TEST",
        ],
        cache = False,
        tools = [build, "heph", "docker"],
        labels = ["skip-e2e"] if skip else ["e2e"],
        src_env = "abs",
        pass_env = ["TERM", "PATH"],
    )
