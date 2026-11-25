# JSON output

Every heph command that has a result a script or an agent would read offers
it as JSON, through three flags under the `Output` heading of its `--help`:

| Flag | Does |
|---|---|
| `--json` | prints the result as one JSON document on stdout, and nothing else there |
| `--fields a,b` | picks the fields to print, and so what heph computes (implies `--json`) |
| `--jq EXPR` | filters the document through a jq expression (implies `--json`) |

```
heph query -e '//...' --json
heph query -e '//...' --fields +driver,+deps
heph query -e '//... && label(test)' --jq '.[].addr'
heph inspect def //cmd:bin --jq '.target_def.outputs'
heph auth status --jq 'map(select(.state != "ok")) | length'
heph inspect failure //web:e2e --jq '.log // empty'
```

A command opts in one at a time; a command that has not has none of the
flags, and any of them exits 2 (see "Commands without JSON").

## The document

- A **list-shaped** command (`query`, `session list`, `auth status`, …) prints
  one array, a **record-shaped** one (`version`, `inspect failure`,
  `session show`, …) one object. Which of the two a command prints is part of
  its contract: a list with nothing in it is `[]`, and a record with nothing to
  report is `null` with exit 0 (`inspect failure` of a target that never
  failed).
- **Whitespace.** On a terminal the document is pretty-printed. Piped, a
  record is one compact line, and a list is `[`, then one compact element per
  line, then `]`, so `grep` and `head` work on it line by line. What is printed
  never depends on the terminal otherwise.
- **Order.** A list comes in the order the command's text output uses; heph
  promises no other. Sort with `--jq 'sort_by(.addr)'` when it matters. An
  object's keys come in the order the command declares them, whatever order
  `--fields` named them in.
- **Streaming.** A list is written as it is found, so the first element reaches
  the reader before the walk ends, and a slow reader slows the walk instead of
  growing a buffer. Under `--jq` the document is collected first, since a
  filter may need all of it (`length`, `group_by`), so a list is held in
  memory whole: `--jq` costs memory in proportion to the list.
- **Nothing else on stdout.** Under any of the three flags the progress display
  is off, and warnings, prompts, target output and the session line all go to
  stderr. Errors are a message on stderr and the command's exit code, never a
  JSON error object.

### A list that does not complete is left open

A list whose walk did not finish is printed without its closing `]`, so `jq`
rejects it instead of reading a short list as whole, with or without
`pipefail`:

- an error in the middle of the walk;
- a keep-going walk that left packages it could not evaluate (`query`,
  `inspect labels`, `auth status`, …), the same gaps the text output reports;
- Ctrl-C (the command's own exit code; 130 only for a `--jq` filter it
  stopped).

The exit code is non-zero and stderr says why. Piped, the elements printed
before it are one per line, so a consumer that wants what there was can still
read them line by line.

### A closed pipe stops the command

A reader that stops early (`| head -1`) closes the pipe: heph cancels the
request and exits 0, so the walk stops there instead of running to the end.
Any other error writing stdout (a full disk) fails the command with a message
naming it.

## `--fields`

Every field of a command, with its tier and whether it is printed by default,
is listed in the command's `--help`, and by `--fields help`, which prints the
table on stdout and exits 0:

```
$ heph query --fields help
Fields of `heph query --json` (* = printed by default):

  FIELD    TIER      DESCRIPTION
* addr     free      The target's address.
* package  free      Its package, as `heph inspect packages` prints it (`a/b`; empty at the root).
* name     free      Its name within the package.
* labels   free      The labels the target declares; null under --candidates.
  driver   free      The driver that runs it; null under --candidates.
  deps     resolved  Its declared inputs (`//pkg:name`, with an output group or filters when it names any), in the order the def declares them; null under --candidates.
  hashin   built     Its input hash, the key of its cache entry; null under --candidates.
