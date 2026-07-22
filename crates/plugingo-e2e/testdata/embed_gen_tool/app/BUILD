# Generated embed asset, produced by the in-tree go tool. `go_embed_src` keeps it
# out of `_golist` (the decoupled lane), so the compile must resolve it through the
# embed_src lane.
target(
    name = "openapi_gen",
    driver = "bash",
    run = [
        "mkdir -p $(dirname $OUT)",
        "$TOOL_GEN > $OUT",
    ],
    out = "openapi/openapi_gen.yaml",
    tools = {"gen": ["//tool:build@v=host"]},
    labels = ["go_embed_src"],
    codegen = "copy",
)
