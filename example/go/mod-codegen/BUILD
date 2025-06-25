target(
    name = "gen",
    driver = "bash",
    run = [
        "echo > $OUT",
        "echo 'package main' >> $OUT",
        "echo 'const GeneratedVar = \"hello\"' >> $OUT",
    ],
    out = "gen.go",
    codegen = "copy",
    labels = "go_src",
)
