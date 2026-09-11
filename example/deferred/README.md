# Deferred values

Run it:

```bash
heph run //deferred:image
heph inspect hashin //deferred:image     # bda27f9cec08eb6f

echo "anything" >> deferred/notes
heph inspect hashin //deferred:image     # unchanged — the producer re-ran, the value did not move

echo "1.5.0" > deferred/version
heph inspect hashin //deferred:image     # 793e79ba462c87de — moved
```

## What to read for

**The value lives in one place.** `//deferred:version` is the only thing that
knows what the version is. `//deferred:image` names it with
`${read://deferred:version}` and holds no copy — so there is no second file for
someone to keep in sync, and no way for the two to drift.

**The key derives from what the value was derived from, not from the string.**
That is the two `inspect hashin` runs above, and it is the thing no other build
system can express. Bazel, Gradle, Buck2, Pants and Nix all pass a computed value
across the boundary as a bare string, so the producer never becomes a node — and
then the cache key can only be all-or-nothing about it: hash the string and
over-invalidate, or hide it and sanction staleness. Every one of them also has to
re-run the producer eagerly on *every* build, which is why every one of them
documents some version of *"only use fast commands here."*

Here the producer declares `version` and `notes` as deps. Editing `notes` re-runs
it, produces the same bytes, and invalidates nothing. Editing `version` moves the
value, and the consumer misses.

**The def stays pure.** `heph inspect def //deferred:image` shows the *reference*,
not the ARN — so `query` and `inspect` never trigger a build, cycle detection still
works at parse, and evaluation never blocks on one. That last is the whole of Nix's
import-from-derivation problem, which nixpkgs forbids outright. What is here is IFD
with the recursion cut off at one level.

**Both modes take a reference, and the discriminator is the argument.** heph
claims a `${…}` only when what follows the kind is an absolute address — so
`${src:0:3}` and `${FOO:-d}` stay bash, `${read://a:b}` is an arithmetic error or
`""` in bash and therefore nobody's, and `$$` is not `${`, so `echo tmp.$$` still
prints a PID.

What *does* change with the shell: heph substitutes, it does not quote.
`//deferred:image` is `bash` and splices the value into a shell program;
`//deferred:version-copy` is `exec` and hands a path to `cp` as one argv element,
where nothing parses it. The two targets are there to show that difference.

## Two kinds

`${read://x:y}` is the producer's **contents**; `${src://x:y}` is the sandbox
**path** of its artifact. `//deferred:version-copy` uses the second — `cp` wants
a file to copy, not a value to paste:

```bash
heph run //deferred:version-copy
```

That target is the case `$SRC_<GROUP>` could never serve. Expanding `$SRC_<GROUP>`
needs a shell; an `exec` target has none, so before this there was no way to put a
dep's path into its argv at all.

Both are hashed edges. The difference is that `${src:}` stages the bytes, and
that it deliberately does *not* import the producer's `transitive` environment
the way a `deps` entry would: writing a path expression asks for a path. Either
may name an output group — `${src://tools:cli|bin}` — for a producer that emits
more than one file.

## What is refused

Two commented-out targets at the bottom of the BUILD file. Uncomment either and
heph says where the value belongs.

A reference never changes which targets *exist* — only what one does. So it is
rejected in `deps`, `tools`, `runner`, `out`, `name`, a glob and an address filter,
in every driver at once, because the reservation lives in the shared string decoder
rather than in a per-field flag.

An unknown `${…}` kind is left alone, so `${FOO:-default}` and every other shell
construct still works in a deferrable field. There is deliberately no
`${env:NAME}`: it is legal bash (`${var:offset}`), and the value it would have
carried composes from a `pass_env` producer plus `${read:}` — which keeps the
variable's *name* in the cache key and gives `inspect deps` something to say.

See `docs/DEFERRED_VALUES.md` for the mechanism, the two hashes, and the
config-or-credential test that decides which of these two features a value belongs
to.
