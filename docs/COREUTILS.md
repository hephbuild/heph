# Builtin coreutils

heph ships 47 POSIX utilities inside its own binary and can put them on every
target's `PATH`, so a recipe that runs `cp`, `install` or `sha256sum` behaves
identically on Linux and macOS.

```
target      run = ["install -D $SRC $OUT"]
   ↓        resolved on the sandbox PATH
shim        <home>/coreutils/<version>/bin/install   ← a symlink to the heph binary
   ↓        exec, argv[0] == "install"
heph        dispatch at the top of main(), before clap
   ↓
applet      uu_install::uumain
```

## Why

The sandbox isolates *files*. It does nothing about the fact that the two hosts
disagree about what `cp` means. GNU coreutils on Linux, a BSD userland on
macOS — and the divergences are the first things anyone writes in a recipe:

| | |
|---|---|
| `install -D` | GNU-only. BSD `install` has no `-D` at all, so the recipe just fails on macOS. |
| `sed -i` | GNU takes an optional suffix, BSD requires one, so `sed -i 's/a/b/' f` eats the next argument as a filename. |
| `wc -l < f` | BSD pads the count with spaces, so comparing against `"3"` passes on Linux and fails on macOS. |
| `sort` | Collation comes from the locale, so the same input orders differently on two machines. |

The last one is the reason this lives in heph rather than in a style guide: it
silently changes build *outputs*, not just exit codes. A build system whose
contract is "same inputs, same outputs" cannot leave the tools that produce
those outputs undeclared and host-defined.

## Using it

Off by default. Turning it on changes what every recipe's `cp` resolves to, and
it moves every exec target's cache key.

```yaml
plugins:
  - builtin: exec
    options:
      coreutils: true
  - builtin: bash
    options:
      coreutils: true
```

`heph tool coreutils list` prints the set. `heph tool coreutils which cp` says
whether heph ships that name and what the host would supply instead — the two
candidates a target's `PATH` chooses between. `heph tool coreutils run cp -r a b`
runs one directly, for checking what a flag does.

## What a target's PATH looks like

`hexecrunner` composes it (see `EXEC_RUNNERS.md`); the driver only contributes a
`PathPolicy`. The builtins go in as a tier directly behind the target's own
tools:

```text
PATH = the target's tools  ++  heph coreutils  ++  what the target declared  ++  the runner's PATH
```

**A target's own tools always win.** A recipe that provisions its own `sed` gets
that `sed`; the builtins never override a deliberately declared tool, they only
displace the host's.

The shim directory is a host path, so it is contributed for a local spawn and
for a `wrap` runner, which still spawns on this filesystem. A `session` or
container runner runs somewhere that path does not exist, and gets nothing.

## The shim directory

Materialized once per (toolbox version, binary path), lazily on first use, under
the config's `home_dir` — `.heph3/coreutils/v1-<hash>/bin` by default. One
symlink per applet, pointing at the `heph` binary.

Keyed on the binary's path as well as the version because the shims point at a
*specific* heph: two installations sharing a home must not share a shim set. A
self-update that rewrites the binary in place keeps the same path, and the
symlinks stay correct.

The per-sandbox cost is one extra `PATH` entry. Nothing is written per target,
nothing is staged, and there is nothing to tear down. The steady-state cost of
resolving it is one `stat`.

Concurrency-safe by construction: the content is a pure function of the
directory's own name, so a populated directory is always complete. A builder
stages into a unique sibling and renames, and loses a race harmlessly because
the winner's content is identical.

Symlink rather than a wrapper script (which would cost a second process per
invocation, and a host `/bin/sh` — the very `PATH` this exists to stop
depending on) or a hardlink (which breaks across filesystems and goes stale).

## The cache key

`COREUTILS_VERSION` in `crates/coreutils` is the toolbox's identity, and every
`exec`/`bash` target folds it into its def hash when the toolbox is on.

The utilities are on a target's `PATH` without being declared, and nothing can
tell which of them a shell command will invoke without parsing it — so the whole
toolbox's identity goes in or none of it does. The consequence is blunt and
deliberate:

> **Bumping `COREUTILS_VERSION` invalidates every exec target in every
> workspace.** It is a release-gated decision, not a routine one.

Bump it on any observable behaviour change: an applet added or removed, an
upstream upgrade that changes output, a fix to a hand-written part. Nothing is
hashed at all while the toolbox is off, so a workspace that never turns it on
keeps the keys it has today.

