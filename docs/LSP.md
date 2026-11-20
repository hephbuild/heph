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
| `diagnostics.rs` | AST checks of `target(` / `provider_state(` keyword arguments |
| `index.rs` | The evaluation side channel (provenance "Targets" hover, undocumented `def` signatures) and the per-document shared state |
| `crates/vendored-starlark-lsp` | `starlark_lsp` 0.14.2's analysis modules, owned by heph; its `NOTICE` lists every divergence |

## One workspace

The server supports a single workspace for now: the first workspace folder the
editor sends in `initialize` (or its `rootUri`), which must lie inside the heph
repo the server was started in. A folder outside it is an `initialize` error;
more than one folder gets a `window/showMessage` warning and only the first is
used. A BUILD file opened outside the repo gets the stock answers only, with
`reason: outside_root`. The driver/provider registry is read once per server
start; until that snapshot lands, heph answers with the stock results and
`reason: providers_loading`.

## Listing packages and targets

An `address` or `load_path` completion asks the engine, through
`LspEngine::list`, for what `heph query` would list: `//pkg` and `load("//pkg`
list the packages under the typed prefix's last whole segment (`//foo/ba` lists
under `//foo` and keeps what starts with `foo/ba`), `//pkg:` and `:` list one
package's targets. Every registered provider answers, with its states and
probes, so `//@heph/go/gocache:modcache` and `//@heph/oci:blobs` complete like
any BUILD target.

- **A fresh root request per completion, so a fresh cache scope.** An editor
  session holds one engine for hours; a scope per call is what makes a package
  created, a target renamed or a BUILD file fixed on disk show on the next
  completion. Nothing about a listing outlives its call: when it returns, the
  scope ends and each provider is told (`scope_ended`). There is no TTL and no
  LSP-side cache of the listing. Within one buffer evaluation the per-buffer
  `loader()` still dedupes the package walk.
- **`Discovery::KeepGoing`.** A provider that fails, or a package that does
  not evaluate, costs only its own entries. The rest still complete, and what
  was left out is logged at `debug` (`lsp: listing left out`) and shown by
  `--explain`.
- **Provenance.** Each entry names the provider it came from: for a package,
  the first provider in registration order that listed it (the merge `heph
  query` walks); for a target, the provider that listed it.

Diagnostics open no scope: they check the document against the driver and
provider schemas with a fresh per-parse loader and never call a provider
through the engine.

A scope per completion was measured before settling on no debounce (release
build, 2000 BUILD packages of 4 targets each,
`crates/e2e/tests/e2e/lsp_listing.rs::scope_per_completion_cost`):

| Listing | Warm median |
|---|---|
| Every package in the workspace (`//`) | 76 ms |
| A subtree's packages (`//d3/e4/`) | 38 ms |
| One package's targets (`//d3/e4/p5:`) | 38 ms |

Completions are not debounced: the costliest listing, the whole workspace,
stays under 100 ms at 2000 packages, and the common case, a subtree or one
package, costs half that. A workspace where `//` grows well past this is the
point to revisit it.

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
| `plugin_namespace` | after `heph.` | plugins and `core` | — | — |
| `plugin_member` | after `heph.<plugin>.`, or on the function name | the plugin's functions | the function's signature and doc | — |
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

To see it for one position without an editor:

```console
$ heph tool build-lsp --explain BUILD:12:20
$ heph tool build-lsp --explain BUILD:12:20 --json
```

Line and column are 1-based, the column counted in characters.

At an `address` or `load_path` site, `--explain` then runs the listing that
site's completion runs, on the engine `heph` builds for the workspace (only
there: classifying any other position needs no workspace), and prints after
the classification:

- `listing`: what was listed (`packages under //foo`, `targets in //foo`) and
  the prefix a candidate must start with;
- `scope`: the id of the cache scope the listing ran in;
- `listed` / `candidates`: how many entries came back, and how many the prefix
  kept;
- one `candidate: "<name>" from <provider>` per entry kept;
- one `skipped:` line per thing the listing left out (`<stage> <provider>
  <scope>: <cause>`);
- one `scope ended: <provider> <outcome>` per provider told the scope ended,
  awaited: `released` (it dropped the scope's caches), `unsupported` (it keeps
  none, or predates scopes), `failed`, `panicked` or `timed_out`.

When the engine cannot be built, or the listing fails, a `listing failed:`
line says why. `--json` puts the same under a `listing` key: `query`,
`prefix`, `scope`, `listed`, `candidates` (`name`, `provider`), `skipped`,
`scope_ended` (`provider`, `outcome`), or `error`.

```console
$ heph tool build-lsp --explain a/BUILD:1:64
a/BUILD:1:64
  site: address
  prefix: "//b:"
  quote: "closed"
  reason: none (heph answers here)
  listing: targets in //b, kept if starting with ""
  scope: 1
  listed: 1, candidates: 1
  candidate: "x" from buildfile
  scope ended: query unsupported
  scope ended: fs unsupported
  scope ended: hostbin unsupported
  scope ended: buildfile released
```

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
