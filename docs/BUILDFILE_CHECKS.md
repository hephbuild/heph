# BUILD-file checks

Every package with a BUILD file gets three targets: `build-lint-check`,
`build-format-check` and `build-format`. They are always on. The code lives in
`crates/plugin-buildfile/src/pluginbuildfile/checks/`.

Upgrading changes what `heph run //...` does: it now rewrites BUILD files and
fails on type errors and unformatted files. See
[What changes on upgrade](#what-changes-on-upgrade).

## What is not checked

`build-lint-check` runs Starlark's static typechecker without executing
anything, and that checker only checks the **body of each `def`**. Top-level
statements are not statically checked, and most of a BUILD file is top level:

```python
def lib(name: str) -> None:
    n: int = name          # reported: type-error, inside a def

x: int = "s"               # not reported: top level
lib(1)                     # not reported: top level
```

A bad top-level statement is not missed, though. Evaluation runs it with
runtime type checks, so `heph run`, `query` and `validate` already fail on it.
A passing `build-lint-check` means that every def body typechecks. It does not
mean the whole file does.

A `def` with no return type gets one inferred from its `return` statements,
and calls to it are checked against that type. Lambdas cannot be annotated, so
they are exempt.

### Declared types

A BUILD file can declare its own types and annotate with them. The available
forms are `record` (with `field` for a default), `enum`, a type alias, and
`typing` (`typing.Any`, `typing.Callable`, `typing.Iterable`,
`typing.Never`). A declared type can be loaded like any other name:

```python
Mode = enum("debug", "release")
Lib = record(name = str, mode = Mode, srcs = field(list[str], []))
Names = list[str]

def lib(l: Lib) -> Names:
    n: Names = 1          # reported: an alias is a type expression
    return [l.name]

def app():
    lib("a")              # not reported: `Lib` is `Any` to the checker
```

Lint checks an alias, since it is a type expression. A `record` or `enum` is
the result of a call, and the checker runs nothing, so it treats the
annotation as `Any`. Evaluation enforces all three: `lib("a")` fails at
`heph run` with ``Value `a` of type `string` does not match the type
annotation `Lib` …``. The same applies to a record field of the wrong type
and to an enum value that is not a variant.

A record or enum value cannot be passed to `target()` itself, whose config
holds only None, str, bool, int, float, list and dict. Pass its fields
(`l.name`, `mode.value`) instead.

## The targets

| Target | Does | Writes | Labels |
|---|---|---|---|
| `//<pkg>:build-lint-check` | Static typecheck of the files the package owns; with `requireTypeAnnotations`, every `def` parameter (`*args`, `**kwargs` included) must have a type. The return type may be left out, and so may the type of a parameter with a scalar literal default (not `None`, and not a list, dict or tuple display; see [Inferred types](#inferred-types)) | nothing | `build-lint-check`, `lint-check` |
| `//<pkg>:build-format-check` | Fails when an owned file differs from the formatter's output, the same rule as `heph tool build-fmt --check` | nothing | `build-format-check`, `format-check` |
| `//<pkg>:build-format` | Rewrites owned files to the formatter's output (`codegen = in_place`) | owned files | `build-format`, `format`, `fix` |

The labels are the Go plugin's, so one label run covers both languages. All
three are cached locally and never remotely: a check costs milliseconds per
package. `//...` includes `build-format`, and so do `label(fix)` and
`label(format)`.

The lint key is the owned files, every file they load (transitively, by
content), `requireTypeAnnotations`, heph's version, the names of the plugin
functions, `driverKwargsDigest` and the BUILD `patterns`.
`driverKwargsDigest` covers the [`Kwargs`](#driver-kwargs) types of every
registered driver: each driver's name, keys and key types. A driver schema
change that leaves those types alone, a doc edit say, does not re-run lint.
The format key is the owned files,
`indent` and heph's version. Nothing about either reaches an ordinary target:
turning the option on or off leaves every other def hash unchanged.

`heph tool build-fmt` stays. Use it for ad-hoc and editor formatting, and
`build-format` for the cached gate. Both share one format function.

## The option

```yaml
plugins:
  - builtin: buildfile
    options:
      requireTypeAnnotations: true   # default false
```

The typechecker always runs. Only the annotation rule sits behind the option.

A profile does not merge plugin options key by key. A profile's
`builtin: buildfile` entry replaces the base entry wholesale, so it must repeat
`requireTypeAnnotations` (and `patterns`, `indent`) to keep them.

## Inferred types

heph's starlark infers two types the rule would otherwise ask for, so their
annotations stay optional and calls are still checked against them:

- **The return type** of a top-level `def`: the union of what its `return`s
  give, plus `None` when the body can run off its end.
- **A parameter with a literal default** takes the literal's type. `1` and
  `-1` give `int`. `1.5` and `-1.5` give `int | float`, the same type as
  `heph.types.Number`, so `f(2)` passes against `scale = 1.0`. `"s"` gives
  `str`, and `True` and `False` give `bool`.

`True` and `False` count only while the module does not rebind them anywhere
(an assignment, a `for`, a `load`, a `def`, even one local to another def).
Where it does, annotate the parameter.

A list display (`[]`, `["a"]`), a dict display (`{}`) or a tuple display
(`()`), empty or not, gives a `list`, `dict` or `tuple` of `Any`: the checker
holds callers to the container, never to its elements. That type describes
nothing, since a display does not say what goes in it, so
`requireTypeAnnotations` still asks for an annotation with no `Any` in it:
write `srcs: list[str] = []`. Only a type a default gives is held to that; an
annotation you write may contain `Any`.

These defaults infer nothing, so the parameter still needs an annotation:
`None` (it means "optional something"; write `x: str | None = None`), a name
(`x = DEFAULT`), a call, an operator (`1 + 1`, `not True`), `+` or `-` on
anything but a number (`-True`, `--1`), and `...`.

To accept more than the default's type, annotate the parameter. An annotation
always wins over the default: `n: heph.types.Number = 1` takes a float too,
and `name: str | None = ""` takes `None`.

A wrapper parameter that forwards to a `target()` field (`deps`, `labels`,
`env`, `tools`, `pass_env`, `runtime_pass_env`) takes its `heph.types` alias:
`deps: heph.types.Deps = []`. With `requireTypeAnnotations` on, an
unannotated `deps = []` gets a finding that suggests the alias.

Only `build-lint-check` checks these types. Evaluation (`heph run`, `query`,
`validate`) checks annotations alone, so it still accepts any argument for a
parameter typed only by its default.

### Allowing `None`

A parameter takes `None` only when its annotation says `| None`, or when its
annotation is a field alias, which includes `None`. An inferred type never
includes `None`.

| Parameter | Takes `None` | Type checked by |
|---|---|---|
| `s: str` | no | lint and evaluation |
| `s: str \| None = None` | yes | lint and evaluation |
| `s = ""` | no | lint only |
| `s = None` | yes, and any other value | nothing; `requireTypeAnnotations` reports it |
| `deps: heph.types.Deps = None` | yes | lint and evaluation |
| `n: heph.types.Number \| None = None` | yes; `heph.types.Number` alone does not | lint and evaluation |

## Failure output

There is one line per finding, sorted by file, line and column. Each line
starts with a stable prefix and ends with the fix where there is one. A count
line follows:

```
pkg/BUILD:3:9: annotation: parameter `x` of `make_lib` has no type; add `: <type>`
pkg/BUILD:7:10: annotation: parameter `srcs` of `make_bin` is typed `list` by its default, a list of `Any`; add a type with no `Any`, e.g. `: list[str]`
pkg/defs.BUILD:12:5: type-error: Expected type `str` but got `int`
3 findings in //pkg
annotations are required by requireTypeAnnotations in .hephconfig (plugins: buildfile, options)
```

The last line appears only when there is an `annotation:` finding. When a
parameter is named like a `heph.types` alias (`deps`, `labels`, …), its finding
suggests that alias: `deps = []` ends with ``add `: heph.types.Deps` ``. A
dict default suggests `dict[str, str]` and a tuple default
`tuple[str, ...]`; put in the element types the parameter really takes.

The expected type in a `type-error:` may come from a parameter's default
rather than from an annotation. After `def f(n = 1)` on line 1, a call
`f("s")` at line 5 of another def reports:

```
pkg/BUILD:5:7: type-error: Expected type `int` but got `str`
```

To accept a `str` there too, annotate `n` (see
[Inferred types](#inferred-types)).

`build-format-check` prints `format: not formatted` lines and ends with
`run heph run //pkg:build-format to fix`.

A format finding names the first line that differs from the formatter's
output, and only that line. There is no diff, on purpose: the fix is to run
`build-format`, not to edit by hand, and one line is enough to find the spot.

The text goes to the target's failure log (`heph inspect failure <addr>`). The
run's error is the count line. The line format is the contract. The checks
have no `--json` of their own. For a machine-readable route,
`heph inspect failure --json <addr>` gives the path of the failure log, which
holds these lines.

## CI

```
heph validate && heph run -e 'label(lint-check) || label(format-check)'
```

A bare `label()` in `-e` selects across the whole workspace, wherever it is
run from. The positional form takes one bare label only, so
`heph run 'label(lint-check) || …' //...` is refused. `heph validate` comes
first because it is what reports a shadowed check (below). `run` stays quiet
about one.

## Ownership: one owner per file

A file is checked and formatted by the package whose directory holds it, never
by a package that only loads it. A loaded file's findings are discarded in the
loading package, but its interface still types the calls made to it.

- **Name shared helpers `*.BUILD`.** A `defs.BUILD` next to a BUILD file is
  owned by that package like any BUILD file, so its def bodies are checked
  and it is formatted.
- **The gap.** A file nobody owns goes unchecked: a non-BUILD file (`defs.star`)
  that only other packages load, or a loaded file in a directory with no BUILD
  file. It is still an input of every check that loads it, so editing it
  re-runs them. Finding these files would take a workspace-wide scan.
- **A symlinked file is never owned**, even in the package's own directory.
  `build-format` neither reads nor rewrites it, since a rewrite would land on
  the link's target, which another package may own. Lint discards its
  findings, but its interface still types the calls made to it. Like the gap
  above, it is checked only by the package that owns the link's target, if
  one does.

## Shadowing

There are no reserved names. A target declared with a check's name (in
`target()`, or by a plugin function) shadows the check: `list` emits it once,
`get` returns the user's, and `heph run //...` runs the user's target and
stays green. `heph validate` fails on every such conflict. It names the
address, where the target was declared and the check it shadows, and ends with
the fix: rename the target, or remove it to get the check back.

## Editor and CI differ

With `requireTypeAnnotations` on, the BUILD-file LSP marks each missing
annotation at its parameter, from the same checker. Type errors are **not**
shown in the editor. Only `build-lint-check` reports them.

## Dev builds reuse stale verdicts

heph's version is in both keys. A dev build always reports `v0.0.0-dev`, so a
checker change (or a starlark fork change) in a local dev build can hit an old
local verdict. That entry is local only, and stamped builds are exact. Run with
`--force` after changing the checker.

## `heph.types`

`heph.types.X` names what `target()` accepts for one field, so a wrapper `def`
can type the parameters it passes through instead of falling back to
`**kwargs`:

```python
def lib(name: str, deps: heph.types.Deps = None, labels: heph.types.Labels = None):
    target(name = name, driver = "bash", run = "true", labels = labels, transitive = {"deps": deps})
```

The field aliases are `Deps`, `Env`, `Labels`, `PassEnv`,
`RuntimePassEnv` and `Tools`. The contract is:

- A field alias means what `target()` accepts for that field at this heph
  version, **`None` included**, since every field is optional.
- A field alias widens with its field and never narrows. Once a name ships, it
  is removed only through a deprecation.
- Field aliases are built from `target()`'s own fields only. A driver's
  fields are typed by its [`Kwargs`](#driver-kwargs) instead.
- `transitive` and `approval` have no alias. No Starlark type expresses a dict
  with fixed keys, so an alias would accept keys `target()` refuses.

One more alias is not built from a field: `heph.types.Number` is
`int | float`, the type a float default gives a parameter (see
[Inferred types](#inferred-types)). It does not include `None`, and no
annotation finding suggests it.

`heph.types.Nope` fails before evaluation, naming the known aliases and the
driver namespaces. The editor completes `heph.types.` with the aliases, then
the driver namespaces, and shows each alias's expansion on hover.

### Driver kwargs

`heph.types.<driver>.Kwargs` types a wrapper's `**kwargs` per key, for one
driver. `<driver>` is the name `target(driver = …)` takes, dots included
(`heph.types.bash.Kwargs`, `heph.types.oci.image.Kwargs`):

```python
def lib(name: str, **kwargs: heph.types.bash.Kwargs):
    target(name = name, driver = "bash", **kwargs)

def app():
    lib(name = "a", run = "echo")   # ok
    lib(name = "a", run = 1)        # type-error: `run` expects the field's type
    lib(name = "a", rnu = "echo")   # type-error: unknown key `rnu`
```

`heph.types.<driver>` exists for every driver the engine registers, and only
those. A builtin factory such as `bash` registers its driver only when
`.hephconfig` `plugins:` names it, so the example needs `bash` listed there.

The contract is:

- The keys are `target()`'s own fields except `driver`, plus the driver's
  fields. A driver field named like a `target()` field is `target()`'s.
- `Kwargs` is closed. A key it does not list is a type error.
- Every key is optional and accepts `None`. A wrapper supplies some fields
  itself, and `target()` reports a missing required field at evaluation.
- `driver` is not a key. The wrapper writes it, and the name of the type
  already fixes it.
- A struct-shaped field (`transitive`, `approval`) is
  `dict[str, typing.Any]`, since no Starlark type has fixed keys.
- A float field's key is `int | float`. It accepts an int, as the field does.
- A key is never narrower than its field. Lint may miss what evaluation
  catches, never the reverse.
- Inside the def, `kwargs` is a dict of the keys' types, so forwarding it to
  `target()` typechecks.
- Lint checks every call, and so does evaluation.

The two report differently. Lint reports at the argument and does not name the
key:

```
pkg/BUILD:6:21: type-error: Expected type `None | str | list[str]` but got `int`
pkg/BUILD:7:21: type-error: Unexpected parameter named `rnu`
```

Evaluation fails the call and names the key:
``Unexpected key `rnu` for argument `kwargs`; known keys: …``, or
``Value `1` of type `int` for key `run` does not match the type annotation …``.

`heph.types.nope.Kwargs` fails before evaluation. The error names the drivers
that have one and points at `.hephconfig` `plugins:`, where drivers come from.
`heph.types.bash.Nope` names the members that exist. A driver whose name
cannot be written as an attribute path gets no namespace: one with an empty
segment, a segment named `Kwargs`, or a first segment named like a field alias
(`Deps`, …). Registry names are lowercase, so none is in practice.

`Kwargs` tracks the registered driver. It is not a frozen copy:

- A driver removed, renamed or left out of `plugins:` fails every file that
  names its `Kwargs`, before evaluation. That includes a file that only
  `load()`s a wrapper using it, even if the wrapper is never called.
- A call that passes a removed or narrowed driver field fails at evaluation,
  naming the key. Lint reports it at the argument.
- A driver field added widens the type.
- A `target()` field added later retypes a driver key of the same name: the
  key becomes `target()`'s.

The editor completes `heph.types.<driver…>.` with `Kwargs` and the namespaces
under it. Hovering `Kwargs` lists its keys and their types, and marks each key
the driver's schema requires with `(required by the driver)`. The mark is for
reading only: the key stays optional in the type.

## What changes on upgrade

- **`heph run //...` now rewrites BUILD files and can fail where it used to
  pass.** `//...` includes `build-format`, which rewrites every unformatted
  BUILD file in place. It also includes `build-format-check`, which fails on
  an unformatted file, and `build-lint-check`, which fails on a type error.
- Every workspace with the buildfile plugin gains three targets per package.
  A target already named `build-format`, `build-format-check` or
  `build-lint-check` keeps working and shadows the check. `heph validate`
  fails until it is renamed.
- `label(fix)` and `label(format)` now include `build-format` too.
- `label(lint-check)` and `label(format-check)` now fail on BUILD files with
  type errors or bad formatting that were never checked before.
- A call to an unannotated def is now checked against its inferred return
  type, which can report findings in code that used to pass.
- A parameter with a literal default now has that literal's type: after
  `def f(n = 1)`, `f(n = "s")` is a type error, and so is using `n` as a
  `str` inside `f`. The commonest new findings are `f(1.5)` against `n = 1`,
  `f(None)` against `name = ""`, and a def passing `deps = "//a:b"` to a
  wrapper declared with `deps = []`, which types it `list`. Annotate the
  parameter to widen it (`n: heph.types.Number = 1`, `name: str | None = ""`,
  `deps: heph.types.Deps = []`).
- With `requireTypeAnnotations` on, a parameter whose default is a list, dict
  or tuple display (`srcs = []`, `creds = {}`, `t = ()`) now needs an
  annotation with no `Any` in it, although its default types it. The finding
  says what the default types it as and suggests a fix.
- A plugin named `types` no longer loads. The name is reserved for
  `heph.types`, like `core`.
- `target(labels = None)` is now accepted and means labels left out, so a
  wrapper can pass `labels: heph.types.Labels = None` straight through. It used
  to be a type error.
