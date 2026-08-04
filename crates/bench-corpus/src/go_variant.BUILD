# Written to `<corpus>/go/BUILD` by `heph-bench corpus`.
#
# Declares the Go build variant every target in the generated corpus is
# parameterized by. The go provider has no implicit default variant — a package
# with none in ancestry lists no build targets at all — so without this file the
# whole `go/` subtree lists nothing, and a Tier B scenario matches zero targets,
# builds nothing, and still exits 0.
#
# The platform is resolved at BUILD evaluation time rather than baked in when the
# corpus is generated, so the generator keeps its "same seed + same params =>
# byte-identical tree" promise and one corpus can be measured on any supported
# target without being regenerated. Both builtins already return canonical Go
# naming (`darwin`/`linux`, `arm64`/`amd64`).
#
# Not named plain `BUILD`: this lives in heph's own source tree, and the name it
# is copied to is the only place it should ever be read as a package definition.
provider_state(
    provider = "go",
    variants = {
        "host": {"goos": heph.core.os(), "goarch": heph.core.arch()},
    },
)
