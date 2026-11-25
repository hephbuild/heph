# The BUILD-file language server

`heph tool build-lsp` serves BUILD files over LSP on stdio. Editors launch it
directly. The code lives in `crates/plugin-buildfile/src/pluginbuildfile/lsp/`.

Every heph-specific answer (what to complete, what to show on hover, where a
name is defined, what to flag) comes from **one classification of the cursor's
site**. That classification is computed from the document's tokens, and from
two name sets of its last good parse. It never depends on whether the file
evaluated, and it answers while the document is broken, which is most of the
time someone is typing.

```
didChange ──> lex.rs ──> token array (once per edit)
                │
                └──> parse ──> diagnostics.rs (AST checks)
                           └──> overlay: rebound names, loaded names

request ──> position.rs (UTF-16/8/32 → byte, once)
        ──> site.rs: resolve(tokens, overlay, offset) → Resolution { site, reason, degraded }
        ──> handlers.rs: items / hover / definition for the site kind
        ──> merge policy: heph replaces the stock answer, or the stock answer passes through
```

| File | What it owns |
|---|---|
| `server.rs` | The loop on `lsp-server`: `initialize` (capabilities, position-encoding negotiation), open/change/close, routing, formatting, shutdown/exit |
| `lex.rs` | The token array: kind, span and bracket link per token, lexer errors as tokens |
| `site.rs` | The resolver: `Site`, `Resolution`, the bracket stack, call extents, the AST overlay |
| `handlers.rs` | The site-kind table and the merge policy; the renderers and item builders |
| `position.rs` | Position encoding, converted at the request boundary and nowhere else |
| `context.rs` | The vendored analysis's `LspContext`: load resolution, listing, evaluation for the index |
| `diagnostics.rs` | AST checks of `target(` / `provider_state(` keyword arguments; with `requireTypeAnnotations` on, each `def` parameter missing a type, from the checker `build-lint-check` runs (type errors are not shown; see `docs/BUILDFILE_CHECKS.md`) |
| `index.rs` | The evaluation side channel (provenance "Targets" hover, undocumented `def` signatures) and the per-document shared state |
| `typed.rs` | Typed members: a document's bindings typed by lint's checker, looked up by def path and name |
| `memo.rs` | The loaded files' interfaces, memoized under a chained key and evicted when no open document reaches them |
| `crates/vendored-starlark-lsp` | `starlark_lsp` 0.14.2's analysis modules, owned by heph; its `NOTICE` lists every divergence |

## One workspace

The server supports a single workspace for now: the first workspace folder the
editor sends in `initialize` (or its `rootUri`), which must lie inside the heph
repo the server was started in. A folder outside it is an `initialize` error;
more than one folder gets a `window/showMessage` warning and only the first is
used. A BUILD file opened outside the repo gets the stock answers only, with
`reason: outside_root`. The driver/provider registry is read once per server
start; until that snapshot lands, heph answers with the stock results and
`reason: providers_loading`. A document evaluated before it lands lacks the
drivers' `heph.types.<driver>.Kwargs`, so when it lands every open document is
evaluated again and its diagnostics are republished, with no edit needed.

## Site kinds

