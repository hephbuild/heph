target(
    name = "gen",
    driver = "bash",
    run = [
        "echo > $OUT",
        "echo 'package main' >> $OUT",
        "echo 'const GeneratedVar = \"hello\"' >> $OUT",
    ],
    out = "gen.go",
    labels = ["go_src"],
    codegen = "copy",
)