```

| Value | Selects |
|---|---|
| `a,b` | exactly `a` and `b` |
| `+a,+b` | the default set, plus `a` and `b` |
| `all` | every field, built ones included |
| `help` | nothing: prints the table above and exits 0 |

Bare and `+` names do not mix: `--fields deps,+driver` exits 2 and names both
spellings, `--fields deps,driver` for exactly those fields and
`--fields +deps,+driver` to add them to the default set. `all` and `help`
combine with either. The flag may be given more than once; its values are
pooled under the same rule, so `--fields deps --fields +driver` is refused
too, and a repeated name is one field. An unknown name, an empty
one (`a,,b`, a trailing comma) or a name in the wrong case exits 2 before
anything runs, listing the valid fields (and the nearest one, for a typo).

**A field is computed only when it is selected.** Its tier says what selecting
it costs, beyond what the command does anyway:

| Tier | Costs |
|---|---|
| `free` | nothing: the command has it already |
| `resolved` | fetching a spec or def the command would not otherwise fetch |
| `built` | hashing inputs, so the target's dependencies build |

The default set is only ever `free` fields, so `--json` alone costs what the
text output costs. `heph query --fields hashin` builds what `hashin` needs;
`heph query --json` builds nothing.

`heph query --candidates` lists targets without resolving them, so every field
read from the target (`labels`, `driver`, `deps`, `hashin`) is `null` there,
whatever `--fields` selects.

## `--jq`

`--jq` runs a jq filter ([jaq](https://github.com/01mf02/jaq)'s dialect) over
the document, inside heph: no `jq` needs to be installed.

- Each result goes on its own line: a string printed raw (no quotes, no
  escaping), anything else as compact JSON. This is `gh --jq`'s rule, so
  addresses and paths come out ready for `xargs`. A filter that produces
  nothing prints nothing and exits 0.
- The filter is compiled while the arguments are parsed. A filter that does
  not parse, or names an unknown function, exits 2 before anything runs, with
  the column of the mistake.
- The filter is pure. `input`, `inputs`, `env`, `$ENV`, `halt` and
  `halt_error` are not available, so a filter cannot read stdin or the
  environment, or exit the process; naming one is a compile error. `now` and
  `debug` (to stderr) are.
- A filter that fails while it runs (`error("x")`, `.a` on a string) exits 2,
  on every command; the results printed before the failure stay on stdout.
- Ctrl-C stops any filter, one that never produces included, and exits 130.

`--jq .` prints the same values as `--json | jq -c .`, keys in the same order.

## Exit codes

| Code | When |
|---|---|
| the command's own | the document was printed; a command's exit code is the same with or without the flags (`auth status` is non-zero unless every row is `ok`, `tool codegen … --check` exits 1 on findings and still prints its record) |
| 0 | `--fields help`; a reader closed the pipe; `inspect failure` of a target with no recorded failure, which prints `null` (its text form exits 1, the one command whose code differs) |
| 2 | a bad `--jq` filter, an unknown `--fields` name, or an output flag on a command without JSON, all before anything runs; a `--jq` filter that failed while it ran |
| 130 | Ctrl-C stopped a running `--jq` filter, one that never produces included |

Ctrl-C anywhere else is the command's own cancellation, and keeps the exit
code the command has without the flags (`run` exits 1); a list it was
printing is left without its `]`.

A `--jq` runtime error is 2 even on the codegen commands, where 1 means
"findings", so the two never read alike. It wins over the command's own exit
code: a command that failed and whose filter then failed exits 2.

## Stability

A field that has shipped is never renamed or removed, and never changes type,
tier or whether it is in the default set. A change that would do any of these
adds a new field instead. A consumer ignores fields it does not know, and
treats an enum value it does not know (a `kind`, `state`, `result`) as opaque:
new values come without notice.

No output carries a `schema` or `version` field to check. `heph version
--json` prints `{version, output_contract}`; `output_contract` (1 today)
moves only if the rule above is ever broken.

The rule is checked by `src/commands/json_fields.snap`, and the test over it
only lets lines be added. It pins one line per JSON path of every command:
each field, and each key of a type nested in one, four levels deep
(`sources[].addr`), with its JSON kind (`string`, `number`, `bool`, `any`,
`object`, `K?` for nullable, `[K]`, `{K}` for a map), tier and default
membership; each value of a string enum is a line of its own. Its
`# contract N` header must equal `output_contract`, and only a bump of that
starts a new snapshot. The keys and kinds of a nested type come from the same
serde attributes that write it (`derive(output::Shape)`), except for a type
with a hand-written `Serialize`: its shape is written by hand next to the test,
and nothing checks it against the impl.

Two kinds of output are outside it:

- **`heph inspect spec` and `heph inspect def`** print JSON by default; their
  shape follows the engine's types and is not stable, a debugging view. They
  take `--json` (which also keeps the progress display off) and `--jq`, and
  have no `--fields`.
- **`heph tool config-schema`** prints a JSON Schema by default, stable as
  `docs/CONFIG_SCHEMA.md` describes. It takes `--json` and `--jq`, no
  `--fields`.

All three are compact when piped, like everything else.

Human text inside a field (`session list`'s `now`, a `reason`, a `fix`) may
change wording; the stable signal is the field next to it that a program is
meant to branch on, which each command's doc names.

## Commands

| Shape | Commands |
|---|---|
| list | `query`; `inspect packages`, `labels`, `hashout`, `deps`, `revdeps`, `path`, `states`, `functions`; `auth status`; `session list`; `tool plugins`, `tool cache measure-latency`, `tool scratch ls`, `push`, `pull` |
| record | `version`, `validate`; `inspect hashin`, `outputs`, `failure` (`null` when none); `auth explain`, `auth logout`; `session show`, `session inflight`; `tool gc`, `clean`, `build-fmt`, `resolve-plugins`, `telemetry status`, `coreutils list`, `coreutils which`, `scratch head`, `path`, `rm`; every `tool codegen` command; `tool build-lsp --explain` |
| always JSON, unstable | `inspect spec`, `inspect def` |
| always JSON | `tool config-schema` |

The fields of each are in its `--help` and `--fields help`. Some docs say more
about one command's document: `docs/CODEGEN.md`, `docs/SESSIONS.md`,
`docs/CREDENTIALS.md`, `docs/FAILURE_OUTPUTS.md`, `docs/TELEMETRY.md`,
`docs/LSP.md`.

### Commands without JSON

These have no output flags; `--json`, `--fields` or `--jq` on them exits 2.

| Command | Why | Instead |
|---|---|---|
| `heph run` | not yet | `heph inspect failure --json <addr>` after a failed run |
| `heph log` | its output is the log's bytes | `heph inspect failure --json` for the log's path |
| `heph session attach` | a terminal UI | `heph session show --json` |
| `heph inspect deps -i` | a terminal UI | `heph inspect deps --json` |
| `heph auth login` | interactive: it runs sign-in commands that talk to the user | `heph auth status --json` for what needs signing in to |
| `heph tool completions` | its output is a shell script | |
| `heph tool coreutils run` | its output is the applet's; after the applet's name every argument is the applet's | |
| `heph tool build-lsp` (server) | it speaks JSON-RPC on stdio | `--explain … --json` |

Hidden commands without JSON: `gen-docs` (markdown for the docs site),
`__auth-helper` (its stdout is a credential-helper protocol), and the removed
`tool gen-gitignore`, which prints its removal message on stderr and exits 2
whatever it is given, `--json` included.

The list is held by a test that walks every command, hidden ones included:
a new command either opts in or is added to it.