| Site kind | Detected from | Completion | Hover | Definition |
|---|---|---|---|---|
| `callee` | `target` / `provider_state` identifier before its `(` | stock passes through | the builtin's signature, narrowed to the selected driver / provider | — |
| `driver_value` | `driver =` inside `target(`, with a closed string, an unterminated one, or no string yet | driver names | the driver's fields | — |
| `provider_value` | `provider =` inside `provider_state(` | provider names | the provider's state fields | — |
| `field_value` | a string value of any other keyword directly inside `target(` / `provider_state(` (`workdir = "tr│"`), unless address-shaped | the field's string literals, when its type has any (`"caller" \| "tree_root" \| …`); otherwise stock | stock | stock |
| `target_key` | a keyword-name position inside `target(` | base fields + the selected driver's, minus keys present | the field's type and doc | — |
| `state_key` | a keyword-name position inside `provider_state(` | `provider` + the selected provider's schema, minus keys present | the field's type and doc | — |
| `arg_key` | a keyword-name position inside a call to any other plain-named function (`my_rule(`, `glob(`; not `x.f(`, not a `def`'s parameters) | the callee's unwritten parameters, each inserting `name = `, ranked first; then stock (see below) | stock | stock |
| `plugin_namespace` | after `heph.` | plugins, `core` and `types` | — | — |
| `plugin_member` | after `heph.<plugin>.`, or on the function name | the plugin's functions; under `heph.types.`, every type alias, then the driver namespaces; under `heph.types.<driver…>.`, `Kwargs` where the path names a driver, then the namespaces under it | the function's signature and doc; for an alias, its expansion; for `Kwargs`, its keys and their types, each key the driver requires marked `(required by the driver)` | — |
| `member` | any other position after a `.` (`typing.│`, `l.na│`, `f().│`), whitespace and newlines between the tokens included | where the receiver is a chain of names whose root is a global the file does not bind (`typing`, `heph.core`), that namespace's members; where it is a name the file binds, or a literal, the members the checker accepts on its type (see [Typed members](#typed-members)); otherwise nothing | a namespace's member: its doc, or nothing for a name it does not have; any other receiver: stock | stock |
| `address` | a string starting `//` or `:`, inside a call or not | packages, then targets, both forms | — | the call that declared the target (see below), or the package's BUILD file |
| `load_path` | the first argument of `load(` | package paths | — | the loaded file |
| `load_symbol` | a later string argument of `load(` | the loaded file's exported `def`s | — | the `def` line, from the loaded file's AST |
| `none` | anything else | stock | stock, plus the evaluation's "Targets" block | stock, plus a loaded name's `def` across the package's files |

`—` means heph answers with nothing there (an empty list, a null hover), not
the stock answer.

An address's definition evaluates the package's BUILD files (the open
document's text wins over the file on disk) and lands on the outermost call
in that file that declared the target. That is `tool("go")` for a target a
helper declares, including one loaded from another file or one whose name
the helper computes, and the `target(` call for a direct one. Evaluation
only enriches the answer. In a package that does not evaluate, the
definition falls back to the line of the target's `name = "…"`, read from the
tokens, and then to the BUILD file.

## The merge policy

Wherever heph controls or knows the answer for sure, heph is authoritative and
**replaces** the stock answer, for completion, hover and definition alike. This
covers keyword names, driver and provider names, plugin namespaces and members,
addresses, and load paths and symbols. The stock answer **passes through**
only where heph cannot know: free expressions, locals, other value positions.

A `member` completion always replaces, even where heph knows nothing of the
receiver: the stock analysis never completes attributes. After a `.` it would
offer the global list, which is never valid there, so an empty list is the
correct answer. A namespace's members come from the globals' documentation,
the same globals evaluation uses, so they need no registry and answer before
it lands. Whether a root is the global is read from the overlay: a file that
binds `typing` itself (an assignment, a parameter, a `load`) gets no member
list for it. That holds for `heph` too: a file that binds `heph` gets an
empty `member` site after `heph.`, not `plugin_namespace`.

A member position is any position whose previous significant token is a `.`:
a `.` only ever means attribute access (a float is one number token), so
`typing. A│`, `typing.\n    A│` and `typing\n  .│` all have the receiver
`typing`, and the chain walks left over name-dot pairs whatever lies between
them (`a.\n typing.│` is `a.typing`). The receiver has one of these shapes,
which `--explain` reports as `receiver`:

- `names`: a chain of names whose root the file does not bind. `path` is the
  chain. A path that names no namespace (`typing.Any.│`, a function's member)
  completes nothing, and `path` shows which.
- `rebound`: a chain of names whose root the file binds; `rebound` names it.
  It lists its type's members ([Typed members](#typed-members)).
- `literal`: a string, list or dict literal (`"-".│`, `[].│`, `{}.│`); `type`
  names which. It lists that type's members.
- `expression`: anything else, such as a call or an index (`f().│`,
  `l[0].│`). It completes nothing.

`receiver` is an open set: a consumer tolerates a value it does not know.

A type position (`def f(x: │`) completes as an expression does. The stock
analysis offered only `str`, `int`, `bool`, `None` and `float` there, and a
Starlark type is any expression: `list[str]`, `typing.Any`, a declared
`record`. This is a divergence of the vendored crate, recorded in its
`NOTICE`.

`field_value` merges **by field**: the field's type decides, not the site. The
field's string literals (`ParamType::Literal`, alone or in a union) matching the prefix **replace** the stock items (which
offer nothing inside a string); a field with no such literal passes the stock
answer through. It is one or the other, never both. A literal that cannot sit
verbatim between quotes (a quote, a backslash, a control character) is not
offered, and neither is `""` (a parser may accept it as "unset"), which would
show as a blank item. Only a string is a `field_value`: before one, `workdir = │`
is where a variable or an expression may start, and an unclosed `target(`
above would make that position depend on the text after the cursor.

One site **prepends**: `arg_key`. A keyword-name position is also where a
positional argument starts, so the stock items (locals, globals) stay, after
the callee's parameters. heph reads the parameters without the document
having to parse: a local `def` from the tokens; a loaded name through its
`load(…)` (from the tokens) to the `def` in the loaded file, read from disk;
otherwise a documented global's keyword-usable parameters (not positional-only
ones). Where heph finds the function, its list replaces the stock parameter
items and every stock item gets `sortText` `1_<label>`; where it does not
(an unknown name, a `def` whose parameter list is unclosed), the stock answer
passes through untouched. heph's own sites keep precedence: `target(` and
`provider_state(` are never `arg_key`.

Every heph completion item carries:

- a `textEdit` over the site's token. For a closed string that is its content.
  For an address it is the whole `//pkg:name`, so editors that break words at
  `/` and `:` behave alike. For an unterminated string it runs from the
  opening quote to the end of the run of characters valid for the site, so a
  `)` or `,` the string swallowed, and a trailing `\r`, are never replaced.
  For a value with no string yet, the edit inserts the quotes.
- a `filterText` matching what the editor compares against the edit range;
- one `kind` per site kind;
- Markdown `documentation`;
- `sortText` `0_<name>`, so heph items lead in a deterministic order.

heph filters by prefix and dedupes by label before the list leaves the
server. The response is always an array; `isIncomplete` is never set.

A completion triggered by `:` or `/` on a non-site returns an empty list at
once, without the handlers or the engine. `:` completes only in an
address-shaped string (`:…`, `//pkg:`, or a package path with a `/` followed
by `:`), so a dict-literal colon or `"a:b"` prose stays quiet. `/` completes
only inside a string.

## Typed members

A `member` site whose receiver is a name the file binds (`l.│`) or a literal
(`"-".│`) lists the members lint's checker accepts on the receiver's type:
exactly the names `n` for which `build-lint-check` accepts `receiver.n`, each
with the type the checker gives it as the item's `detail`. There is no second
method list to drift from the checker.

- A declared `record` lists its fields; a declared `enum`, and a value of
  one, what the checker accepts on each. `str`, `list` and `dict` list their
  methods. A chain folds over its segments:
  `l.mode.│` lists the members of the type of `l`'s `mode` field.
- A union lists every alternative's members, because the checker accepts an
  attribute that any alternative has. So for `T | None`, a listed `.x` passes
  lint and can still fail at run time when the value is `None`.
- An open type lists nothing: `Any`, a callable, an iterator, a type, a
  namespace, a struct, a record whose fields are unknown. A driver's `Kwargs`
  lists the dict methods the checker accepts on it.
- A name that a comprehension or a `lambda` binds in the cursor's bracket
  group, or on its line, lists nothing. The checker reports such a binding
  under the enclosing def, so the answer could be another binding's.

A receiver is looked up by the cursor's def path and its name, never by
position, so a type found is never a different binding's. A name bound twice
in one def, or one the checker cannot resolve, lists nothing, and
`empty_because` says why.

### Literal receivers

The receiver is a literal when the token before the `.` is a string (or an
implicitly concatenated run of them), a `]` closing a list display, or a `}`.
It types as `str`, `list[Any]` or `dict[Any, Any]`: the element types are
unknown. A `]` closing an index (`xs[0].│`, `"ab"[0].│`) or a parenthesized
group (`([1]).│`) is not a literal, nor is a call on one (`"a".join(xs).│`).
Which `[` a `]` closes is found by a balanced backward walk over the lexer's
bracket links; the `[` opens a display when the token before it does not end
an expression (a name, a literal, `)`, `]` or `}`). At bracket depth 0, a line
break does not end an expression, so `[1, 2].│` at the start of a line is a
display. A literal needs no parse and no registry, so it answers in a broken
document and at startup.

### Where the types come from

The types are lint's, so completion and `build-lint-check` agree:

- The same checker and the same globals: lint's (`check_globals`), not the
  globals the server evaluates with. A plugin function is opaque to the
  checker (`Any`), so its return type never reaches completion.
- The same loads: each `load()` string resolves through evaluation's
  resolver, and a loaded file's interface is lint's. A directory load is
  every BUILD file in it. The `//pkg:file.star` fallback that evaluation
  rejects is not typed.
- Loaded files are read from disk, as lint reads them. An unsaved edit in
  another buffer is not seen until it is saved.
- Before the registry lands, the checker runs with the evaluation surface
  alone (`heph_globals`): a record still types, but a parameter annotated
  with a driver's `Kwargs` is `Any` and lists nothing until the registry
  lands. A registry that fails to load leaves it that way. `registry` says
  which globals answered.

### Lifetimes

- The typecheck is lazy. It runs only at a member site whose receiver the
  file binds, on the document's last good parse, and its result is kept for
  that text and the registry generation. Other keystrokes pay nothing. Only
  the source and the bindings' types are kept, never the syntax tree.
- Typing `l.` breaks the parse, so the answer comes from the last good parse
  and says `stale: true`. A name renamed in the same document is seen at the
  next good parse.
- Each loaded file's interface is kept across requests and documents, under a
  chained key: the digest of its content plus the keys of the files its loads
  read. The digest is checked with a stat, and the file is read again only
  when the stat changes; it is typechecked again only when its key changes.
  Every member lookup walks the document's loads again and compares their
  keys with the ones its types were checked against. So an edit on disk to a
  file loaded two hops away is seen at the next member completion, with no
  edit to the document.
- An interface is evicted once no open document's loads reach it. That check
  runs on `didClose` and after every typecheck, since a document that changes
  its loads leaves the old ones behind. A document's types are dropped on
  `didClose`, so reopening it starts clean.

## Broken documents

The site kind, the enclosing call, its keyword and the string prefix are
computed from the tokens up to the cursor and the token under it. An error
after the cursor never changes the answer. An error before it costs only the
bytes of its span:

- The lexing loop drops the tokens the lexer re-emits inside an error span
  (the high-water mark). It restarts on the next line when an error makes no
  progress or an f-string has no end on its line, and it stops at `2n + 8`
  items. An error never leaks past its line unless the construct itself runs
  on: an open `"""` swallows the rest of the file.
- An unterminated string reads as a string whose content runs from the quote
  to the span's end, so `driver = "ex│` classifies exactly as `driver = "ex│"`.
- An unclosed call extends to the first later line whose first non-blank
  byte sits at an indent no deeper than the opener's line and is not a
  closer, skipping blank lines and lines that begin inside a string or
  comment, or to EOF. At top level that is the next column-0 line. Present
  keys and the selected driver are read from that extent for every feature.
- Shadowing and loaded names come from the last good parse. They are name
  sets, never positions, so they can be stale but never misplaced.

Every parse and lexer error is published as a diagnostic on each `didChange`,
alongside the completions. Only the first error on a line is published, its
root cause: the string re-opened after a bad escape is a consequence.

## Why did it do that?

Every answer carries a `Resolution`, logged at `debug` with the method, site,
reason, degradations, item count and time:

- `reason` says why a heph answer is empty: `callee_rebound` (the file rebinds
  `target`), `in_lex_error` (with the root cause's kind, span, line and
  message), `not_a_site`, `providers_loading` (the registry snapshot has not
  landed; the stock answer stands in), `outside_root` (the document lies
  outside the repo the server is bound to).
- `degraded` says what a present answer could not do in full:
  `call_extent_guessed` (with the line the extent stopped at),
  `edit_to_word_end`, `ast_stale` (with the versions behind), `ast_absent`,
  `lex_error_before` (with its span).
- A `member` site carries `path` (the receiver's chain of names, or `null`),
  `receiver` (`names`, `rebound` with the bound name in `rebound`, `literal`,
  or `expression`) and `scope` (the cursor's def path, outermost first),
  which together say why its list is empty.
- A typed member site ([Typed members](#typed-members)) adds:
  - `binding`: how the receiver's root is bound: `param`, `local`,
    `comprehension`, `lambda`, `module`, `loaded` or `none`;
  - `type`: the receiver's type as the checker prints it, or `null`;
  - `source`: where the root came from: the declaration as `path:line`, or
    the `load()` string;
  - `approximation`: the checker's reason for typing a declaration `Any`
    (`conditional_binding`, `rebound`, `in_def`, …), or `null`;
  - `stale`: the types are from an earlier text than the current one;
  - `registry`: `ready`, or `pending` when the types were checked without the
    registry;
  - `empty_because`: why the list is empty, or `null`: `any`, `open_type`,
    `unknown_binding`, `ambiguous_binding`, `no_members`, `no_types_yet`
    (the document has never parsed), `approximation`.

  A literal receiver carries `type`, `registry` and `empty_because` only.
  `receiver`, `binding` and `empty_because` are open sets: a consumer
  tolerates a value it does not know.

To see it for one position without an editor:

```console
$ heph tool build-lsp --explain BUILD:12:20
$ heph tool build-lsp --explain BUILD:12:20 --json
```

Line and column are 1-based, the column counted in characters.

## Adding a site kind

1. Add a `Site` variant in `site.rs` and detect it in `classify`. Read only
   tokens at or before the cursor, plus the token under it. A rule that looks
   past the cursor breaks the broken-document guarantee.
2. Give it a row in `handlers.rs`: its `merge_for` policy, its `item_kind`, and
   its arms in `items`, `hover` and `definition`. Reuse the conventions in
   `items`; a site does not build its own `CompletionItem`s.
3. Add its editor-shaped test in `crates/plugin-buildfile/tests/lsp.rs`, and
   its three `matrix_<site>_<feature>` cells. A site kind without an
   end-to-end test does not exist.
4. Add a fixture for it to `tests/fixtures/lsp_fixtures.rs`. The sweeps then
   check that its answer survives the four broken variants, that its edits
   stay inside its token, and that every prefix of it lexes within the cap.

## The vendored analysis

`crates/vendored-starlark-lsp` is `starlark_lsp` 0.14.2's analysis code, copied
under Apache-2.0 and owned. Bugs are fixed in place, and each divergence is
listed in its `NOTICE` with the reason. There is no verbatim refresh at
starlark releases. An upstream fix is ported by hand when it matters to BUILD
files. The crate keeps upstream's style under its own `[lints]`, exempted in
`tests/root/lint_gate.rs`.
