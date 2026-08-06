target(
    name = "t1",
    driver = "bash",
    run = ["echo t1 > $OUT"],
    out = "t1.txt",
)
target(
    name = "t2",
    driver = "bash",
    run = ["echo t2 > $OUT"],
    out = "t2.txt",
)

# Aggregate: `g` re-exports t1 and t2's outputs unchanged. It is inlined into
# whoever depends on it and never executes.
target(
    name = "g",
    driver = "group",
    deps = [":t1", ":t2"],
)

target(
    name = "lib",
    driver = "bash",
    run = ["mkdir -p build/out && echo lib > build/out/lib.txt"],
    out = "build/out/lib.txt",
)

# Filter and relocate: `dist` re-exports the same bytes at different paths.
#
# This copies nothing — no sandbox, no subprocess, and no cache entry of its
# own. `lib` stays cached as it was, and `dist` is re-derived from a path list
# on each build. The alternative (a bash target running `cp`) would store a
# second copy of every byte locally and remotely.
#
# Patterns are written the way the *producing* target declares its outputs, so
# you never need to know where heph materializes them:
#
#   build/out/lib.txt  ->  release/lib.txt
#   t2.txt             ->  release/renamed.txt
#
# `heph inspect outputs //group:dist` prints the resulting paths.
target(
    name = "dist",
    driver = "group",
    deps = [":lib", ":t2"],
    include = ["**/*.txt"],
    strip_prefix = "build/out",
    prefix = "release",
    # The dict form moves specific paths, matched exactly against what the
    # target emits. It leaves everything else to strip_prefix/prefix above.
    rename = {"group/t2.txt": "release/renamed.txt"},
)

# The simpler form: when a group selects exactly one file, `rename` is just the
# destination. No source path is named, so this keeps working if `lib` changes
# where it writes — or moves to another package.
target(
    name = "server",
    driver = "group",
    deps = [":lib"],
    rename = "bin/server.txt",
)
