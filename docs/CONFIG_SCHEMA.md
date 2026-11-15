# The `.hephconfig` schema

`heph tool config-schema` prints the JSON Schema (draft 2020-12) of
`.hephconfig`, its legacy name `.hephconfig2`, and every profile laid over
them (`ci.hephconfig`). An editor uses it, through yaml-language-server, for
completion, hover and validation.

```sh
heph tool config-schema > .heph/hephconfig.schema.json
```

The schema comes from the binary, not from a website. Run in a workspace that
pins a version, the self-upgrade re-executes into that version first, so the
schema is the pinned version's and matches the parser that will read the file.
The command reads no config and starts no engine, so it also runs outside a
workspace and next to a config that does not load.

`docs/hephconfig.schema.json` is a copy of the current schema, committed so
that every schema change shows up in review. A test in `crates/config` fails
when it is stale; `UPDATE_GOLDEN=1 cargo test -p config golden` rewrites it.

## Editors

**Neovim (yamlls).** Write the schema to a file, then map it to the config
names in `yaml.schemas`:

```lua
require("lspconfig").yamlls.setup({
  settings = {
    yaml = {
      schemas = {
        ["/path/to/repo/.heph/hephconfig.schema.json"] = {
          ".hephconfig", "*.hephconfig", ".hephconfig2", "*.hephconfig2",
        },
      },
    },
  },
})
```

The editor also has to treat those files as YAML (`vim.filetype.add` with a
`pattern` for `%.hephconfig2?$`).

**One file.** A modeline at the top of the file works in any editor that runs
yaml-language-server:

```yaml
# yaml-language-server: $schema=.heph/hephconfig.schema.json
```

**VS Code.** The heph extension will register the schema with the YAML
extension. Until then, set `yaml.schemas` in the settings the same way as
for Neovim.

## What the schema does not check

The parser at load time stays the authority. The schema never rejects a
config that loads (`crates/config/src/schema.rs` tests this against every
config in `example/` and a table of edge cases), but it lets through some that
do not:

- **Value limits a pattern cannot express.** A resource amount past
  `u64::MAX`, or a `cpu` fraction below 0.000001. The patterns otherwise match
  the parser's grammar for counts, CPUs, sizes and percentages exactly.
- **Plugin `options`.** Each plugin checks its own keys; the schema only says
  `options` is a map.
- **Profile merges.** The schema sees one file. A profile that sets a cache's
  `read` without a `uri`, or a custom pool's value without its unit, is valid
  on its own; whether the merged result is complete is checked when the files
  are loaded together.
