# pluginexec Sandbox

This document describes how the pluginexec driver constructs and isolates the process environment for target execution.

## Environment Isolation

**The parent process environment is fully cleared** (`env_clear()`). No host variables leak into the sandbox. Only variables explicitly declared in the target spec or injected by the engine are available.

## Environment Variables

### User-declared variables

Three mechanisms exist for injecting environment variables, differing in when they are resolved and whether they affect the cache hash:

| Field             | Resolved at  | Affects hash | Description                                    |
|-------------------|-------------|--------------|------------------------------------------------|
| `pass_env`        | parse time   | yes          | Capture named vars from host at parse time     |
| `runtime_pass_env`| execute time | no           | Pass named vars from host at execution time    |
| `runtime_env`     | execute time | no           | Inject literal key=value pairs at runtime      |

- `pass_env` and `runtime_pass_env`: if the named variable is absent from the host, it is silently skipped (not an error).
- Only `run`, `deps`, and `pass_env` contribute to the target's cache hash. `runtime_pass_env` and `runtime_env` are excluded intentionally so they can carry dynamic values without invalidating cached outputs.

### Auto-injected output variables

For each declared output group:

| Variable      | Value                                                      |
|---------------|------------------------------------------------------------|
| `OUT`         | Space-separated paths for the default (unnamed) group      |
| `OUT_<GROUP>` | Space-separated paths for the named group (uppercased)     |

Glob-pattern outputs do not populate these variables (paths are unknown before execution).

### Auto-injected input/dependency variables

For each dependency group after inputs are unpacked:

| Variable          | Value                                                              |
|-------------------|--------------------------------------------------------------------|
| `SRC`             | Space-separated unpacked file paths for the default group          |
| `SRC_<GROUP>`     | Space-separated unpacked file paths for the named group (uppercased)|
| `LIST_SRC`        | Absolute path to a `.list` file containing one path per line (default group) |
| `LIST_SRC_<GROUP>`| Absolute path to a `.list` file for the named group                |

The `.list` files are written to the sandbox root as `dep_<group>.list` before the process starts. They are useful when the number of inputs would overflow command-line limits.

## Working Directory and Filesystem Layout

```
<engine_home>/sandbox/<package>/<target>/
├── ws/
│   └── <package>/          ← process current_dir (cwd)
├── dep_<group>.list         ← one per dep group (SRC list files)
└── log.txt                  ← merged stdout + stderr
```

- The process runs with `cwd = ws/<package>`.
- All declared output paths are relative to this directory.
- Input artifacts are unpacked into `ws/` before the process starts.
- There is no OS-level filesystem isolation (no chroot or container). The sandbox is a logical boundary enforced by path scoping.

## Driver Modes

### `exec`
Commands are executed directly. `run` must be a list of the executable and its arguments.

### `bash`
Commands are joined with newlines and passed to `bash` with:
- `--noprofile --norc` — skip bash startup files
- `-u` — error on undefined variables
- `-e` — exit on first error
- `-o pipefail` — pipeline fails if any stage fails

## Process Lifecycle

- `kill_on_drop(true)`: process is terminated if the driver is dropped.
- Cancellation: if the cancellation token fires, the child is killed and the run returns `Err("cancelled")`.
- Stdin: piped from the request if provided, otherwise `/dev/null`.
- Stdout/stderr: teed to both `log.txt` and the optional caller sink simultaneously.
- Non-zero exit code: run fails; `log.txt` content is included in the error message.

## Target Spec Reference

```starlark
exec(
    run = ["executable", "arg1"],   # or bash(run = ["shell command"])

    deps = {
        ""       : ["//pkg:target"],        # default group → SRC
        "tools"  : ["//tools:compiler"],    # named group   → SRC_TOOLS
    },

    out = {
        ""      : ["output.bin"],           # default group → OUT
        "dist"  : ["dist/"],                # named group   → OUT_DIST (trailing / = dir)
        "objs"  : ["*.o"],                  # glob (no OUT_ variable set)
    },

    cache = True,                   # default; False disables local + remote caching

    pass_env         = ["CC", "CXX"],               # captured at parse time, hashed
    runtime_pass_env = ["HOME", "USER"],             # captured at run time, not hashed
    runtime_env      = {"BUILD_MODE": "release"},    # literal, not hashed
)
```

Output path rules:
- Trailing `/` → collected as a directory tree (`DirPath`)
- Contains `*`, `?`, or `[` → treated as a glob (`Glob`)
- Otherwise → single file (`FilePath`)
