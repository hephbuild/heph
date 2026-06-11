# Faithful minimal repro of content_buddy's `go/BUILD` failure:
#   - `go_codegen_root` set on the module root,
#   - a codegen target that writes generated Go into a *sub*-package tree
#     (`gen/pb/v1/`, like `buf generate` → `go/gen/core/...`),
#   - the codegen target labelled `"codegen"` — NOT `"go_src"` — so it is never
#     wired into consumers via the go provider's `q@label=go_src` query. The
#     generated packages are expected to be found by the filesystem walk
#     (`codegen = "copy"` writes them to disk).
#
# Combined with `fs.skip = ["gen/**"]` (content_buddy skips `go/gen/**`), the go
# provider prunes the generated packages from its walk, so `//gen/pb/v1:_golist`
# resolves to `TargetNotFound` and every importer fails with that + cascading
# cyclic-dependency errors — exactly `heph3 r build ./go`.
provider_state(
    provider = "go",
    go_codegen_root = True,
)

target(
    name = "gen",
    driver = "bash",
    run = [
        "mkdir -p $(dirname $OUT)",
        "echo 'package v1' > $OUT",
        "echo 'const GeneratedVar = \"hello\"' >> $OUT",
    ],
    out = "gen/pb/v1/gen.go",
    labels = ["codegen"],
    codegen = "copy",
)
