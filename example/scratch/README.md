# Scratch caches

A **scratch** is a directory a target keeps between runs: a compiler cache, a
download cache, an index. Everything else heph gives a target is hermetic —
declared inputs in, declared outputs out, sandbox thrown away. A scratch is the
deliberate exception.

One rule makes it safe:

> A target's outputs must be identical whether its scratch is warm, cold, or
> absent.

A scratch may only make a target **faster**. It is never hashed, so it never
invalidates anything: changing one rebuilds nothing, and deleting one costs time
and nothing else.

## Run it

```sh
heph run //scratch:fetch          # "downloading..."
heph run --force //scratch:fetch  # "cache hit"
```

`--force` because a second run would otherwise be an ordinary cache hit and never
reach the sandbox at all — a scratch does not change a target's key, so nothing
about it makes heph re-run anything.

## Prove the contract

This is the part worth doing on your own targets:

```sh
heph inspect hashout //scratch:fetch
heph --no-scratch inspect hashout //scratch:fetch
```

Same hash, or the target depends on carried-over state and is broken.
`--no-scratch` runs with every scratch **absent** — not empty: no directory, and
no environment variable either. It implies `--force`, because without a rebuild
the audit would just replay the answer it exists to re-derive.

That has an edge worth knowing: a target reading `$MYCACHE` unguarded fails under
`set -u` rather than running cold. Write `${MYCACHE:-}` if you want the audit to
report on your target rather than on your shell.

## Look at them

```sh
heph tool scratch ls                        # what exists, how big, which lineage
heph tool scratch head //scratch:downloads  # why a build was cold
heph tool scratch rm //scratch:downloads    # drop one; always safe
heph tool gc --scratch-max-size 20GiB       # sweep by size and age
```

`head` is the one to remember. It prints every lineage a build would consult, in
order, and what each holds — because "why did my branch start cold?" is a question
the directory itself cannot answer, the interesting part being what was *not*
found.

## The three shapes

| target | shape | why |
|---|---|---|
| `downloads` | **no `path`**, portable, `remote = True` | The one to reach for first. Nothing is mounted, so no output can collect it and no dependency can be shadowed by it. Downloaded bytes are the same everywhere, so no `version`. |
| `objcache` | mounted, `shared`, host-keyed | Compiled objects depend on the machine, so `version` says so. `shared` asserts the tool is safe under concurrent use — an assertion about the tool, not a wish. |
| `index` | `exclusive` (the default) | Most tools assume they own their cache directory. heph serializes every referencing target, across processes too. The cost is that those targets stop running in parallel. |

## `version` is the whole identity

Two declarations share a directory if and only if they agree on address and
`version`. heph contributes nothing of its own — it does not fold in your OS or
architecture behind your back, because it cannot know whether your cache depends
on them, and a closed guess could never express a toolchain release or a target
triple anyway.

```python
version = heph.core.os() + "/" + heph.core.arch()   # host-specific
version = goos + "/" + goarch + "/" + go_version    # target-specific
version = ""                                        # portable (the default)
```

The default is portable, which is the *less* safe direction — deliberately. Say
what your cache depends on.

## Sharing through a remote cache

With a remote cache configured, `remote = True` makes a slot travel. Builds pull
automatically when cold; publishing is always an explicit command, so a build
never pushes as a side effect:

```sh
heph tool scratch push --all --producer "$CI_RUN_ID"   # CI, as its last step
heph tool scratch pull --all                           # warm a machine early
```

Lineages are per-branch, and a branch falls back to its base — configure
`scratch.scope` / `scratch.restoreScopes` in `.hephconfig`. A fresh CI runner for
a PR picks up `master`'s cache without the workflow mentioning any of it.
