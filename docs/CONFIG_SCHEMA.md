# The `.hephconfig` schema

`heph tool config-schema` prints the JSON Schema (draft 2020-12) of
`.hephconfig`, its legacy name `.hephconfig2`, and every profile laid over
them (`ci.hephconfig`). An editor uses it, through yaml-language-server, for
completion, hover and validation.

```sh
heph tool config-schema > tools/hephconfig.schema.json
```

The schema comes from the binary, not from a website. Run in a workspace that
pins a version, the self-upgrade re-executes into that version first, so the
schema is the pinned version's and matches the parser that will read the file.
The command reads no config and starts no engine, so it also runs outside a
workspace and next to a config with keys this binary doesn't know.

`docs/hephconfig.schema.json` is a copy of the current schema, committed so
that every schema change shows up in review. A test in `crates/config` fails
when it is stale; `UPDATE_GOLDEN=1 cargo test -p config golden` rewrites it.

## Stability

What a consumer may rely on:

- **Stable.** The command exists, exits 0, and prints exactly one JSON
  document on stdout. Its `$schema` is draft 2020-12 today; a change of draft
  would be documented here.
- **Stable in meaning.** The document validates the config of the binary that
  printed it. The parser stays the authority on what loads.
- **Not stable.** Key order, the names under `$defs`, which definitions are
  inlined, titles and descriptions, and how alternatives are structured
  (`anyOf`, `oneOf`, `if`/`then`). Treat the document as opaque: hand it to a
  validator whole, and do not `$ref` into it from another schema.

## Getting the schema from a tool

An editor plugin or script that runs the command should:

- Use the output only if the command exits 0 **and** stdout parses as JSON
  with a `$schema` key. Otherwise fall back, to a bundled copy or to no schema,
  and do not retry.
- Run it with a timeout. In a workspace that pins a version the binary may
  download that version before the command runs.

It fails in these cases, all of which the fallback covers:

- A workspace pinned to a heph older than this command: clap rejects the
  unrecognized subcommand, exit 2.
- A pinned workspace while offline, when the pinned binary is not installed
  yet: the download fails.
- A `.hephconfig` that is not valid YAML at all: the self-upgrade fails to read
  the pin before the command runs (the same holds for `heph version`).

## Editors

Write the schema to a file the editor can read, and regenerate it when the
pin moves. Either a tracked path in the repository (for example
`tools/hephconfig.schema.json`, so the whole team gets it), or a per-user path
outside the repository. Not `.heph/`: that is the heph home, gitignored, and
holds the cache, locks and sandboxes.

Prefer the command to a URL. A URL on `master` describes `master`'s config,
not the one a pinned workspace runs. If you do use a URL, pin it to a tag
(`https://raw.githubusercontent.com/hephbuild/heph/<tag>/docs/hephconfig.schema.json`),
and treat it as best-effort: a release only has one when its source was
tagged.

**Neovim (0.11+, yamlls).** Treat the config files as YAML, then map the
schema to them:

```lua
vim.filetype.add({
  filename = {
    [".hephconfig"] = "yaml",
    [".hephconfig2"] = "yaml",
  },
  pattern = {
    [".*%.hephconfig2?$"] = "yaml",
  },
})

vim.lsp.config("yamlls", {
  settings = {
    yaml = {
      schemas = {
        ["/path/to/repo/tools/hephconfig.schema.json"] = {
          "**/.hephconfig",
          "**/*.hephconfig",
          "**/.hephconfig2",
          "**/*.hephconfig2",
        },
      },
    },
  },
})
vim.lsp.enable("yamlls")
```

The `**/` globs match the base file and every profile, in any directory.

**One file.** A modeline at the top of the file works in any editor that runs
yaml-language-server:

```yaml
# yaml-language-server: $schema=tools/hephconfig.schema.json
```

**VS Code.** The heph extension does not register the schema yet. Until it
does, map the file in the YAML extension's `yaml.schemas` setting, with the
same globs as for Neovim.

## The contract

The schema never rejects a config the parser loads, with one named exemption.
It may be looser than the parser.

**The exemption: numbers in string fields.** serde_yaml reads a plain number
or boolean into a field that holds text: `version: 1`, `homeDir: 5`,
`skip: [2024]`, `scope: true` all load. The schema keeps `type: string` on
those fields anyway, because a number where heph expects text is almost always
a mistake: `version: 1.10` loads as `1.1`. Quote the value (`version: "1.10"`)
and both agree.

`crates/config/src/schema.rs` tests the contract: every config in `example/`
and a table of edge cases, and, at every position the schema describes, each
kind of YAML value (a number, a string, a boolean, `~`, an empty value, a
sequence, a mapping) against the parser. The exemption is the only
disagreement those tests allow, and a table of its cases keeps it in view.

Those tests model the editor through serde_yaml's reading of scalars, not
YAML 1.2 core's, which yaml-language-server uses. For ordinary values the two
agree. An exotic scalar (`0b101`, `0o17`, an `!fs`-style tag) may read as a
different type in the editor than in the tests.

## What the schema does not check

Where the schema is looser than the parser:

- **Value limits a pattern cannot express.** A resource amount past
  `u64::MAX`, or a `cpu` fraction below 0.000001. The patterns otherwise match
  the parser's grammar for counts, CPUs, sizes and percentages exactly.
- **`~` where an empty value loads.** `resources:` with nothing after it loads
  as no pools, and so do `plugins:`, `caches:` and the like; JSON has one
  `null` for both that and `~`, so the schema accepts `resources: ~`, which
  the parser refuses.
- **Plugin `options`.** Each plugin checks its own keys; the schema only says
  `options` is a map.
- **Profile merges.** The schema sees one file. A profile that sets a cache's
  `read` without a `uri`, or a custom pool's value without its unit, is valid
  on its own; whether the merged result is complete is checked when the files
  are loaded together.
