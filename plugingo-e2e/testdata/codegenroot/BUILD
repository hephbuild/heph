# Mirrors content_buddy's `go/BUILD`: the go provider's codegen *root* lives
# here, and a codegen target emits generated Go into a *sub*-package (`genpb/`),
# exactly like `buf generate` writing into `go/gen/...`.
provider_state(
    provider = "go",
    go_codegen_root = True,
)

# Codegen target labelled `go_src`. Its output is a Go *package* one directory
# down (`genpb/gen.go`), so other first-party packages import it as
# `codegenroot/genpb`.
target(
    name = "gen",
    driver = "bash",
    run = [
        "mkdir -p $(dirname $OUT)",
        "echo 'package genpb' > $OUT",
        "echo 'const GeneratedVar = \"hello\"' >> $OUT",
    ],
    out = "genpb/gen.go",
    labels = ["go_src"],
    codegen = "copy",
)
