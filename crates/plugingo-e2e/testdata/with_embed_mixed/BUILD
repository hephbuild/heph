target(
    name = "ui_dist",
    driver = "bash",
    run = [
        "mkdir -p $(dirname $OUT)",
        "echo '<html>generated</html>' > $OUT",
    ],
    out = "ui_dist/index.html",
    labels = ["go_embed_src"],
    codegen = "copy",
)
