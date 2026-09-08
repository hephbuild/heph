# Deferred values

Run it:

```bash
heph run //deferred:image
heph inspect hashin //deferred:image     # note the hash

echo "anything" >> deferred/notes
heph inspect hashin //deferred:image     # unchanged — the producer re-ran, the value did not move

echo "1.5.0" > deferred/version
heph inspect hashin //deferred:image     # moved
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

**`exec`, not `bash`.** There is no shell in the consumer, so `$(cat $SRC_CFG)` was
never available — this is the case with no workaround today. Bash mode is
deliberately *not* deferrable: `${src:0:3}` is valid bash (substring expansion on a
lowercase variable named `src`), and removing the collision class beats documenting
an escape for it. Bash already has `$SRC_<GROUP>`.

## What is refused

Two commented-out targets at the bottom of the BUILD file. Uncomment either and
heph says where the value belongs.

A reference never changes which targets *exist* — only what one does. So it is
rejected in `deps`, `tools`, `runner`, `out`, `name`, a glob and an address filter,
in every driver at once, because the reservation lives in the shared string decoder
rather than in a per-field flag.

An unknown `${…}` kind is left alone, so `${FOO:-default}` and every other shell
construct still works in a deferrable field. `${src://…}` and `${env:NAME}` are
named by the design and not implemented, and say "not yet" rather than being read
as literals — but only *inside a driver that takes references*, where an author
writing one plainly meant it to resolve. Anywhere else they are somebody else's
syntax and are left alone, because `${src:0:3}` is bash and `${env:FOO}` is a
form several tools use.

And heph substitutes; it does not quote. Filling an argv element needs no
quoting, which is most of why exec mode is the deferrable one — but the consumer
here hands that element to `sh -c`, so the bytes are spliced into a shell program
rather than passed to one. See the note in the BUILD file.

See `docs/DEFERRED_VALUES.md` for the mechanism, the two hashes, and the
config-or-credential test that decides which of these two features a value belongs
to.
