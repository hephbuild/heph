# In-place codegen — a formatter that rewrites its OWN, committed source files.
#
# `codegen = "in_place"` tells heph to write the target's collected outputs back
# onto the source tree. These files are tracked/committed (unlike `codegen =
# "copy"` outputs, which are generated & meant to be gitignored — heph stamps
# them so a later `glob()` excludes them).
#
# The magic `//@heph/introspect:outputs` dep is expanded by the engine, per
# declared output path, into an input that reads that path. So the target's
# inputs ARE its outputs: "format these files in place". The expanded inputs are
# surfaced to the run script through `$SRC_<GROUP>` / `$LIST_SRC_<GROUP>`.
#
# Because the transform is idempotent, re-running over the already-formatted tree
# is a no-op CACHE HIT (fixed-point caching) — no re-execution. And:
#
#   heph run //fmt:fmt            # format the .txt files in place
#   heph run //fmt:fmt --frozen   # CI check: fail (with a diff) if not formatted
#
target(
    name = "fmt",
    driver = "bash",
    codegen = "in_place",
    deps = {"src": ["//@heph/introspect:outputs"]},
    out = "*.txt",
    run = [
        # cwd = this package's sandbox workspace. Our "formatter" just uppercases
        # every input file in place (an idempotent transform: f(f(x)) == f(x)).
        'for f in $SRC_SRC; do tr a-z A-Z < "$f" > "$f.fmt" && mv "$f.fmt" "$f"; done',
    ],
)
