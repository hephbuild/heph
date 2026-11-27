# Archives

The builtin `archive` plugin has two drivers, both in-process (no host
`tar`/`unzip`/`zip`), with the same code on every supported platform:

- **`archive.extract`** unpacks the one archive file a `src` target produced
  into a directory.
- **`archive.create`** packs the outputs of `deps` into one archive file.

The formats are `tar`, `tar.gz` (`.tgz`), `tar.zst` (`.tzst`) and `zip`.
`archive.extract` also reads two that `archive.create` does not write:
`tar.xz` (`.txz`), and `gz`, one gzipped file that is not a tar
(`tool-linux.gz`; see [A bare `gz`](#a-bare-gz)). The code is
`crates/plugin-archive` (the drivers) over `crates/archive` (the unpacker
and the writer). `heph.gh.release_asset(..., extract = True)` declares an
`archive.extract` ([GH_RELEASES.md](GH_RELEASES.md)), and the `tar` coreutil
writes through the same writer as `archive.create`
([COREUTILS.md](COREUTILS.md)).

`archive` is a **reserved plugin name**: the plugin is compiled into heph, and
a `plugins:` entry or cdylib named `archive` fails at startup. A plugin that
declares `archive.*` targets (the `gh` plugin does) needs a heph that ships
this builtin; an older one fails with "unknown driver".

```python
# //tool/BUILD: :bin and :docs declare outputs under out/
# (out/bin/tool, out/share/doc/README), so they stage at tool/out/…
target(
    name = "dist",
    driver = "archive.create",
    deps = ["//tool:bin", "//tool:docs"],
    strip_prefix = "tool/out",     # tool/out/bin/tool → bin/tool
    prefix = "tool-1.0",           #                   → tool-1.0/bin/tool
    out = "tool-1.0.tar.gz",       # format inferred from the suffix
)

target(
    name = "unpacked",
    driver = "archive.extract",
    src = ":dist",                 # format inferred: tool-1.0.tar.gz
    strip_components = 1,          # tool-1.0/bin/tool → bin/tool
)
```

`create`'s `strip_prefix` + `prefix` and `extract`'s `strip_components` are
inverses: the example round-trips to `unpacked/bin/tool`.

A tool is one file, so the unpacked directory is a tool only when it holds
exactly one. To put one binary from a larger archive on a target's `PATH`,
extract only that binary with `include` (still in `//tool/BUILD`):

```python
target(
    name = "tool",
    driver = "archive.extract",
    src = ":dist",
    strip_components = 1,
    include = "bin/tool",          # the path after strip_components
    executable = True,             # in case the archive recorded no mode
)

target(name = "use", driver = "bash", run = ["tool --version"],
       tools = [":tool"])
```

To pick one file out of a dep that is not an extract, use a `group`:

```python
target(name = "tool", driver = "group", deps = [":unpacked"],
       include = ["tool/unpacked/bin/tool"])
```

A `group`'s `include` matches a dep's paths workspace-relative,
`<package>/<out>/…` (here package `tool`, directory `unpacked`), or as the
dep's own BUILD file writes them (`unpacked/bin/tool`). `heph inspect outputs
//tool:unpacked` lists them. A `[filter]` on the tool reference does the same
job without a group, and takes the workspace-relative form only:
`tools = ["//tool:unpacked[tool/unpacked/bin/tool]"]`.

A tool whose output is a glob or a directory, as a filtering `group`'s or an
`archive.extract`'s is, is held to one file when the consumer runs, not at
`heph query` or `heph validate`: an `include` that matches two files, or
none, fails the consumer, naming the tool and the files it found.

## `archive.extract`

| Option | Default | |
|---|---|---|
| `src` | required | The target whose **single** output file is the archive. A target with several files is refused with the count: which one is meant would depend on walk order. |
| `format` | inferred | `tar`, `tar.gz`, `tar.xz`, `tar.zst`, `zip` or `gz`. When omitted, it is taken from the archive file's name suffix (the longest one that matches: `x.tar.gz` is a `tar.gz`, `x.gz` a `gz`), else from its leading bytes (gzip `1f 8b`, xz `fd 37 7a 58 5a 00`, zstd `28 b5 2f fd`, zip `PK\3\4` or `PK\5\6`, tar `ustar` at offset 257; a gzip stream sniffs as a `tar.gz`); neither → an error asking for `format`. Given, it overrides both and is hashed. Omitted, the key holds the inference rules' version (`INFERENCE_VERSION` in `crates/plugin-archive/src/extract.rs`), so a change to those rules re-keys only the targets that infer. |
| `strip_components` | `0` | Leading path components dropped from every entry, as `tar --strip-components`. Refused with a `gz`: at parse when `format = "gz"` is named, at run when it is inferred. |
| `include` | every entry | Glob patterns, a string or a list, in the syntax of `group`'s `include` (`*` within a path component, `**` across them, `{a,b}`, `[…]`), matched against each entry's path below `out` after `strip_components` (`bin/tool`). Only the files and links that match one are extracted; see [Unpacking rules](#unpacking-rules) and [Patterns](#patterns). An empty, NUL-holding or invalid pattern is refused at parse, and so is one no entry path can match (a leading `/` or `./`, a trailing `/`, a `.`, `..` or empty component). Hashed when set, sorted and deduped, with `GLOB_VERSION`. |
| `executable` | `False` | `True`: every extracted regular file is `0o755`, as a `gz`'s one file (gzip records no mode) or a zip built on Windows (no Unix mode) needs. A list of glob patterns (a string or a list, `include`'s syntax and refusals): only the files that match one. Holds for every format. Hashed when set; `[]` is `False`. |
| `out` | the target name | A **directory**, relative to the package, the archive unpacks into. |
| `max_bytes` | 4 GiB | The most bytes the archive may decode to for its entries, written or read past: in a tar, the entries `include` or `strip_components` leave out are decoded to reach the next one, and count. A zip entry left out is never read and costs nothing. |
| `max_entries` | 1,000,000 | The most entries the archive may hold. |
| `cache` | `True` | See [Cache keys](#cache-keys). |
| `resources` | `{"cpu": 1}` | Never hashed ([RESOURCES.md](RESOURCES.md)). |

### Unpacking rules

The archive is **untrusted input**. Each rule below is a security property
with a unit test in `crates/archive`, not a nicety:

- An absolute path or a `..` component is refused, and so is an entry written
  through a symlink the archive created.
- A symlink must stay inside the output: an absolute target is refused, and
  every link is resolved through the extracted tree, following the links it
  meets, and refused if it climbs above it. A hard link becomes a copy of an
  entry already extracted.
- Device and fifo entries are refused. Paths must be UTF-8.
- Modes are normalized: `0o755` when any execute bit is set, else `0o644`;
  directories `0o755`. Owners and mtimes are not applied.
- A path that appears twice is an error, except a directory listed again. A
  zip whose central directory names a path twice is refused as a whole.
- The output and the decoding are bounded: at most `max_bytes` decoded for
  the archive's entries, written or read past, and `max_entries` entries,
  counted while unpacking. A tar entry left out (by `include`, or removed
  whole by `strip_components`) is decoded to reach the next one, so its
  bytes count as a written entry's do: an excluded entry of terabytes of
  zeros fails instead of decoding forever. A zip entry left out is never
  read, and is not counted. The bounds are not hashed: exceeding one fails
  the run, and a failure is never cached.
- `strip_components` counts a leading `.` as GNU tar and bsdtar do
  (`./tool-1.0/bin/tool` with 1 is `tool-1.0/bin/tool`); a hard link's target
  is stripped too, a symlink's is not. An entry with no more components than
  `strip_components` is skipped; an archive from which nothing remains is an
  error listing its first five entries.
- A pattern sees an entry's path after both steps, in this order: first
  `strip_components` (which counts a `.`), then every `.` and empty
  component dropped. So `./tool-1.0/bin/tool` with `strip_components = 1` is
  matched as `tool-1.0/bin/tool` (the strip ate the `.`), and `./bin/tool`
  and `bin/./tool` with none are both matched as `bin/tool`.
- With `include` set, only the files, symlinks and hard links whose path
  matches a pattern are extracted. Directory entries are skipped, and the
  parent directories an extracted entry needs are created. An entry left out
  is still checked by the rules above and counts towards `max_entries` (and
  in a tar towards `max_bytes`, above), but is not written.
- Each `include` pattern must match a file or link in the archive, and each
  `executable` pattern an extracted regular file; one that matches nothing
  fails the run, naming it (a typo guard). A pattern matching only a
  directory matches nothing.
- A hard link whose target `include` leaves out is an error: the link is a
  copy of its target, which was never extracted.
- With `include` set, every extracted symlink must resolve to a path in the
  output: one whose target, or a link on the way to it, `include` left out
  fails the run (`` symlink "bin/tool": points at "../libexec/tool", which
  `include` left out …; include it too ``). That includes a link that already
  dangles in the archive: under `include` it is refused, since a dangling
  link there is likelier a target the patterns missed. Without `include`, a
  dangling link is extracted as it is.
- `executable` only ever adds the execute bit: a file the archive marks
  `0o755` stays so whatever `executable` says. `True` raises every regular
  file, hard-link copies included. With patterns, a hard-link copy is its
  target's file in the archive: it takes the mode its target was written
  with, a bit a pattern raised included, and is raised too when its own path
  matches. Symlinks are untouched.

- An xz stream's decoder may use at most 256 MiB. The decoder adds about
  100 KiB to the dictionary, so a dictionary of 256 MiB or more is refused
  before it is allocated, and `xz -9`'s 64 MiB is accepted. The header names
  the size, up to 4 GiB, so the cap is what stops an archive from exhausting
  memory rather than disk. The cap is not hashed, like the bounds above.
- What integrity check is verified depends on the format:
  - A `tar.xz` is read to the end of the stream, past tar's end marker, so
    every block's check (CRC32, CRC64 or SHA-256), the index and the footer
    are verified. Concatenated streams are read, as `xz -d` reads them. What
    the stream decodes to after the end marker counts against `max_bytes`:
    at least 512 bytes of zeros, and up to 10 KiB with GNU tar's record
    padding.
  - A bare `gz` is read to its end, so each member's CRC32 and length are
    verified.
  - A `tar.gz` or `tar.zst` is read only to tar's end marker, as it always
    has been: the gzip trailer or zstd checksum of the last member or frame
    is not verified, and bytes after the marker are not read. Pin the
    archive's own bytes with `sha256` where it is fetched.
  - A `zip` entry's CRC32 is verified as the entry is read.

### Patterns

`include` and an `executable` list are matched by wax, the engine behind
`group`'s `include`, against an entry's path as the rules above give it
(`bin/tool`). What to expect:

- Patterns are **case-sensitive**: `bin/*` does not match `Bin/tool`
  (`(?i)bin/*` does).
- `*` stays within a component and **matches dotfiles**: `*` matches
  `.hidden`, and `bin/*` matches `bin/.hidden`.
- `**/x` also matches a top-level `x`, and `bin/**` matches everything
  below `bin` (and a file named `bin` itself).
- A class is negated with `!`, not `^`: `[!x]` is any character but `x`,
  while `[^x]` is the class holding `^` and `x`.
- Names are compared **byte for byte**, with no Unicode normalization. A
  zip made on macOS may store `é` decomposed (NFD: `e` + U+0301); a
  pattern typed as the precomposed `é` (NFC) does not match it. Write the
  pattern in NFD, or use a `*` there.
- A pattern no entry path can match is refused at parse, with the fix: a
  leading `/` or `./`, a trailing `/` (`"bin/"`: write `"bin/**"` to take a
  directory's files), a `.`, `..` or empty (`bin//tool`) component.

The key holds the patterns' text, not what they match, so it also holds
`GLOB_VERSION` (`crates/plugin-archive/src/extract.rs`) in each pattern
section: a wax upgrade or a matcher change that changes what a pattern
matches bumps it. `crates/archive`'s `matching_is_pinned` test pins the
answers above and names `GLOB_VERSION` when one moves. A target without
patterns hashes no `GLOB_VERSION`, and keeps its key across such a bump.

### A bare `gz`

A `gz` is one file, gzip-compressed, not a tar: some projects ship a binary
that way (`rust-analyzer-x86_64-unknown-linux-gnu.gz`). It unpacks to one
file in `out`, named after the archive file with a trailing `.gz` removed,
as `gzip -d` names it (`a.gz.gz` is `a.gz`; the whole name when it has no
`.gz` or is only `.gz`). A name that leaves `.` or `..` (`..gz`) is
refused. The name the gzip header may record is ignored: it is the archive
author's to choose. gzip records no mode, so the file is `0o644` unless
`executable` makes it `0o755`. It is held to the rules above as a tar's one
file entry would be, `include` included, so the output is bounded;
`strip_components` with a `gz`
would leave nothing, so it is refused. Concatenated gzip members are one
file, and an empty file gzipped is one empty file.

It is stricter than `gzip -d` about what follows the last member: trailing
garbage or trailing zeros are an error, where `gzip -d` warns or ignores
them.

With `format` omitted, a `.gz` name is a bare `gz` unless the stream
inflates to a tar (tar's `ustar` magic at offset 257 of its first 262
inflated bytes): then it is a `tar.gz`, as a `.gz`-named tarball was before
`gz` existed. `heph.gh.release_asset` infers from the asset's name alone,
so a `.gz` asset that is a tarball needs `format = "tar.gz"`.

Changing the gz naming rule or its mode rule changes the trees extracted
from the same archive, and bumps `EXTRACT_FORMAT_VERSION`
(`crates/plugin-archive/src/extract.rs`).

One difference between platforms is the filesystem's: two entries whose names
differ only by case (or Unicode normalization) both extract on Linux, while on
macOS's case-insensitive APFS they are one file, and the second entry fails
with an error saying it collides with the first on a case-insensitive
filesystem.

## `archive.create`

| Option | Default | |
|---|---|---|
| `deps` | required | Targets whose outputs are packed, at least one. `\|group` and `[filters]` pick which of a target's files. |
| `format` | from `out` | `tar`, `tar.gz`, `tar.zst` or `zip`. Inferred from `out`'s suffix when omitted; one of the two must name it. A `format` contradicting `out`'s suffix (`zip` with `x.tar.gz`) is refused, and so is `tar.xz` or `gz` as `format`, or implied by `out` when `format` is omitted: they are read only. With `format` set, an `out` whose suffix names only a read-only format (`x.gz`, `x.txz`) names none, and takes `format`. |
| `out` | `<name>.<format>` | A **file**, relative to the package: the archive. It never overwrites a file a dep staged at the same path; that is an error. |
| `strip_prefix` | none | Leading directory removed from every entry, written workspace-relative (`pkg/out`). |
| `prefix` | none | Leading directory prepended to every entry, after `strip_prefix`. |
| `cache` | `True` | See [Cache keys](#cache-keys). |
| `resources` | `{"cpu": 1}` | Never hashed. |

### Where a file lands

An entry's name is the **workspace-relative path its file was staged at** —
the path its producer declared, such as `tool/out/bin/tool` — with
`strip_prefix` removed, then `prefix` prepended:

| Staged at | `strip_prefix` | `prefix` | Entry |
|---|---|---|---|
| `tool/out/bin/tool` | | | `tool/out/bin/tool` |
| `tool/out/bin/tool` | `tool/out` | | `bin/tool` |
| `tool/out/bin/tool` | `tool/out` | `tool-1.0` | `tool-1.0/bin/tool` |

`strip_prefix` matches whole components: `tool/outer/x` is not under
`tool/out`. A packed file **not under `strip_prefix` is an error** naming the
file, rather than kept at its full path: a partly-stripped archive is never
what was meant. Narrow `deps` with `|group` or `[filters]` instead. A file two
deps both stage is packed once; two different files landing on one entry name
are an error. Only files and symlinks are written, so an empty directory is
not carried.

Path options are relative, `/`-separated, with no empty, `.` or `..`
component. The writer checks every entry name by the same rule (and no NUL)
for every format, before writing a byte. A non-UTF-8 name is carried by tar
and refused by zip, which stores names as UTF-8; staged paths are UTF-8 in
practice, so this only matters to the `tar` coreutil.

A member is read for exactly the length it had when it was listed: a file
that grows or shrinks while being packed fails the run instead of producing
a tar whose header and data disagree. A zip member switches to zip64 from
`u32::MAX − 16 MiB`, leaving room for deflate's worst-case expansion.

### Reproducible by construction

The same deps give the same bytes on every machine, at every time, for every
user (one exception on macOS: names differing only by case or Unicode
normalization, see [Cache keys](#cache-keys)). Everything the bytes depend on is the deps' content or is in the def
hash (the format, `out`, the layout options and the sorted dep addresses):

- entries are sorted by name, so neither the deps' order nor directory order
  shows;
- owners are flattened (uid/gid 0, empty names) and modes normalized
  (`0o755` with any execute bit, else `0o644`; a symlink `0o777`);
- every tar entry's mtime is **0**; a zip entry's is 1980-01-01, the earliest
  zip can record;
- the compressor settings are fixed, and gzip's header carries no name or
  timestamp;
- a symlink is stored as a symlink, never followed.

**Nothing is read from the environment.** `SOURCE_DATE_EPOCH` is the `tar`
coreutil's convention; read by the driver, it would be an input no cache key
covers. A changed rule (a header field, a compressor setting, entry naming)
bumps `CREATE_FORMAT_VERSION` in `crates/plugin-archive/src/create.rs`, which
moves every archive's key. `crates/archive/src/write.rs` pins the bytes of
every format, and CI runs that test on all three platforms, so a dependency
bump that changes them fails there first. The `tar` coreutil writes through
the same code, so such a change is also a `COREUTILS_VERSION` decision
([COREUTILS.md](COREUTILS.md)).

## Cache keys

Both drivers cache by default. A target's key covers its own options (above)
and the **path and bytes** of every output of its deps, so a changed archive,
a changed dep file or a renamed one moves it. Three caveats, all in the
engine, not here:

- **Over-hashing.** A dep contributes every output group's content, not only
  the `|group` it was picked by: a change to another group of the dep reruns
  the target, needlessly but never wrongly.
- **`|group` is not bound.** The key holds the dep's output contents without
  the group names. If a dep swaps which files belong to groups `x` and `y`,
  the combined content is unchanged, so `deps = [":a|x"]` keeps its key and
  serves the archive built from the old `x`. Until the engine records the
  group name alongside each output (a change that moves every key, tracked
  separately), a dep reshuffling its groups needs a clean rebuild of its
  consumers.
- **Case and Unicode normalization on macOS.** Two deps staging names that
  differ only by case (`A.txt`, `a.txt`) or by Unicode normalization (NFC
  vs NFD) collapse into one file in the sandbox on macOS's default
  filesystem, so `create` packs one entry there and two on Linux. "The same
  bytes on every machine" holds except in that case; the sandbox fix is in
  the engine and tracked separately.

## Troubleshooting

- `nothing to extract: strip_components = N removed every entry`: the error
  lists the first entries; count their leading directories and lower
  `strip_components`.
- `raise max_bytes` / `raise max_entries`: the archive is larger than the
  default bounds; set the option if it is meant to be.
- `… not under strip_prefix …` (create): the error lists the first staged
  paths that miss it and suggests the deepest directory they all share
  (`did you mean strip_prefix = "…"?`); use it, narrow `deps`, or shorten
  `strip_prefix`.
- `` `format` is "zip" but `out` "x.tar.gz" names a "tar.gz" `` (create):
  drop `format`, or rename `out`.
- `a dep staged that path` (create): `out` collides with a file a dep
  produced; rename `out`.
- `set format` (extract): the archive's name has no known suffix and its
  bytes match no format; set `format`.
- `must produce exactly one file` (extract): `src` names a target with
  several outputs; point it at one file, with `|group` if needed.
- `the xz stream's dictionary needs more decoder memory than the 256 MiB
  cap` (extract, `tar.xz`): the archive was compressed with a dictionary of
  256 MiB or more; recompress it with a smaller one (`xz -9` uses 64 MiB).
- `` `include` pattern "…" matches no file or link in the archive ``
  (extract): the error lists the first paths the pattern was matched
  against, below the output and after `strip_components`. Fix the pattern
  to match one of them; a pattern written with the top-level directory
  `strip_components` removed (`tool-1.0/bin/tool` for `bin/tool`) is the
  usual cause, and so is one matching only a directory (`bin` for
  `bin/*`).
- `` `executable` pattern "…" matches no extracted regular file ``
  (extract): the pattern names no file that was extracted, because of a
  typo, because it names a directory or a symlink, or because `include`
  left the file out.
- `hard link target "…" is not included` (extract): `include` keeps a hard
  link but not the file it is a copy of; add that file's path to `include`.
- `` symlink "…": points at "…", which `include` left out `` (extract):
  `include` keeps a symlink whose target (or a link on the way to it) was
  not extracted, or that dangles in the archive itself; add the target's
  path to `include`.
- `"bin/" matches no file; to take a directory's files write "bin/**"`
  (extract, at parse): the pattern can never match an entry path; write
  what the error suggests.
- `` `strip_components` with `format = "gz"` `` (extract, or with a `gz`
  asset): a bare `gz` is one file; drop `strip_components`.
- `archive.create cannot write a "tar.xz"` (or `"gz"`): pick a written
  format, or rename `out`.
