# Exec runners

*In what environment is a target's process created?*

By default, the host process — same as before exec runners existed. A target can
instead name a **runner**: a target whose artifact describes an environment.

```python
devenv = target(name = "devenv", driver = "devenv")

target(name = "build", driver = "bash", run = ["cargo build"], runner = devenv)
target(name = "bootstrap", driver = "bash", run = ["…"], runner = None)
```

```yaml
# .hephconfig — the expected way to use this
defaultRunner: //:devenv
```

Selection is **per-target `runner =` → `defaultRunner` → `local`**. `runner = None`
is the explicit opt-out that no default overrides; a bootstrap target needs it,
since something has to build before the environment exists. The runner target is
itself excluded from `defaultRunner`, or it would be its own dependency.

`runner =` names a **target**, not a driver-style name — the one place it differs
from `driver =` beside it. That is not cosmetic: only a target has a `hashout`,
and a `hashout` is what carries the environment into the cache key.

## How it reaches the cache key

The runner is wired in as a `hashed = true, runtime = false` input — the existing
`hash_deps` shape. Its `hashout` reaches every consumer's `hashin`, while its
bytes are **never materialized** into a consumer's sandbox.

Three rules follow, and each exists because its absence is silent:

- **A runner target with no outputs is rejected.** `hashin` folds input
  *hashouts*, and a zero-output target has none — so two runners describing
  completely different environments would give their consumers byte-identical
  keys.
- **No runner selected contributes nothing**, so shipping this invalidated no
  existing artifact. Sound only while `local` stays the zero-configuration
  runner: any configuration on it must reach the key.
- **A runner never changes whether a target is cached.** Choosing a
  weakly-pinned environment is the user's call; heph reports how well-pinned it
  is (`Pinned` vs `Asserted`) and never overrides `cache`.

Note that adopting a runner ends cross-platform cache sharing for the targets
under it: the environment's contents are platform-specific, so its `hashout` is
too. That is inherent to putting the environment in the key.

## Environment layering

A runner may declare `env`, `pass_env`, `runtime_env` and `runtime_pass_env` —
the same four keys a target has. What differs is where the key is drawn:

| key | resolved | in the key |
|---|---|---|
| `env` | at capture, literal | yes |
| `pass_env` | at capture, from the host | yes — the *value* is baked in |
| `runtime_env` | at spawn, literal | the declaration only |
| `runtime_pass_env` | at spawn, from the host | the **name** only |

Use `runtime_pass_env` for anything that legitimately differs per machine or per
login — `SSH_AUTH_SOCK`, `DOCKER_HOST`. Passing those with `pass_env` bakes one
machine's value into the environment's identity and hands every other machine a
key that lies about what produced the artifact.

Applied weakest to strongest:

```
host passthrough  <  captured environment  <  declared literals  <  the target
```

A runner's `runtime_pass_env` sits **underneath** what it captured, where a
*target's* sits on top of its own `env`. The asymmetry is deliberate: a runner
exists to provide an environment, so a passed-through host variable outranking
the captured one would let `runtime_pass_env = ["*"]` silently replace that
environment with the developer's own — while the build stayed keyed as though it
ran in the runner's.

The target always wins. A runner that could overwrite `$OUT`, `$SRC` or a
target's own `env` would silently change what the target builds.

`PATH` is the exception to "merge": a session's `PATH` **replaces** the driver's
rather than sitting under it. Appending the driver's default would let a tool
missing from the environment fall through to the host — the ambient dependency a
runner exists to remove — under a key asserting the runner's environment. An
explicitly written `path` under a runner is an error rather than a silent
discard.

## Modes

A runner returns a **session**, and every mode is a pure transformation of the
process spec — `prepare(spec) -> spec`. That is what keeps `proc_exec::spawn`
synchronous, its `Handle` invariants intact and its PTY handling in one place:
no mode owns a process type of its own.

| mode | who forks the target | what `prepare` does | used by |
|---|---|---|---|
| `Local` | heph | nothing — the identity | the default |
| `Env` | heph | layers env under the target's own | devenv `mode = "snapshot"` |
| `Wrap` | the wrapper | prepends wrapper argv | devenv `mode = "wrap"` |
| `Agent` | a helper inside a live environment | rewrites argv to the client | devenv `mode = "session"` |

Sessions are pooled per environment (keyed by content, so byte-identical
artifacts share one) and opened at most once. Teardown is explicit and
idempotent: heph's hard-abort path exits without running destructors, so a
`Drop`-only teardown would leak a container or shell exactly when it matters.

### `__runner-agent` and `__runner-exec`

Two hidden `heph` subcommands, used only by `Agent` mode. They exist for one
reason, so it is worth stating plainly.

The goal is *one* `devenv shell` for the whole build, with every target's
process created inside it. But a process can only be created inside that shell
by something already living there — so heph starts one helper there and asks it.
That helper is **`heph __runner-agent`**: `devenv shell -- heph __runner-agent
--socket S` runs it inside the environment, where it listens on a unix socket
and forks a target on request.

