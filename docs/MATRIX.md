# Matrices: one declaration, many variants

`heph.core.matrix()` enumerates the cells of a matrix (platforms, or any
other axes), calls a cell function once per cell, and returns what each call
returned keyed by cell, plus the value of a default cell when you ask for one.
What a cell does is its own business: declare targets, compute a URL, or
nothing at all.

```python
def _tool(platform):
    asset, sha = ASSETS[platform]
    n = "tool." + platform.replace("/", "-")
    heph.gh.release_asset(name = n, repo = "acme/tool", tag = "v1.2.0",
                          asset = asset, sha256 = sha, extract = True)
    return ":" + n

TOOL = heph.core.matrix(
    axes = {"platform": ["linux/amd64", "linux/arm64", "darwin/arm64"]},
    cell = _tool,
    default = {"platform": heph.core.host()},
)
# TOOL.cells   == {"linux/amd64":  ":tool.linux-amd64",
#                  "linux/arm64":  ":tool.linux-arm64",
#                  "darwin/arm64": ":tool.darwin-arm64"}
# TOOL.default == ":tool.darwin-arm64"   (on a darwin/arm64 host)

target(name = "fmt", driver = "bash", run = "tool fmt .", tools = [TOOL.default])
target(name = "bundle", driver = "archive.create", deps = TOOL.cells.values())
```

`:fmt` reads `TOOL.default`, so it has the host's cell in its config and its
cache key moves with the host; `:bundle` takes `cells` explicitly and does not.

The matrix itself declares nothing and adds nothing to any hash. A target a
cell declares is exactly what the cell called, as if the BUILD file had written
the same calls on that line: `:tool.linux-arm64` is an ordinary target, built
from a darwin laptop like any other. Because a cell built on one host is served
from the cache to every other, **a cell's output must not depend on the host**:
pass the cell's values (`platform`, `go`, ...) into its config rather than
reading `heph.core.host()` inside it.

`.cells`, `.default`, the key shape (the bare value for one axis, a tuple for
several) and the cell names below are stable contracts: BUILD files and
`load()`ers depend on them.

## Arguments

| Argument | Meaning |
| --- | --- |
| `axes` | A dict of axis name to a list of values, in declaration order. That order orders tuple keys. Axis names are made of `A-Z a-z 0-9 _` and do not start with a digit (they become keyword arguments of `cell`). A keyword such as `if` is allowed; a `def` cannot name it as a parameter, so read it from `**kwargs`. At least one axis, at least one value each, no repeated value. Values are one or more of `A-Z a-z 0-9 . _ / + -`. A string, tuple or dict where a list is expected is refused, not iterated. |
| `cell` | Called once per cell, in cell order, with one keyword argument per axis. Returns any value, `None` included. Omitted: each cell's value is its dict of axis values. |
| `default` | Which cell `.default` reads: `None` (the default: no `.default` at all) or a dict giving a value for every axis, e.g. `{"platform": heph.core.host()}`. |
| `exclude` | A list of dicts: cells removed from the cross product. An entry names at least one axis (an empty `{}` is refused: it would match every cell), and only axes that exist. Each must match a cell of the full cross product (a stale exclude is a typo); overlapping excludes are fine, and excluding every cell with entries that each name an axis is allowed. |
| `include` | A list of dicts: cells added after the cross product, in list order. Each names every axis and nothing else; a value may be outside the axis list, and does not extend it (an `exclude` naming that value is still stale). An include equal to an excluded cell adds it back; one equal to a cell still present is refused. |

