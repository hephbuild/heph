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
  report is `null` with exit 0 (`inspect failure` of a target with no
  recorded failure: one that never failed, or one skipped because a
  dependency failed, since only a culprit keeps a record).
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
- a `run` that `--fail-fast` stopped before every matched target ended;
- a cancel: Ctrl-C, or SIGTERM or SIGHUP on a command that a cancel ends
  (`run` and every command that can build under the progress view). A
  cancelled command exits 130, as a `--jq` filter it stopped does.
  `tool build-lsp` and `log -f` are not cancelled by TERM or HUP: they die of
  it. A HUP that heph was started with ignored (`nohup heph run …`) stays
  ignored and cancels nothing.

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
| 2 | a bad `--jq` filter, an unknown `--fields` name, or an output flag on a command without JSON (or on a `run` of an entrypoint), all before anything runs; a `--jq` filter that failed while it ran |
| 130 | Ctrl-C stopped a running `--jq` filter, one that never produces included |

Ctrl-C anywhere else (and SIGTERM or SIGHUP, where they cancel) is the
command's own cancellation, and keeps the exit code the command has without
the flags: every command that cancels exits 130, where contract 1 exited 1.
A list it was printing is left without its `]`.

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
--json` prints `{version, output_contract}`; `output_contract` (2 today)
moves only if the rule above is ever broken. Contract 2 broke it for
`run` (see "Migrating from contract 1" under `heph run`) and made
`inspect failure`'s `addr`, `hashin` and `failed_at` nullable.

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
| list | `query`; `run` (below); `inspect packages`, `labels`, `hashout`, `deps`, `revdeps`, `path`, `states`, `functions`; `auth status`; `session list`; `tool plugins`, `tool cache measure-latency`, `tool scratch ls`, `push`, `pull` |
| record | `version`, `validate`; `inspect hashin`, `outputs`, `failure` (`null` when none); `auth explain`, `auth logout`; `session show`, `session inflight`; `tool gc`, `clean`, `build-fmt`, `resolve-plugins`, `telemetry status`, `coreutils list`, `coreutils which`, `scratch head`, `path`, `rm`; every `tool codegen` command; `tool build-lsp --explain` |
| always JSON, unstable | `inspect spec`, `inspect def` |
| always JSON | `tool config-schema` |

The fields of each are in its `--help` and `--fields help`. Some docs say more
about one command's document: `docs/CODEGEN.md`, `docs/SESSIONS.md`,
`docs/CREDENTIALS.md`, `docs/FAILURE_OUTPUTS.md`, `docs/TELEMETRY.md`,
`docs/LSP.md`.

### `heph run`

`heph run --json` prints one entry per matched target, written as each one
finishes (completion order, not address order), and one per **culprit**: a
target or BUILD file whose own failure stopped something the selection asked
for. A culprit the selector did not match is still written, with
`selected: false`, and always before any record naming it as `root` or
`root_file`. Any other
dependency the selector did not match gets no entry.

A selected target can get a second record: when it ended `ran` or `cached`
and is then blamed as a culprit (a target depending on it failed on something
it declared), its `failed` record follows and supersedes the first. A reader
that keys records by `addr` keeps the last one.

Each record is output contract 2 (`heph version --json` reports
`output_contract: 2`).

| Field | Default | Meaning |
|---|---|---|
| `addr` | yes | the target's address; null for a BUILD file that did not evaluate, whose path is in `file`. Read `.addr // .file` for a name every record has |
| `file` | yes | the workspace-relative BUILD or `.bzl` file that did not evaluate; null on every other record |
| `outcome` | yes | `cached`, `ran`, `failed`, `skipped` or `cancelled` |
| `cache` | yes | `local`, `remote` or `miss`; `none` when the target does not cache (`cache = False`) and ran, failed run included; null when no cache was consulted for another reason (`--force`, or it failed before its run) |
| `remote` | yes | the name of the remote cache a `remote` hit came from; null otherwise |
| `duration_ms` | yes | from the start of its resolution to its end |
| `exit_code` | yes | its process's exit code: 0 for `ran`, the code for a `failed` run; null when nothing ran or a signal ended it. Read from the run's structured exit status, never parsed out of the error text |
| `timed_out` | yes | it failed because its run outlasted its `timeout` |
| `error` | yes | why it failed, on one line; null on success and on `skipped`. Its wording is not contract |
| `root` | yes | the culprit, when it is a target: on a failed culprit, the record itself; on `skipped`, the nearest culprit it depends on. Always an address. Null when the culprit is a file (`root_file` names it), and for `ran`, `cached` and `cancelled` |
| `root_file` | yes | the culprit, when it is a BUILD or `.bzl` file that did not evaluate: on the file's own record, its `file`; on `skipped`, the nearest culprit it depends on. Always a workspace-relative path. Null when the culprit is a target, and for `ran`, `cached` and `cancelled`. Exactly one of `root` and `root_file` is set on a `failed` or `skipped` record that has a culprit, as `addr` and `file` are on every record |
| `more` | yes | on `skipped`: how many culprits other than its own it also reaches; null elsewhere |
| `path` | yes | on a failed culprit: a shortest chain of addresses from a requested target down to it (see below for when it can differ between runs). On `skipped`: from this target down to the address its culprit was recorded at. Inclusive at both ends and never elided; null elsewhere |
| `selected` | yes | the selector matched it. False only on a culprit written because something selected depends on it; a `file` record is never selected |
| `failure` | yes | `{log, out, fixes}`, as `heph inspect failure`; null unless it failed running |
| `stage` | yes | on a failed culprit (a target or a file record): the stage of the build that raised it, `eval`, `spec`, `parse`, `link`, `run`, `cache`, `config` or `cli`; null elsewhere |
| `location` | yes | on a failed culprit: `{file, line, col}`, where in a BUILD or `.bzl` file it was raised, `file` workspace-relative; null when it names no place, and elsewhere |
| `hints` | yes | on a failed culprit: `[{kind, text}]`, what can be done about it; `kind` is `did_you_mean`, `valid_values`, `declare_tool`, `see_log` or `other`, and `text` is not contract. Empty elsewhere |
| `truncated` | yes | on a failed culprit: a cap cut something from its record (the message, its causes, a source line); false elsewhere |
| `log_replaced` | yes | on a failed culprit: a newer failed run filled its failure slot again, so the slot's log is no longer this failure's; false elsewhere |
| `streamed` | yes | on a failed culprit: its run's output reached a terminal as it ran; false elsewhere |
| `outputs` | no | its output files, the set `--list-out` prints, narrowed by `--output`. Its tier is *free* (no spec, def or hash is added), but selecting it costs what `--list-out` does: a remote hit's outputs are downloaded, which the default fields never do. That is why it is not a default |
| `hashin` | no (built) | its input hash |