Which raises the real problem: heph now has a target process it did not fork.
It cannot wait on it, cannot put it in a process group, and cannot give it a
pipe or a PTY — a `Handle` can only be built for a child of *this* process, and
all of heph's output streaming and cancellation is built on that.

**`heph __runner-exec`** is the answer. Per target, heph forks it as an
ordinary child, exactly as it would have forked the target itself:

```
heph ──fork──> __runner-exec ──socket──> __runner-agent ──fork──> the target
                    │          SCM_RIGHTS      │
                    └──── its own 0,1,2 ───────┘
```

It sends the command and hands over its own **stdio descriptors** — the ones
heph already wired to the target's pipes or PTY. Passed, not proxied: the agent
`dup2`s them onto the target's 0/1/2, so the bytes never travel through this
path and none of the bounded-drain or line-discipline handling is re-derived on
a new transport. Then it waits and exits with the target's real status.

So from heph's side an `Agent`-mode target looks like any other child: one
process it forked, whose output it reads and whose exit code it trusts. The
client is that illusion, and it costs one small process per target.

Two details that are not obvious:

- The client forwards **its own environment** in the request, and the agent
  applies it with `env_clear`. The agent lives inside the dev shell, so letting
  a target inherit *its* environment would put the developer's ambient
  `GOFLAGS` into every build — unhashed, under a lockfile-pinned key.
- The target is `setsid` into its own session, so no kill heph issues reaches
  it. The agent therefore watches the socket: when the client goes away — a
  cancelled build — the peer hangs up and the agent kills the target's process
  group. Without that, cancelling a build left its targets running to
  completion.

### What a plugin-hosted driver can and cannot use

Only the *environment* crosses the plugin seam, so a driver in a cdylib (`go`,
`oci`, devenv's own) can use `Local` and `Env` sessions but not `Wrap` or
`Agent` — those put a wrapper or a client in the argv, which does not cross.
The host marks such a session opaque and the guest **refuses the run**, naming
the driver, the runner and the ways out. It does not quietly fall back to
running the target on the host, which is what it used to do while still passing
the `runner_key` ack.

Built-in drivers (`exec`, `bash`) hold the real session and are unaffected.

## devenv

```python
target(name = "env", driver = "devenv")                  # snapshot (default)
target(name = "live", driver = "devenv", mode = "session")
target(name = "wrapped", driver = "devenv", mode = "wrap")
```

**Snapshot** captures `devenv print-dev-env --json` once as an artifact:
cacheable, and its bytes are its identity. `PATH` is filtered to `/nix/store`
entries, and any variable naming the tree root, `$HOME` or `$TMPDIR` is dropped
— without which two checkouts of one commit produce different keys for every
target. Snapshots are local-cache only, since they name host-local store paths.

It cannot provide shell functions or `enterShell` effects. A target calling one
gets an error saying so by name rather than a misleading "not found in PATH".

**Session** additionally holds a `devenv shell` open and forks every target's
process from inside it, which is what makes those available. It costs one shell
per heph process and cannot amortize across machines.

**Wrap** (`mode = "wrap"`) prefixes every spawn with `devenv shell --`. It is a
**demonstration of the `Wrap` lane, not a recommendation** — measured before it
was written:

- `devenv shell -- true` is **~4.5 s warm**. Snapshot pays `devenv
  print-dev-env --json` once, as a *cached target*; wrap pays a full shell entry
  per spawn, and a Go build is thousands of processes.
- `enterShell` runs once per target rather than once per build, so any side
  effect it has happens N times.
- It does **not** buy shell functions: `devenv shell -- bash -c 'declare -F'`
  reports none, because `devenv shell -- prog` execs `prog` directly.

So its identity is `Asserted`, not `Pinned`. Reach for it only for a runner a
handful of targets name — and note that the snapshot's `PATH` is store-only, so
`bin` usually has to be an absolute path for `devenv` itself to be found.

## Go

| knob | applies to |
|---|---|
| `goenv` (provider option) | the Go toolchain — `go list`, compile, assemble |
| `provider_state(provider="go", test={"runner": …})` | the environment a **test binary** runs in |

Deliberately separate: a test binary often wants a database client or a browser
on `PATH`, while the build wants the hermetic toolchain and nothing else. One
setting for both would either leak the runtime's tools into every compile's key
or force the build environment onto the test.

`goenv` is hashed into each target's def. Lint and format do not take it — they
exec `heph-govet`, not `go`.

## Diagnosability

- `heph inspect def <addr>` — the resolved runner and **how it was selected**.
- `heph inspect spec <addr>` — the authored `runner =`, before resolution.
- `heph inspect runners` — what the workspace can use.

All static. Every `heph` invocation owns its own session pool, so live sessions
belong to the build that opened them and appear in its own output, not in a
separate process that would render an empty table.

## Examples

`example/exec_runner/BUILD`.