These are [GitHub Actions' `exclude`/`include` rules](https://docs.github.com/en/actions/writing-workflows/choosing-what-your-workflow-does/running-variations-of-jobs-in-a-workflow#expanding-or-adding-matrix-configurations).

## The result

`cells` is a dict, cell key to what the cell function returned, unchanged, in
cell order: the cross product (last axis fastest) minus `exclude`, then
`include`. A key is the bare value for one axis (`"linux/arm64"`) and a tuple in
axis order for several (`("1.23.1", "linux/arm64")`). Neither keys nor order
depend on the host, so `cells.values()` lists an image's layers in the same
order on every machine.

```python
URLS = heph.core.matrix(
    axes = {"go": ["1.22.5", "1.23.1"], "platform": PLATS},
    cell = lambda go, platform: "https://go.dev/dl/go%s.%s.tar.gz" % (go, platform.replace("/", "-")),
)
# URLS.cells[("1.23.1", "linux/arm64")] == "https://go.dev/dl/go1.23.1.linux-arm64.tar.gz"

CELLS = heph.core.matrix(axes = {"go": ["1.22.5", "1.23.1"], "race": ["on", "off"]})
# CELLS.cells[("1.23.1", "on")] == {"go": "1.23.1", "race": "on"}
```

The builtin never reads, checks or rewrites a cell's value. A relative address
a cell returns stays relative: it means the package whose BUILD file made the
call, as any string in that file does. A cell that is several targets returns a
dict, and `cells` holds the dicts:

```python
def _sdk(platform):
    p = platform.replace("/", "-")
    # ... declares :sdk.<p>.bin and :sdk.<p>.headers
    return {"bin": ":sdk." + p + ".bin", "headers": ":sdk." + p + ".headers"}

SDK = heph.core.matrix(axes = {"platform": PLATS}, cell = _sdk)
# SDK.cells["linux/arm64"]["bin"] == ":sdk.linux-arm64.bin"
```

The result is an ordinary value: a `.star` file may build it at top level and
several packages may `load()` it.

## The default

`default` is a dict giving one value for every axis; `.default` is the value
of the cell it names. The values are fixed strings: the builtin never reads the
host itself.

- The host's cell: `default = {"platform": heph.core.host()}`, where
  `heph.core.host()` returns `os/arch` (`darwin/arm64`). With `os` and `arch`
  axes, `{"os": heph.core.os(), "arch": heph.core.arch()}`.
- A matrix with another axis names its value too:
  `default = {"platform": heph.core.host(), "go": "1.23.1"}`.
- The default need not be the host: `default = {"go": "1.23.1", "race": "off"}`
  names one cell of a test matrix outright.
- Without `default` (or with `default = None`) the result has no `default`
  attribute: `hasattr(M, "default")` is false, and reading it fails.
- `default = "host"`, or any other string, is refused with the spelling above.

Every mistake fails at the call: an unknown axis, a missing one, or values that
name no cell. A host-derived dict on a host with no cell is the last of these,
so on a riscv64 machine the call itself fails (a matrix without `default`
evaluates anywhere):

```
heph.core.matrix() at p/BUILD:12: default {platform=linux/riscv64} names no cell of this
matrix. Its cells: platform=linux/amd64, platform=linux/arm64, platform=darwin/arm64.
Add it with include, or name another cell
```

**A host-derived `.default` depends on the host, and so does every target that
uses it.** A consumer of `TOOL.default` has the host's cell address in its
config, so its cache key moves with the host even when its output would not (a
formatter misses across hosts). A cell's own key does not move:
`:tool.linux-arm64` is the same target on every host. To see which cell a
consumer got, run `heph inspect spec //p:fmt`.

There is no alias target. A consumer that wants one stable name writes one
`group` over `TOOL.default` itself.

## Naming cells

A cell names its own targets. The convention, which these docs and every paved
road follow, is `<name>.<v1>.<v2>…` with `/` written as `-`:
`tool.linux-arm64`, `test.1.23.linux-arm64`. These names are addresses users
type, so treat them as stable once published.

Two cells that map to one name (`linux/arm64` and `linux-arm64`, or `1.2` + `3`
against `1` + `2.3`) declare one address twice, which is refused (below).

## Duplicate declarations

Two declarations of one address in a package with different configs fail
evaluation, wherever they are, in a cell or not, in one BUILD file or two of one
package:

```
//p:tool is declared twice with different configs: p/BUILD:12 (cell platform=linux/amd64)
and p/BUILD:12 (cell platform=linux/arm64). They differ in config.url. If both come from
a matrix cell, put the cell's axis values in the target name; otherwise remove one
declaration, or make them identical
```

Identical declarations (the same `.star` helper called twice) collapse to one,
which keeps the first one's position. "Identical" compares what becomes the
target: the driver as written (an omitted `driver` and the explicit default
driver differ), the config as values (dict order does not matter; `-0.0` and
`0.0` differ), the labels, `transitive` and `approval`.

Before matrices, the first of two same-named declarations won and the second
was dropped without a word, so `name = "tool"` in every cell would have shipped
one platform's bits for all of them.

## Limits

- At most 1,024 cells per call, counted as the cross product plus `include`,
  before the first cell runs: `heph.core.matrix() at p/BUILD:12 would have
  2,048 cells; the limit is 1,024`. Real matrices are 3 to about 120 cells.
- A cell may not call `heph.core.matrix()`.

## Per-platform release assets

The common download case: one `heph.gh.release_asset` per platform, each
pinned by its own `sha256`, plus the host's for tools. `release_asset` itself
is unchanged.

<!-- crates/e2e gh_release_asset::gh_release_matrix_example evaluates the block below. -->
```python
ASSETS = {
    "linux/amd64":  ("tool-x86_64-unknown-linux-gnu.tar.gz",  "<sha256>"),
    "linux/arm64":  ("tool-aarch64-unknown-linux-gnu.tar.gz", "<sha256>"),
    "darwin/arm64": ("tool-aarch64-apple-darwin.tar.gz",      "<sha256>"),
}

def _tool(platform):
    asset, sha = ASSETS[platform]
    n = "tool." + platform.replace("/", "-")
    heph.gh.release_asset(
        name = n,
        repo = "acme/tool",
        tag = "v1.2.0",
        asset = asset,
        sha256 = sha,
        extract = True,
        strip_components = 1,
    )
    return ":" + n

TOOL = heph.core.matrix(
    axes = {"platform": ["linux/amd64", "linux/arm64", "darwin/arm64"]},
    cell = _tool,
    default = {"platform": heph.core.host()},
)
```

`TOOL.default` is the host's tool (`tools = [TOOL.default]`), and
`TOOL.cells` every platform's.

## Multi-arch OCI images

`oci.image(layers_by_platform = ...)` takes one address per platform, so
`cells` passes straight in:

```python
LAYER = heph.core.matrix(axes = {"platform": ["linux/amd64", "linux/arm64"]}, cell = _layer)
heph.oci.image(name = "image", layers_by_platform = LAYER.cells, ...)
```

## What a matrix is not

- **Not a configuration.** Building `:app.linux-arm64` does not rebuild its
  deps for `linux/arm64`: each cell names its own deps.
- **Not remote execution.** A cell whose command only runs on its own platform
  still runs on the host, and fails there as it would today. Pick which cells to
  run.
- **No host override.** A plugin function called in a cell does not see the
  cell's platform as its host: pass `goos`/`goarch` (or whatever it takes)
  explicitly.
- **No address sugar.** A cell is a named target; there is no
  `//p:tool@platform=linux/arm64` form, and `@args` keep their meaning for
  `http_fetch` and Go.

`heph.core.host()` returns the host as `os/arch` (`darwin/arm64`), the value
`default = {"platform": heph.core.host()}` names.
