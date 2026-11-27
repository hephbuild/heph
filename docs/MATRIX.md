# Matrices: one declaration, many variants

`heph.core.matrix()` is a cross product. It enumerates the cells of some axes,
calls a cell function once per cell with that cell's values, and returns a list
of what each call returned, plus the value of a default cell when you ask for
one. It knows nothing about targets, platforms or the host. What a cell does is
its own business: declare targets, compute a URL, or nothing at all.

```python
def _test(go, race):
    return target(
        name = "test.go" + go + (".race" if race else ""),
        driver = "bash",
        run = "go test %s ./..." % ("-race" if race else ""),
        tools = ["//tools/go:go" + go],
    )

TESTS = heph.core.matrix(
    axes = {"go": ["1.22", "1.23"], "race": [False, True]},
    cell = _test,
    default = {"go": "1.23", "race": False},
)
# [c.axes for c in TESTS.cells] == [{"go": "1.22", "race": False}, {"go": "1.22", "race": True},
#                                   {"go": "1.23", "race": False}, {"go": "1.23", "race": True}]
# [c.value for c in TESTS.cells] == ["//p:test.go1.22", "//p:test.go1.22.race",
#                                    "//p:test.go1.23", "//p:test.go1.23.race"]
# TESTS.default == "//p:test.go1.23"

target(name = "test", driver = "group", deps = [TESTS.default])
target(name = "test-all", driver = "group", deps = [c.value for c in TESTS.cells])
```

The matrix itself declares nothing and adds nothing to any hash. A target a
cell declares is exactly what the cell called, as if the BUILD file had written
the same calls on that line: `:test.go1.22.race` is an ordinary target. A cell
names its own targets; two cells that build the same name declare one address
twice, which is refused (see "Duplicate declarations").

A target a cell declares is cached and served to every machine like any other,
so **its config should come from the cell's arguments**, not from
`heph.core.os()` or `heph.core.arch()` read inside the cell.

`.cells` (a list in cell order), each cell's `.axes` and `.value`, and
`.default` are stable contracts: BUILD files and `load()`ers depend on them.

## Arguments

| Argument | Meaning |
| --- | --- |
| `axes` | A dict of axis name to a list of values, in declaration order. A name is any string and is passed to `cell` as a keyword argument; a name a `def` cannot take as a parameter (`go-version`, `if`) reaches a `**kwargs` cell. A value is any Starlark value (a string, a bool, a list, a dict...) and is passed through unchanged. At least one axis, at least one value each, no two equal values in one axis. A string, tuple or dict where a list is expected is refused, not iterated. |
| `cell` | Called once per cell, in cell order, with one keyword argument per axis. Returns any value, `None` included. Omitted: each cell's value is its `axes` dict. |
| `default` | Which cell `.default` reads: `None` (the default: no `.default` at all) or a dict giving a value for every axis, e.g. `{"go": "1.23", "race": False}`. |
| `exclude` | A list of dicts: cells removed from the cross product. An entry names at least one axis (`exclude = [{}]` is refused: it would match every cell), and only axes that exist. Each must match a cell of the full cross product (a stale exclude is a typo); overlapping excludes are fine, and excluding every cell with entries that each name an axis is allowed. |
| `include` | A list of dicts: cells added after the cross product, in list order. Each names every axis and nothing else; a value may be outside the axis list, and does not extend it (an `exclude` naming that value is still stale). An include equal to an excluded cell adds it back; one equal to a cell still present is refused. |

Values are compared by Starlark equality: for repeated values, and for matching
`exclude`, `include` and `default` against cells. So `1` and `1.0` are one
value, and `["-race"]` matches `["-race"]`.

