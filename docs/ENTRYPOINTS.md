# Entrypoints

An entrypoint is a target you *run*, not build: a dev server, a formatter, a
CLI pinned to the workspace's version of a tool. `entrypoint = True` on an
`exec` or `bash` target makes `heph run` build its closure (deps, tools,
credentials), then run its command in **your** terminal, in **your**
directory, with **your** environment — the way you would run the tool by hand.

```python
# web/BUILD
target(name = "dev", driver = "bash", entrypoint = True, args = True,
       deps = {"node": "//tools:node"},
       run = '"$SRC_node/bin/node" server.js "$@"')
```

```sh
heph run //web:dev -- --port 4000
```

An entrypoint is never cached, never another target's input, and never part of
a multi-target selection: it is invoked on its own, by address.

## Options

| Option | Default | Meaning |
|---|---|---|
| `entrypoint` | `False` | Run the target in the caller's world instead of building it. |
| `args` | `False` | Append the arguments after `--` to the command (`"$@"` in `bash`). Without it, arguments are refused before anything builds. |
| `workdir` | `"caller"` | Where the command runs: `"caller"` (the directory you ran `heph` from, which may be outside the workspace), `"root"` (the workspace root), or `"sandbox"` (the invocation's own sandbox, as a build runs). Nothing written in the sandbox survives the run, so `"sandbox"` suits tools that read the closure or print, not ones that act on your tree. `scratch` is accepted only with `"sandbox"`. |

Refused on an entrypoint, each with what to do instead: `out`, `cache`,
`codegen`, `support_files`, `pass_env`, `runtime_pass_env`,
`runtime_env`, `runner`. An `exec` argv element naming `$SRC_x`, `$TOOL_x` or
`$OUT` is refused too — even inside `["sh", "-c", "…"]`: exec passes its argv
as written, and heph cannot tell a variable meant for a shell you start from
literal text handed to the program. Use `${src://<pkg>:<name>}` for a dep's
path, or `driver = "bash"`, where the shell expands `$SRC_x` (single-quote a
literal `$`, as in awk's `'$OUT'`).

Entrypoints always run on the OS sandbox: `fuse.enabled` is ignored for them.

## What the tool sees

- **Environment**: yours, byte for byte, with the target's `env` and its
  credentials layered on top.
- **`$WORKSPACE_ROOT`** is the *sandbox's* workspace dir, consistent with
  `$SRC_x`: the closure's paths live there.
- **`$HEPH_ROOT`** is the *real* workspace root, the one you checked out. Use it
  for a tool that acts on your tree from another `workdir`.
- **Inputs** are laid out as for a build, in a sandbox private to this
  invocation: a dep is a copy, while tools and `read_only_deps` are linked
  read-only from heph's shared stage. Do not write next to a linked input (a
  `.vite` cache, `__pycache__`): the write would reach every build that links
  the same entry. Two invocations of the same target run concurrently, each in
  its own sandbox, which is removed when the tool exits.
- **Terminal and signals**: the tool inherits heph's stdin, stdout and stderr,
  and the signal dispositions and signal mask heph itself inherited (under
  `nohup`, SIGINT stays ignored; under `trap '' PIPE`, SIGPIPE stays ignored).
  The open-file limit is yours, not heph's raised one — for every driver,
  plugin drivers included.

## Exit status

| Status | When |
|---|---|
| the tool's code | The tool exited, with any code, 0 included. Nothing is added. |
| the tool's signal | The tool was killed by a signal: heph dies of the same signal (no core dump of its own), so a shell sees "killed by SIGINT", not `130`. |
| `125` | heph failed after the target resolved to an entrypoint, before the tool ran: a dependency failed to build, an argument was refused, the closure could not be materialized, a credential could not be obtained, or the program could not be started (not found, not executable). The error is shown. |
| `130` | Ctrl-C cancelled the run before the tool started, while its dependencies were building. |
| `1` | The target could not be resolved at all (a typo, a broken BUILD file, not an entrypoint). |

heph writes nothing to stdout during an invocation; its progress goes to
stderr.

## Signals while the tool runs

From the moment the tool is about to start until it exits, heph is
transparent: every signal is the tool's, and heph acts on none of them.

Once the tool has exited, heph may still be finishing on its behalf —
uploading the freshly built dependencies to a remote cache. Signals are heph's
again then: Ctrl-C, TERM or HUP cancel what is left and heph exits at once,
still with the tool's status, and without printing a cancellation.

- Ctrl-C, Ctrl-\ and Ctrl-Z come from the terminal straight to the tool, which
  shares heph's process group. A second Ctrl-C is not special.
- A signal sent to heph's own pid — `kill <pid>`, `timeout 10 heph run …`, a CI
  runner stopping the job — is forwarded to the tool once: TERM, HUP, INT, QUIT,
  USR1, USR2 and WINCH. heph then exits with the tool's status, so `timeout`
  leaves no tool behind.
- Two limits of sharing the terminal's process group:
  - While heph is the terminal's foreground job, a `kill -INT`, `-QUIT` or
    `-HUP` sent to heph's pid from elsewhere is dropped: heph cannot tell it
    from the terminal's own, which has already reached the tool. TERM, USR1
    and USR2 always arrive.
  - A signal sent to the whole group (`kill -TERM -<pgid>`, `kill 0` from a
    script in it) reaches the tool directly *and* through heph, so it may
    arrive twice.
- A tool that traps INT and QUIT and keeps running can still be stopped with
  `kill -9 <heph pid>`: heph's supervisor then kills and reaps the tool.

Before the tool starts — dependencies building, the closure being laid out —
heph is an ordinary run: Ctrl-C cancels it, TERM and HUP end it, SIGQUIT
writes its diagnostic dump, as for any command. Nothing sent then is held for
the tool. While the TUI has stepped aside, laying out a large closure prints
nothing; it is still heph's, and Ctrl-C still cancels it.

## What will it run?

`heph run --shell //web:dev -- --port 4000` opens a shell where the tool would
run, with the same environment and the arguments as the shell's positional
parameters. `show` prints the command `run` would execute, with the arguments
quoted; `run` runs it (`run --port 5000` replaces the arguments for that call);
`exit 3` leaves the shell and heph exits 3.

`heph inspect def //web:dev` shows `"runs_as": "exec, entrypoint (uncached)"`,
a line for people. A script or an agent reads `target_def.entrypoint` from the
same output instead: `{"accepts_args": true, "workdir": "caller"}`, absent
for a target that is not an entrypoint.

## Plugins

Any driver can declare an entrypoint: its `parse` sets
`TargetDef.entrypoint`, and its ordinary `run` honours the generic
`RunOptions` the engine sends (cwd, base environment, args, the caller's
terminal and the relayed signals). A cdylib plugin declares one only on a host
that advertises the entrypoint capability; the SDK refuses it otherwise.