Resolving the shim directory is fallible on purpose. Degrading to "no builtins"
would run the target against the host's utilities while its cache key claims
heph's — a silently wrong build, and the exact failure the version in that key
exists to prevent.

## The applets

Forty MIT-licensed [uutils/coreutils] crates, tested upstream against the GNU
test suite; `find` and `xargs` from [uutils/findutils]; and a `grep` built on
ripgrep's search engine. Adding one is a line in the `applets!` table in
`crates/coreutils/src/lib.rs` plus its crate in `Cargo.toml` — and a
`COREUTILS_VERSION` bump.

The selection rule: **a utility ships only if its behaviour or flags actually
differ between GNU and BSD userland, or it is missing on one of them — and it
shows up in build recipes.** Uniform ones are included only where they are
nearly free and their absence would push a pipeline back onto the host `PATH`
halfway through.

Not shipped, deliberately: `awk`/`perl`/`python` (implementing a language is a
project, not an applet), `curl`/`wget` (the `http_fetch` driver already covers
fetching, hermetically and cacheably), `git`, `uname` (normalising `arm64` vs
`aarch64` would change recipes that already switch on it), `du`/`df` (they
answer questions about the machine, not the build), and anything interactive.

`sed` is not in yet; it is the last slice. `diff`/`cmp`
are not planned: the `diffutils` crate exposes its algorithms but keeps its CLI
in a private `main`, so wiring it up would mean reimplementing its argument
parsing for the lowest-value pair in the set.

### `grep`, and where it departs from GNU

Built on `grep-searcher`/`grep-regex` — the engine ripgrep uses — with a POSIX
flag surface of our own (`-EFivnclLqwxrhHs`, `-m`, `-e`, `-f`, `--`). Two
departures, both deliberate:

* **No `-P`.** The `regex` crate has no backreferences or lookaround, by
  design. `-P` fails with that explanation rather than with "invalid option",
  because a pattern needing it has to be rewritten, not retried.
* **Never colourised.** Output goes into build logs and gets parsed; a
  `--color=auto` that guessed from a tty would make a recipe's behaviour depend
  on how it was invoked.

The argument parser is hand-rolled rather than clap, because `grep -e -v file`
must treat `-v` as the *pattern*. Line numbers are always counted and only
printed under `-n`: the `UTF8` sink asks every match for its line number and
fails if the searcher was not tracking them.

### `tar`, `gzip`/`gunzip` and `zstd` — reproducible by default

The applets where the divergence is not a flag but the *bytes*. GNU tar and
bsdtar disagree about whether `--transform`, `--sort`, `--owner` and `--mtime`
exist at all; `gzip` writes the source filename and its mtime into the header
unless told not to; `zstd` is installed by default on neither host.

Archiving the same tree twice, on two machines, should produce the same bytes.
With the host tools it does not — gzipping identical content a second later
gives a different file. So the reproducible settings are the **defaults**, not
flags anyone has to remember:

* entries sorted by path, so the archive does not inherit directory order;
* uid/gid 0 and empty owner *names*, so it does not inherit whoever built it;
* mode normalised to the executable bit, since the rest is umask;
* mtime from `SOURCE_DATE_EPOCH` when set, otherwise 0;
* no gzip header name or timestamp.

There is deliberately no flag to turn any of that off. A recipe that wants a
non-reproducible archive is a recipe with a bug.

Compression is detected from the magic bytes rather than the file name, so a
`.tar` that is actually gzipped still extracts instead of failing confusingly.

### `find` and `xargs`

Straight adapters over findutils. Its entry points take `&[&str]`, so a
non-UTF-8 argument is refused by name rather than lossily converted — GNU `find`
accepts arbitrary bytes in a path and this cannot, and searching a *different*
path than the one asked for is worse than saying so.

## One process per invocation

`uucore` keeps the exit code in a process-global, and an applet owns stdout and
may call `exit`. Both are fine because every invocation is its own process —
which is also why the engine must never call into `hcoreutils` in-process, and
why the crate's own tests take a lock and reset that global explicitly.

Dispatch happens at the top of `main()`, before logging, before clap, before the
self-update check, and before any runtime exists. A build may invoke `cp`
thousands of times, and each one is a fresh `heph` process.

[uutils/coreutils]: https://github.com/uutils/coreutils
[uutils/findutils]: https://github.com/uutils/findutils
