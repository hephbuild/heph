# Exec runners

An **exec runner** decides *where* a target's command actually runs: on this
host as before, inside a `devenv shell`, inside a container.

Every subprocess heph spawns goes through one seam (`hexecrunner`). A driver no
longer reaches into `hproc::proc_exec` directly — `tests/execrunner_gate.rs`
fails the build if one starts to — so a target can be moved into another
environment without every driver growing its own idea of how.

```
target        runner = "//tools/devenv:runner"   ← an ADDRESS
   ↓          a hashed dependency, built before hashin exists
runner.json   { "runner": "wrap", "config": {…} }   ← a NAME
   ↓          registry lookup
runner        the builtin, or whichever plugin registered that name
```

Two identifiers, two jobs. The **address** is what you write and what reaches
the cache key — only a target has a hashout. The **name** inside the file
selects code.

A worked example of both, wired together, is `example/execrunner`: a service
whose config is built under a devenv toolchain, packaged into an image, and
checked inside that image.

## Using one

Per target:

```python
target(
    name = "build",
    driver = "bash",
    run = "make",
    out = "out/",
    runner = "//tools/devenv:runner",
)
```

Or for a whole workspace, once, in `.hephconfig2`:

```yaml
plugins:
  - builtin: exec
    options:
      runner: "//tools/devenv:runner"
  - builtin: bash
    options:
      runner: "//tools/devenv:runner"
```

Precedence is target field → driver option → local. `runner = "local"` is the
explicit opt-out, for a package that must escape a workspace-wide default.

> **A runner target must not inherit the workspace default.** The natural way to
> write one is a `bash` target, which would otherwise become its own runner and
> cycle on the first build. The exec driver excludes the *implicit* default for
> exactly that target. A runner target whose own **dependencies** are also
> exec targets still needs `runner = "local"` on them; without it you get a
> `CycleError`, which is at least diagnosable.

## Writing one

A runner is a target whose single output is a `runner.json`. Any driver can
produce it — a hand-written `text_file` target is a legitimate runner, which is
what keeps this from being a plugin-only feature.

```json
{
  "version": 1,
  "fingerprint": "devenv:9f2c…",
  "runner": "wrap",
  "config": {
    "prefix": ["/nix/store/…/bin/devenv", "shell", "--"],
    "env": { "PATH": "/nix/store/…/bin:…" },
    "runtime_pass_env": ["SSH_AUTH_SOCK"]
  }
}
```

`version` is checked by exact match. `fingerprint` is required — see below, it
is the field the correctness of the whole feature rests on.

### The builtin runners

| name | what it does | needs |
|---|---|---|
| `local` | nothing; spawn here | — |
| `wrap` | static rewrite: argv prefix, environment | just the config |
| `session` | holds an environment open, runs targets inside it | a `launch` argv |

There is no plugin-exported runner, and that is a deliberate absence. `session`
takes the argv that enters an environment and appends the agent invocation to
it, so a plugin that wants agent mode writes a `runner.json` and no runner code
— the descriptor passing, cancellation, signal fidelity and pooling are shared.
Both in-tree plugins that run targets elsewhere (devenv, oci) work that way. A
runner with a lifecycle the `session` pool cannot express would need an ABI
surface; none exists yet, so none is carried.

`wrap` config:

- `prefix` — argv prepended to the command. Its head becomes the program.
- `env` — environment applied over the target's. **Hashed**, because it lives in
  the runner target's output.
- `runtime_pass_env` — host variables pulled through at spawn, by name.
  **Unhashed**, and named to say so.

There is deliberately no `pass_env` here and no `cwd`. In the exec driver
`pass_env` means *snapshotted at parse and hashed*; a runner config resolves at
run time, so a name list pulled at spawn has `runtime_pass_env` semantics, and
offering it under the other name would read as hashed to anyone who knows this
codebase. Put hashed environment in `env`, baked at runner-build time. `cwd` is
absent because every absolute path the driver computed — tool symlinks, `$OUT`,
`SRC_*` — is relative to the sandbox.

`session` config:

