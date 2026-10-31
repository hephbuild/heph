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
  the same entry. (On macOS a transitive tool's files are copy-on-write clones,
  so a write to one stays in the sandbox; on Linux they are hardlinks, and
  `read_only_deps` is a directory symlink on both.) Two invocations of the same target run concurrently, each in
  its own sandbox, which is removed when the tool exits.
- **Signal state and limits**: in every mode, the signal dispositions and signal
  mask heph itself inherited (under `nohup`, SIGINT stays ignored; under
  `trap '' PIPE`, SIGPIPE stays ignored). The open-file limit is yours, not
  heph's raised one — for every driver, plugin drivers included.
- **Parent**: `$PPID` and `ps` show heph as the tool's parent, in every mode.
- **Terminal**: depends on the mode, below.

**Credentials are not redacted.** A tool that prints a credential prints it in
clear, in every mode, and nothing logs the run's output. The scrubbing described
in [`CREDENTIALS.md`](CREDENTIALS.md#redaction) applies to captured builds only.

### The two modes

A run on your terminal takes the **PTY** mode. `--raw`, or any of stdin, stdout
and stderr not being a terminal, or heph not being the terminal's foreground job,
takes the **inherited** mode.

| | PTY (the default on a terminal) | Inherited |
|---|---|---|
| TUI | While dependencies build, then paused for the tool | None with `--raw`; otherwise as for any run, then paused for the tool |
| Tool's stdio | A PTY heph owns, as fds 0–2; heph copies the keyboard to it and its output to fd 1 | Your fds. No byte passes through heph |
| Process | A session of its own, with the PTY as its controlling terminal | heph's process group |
| Ctrl-C, Ctrl-\ | The PTY signals the tool's foreground group | The kernel signals the shared group |
| Ctrl-Z | A plain tool does not stop (see [below](#ctrl-z)) | Stops; `fg` resumes |
| Window resize | heph copies the size to the PTY | The kernel delivers it |

Use `--raw` when a tool needs real Ctrl-Z or your terminal's own fds. It costs
two signal limits, under [Signals while the tool runs](#inherited-mode).

Without `--raw`, pipes and files stay byte-exact, because a redirected run
inherits them. In the PTY mode, the tool's stdout and stderr share the PTY.

### Which mode am I in?

```sh
HEPH_LOG=entrypoint_mode=debug heph run //x
```

heph prints one line to stderr before the tool starts, with the `mode` and the
`reason`. By default it prints nothing, so `2>&1 | jq` stays clean. These are
checked in this order, and the first that holds wins:

| `mode` | `reason` | When |
|---|---|---|
| `inherit` | `flag` | `--raw` was passed. |
| `pty` | `shell` | `--shell` was passed. |
| `inherit` | `fd0_not_tty` | stdin is not a terminal. |
| `inherit` | `fd1_not_tty` | stdout is not a terminal. |
| `inherit` | `fd2_not_tty` | stderr is not a terminal. |
| `inherit` | `not_foreground` | heph is not the foreground job of the terminal on stdin: `heph run //x &`, or `timeout 5 heph run //x` in a script. |
| `inherit` | `plugin_no_pty` | The target's driver offers no PTY. heph also warns. |
| `pty` | `default` | None of the above. |

Asking can change the answer. The line goes to stderr, so capturing it with
`2>file` makes the run `fd2_not_tty`. Read it on the terminal.

A background job or a backgrounding wrapper also loses the TUI, for any `heph`
command. With no TUI to prompt in, the approval handler refuses an
`approval`-gated target rather than prompting; `--auto-approve` lets it through.

`timeout` is such a wrapper only sometimes, because it puts itself in a process
group of its own. In a script, that group is not the terminal's foreground, so the
run inherits. At an interactive shell, job control has already made `timeout` the
leader of the foreground job, so heph stays in the foreground and takes the PTY.

### Ctrl-Z

**With `--raw`**, and in any inherited run in the foreground, Ctrl-Z stops the
tool and heph together, and `fg` resumes them, as for a tool run by hand.

**In the PTY mode**, Ctrl-Z does nothing to a plain tool. The tool's process group
is orphaned, so the kernel discards the stop. A tool that handles SIGTSTP still
sees it.

A tool that sends itself SIGSTOP does suspend. heph flushes the tool's output,
restores your terminal and stops too, so your shell's prompt returns. `fg` makes
the terminal raw again, copies the window size and continues the tool.

- **The tool shows as stopped after `bg`; run `fg`.** heph stops again until it is
  the foreground job, because a raw terminal from the background breaks your
  shell's.
- While it is stopped, a signal sent to heph stays pending until `fg`. Only
  `kill -9` acts.

### After `kill -9`, run `reset`

Nothing in heph runs after SIGKILL, so in the PTY mode your terminal stays raw
until you run `reset`. heph's supervisor still kills the tool. In the inherited
mode the terminal is untouched.

### Output after the tool exits

In the PTY mode, once the tool has exited, heph copies what is left on the PTY to
the screen, for at most 500ms. A background job the tool left behind can hold the
PTY open past that. The run then ends at the limit, with the tool's status, and
heph warns:

```
the tool's output was cut: its PTY was still open at the drain limit
```

The warning carries `bytes_drained` and `limit_ms`. Whatever that job writes
afterwards is lost.

## From a script or an agent

```sh
heph run //web:dev --raw -- --port 4000
```

stdout carries only the tool's bytes, stderr only failures, and no TUI starts.
The exit status is the one below. An agent's stdio is usually not a terminal, so
it would inherit without the flag too; `--raw` also drops the progress, and holds
on a terminal as well.

With no TUI, an `approval`-gated target in the closure is refused unless you pass
`--auto-approve`. `run` has no `--json`.

On a build target, `--raw` only changes the display: the action's output is
printed plainly, redacted as in `log.txt`, the action reads no stdin, and capture,
runners and the cache are unchanged. A cache hit prints nothing.

## Exit status

| Status | When |
|---|---|
| the tool's code | The tool exited, with any code, 0 included. Nothing is added. |
| the tool's signal | The tool was killed by a signal: heph dies of the same signal (no core dump of its own), so a shell sees "killed by SIGINT", not `130`. |
| `125` | heph failed after the target resolved to an entrypoint, before the tool ran: a dependency failed to build, an argument was refused, the closure could not be materialized, a credential could not be obtained, or the program could not be started (not found, not executable). The error is shown. |
| `130` | Ctrl-C cancelled the run before the tool started, while its dependencies were building. |
| `1` | The target could not be resolved at all (a typo, a broken BUILD file, not an entrypoint). |

The table holds in every mode. The number alone does not say who failed, since a
tool can exit `1` or `125` itself: a failure of heph's also prints an error to
stderr.

heph writes nothing to stdout during an invocation; its progress goes to
stderr.

## Signals while the tool runs

From the moment the tool is about to start until it exits, heph is
transparent: every signal is the tool's, and heph acts on none of them.

Once the tool has exited, heph may still be finishing on its behalf —
uploading the freshly built dependencies to a remote cache. Signals are heph's
again then: Ctrl-C, TERM or HUP cancel what is left and heph exits at once,
still with the tool's status, and without printing a cancellation.

In every mode, a tool that traps INT and QUIT and keeps running can still be
stopped with `kill -9 <heph pid>`: heph's supervisor then kills and reaps the
tool. A second Ctrl-C is not special.

### PTY mode

- Ctrl-C and Ctrl-\ pass through heph as bytes, and the PTY signals the tool's
  foreground group.
- A signal sent to heph's own pid or to its whole group — TERM, HUP, INT, QUIT,
  USR1 or USR2 — reaches the tool's group once, with no limits. So
  `kill -INT <heph pid>` reaches the foreground work of
  `bash -c 'sleep 100; …'`. heph then exits with the tool's status. A job in a
  group of its own, under `bash -i`, is not reached.
- A resize reaches the tool once: heph copies the size to the PTY, and the kernel
  signals the tool.

### Inherited mode

- Ctrl-C, Ctrl-\ and Ctrl-Z come from the terminal straight to the tool, which
  shares heph's process group.
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
    arrive twice. The HUP of a terminal that closed may too.

### Before the tool starts

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

`--shell` always runs on heph's PTY, whatever the fds are. Its output is not
logged or redacted, so `env` there shows your credentials. With no terminal, heph
goes neither raw nor stops itself. A driver with a shell of its own that offers no
PTY is refused before anything builds. `--raw` is refused with it, also before
anything builds:

```
`--raw` and `--shell` cannot be combined: `--raw` keeps heph off the terminal, and a shell needs heph's PTY; drop one of them
```

`heph inspect def //web:dev` shows `"runs_as": "exec, entrypoint (uncached)"`,
a line for people. A script or an agent reads `target_def.entrypoint` from the
same output instead: `{"accepts_args": true, "workdir": "caller"}`, absent
for a target that is not an entrypoint.

## Plugins

Any driver can declare an entrypoint: its `parse` sets
`TargetDef.entrypoint`, and its ordinary `run` honours the generic
`RunOptions` the engine sends (cwd, base environment, args, the caller's
terminal and the relayed signals). A driver that can spawn on heph's PTY offers it
with `Entrypoint::with_pty`; without that, a default run inherits and `--shell`
is refused. A cdylib plugin declares one only on a host
that advertises the entrypoint capability; the SDK refuses it otherwise.
