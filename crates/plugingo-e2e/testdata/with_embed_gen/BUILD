target(
    name = "resources",
    driver = "bash",
    run = [
        "mkdir -p $(dirname $OUT)",
        "echo 'echo generated' > $OUT",
    ],
    out = "resources/gen.sh",
    labels = ["go_embed_src"],
    codegen = "copy",
)
