# Repro of content_buddy's false-cycle `heph3 r build ./go` (after the `go_src`
# label fix): building the *whole graph* concurrently — not a single target —
# trips `build_lib -> _golist` self-package cycles.
#
# Mechanism: with `go_codegen_root`, every package's `_golist` issues a broad
# `q@label=go_src,package_prefix=<root>` query. That query `get_spec`s every
# sibling target to read its labels. When a shared library's `build_lib` is
# already in-flight via a real consumer (here both `cmd/a` and `cmd/b` import
# `shared`), the query re-enters that in-flight `build_lib`, closing a FALSE
# `build_lib -> _golist` cycle on the request DAG. Building a single target
# directly never overlaps that way, so it succeeds — confirming a product bug.
provider_state(
    provider = "go",
    go_codegen_root = True,
)

# Codegen target labelled `go_src` (the applied content_buddy fix), emitting a
# generated Go package one dir down.
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

# Dist targets labelled `build` (mirroring content_buddy's `*-lambda`/`*-compose`
# wrappers). `heph r build .` matches `And[Label(build), Package(...)]`, so the
# whole-graph query selects BOTH of these and builds them concurrently —
# `shared:build_lib` is in-flight via one while a sibling package's `go_src`
# query (run for its `_golist`) re-enters it, closing the false cycle.
target(
    name = "a-dist",
    driver = "sh",
    run = "mv ${SRC} ${OUT}",
    deps = ["//cmd/a:build"],
    out = "dist/a",
    labels = ["build"],
)

target(
    name = "b-dist",
    driver = "sh",
    run = "mv ${SRC} ${OUT}",
    deps = ["//cmd/b:build"],
    out = "dist/b",
    labels = ["build"],
)
