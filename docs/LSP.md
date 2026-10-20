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

## Site kinds

| Site kind | Detected from | Completion | Hover | Definition |
|---|---|---|---|---|
| `callee` | `target` / `provider_state` identifier before its `(` | stock passes through | the builtin's signature, narrowed to the selected driver / provider | — |
| `driver_value` | `driver =` inside `target(`, with a closed string, an unterminated one, or no string yet | driver names | the driver's fields | — |
| `provider_value` | `provider =` inside `provider_state(` | provider names | the provider's state fields | — |
| `target_key` | a keyword-name position inside `target(` | base fields + the selected driver's, minus keys present | the field's type and doc | — |
| `state_key` | a keyword-name position inside `provider_state(` | `provider` + the selected provider's schema, minus keys present | the field's type and doc | — |
| `plugin_namespace` | after `heph.` | plugins and `core` | — | — |
| `plugin_member` | after `heph.<plugin>.`, or on the function name | the plugin's functions | the function's signature and doc | — |
| `address` | a string starting `//` or `:`, inside a call or not | packages, then targets, both forms | — | the target's line, or the package's BUILD file |
| `load_path` | the first argument of `load(` | package paths | — | the loaded file |
| `load_symbol` | a later string argument of `load(` | the loaded file's exported `def`s | — | the `def` line, from the loaded file's AST |
| `none` | anything else | stock | stock, plus the evaluation's "Targets" block | stock, plus a loaded name's `def` across the package's files |

`—` means heph answers with nothing there (an empty list, a null hover), not
the stock answer.

## The merge policy

Wherever heph controls or knows the answer for sure, heph is authoritative and
**replaces** the stock answer, for completion, hover and definition alike. This
covers keyword names, driver and provider names, plugin namespaces and members,
addresses, and load paths and symbols. The stock answer **passes through**
only where heph cannot know: free expressions, locals, other value positions.
There is no append policy. No site mixes heph and stock items.

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
  landed; the stock answer stands in).
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
`tests/lint_gate.rs`.