These are [GitHub Actions' `exclude`/`include` rules](https://docs.github.com/en/actions/writing-workflows/choosing-what-your-workflow-does/running-variations-of-jobs-in-a-workflow#expanding-or-adding-matrix-configurations),
with one difference: an Actions matrix is a list of jobs, and a heph cell is a
function call whose return value you keep.

## The result

`cells` is a list, one entry per cell, in cell order: the cross product (last
axis fastest) minus `exclude`, then `include`. Each entry has two attributes:

- `axes`: a dict of axis name to the cell's value, in axis declaration order.
- `value`: what `cell` returned, unchanged. Without `cell`, a dict equal to
  `axes`.

Cell order depends only on the arguments, so `[c.value for c in M.cells]` is
the same list on every machine.

```python
CMDS = heph.core.matrix(
    axes = {"go": ["1.22.5", "1.23.1"], "flags": [[], ["-race"]]},
    cell = lambda go, flags: "go%s test %s" % (go, " ".join(flags + ["./..."])),
    exclude = [{"go": "1.22.5", "flags": ["-race"]}],
)
# [c.value for c in CMDS.cells] ==
#     ["go1.22.5 test ./...", "go1.23.1 test ./...", "go1.23.1 test -race ./..."]
# CMDS.cells[2].axes == {"go": "1.23.1", "flags": ["-race"]}
```

To look a cell up by its values, build the dict you need:

```python
PLAIN = {c.axes["go"]: c.value for c in CMDS.cells if c.axes["flags"] == []}
# PLAIN == {"1.22.5": "go1.22.5 test ./...", "1.23.1": "go1.23.1 test ./..."}
```

An axis whose name is not an identifier reaches a `**kwargs` cell:

```python
V = heph.core.matrix(
    axes = {"go-version": ["1.22", "1.23"]},
    cell = lambda **kw: "go" + kw["go-version"],
)
# [c.value for c in V.cells] == ["go1.22", "go1.23"]
```

The builtin never reads, checks or rewrites a cell's value. A relative address
a cell returns stays relative: it means the package whose BUILD file made the
call, as any string in that file does. A cell that is several targets returns a
dict, and each `.value` is that dict.

The result is an ordinary value: a `.star` file may build it at top level and
several packages may `load()` it.

## The default

`default` is a dict giving one value for every axis; `.default` is the `value`
of the cell it names.

- `default = {"go": "1.23", "race": False}` names one cell of the matrix
  above. Dict order does not matter.
- Without `default` (or with `default = None`) the result has no `default`
  attribute: `hasattr(M, "default")` is false, and reading it fails.
- A string or any other non-dict is refused.

Every mistake fails at the call: an unknown axis, a missing one, or values that
name no cell.

```
heph.core.matrix() at p/BUILD:12: default {go="1.24", race=False} names no cell of this
matrix. Its cells: {go="1.22", race=False}, {go="1.22", race=True}, {go="1.23", race=False},
{go="1.23", race=True}. Add it with include, or name another cell
```

A default built from `heph.core.os()` and `heph.core.arch()` (see the release
example below) depends on the host, and so does every target that uses it: a
consumer's config holds this host's cell, so its cache key moves with the host.
On a host with no cell the call itself fails; a matrix without `default`
evaluates anywhere. To see which cell a consumer got, run
`heph inspect spec //p:test`.

There is no alias target. A consumer that wants one stable name writes one
`group` over `M.default` itself.

## Errors

Every error names the call site, and a failing cell names the cell by its
values, shown as Starlark reprs:

```
heph.core.matrix() at p/BUILD:4: cell go="1.23", race=True: Found `race` extra named
parameter(s) for call to …
```

## Duplicate declarations

Two declarations of one address in a package with different configs fail
evaluation, wherever they are, in a cell or not, in one BUILD file or two of one
package:

```
//p:tool is declared twice with different configs: p/BUILD:3 (cell go="1.22") and p/BUILD:3
(cell go="1.23"). They differ in config.url. If both come from a matrix cell, put the cell's
axis values in the target name; otherwise remove one declaration, or make them identical
```

Identical declarations (the same `.star` helper called twice) collapse to one,
which keeps the first one's position. "Identical" compares what becomes the
target: the driver as written (an omitted `driver` and the explicit default
driver differ), the config as values (dict order does not matter; `-0.0` and
`0.0` differ), the labels, `transitive` and `approval`.

Before matrices, the first of two same-named declarations won and the second
was dropped without a word, so `name = "tool"` in every cell would have shipped
one cell's bits for all of them. Joining values into a name can collide too
(`"1.2"` + `"3"` against `"1"` + `"2.3"`); that is refused the same way.

## Limits

- At most 1,024 cells per call, counted as the cross product plus `include`,
  before the first cell runs: `heph.core.matrix() at p/BUILD:12 would have
  2,048 cells; the limit is 1,024`. Real matrices are 3 to about 120 cells.
- A cell may not call `heph.core.matrix()`.

## Per-platform release assets

The common download case: one `heph.gh.release_asset` per platform, each
pinned by its own `sha256`. `release_asset` returns `//<pkg>:<name>`, so the
cell returns its result.

<!-- crates/e2e gh_release_asset::gh_release_matrix_example evaluates the block below. -->
```python
ASSETS = {
    "linux/amd64":  ("tool-x86_64-unknown-linux-gnu.tar.gz",  "<sha256>"),
    "linux/arm64":  ("tool-aarch64-unknown-linux-gnu.tar.gz", "<sha256>"),
    "darwin/arm64": ("tool-aarch64-apple-darwin.tar.gz",      "<sha256>"),
}

def _tool(platform):
    asset, sha = ASSETS[platform]
    return heph.gh.release_asset(
        name = "tool." + platform.replace("/", "-"),
        repo = "acme/tool",
        tag = "v1.2.0",
        asset = asset,
        sha256 = sha,
        extract = True,
        strip_components = 1,
    )

TOOL = heph.core.matrix(
    axes = {"platform": ["linux/amd64", "linux/arm64", "darwin/arm64"]},
    cell = _tool,
    default = {"platform": heph.core.os() + "/" + heph.core.arch()},
)
```

`TOOL.default` is this machine's tool (`tools = [TOOL.default]`), and
`[c.value for c in TOOL.cells]` every platform's, so a darwin laptop can bundle
the `linux/arm64` one.

## Multi-arch OCI images

`oci.image(layers_by_platform = ...)` takes a dict of platform to address, so
build it from the cells:

```python
LAYER = heph.core.matrix(axes = {"platform": ["linux/amd64", "linux/arm64"]}, cell = _layer)
heph.oci.image(
    name = "image",
    layers_by_platform = {c.axes["platform"]: c.value for c in LAYER.cells},
    ...
)
```

## What a matrix is not

- **Not a configuration.** Building a `linux/arm64` cell does not rebuild its
  deps for `linux/arm64`: each cell names its own deps.
- **Not remote execution.** A cell whose command only runs on one platform
  still runs on the host, and fails there as it would today. Pick which cells
  to run.
- **No implicit context.** A cell sees only its arguments. A plugin function
  called in a cell does not learn the cell's values: pass them (`goos`,
  `goarch`, or whatever it takes) explicitly.
- **No address sugar.** A cell's target is a named target; there is no
  `//p:tool@platform=linux/arm64` form, and `@args` keep their meaning for
  `http_fetch` and Go.
