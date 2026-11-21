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

## The targets

| Target | Does | Writes | Labels |
|---|---|---|---|
| `//<pkg>:build-lint-check` | Static typecheck of the files the package owns; with `requireTypeAnnotations`, every `def` parameter (`*args`, `**kwargs` included) must have a type. The return type may be left out, and so may the type of a parameter with a literal default (see [Inferred types](#inferred-types)) | nothing | `build-lint-check`, `lint-check` |
| `//<pkg>:build-format-check` | Fails when an owned file differs from the formatter's output, the same rule as `heph tool build-fmt --check` | nothing | `build-format-check`, `format-check` |
| `//<pkg>:build-format` | Rewrites owned files to the formatter's output (`codegen = in_place`) | owned files | `build-format`, `format`, `fix` |

The labels are the Go plugin's, so one label run covers both languages. All
three are cached locally and never remotely: a check costs milliseconds per
package. `//...` includes `build-format`, and so do `label(fix)` and
`label(format)`.

The lint key is the owned files, every file they load (transitively, by
content), `requireTypeAnnotations`, heph's version, the names of the plugin
functions, and the BUILD `patterns`. The format key is the owned files,
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
  `-1` give `int`, `1.5` gives `float`, `"s"` gives `str`, `True` and `False`
  give `bool`. A list, dict or tuple display gives a list, dict or tuple whose
  elements are unchecked: `xs = ["a"]` is an example, not a schema.

These defaults infer nothing, so the parameter still needs an annotation:
`None` (it means "optional something"; write `x: str | None = None`), a name
(`x = DEFAULT`), a call, and an operator (`1 + 1`, `not True`).

## Failure output

There is one line per finding, sorted by file, line and column. Each line
starts with a stable prefix and ends with the fix where there is one. A count
line follows:

```
pkg/BUILD:3:9: annotation: parameter `x` of `make_lib` has no type; add `: <type>`
pkg/defs.BUILD:12:5: type-error: Expected type `str` but got `int`
2 findings in //pkg
annotations are required by requireTypeAnnotations in .hephconfig (plugins: buildfile, options)
```

The last line appears only when there is an `annotation:` finding. When a
parameter is named like a `heph.types` alias (`deps`, `labels`, …), its finding
suggests that alias. `build-format-check` prints `format: not formatted` lines
and ends with `run heph run //pkg:build-format to fix`.

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

The aliases are `Addr`, `Deps`, `Env`, `Labels`, `PassEnv`, `RuntimePassEnv`
and `Tools`. The contract is:

- An alias means what `target()` accepts for that field at this heph version,
  **`None` included**, since every field is optional.
- An alias widens with its field and never narrows. Once a name ships, it is
  removed only through a deprecation.
- Aliases are built from `target()`'s own fields only, never from a driver's
  schema, which would have to enter the lint key.
- `transitive` and `approval` have no alias. No Starlark type expresses a dict
  with fixed keys, so an alias would accept keys `target()` refuses.

`heph.types.Nope` fails before evaluation, naming the known aliases. The editor
completes `heph.types.` and shows each alias's expansion on hover.

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
  `str` inside `f`.
- A plugin named `types` no longer loads. The name is reserved for
  `heph.types`, like `core`.
- `target(labels = None)` is now accepted and means labels left out, so a
  wrapper can pass `labels: heph.types.Labels = None` straight through. It used
  to be a type error.
