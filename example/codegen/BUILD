target(
    name="gen_file",
    driver = "bash",
    run = "echo hello > $OUT",
    out = "file",
    codegen = 'copy',
)

target(
    name="gen_dir",
    driver = "bash",
    run = [
        "mkdir $OUT",
        "echo hello > $OUT/hello",
        "echo world > $OUT/world",
    ],
    out = "dir/",
    codegen = 'copy',
)
