# Mirrors a real repo: the codegen root + the generator live in the *sub*-package
# that holds the generated asset, while the Go package that embeds it is the
# PARENT (`app`). The generated file lands at `app/openapi/openapi_gen.yaml`, i.e.
# inside app's tree, so `tree_output(app)` matches it.
provider_state(
    provider = "go",
    go_codegen_root = True,
)

target(
    name = "openapi-build",
    driver = "bash",
    run = ["echo 'openapi: generated' > $OUT"],
    out = "openapi_gen.yaml",
    codegen = "copy",
    labels = ["codegen", "go_src"],
)