- `launch` — argv that runs a command *inside* the environment. The agent
  invocation is appended to it.
- `cwd` — where to run `launch`. Not where targets run; they get their own
  sandbox.

There is no `env`, deliberately. The agent is a process the launch put *inside*
the environment, so its own `environ` **is** the environment — a declared copy
would be a second thing to keep in sync with the first. That is sound for the
cache because a consumer names the runner *target*, whose hashout is one of its
hashed inputs: whatever the environment provides is already in the consumer's
cache key by construction.

## The environment a target actually gets

```text
env_clear  +  the runner's environment  +  the target's own
```

For `wrap` the runner's half is the `env` map in its config; for `session` it is
the agent's `environ`. The target's half — its `env`, its `pass_env`, its deps
and tools — goes on top, so a target that declares something gets what it
declared even when the environment it runs in has an opinion.

`PATH` is the exception, because it is a *list* and both sides legitimately
contribute:

```text
PATH = the target's tools  ++  what the target declared  ++  the runner's PATH
```

Tools lead, so a target that declares a tool gets that one even when the
environment ships another by the same name.

What is **not** in there is the exec driver's own sandbox `PATH`
(`/usr/local/bin:/usr/bin:/bin`, or its `path:` option). Under a runner the
driver does not inject it at all: it is a fallback for a local spawn, and
putting it in front of the environment would let a host-installed tool silently
shadow the one the target asked to run beside — inside a cache key that claims
the runner's environment.

## The fingerprint, and why it exists

A consumer's cache key comes from the runner target's **hashout** — the hash of
the `runner.json` bytes. So:

> If those bytes do not move when the environment moves, every consumer keeps
> serving artifacts built in the old environment. Silently, forever, across the
> shared remote cache.

A config that names only a *reference* — `{"root": "/repo", "profile": "ci"}` —
is byte-identical after a `devenv.lock` change. The lockfile moved; the hashout
did not. `fingerprint` is what closes that.

Two rules for anyone writing a runner:

1. **Derive it, never author it.** Hash the resolved environment, not the files
   you asked to read. `devenv.nix` can `import ./nix/rust.nix` or read
   `devenv.local.nix`, and a source-file hash misses exactly the change that
   matters. A hand-written runner with a pasted fingerprint is a
   cache-poisoning foot-gun.
2. **It must be stable across runs.** If one ambient variable leaks into the
   capture, the fingerprint moves on every invocation and *every consumer in the
   workspace full-misses forever*. Nothing errors; the build is just always
   cold, and nobody traces it back here.

The runner is a **hash dep**: `hashed: true, runtime: false`. Its hashout folds
into `hashin`, nothing about it enters the sandbox, and its own transitive
tools and env do not leak into consumers. Its *address* is deliberately not in
the def hash, following the convention `hash_deps` already set — so two runner
targets emitting identical `runner.json` describe the same environment and
correctly share cache entries.

## devenv

```python
target(
    name = "runner",
    driver = "devenv_runner",
    mode = "wrap",              # or "session"
    deps = glob("devenv.*"),
)
```

`wrap` captures the environment once, at runner-build time, and emits it as a
literal env map. Targets then spawn locally with that environment: no devenv
process per target, no shell evaluation on the hot path. **Start here.**

`session` holds one `devenv shell` open for the build and runs targets inside
it. Targets get that shell's environment because they are started from inside it
(see above), plus everything the environment *is* beyond its variables: shell
activation with side effects, services devenv starts, state under `.devenv/`. It
earns its cost when what matters is process ancestry rather than `environ` alone.

Both modes resolve the environment to fingerprint it, so `session` pays one
evaluation it would not strictly need in exchange for a fingerprint that
describes the environment rather than the paperwork.

Cached locally, never remotely: the captured environment names this machine's
nix store paths.

## Containers

```python
target(
    name = "runner",
    driver = "oci_runner",
    image = "myimage:dev",
    deps = ["//svc:load"],      # the oci_load that puts it in the daemon
)
```