How the outcome is decided:

- `cached`: no process ran; the result came from a cache (`cache` says which).
- `ran`: it ran and succeeded (`cache: miss`, `none` for a `cache = False`
  target, or null under `--force`). A
  cache hit whose blobs turned out to be gone is rebuilt, and reads as a miss.
- `failed`: its own run failed, or resolving it did (parse, hash, sandbox).
- `skipped`: it did not run because a culprit it depends on failed (`root`
  or `root_file` names it). It carries `error: null`: the error is on the
  culprit's record.
- `cancelled`: Ctrl-C, SIGTERM or SIGHUP stopped the run while it was pending.

To find what broke, read the culprits' own records: they are the
`outcome: "failed"` ones, and `select(.outcome == "failed") | .addr // .file`
names each. On a culprit's record `.path` is how the selection reached it.
`.root // .root_file` is a skipped record's culprit.

**Skipped records are a closing batch.** They come from one pass over the
dependency graph at the end of the request, so they are all written once the
run returns, after every other record. Within the batch the order is:

1. the culprits still waiting to learn whether the selection named them, when
   the matched set never completed (in the order they failed);
2. the skipped records, in the order the run gave each target up (its
   dependency's failure reached it; for a target the walk reported only
   later, when it was matched).

So every culprit precedes every record naming it. The batch is in completion
order like the rest of the list, not address order: sort on `.addr` for a
stable one. A reader that stops at the first failure has read none of the
skipped records.

**`path`.** A skipped record's `path` is read off the final graph, as is the
text end report, so it is the same on every run of the same graph. A
culprit's own record is streamed when it fails, so its `path` is *a shortest
path known when it failed*: in rare cases, when a shorter route into it was
discovered after it failed, it differs between runs. For a file culprit, the
`path` ends at the address that led into the file, the target it would have
defined and that was not found (`["//a:t", "//c:y", "//d:t"]`). The text box's
`reached by:` row stops one hop earlier, at the declarer, and adds a row for
the last edge: `//c:y depends on //d:t, which this file defines`. Its wording,
the elided middle of a long path included (`… 3 more (--json has the full
path) …`), is not contract: read `path` here, not the box.

Skipped records arrive only at request end, and a skipped record's `path` is
the full chain, so the output grows with chain depth × skipped count: for one
deep selected chain, with the square of its depth.

**A broken BUILD file reached by address.** A selection of addresses only
(`heph run //d:t1 //d:t2`) whose BUILD file does not evaluate gets the file's
record, `failed`, then a `skipped` record for each address, and a closed list;
the run exits 1 through the failure path, with the file's failure box.
Contract 1 reported that as a gap in the selection (a skipped package, the
list left open). A package selection (`//w/...`) over a package whose BUILD
does not evaluate still leaves that package a gap.

#### Migrating from contract 1

- `blocked_by` is removed, and the `"blocked"` outcome is now `"skipped"`:
  a `select(.outcome == "blocked")` matches nothing now, and must become
  `select(.outcome == "skipped")`. `root` (a target) or `root_file` (a file)
  replaces `blocked_by`, and names the nearest culprit, not the failed
  dependency at the far end of the chain.
- `root`, `root_file`, `more`, `path`, `selected` and `file` are added.
- A culprit the selector did not match is now written, with
  `selected: false`. A consumer that wants only what it asked for filters on
  `.selected`.
