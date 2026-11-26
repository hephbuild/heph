# Archives

The builtin `archive` plugin has two drivers, both in-process (no host
`tar`/`unzip`/`zip`), with the same code on every supported platform:

- **`archive.extract`** unpacks the one archive file a `src` target produced
  into a directory.
- **`archive.create`** packs the outputs of `deps` into one archive file.

The formats are `tar`, `tar.gz` (`.tgz`), `tar.zst` (`.tzst`) and `zip`. The
code is `crates/plugin-archive` (the drivers) over `crates/archive` (the
unpacker and the writer). `heph.gh.release_asset(..., extract = True)`
declares an `archive.extract` ([GH_RELEASES.md](GH_RELEASES.md)), and the `tar`
coreutil writes through the same writer as `archive.create`
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

## `archive.extract`

| Option | Default | |
|---|---|---|
| `src` | required | The target whose **single** output file is the archive. A target with several files is refused with the count: which one is meant would depend on walk order. |
| `format` | inferred | `tar`, `tar.gz`, `tar.zst` or `zip`. When omitted, it is taken from the archive file's name suffix, else from its leading bytes (gzip `1f 8b`, zstd `28 b5 2f fd`, zip `PK\3\4` or `PK\5\6`, tar `ustar` at offset 257); neither → an error asking for `format`. Given, it overrides both and is hashed. Omitted, the key holds the inference rules' version (`INFERENCE_VERSION` in `crates/plugin-archive/src/extract.rs`), so a change to those rules re-keys only the targets that infer. |
| `strip_components` | `0` | Leading path components dropped from every entry, as `tar --strip-components`. |
| `out` | the target name | A **directory**, relative to the package, the archive unpacks into. |
| `max_bytes` | 4 GiB | The most bytes the archive may unpack to. |
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
- The output is bounded: at most `max_bytes` written and `max_entries`
  entries, counted while unpacking. The bounds are not hashed: exceeding one
  fails the run, and a failure is never cached.
- `strip_components` counts a leading `.` as GNU tar and bsdtar do
  (`./tool-1.0/bin/tool` with 1 is `tool-1.0/bin/tool`); a hard link's target
  is stripped too, a symlink's is not. An entry with no more components than
  `strip_components` is skipped; an archive from which nothing remains is an
  error listing its first five entries.

One difference between platforms is the filesystem's: two entries whose names
differ only by case (or Unicode normalization) both extract on Linux, while on
macOS's case-insensitive APFS they are one file, and the second entry fails
with an error saying it collides with the first on a case-insensitive
filesystem.

## `archive.create`

| Option | Default | |
|---|---|---|
| `deps` | required | Targets whose outputs are packed, at least one. `\|group` and `[filters]` pick which of a target's files. |
| `format` | from `out` | `tar`, `tar.gz`, `tar.zst` or `zip`. Inferred from `out`'s suffix when omitted; one of the two must name it. A `format` contradicting `out`'s suffix (`zip` with `x.tar.gz`) is refused. |
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