The fingerprint is the image's content digest, resolved from the daemon. A tag
is a moving pointer; retagging `:latest` moves the digest, which re-keys every
consumer. The container is launched **by digest**, so the container that runs
the build is the one the fingerprint describes even if the tag moves mid-build.

One container is held open and targets run inside it, rather than a fresh
`docker run` per exec — partly for speed, but mostly because a per-exec run
needs the *target's* sandbox and cwd in its argv, and a wrap prefix is static by
construction.

The workspace root and heph's home are bind-mounted **at the same paths**
inside. This is correctness, not preference: every absolute path the driver
computed is a host path, and a remapped mount would leave targets resolving
paths that do not exist, silently.

## Go

Every Go driver takes a `runner`, so `go list`, `go tool compile`/`asm`/`pack`,
`gofmt` and `heph-govet` can each run inside an environment.

Test targets go through the `go` provider state:

```python
provider_state(
    provider = "go",
    test = { "runner": "//tools/devenv:runner" },
)
```

That reaches the generated target as the exec driver's own `runner` field —
which is the argument for `runner` living on that driver rather than being
reinvented per plugin.

## Agent mode, in one picture

```
heph ──fork──▶ __runner-exec ──socket──▶ __runner-agent ──fork──▶ target
                     │          SCM_RIGHTS        │
                     └────── its own fds 0,1,2 ───┘
```

Only something already inside an environment can create a process there, so
heph starts one helper in it (`heph __runner-agent`). But heph then has a
target it did not fork — it cannot `waitpid` it, put it in a process group, or
hand it a pipe or a PTY, and all of heph's output streaming and cancellation
assumes the target is its own child. So heph forks a small client
(`heph __runner-exec`) exactly where it would have forked the target, and the
client hands its already-wired descriptors to the agent.

**Passed, not proxied**: the output bytes never travel through the socket, so
the bounded drain and PTY line discipline are not re-derived on a new
transport, and `--shell` works because the target gets the real PTY slave. From
heph's side an agent-mode target looks like every other target. That illusion
costs one small process per target.

Both subcommands are hidden and internal. Run by hand they fail with a
sentence.

The agent holds heph's stdin as a **keepalive pipe** and exits when it reads
EOF. That is the only teardown that cannot be skipped: the OS closes descriptors
at process exit whether or not any destructor runs, so it covers a panic, a
`process::exit`, and a `SIGKILL`. Its stdout and stderr are piped rather than
inherited, for the same reason in reverse — an agent that co-owned heph's own
descriptors would outlive it and hang anything reading heph's output to EOF.

## Diagnosability

- An unknown runner name is caught at **resolution**, before any consumer
  executes, and lists the registered names.
- A missing or empty `fingerprint` is refused with the reason.
- An unknown `version` is refused by number.
- A runner target producing no `runner.json` (or more than one) names the
  target and says what it produced instead.
- A spawn failure under a runner names the runner and the program that was
  *actually* executed — under a wrapper those differ.

## Known gaps

- **FUSE.** Runners are untested under `fuse.enabled`; bind-mounting a FUSE
  mount into a container, in particular, is not something to assume works.
- **`docker exec` tty fidelity.** A container allocates its own tty rather than
  receiving heph's PTY slave, so line discipline and winsize propagation are
  docker's, not ours.
- **Client startup cost.** Agent mode costs one `execve` of the heph binary per
  target (measured at ~3 ms on darwin/arm64, warm and uncontended). For a
  `go list`-heavy build that is worth measuring before turning it on; the
  captured-env wrap runner has no such cost. The number under `2 × ncpu`
  contention, and the peak RSS of that many concurrent clients, are unmeasured.
- **Running a target inside a container is untested.** The `oci_runner` suite
  covers digest resolution and the launch argv against a real daemon, but not a
  target actually executing in the container: that needs the heph binary to run
  inside the image, and on macOS the binary is Darwin while the container is
  Linux, so the test could never pass there.
- **The devenv suite is opt-in** (`HEPH_E2E_DEVENV=1`), because an environment
  nix has never evaluated costs ~2m40s and CI runners are ephemeral. Run it when
  touching the devenv driver.