- A BUILD file that did not evaluate is a record with `addr: null` and `file`
  set. Name a record with `.addr // .file`.
- Skipped records are written at the end of the request, as a closing batch,
  not as each target is given up, and carry `error: null`. A reader that
  stops early (`first(select(.outcome == "failed"))`, `head -1` on `--jq`)
  misses them.
- An address-only selection that hits a BUILD file that does not evaluate
  fails through the failure path (exit 1, a closed list), where it reported
  a selection gap.
- `heph inspect failure --json`'s `addr`, `hashin` and `failed_at` are
  nullable: `addr` is `null` on a BUILD file's record (its path is in
  `file`), and `hashin` and `failed_at` on every failure before the run. A
  reader that indexes them must accept `null`. See `heph inspect failure`
  below.
- A cancelled command (Ctrl-C, SIGTERM or SIGHUP) exits 130, also when a
  failure had already failed it. It exited 1. A cancel that `--fail-fast` made
  after a failure is not a user's, and exits 1.
- SIGTERM and SIGHUP used to kill heph by the signal (status 143 for TERM,
  129 for HUP, no summary). For `heph run` and every command that can build
  under the progress view, they now cancel as Ctrl-C does: the work in flight
  is cancelled, the summary is printed, and the command exits 130; a second
  one aborts. `heph tool build-lsp` and `heph log -f`, which a cancel would not
  end, still die of them.

The exit code is the one the run has without `--json`, and so is everything on
stderr: failure boxes, the skipped-package report. The list is closed only
when it is whole: every matched target has its entry, the selector walk
skipped no package, and the run was not cancelled. A failed target does not
leave it open; `--fail-fast` stopping the walk, a skipped package and a
cancel do. A cancelled build run (Ctrl-C, SIGTERM or SIGHUP) exits 130, with
or without `--json`, and so does one that a failure had already failed. A
selector that matches nothing prints `[]` and fails as it does without
`--json`.

When the list cannot be printed whole for a reason of its own:

- A field that cannot be read for one entry (its `outputs` could not be
  listed, say) skips that entry and nothing else: the build carries on, the
  other entries are printed, the list is left without its `]`, and the error
  is printed after the run's summary. The command fails with it unless the
  build failed for itself (a failed target, a skipped package), whose exit
  code it keeps.
- A reader that closes stdout ends the command with exit 0, as for every
  command. Any other error writing stdout cancels the build and is printed
  after the summary; the command exits 1.

`--shell`, `--cat-out`, `--list-out` and `--raw` all want stdout or the
terminal, so with `--json`, `--fields` or `--jq` they exit 2 at parse time.
`--copy-out` and `--output` combine with it. An entrypoint
(`entrypoint = True`) runs a program on your terminal instead of building, so
it has no entry to report: `heph run <entrypoint> --json` is refused once the
selector resolves, before anything runs, naming the target. It is a usage
error, exit 2, like the flags refused at parse time.

### `heph inspect failure`

`heph inspect failure <addr> --json` prints one record: a culprit's last
recorded failure and where its slot kept each part (`docs/FAILURE_OUTPUTS.md`
has the example).

| Field | Meaning |
|---|---|
| `addr` | the target's address; null on a BUILD or `.bzl` file's record |
| `file` | the workspace-relative file that did not evaluate; null on a target's record |
| `slot` | the failure slot's directory, absolute |
| `stage` | `eval`, `spec`, `parse`, `link`, `run`, `cache`, `config` or `cli`; never null |
| `message` | what went wrong, on one line; never null. Its wording is not contract |
| `location` | `{file, line, col}` in a BUILD or `.bzl` file; null when it names no place |
| `hints` | `[{kind, text}]`; a hint's wording is not contract |
| `truncated` | a cap cut something from the record |
| `hashin` | the input hash of the run that failed; null for a failure before the run |
| `failed_at` | when it failed, ms since the Unix epoch; null for a failure before the run |
| `log` | the run's full process log; null when it kept none, and for a failure before the run |
| `log_lines` | how many lines `log` holds (0 for a run that printed nothing); null with no log |
| `log_replaced` | a newer failed run filled the slot again while it was read |
| `out` | the `failure_out` matches; null when none matched |
| `fixes` | the list of files an `in_place` write-back changed; null when none |

`heph inspect failure //d:t` for a target whose BUILD file did not evaluate
answers with the file's record: `addr` null, `file` `d/BUILD`. It answers for
culprits only: for a target skipped because a dependency failed it prints
`null` (exit 0), as for one that never failed. The run's own `--json` carries
the skipped target's `root` and `path`.

### `heph query` on a broken BUILD file

`heph query <addr> --json`, naming one address, on a target whose BUILD or
`.bzl` file does not evaluate prints `[]`, a closed list, and exits 1 with
the file's failure box on stderr, as `heph run` does. It does not report a
selection gap: the address named one package, and that package's file is the
culprit.

### Commands without JSON

These have no output flags; `--json`, `--fields` or `--jq` on them exits 2.

| Command | Why | Instead |
|---|---|---|
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
