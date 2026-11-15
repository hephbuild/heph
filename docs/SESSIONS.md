# Sessions

Every heph process that runs a command under the TUI (`run`, `query`, `gc`, …:
anything going through `tui::run_app`) records what it is doing to a session
directory. Another terminal lists the sessions, prints a snapshot of one, or
attaches the same TUI to it. Nothing has to be decided before the run starts.

```
heph session list [--all] [--json]
heph session show <id> [--json]
heph session attach <id>
heph session inflight <id> [--json]
```

The line backend announces the session on stderr when it starts, never on
stdout:

```
heph: session d1v9q02kf5 (heph session attach d1v9q02kf5)
```

An `<id>` is the full id, the pid of a **live** session, or a unique prefix
or suffix of an id; an ambiguous one lists the candidates. Give the suffix
(the last 4–5 characters, `heph session attach 02kf5`) or the pid: an id
is 42 bits of start time over 8 random bits, so its leading characters are
the time, and two runs started within about 2 minutes share a 5-character
prefix.

## Commands and exit codes

| Command | Does | Exit |
| --- | --- | --- |
| `list` | One row per session of this workspace (`--all`: every workspace; outside a workspace, every one, with a note on stderr), live first: ID, WORKTREE, COMMAND, STATE, EXIT, PROGRESS, NOW. Reads `meta.json` and `status.json` only. | 0 |
| `show <id>` | The TUI's frame printed once as text, then a "Stuck on" block: lock waits naming the session that holds the lock (followed to the root of the chain, falling back to the pid), admission waits, targets running over 30s, and one `Next:` line. | 0 |
| `attach <id>` | The interactive TUI on the session, following it until it ends; `q` or Ctrl-C detaches and never cancels it. Without a terminal it follows in line form. | the session's exit code; 0 on detach |
| `inflight <id>` | The session's in-flight report (what the `SIGQUIT` dump carries), asked for through the session directory; waits 5s. | 0 |

Every error of `heph session` itself exits **125**: no such session, an
ambiguous id, a directory format newer than this heph, a lost session for
`attach`, a session whose `End` carries no exit code, an `inflight` that is
not answered or is asked of an ended session. By code alone 125 is ambiguous:
a session can exit 125 itself (an entrypoint whose heph-side setup failed, or
a tool that exits 125), and `attach` mirrors it. So whenever `attach` exits
125 for a reason other than its own error, it says so on stderr: `heph:
session <id> exited 125` for a mirrored 125, `heph: session <id> exited N, not
an exit status (0-255); exiting 125` for a recorded code outside 0-255. A
session whose `End` carries no code (killed by a signal, or it panicked) is
reported as an error with that reason. An agent tells "the run failed" from
"the wait failed" by that line, or by `heph session show <id> --json`.

STATE is computed by the viewer, never stored: `running`; `waiting` (every
open target is in a lock or admission wait); `quiet` (no event for longer
than the stall notice, 60s when it is off: not a verdict, a long compile is
quiet too, and `show` says "no events for 1m05s; N targets still running");
`ok` or `failed` once `End` is
written; `lost` when the lock is free and there is no `End` (killed, or
crashed).

`HEPH_NO_SESSION=1` records nothing: no directory, no start line. It is read
once, by the front end, beside `HOME`; with no `HOME` or an unwritable
`~/.heph` the run records nothing and says so once with a `warn!`.

`heph run --raw` sets up no session at all: no directory, no recorder, no
start line, and no warning, so its stderr carries only the tool's bytes and
heph's failures. (`--raw` is also what lowers heph's log to `warn`; there is
no separate quiet flag.) It still names another session in a lock-wait
notice: that only reads.

### heph inside a target

A target that runs heph (a test of a BUILD file, a tool wrapping `heph
query`) starts a heph of its own, and that heph records a session like any
other. Where it lands depends on what the target passes through:

- With no `HOME` in the target's environment (the default: a target sees only
  what it declares), the nested heph records nothing and says so once with a
  `warn!`.
- With `pass_env = ["HOME"]` (or `["*"]`, or `runtime_pass_env`), and in an
  `entrypoint = True` target, which runs with the whole environment, the
  nested heph writes to the real `~/.heph/sessions`, beside the outer run's.
  `heph session list` shows it as one more session, and retention removes it
  like any other.
- With `HOME` pointed inside the target's sandbox (a test that gives heph a
  scratch home), the nested heph writes `.heph/sessions/<id>/` there, inside
  the sandbox. A directory or glob output that covers it captures the session
  directory into the target's outputs, and with it a fresh id and timestamps
  on every run. Set `HEPH_NO_SESSION=1`, or use `--raw`.

Either way the outer run is unaffected: nothing about its session reaches the
target (I1), so the nested run is a separate session, never a child of the
outer one. A target that runs heph should set `HEPH_NO_SESSION=1` in its
`env`, or use `heph run --raw` when it only wants the tool's bytes, which
records nothing and prints no start line.

## Layout

`~/.heph/sessions/<id>/`, directory 0700, files 0600 (the per-user directory
that also holds the plugin cache; `hcore::userdir`). `heph tool clean` and
`gc` work on the cache by target and never touch `sessions/`, even for a
workspace rooted at `$HOME`, whose home is `~/.heph`.

| File | Written | Holds |
| --- | --- | --- |
| `meta.json` | once at start, temp + rename | `format`, id, `owner_pid`, start time, heph version, argv, cwd (`HEPH_CWD` when set), workspace root and home, label, interactive, `capabilities` |
| `lock` | `flock` taken at start, held until exit | nothing: held means alive |
| `events.ndjson` | appended, flushed every 250ms and at exit | one record per line |
| `status.json` | temp + rename, at most once a second, only when changed | a `SessionSnapshot`, each list capped at its first 100 entries |
| `inflight.request` | by a viewer, temp + rename | nothing; the owner consumes it |
| `inflight.txt` | by the owner, temp + rename | the last in-flight report |

`status.json` keeps at most the first 100 entries of each list (`running`,
`waiting`, `failed`, `lock_waits`, `admission_waits`), plus
`totals.<list> = {total, truncated}` with the full length. This reverses A17
(user decision), the design amendment that kept full lists: a 50k-target
build that fails wide would otherwise rewrite megabytes every second, and
`list` reads it for every session. The counts (`counts`, including
`running_unblocked`, which `waiting` is derived from) are over the full
lists. A `status.json` without `running_unblocked` (a writer from before the
field) is read by comparing the `running` and `waiting` lists, as it was
before the cap. `list` reads only `status.json` and stays cheap; `show` (text and
`--json`) and `attach` fold `events.ndjson` and have every entry. Past the
event log's 64 MiB cap a fold is short of the run, so `show --json` prints
`status.json` instead, capped, and its `totals` say so.

The id is 10 Crockford base32 characters: 42 bits of start time in ms, then 8
random bits, so ids sort by start. Uniqueness comes from `create_dir` (not
`create_dir_all`), regenerated on `EEXIST`.

## Records

One line of `events.ndjson` is

```json
{"at_unix_ms": 1794666004822, "kind": {"type": "ExecuteStart", "addr": "//p:t", …}}
{"at_unix_ms": 1794666004822, "session": {"type": "End", "exit_code": 1, "panicked": false}}
```

`kind` is a `BuildEventKind` byte for byte as on the plugin wire
(`crates/core/src/events.rs`), with the same skew rules: an unknown variant
decodes as `Unknown`. `session` is a `SessionRecordKind`: `Log {level, msg}`
(`info!` and above, lossy UTF-8, stored verbatim and escaped by the viewer on
output), `End {exit_code, panicked}`, `Truncated`, `InflightWritten`, and
`Unknown` for anything newer. Both keys are optional; a line with neither is
skipped.

What the run's terminal showed besides events and log lines is recorded too,
so `attach` draws it (see [Attach](#attach)):

- `Label {label}`: the TUI's bottom label changed (`meta.json` holds the first).
- `Approval {prompt}`: the approval banner changed; `prompt` is `{addr,
  queued_behind, notices: [{name, path}]}`, or absent once nothing is pending.
  Never the notice's content, which stays in its file.
- `Finished`: the TUI was torn down; `at_unix_ms` is the instant its final
  summary was measured against.
- `Output {text}`: text the process wrote straight to its terminal, today the
  failure report printed after the teardown; its first 1 MiB, with a marker
  line when it was cut.

All four were added without a format bump: a heph from before them decodes
each as `Unknown` and skips it. A reader skips and counts a complete line that does not decode and
never stops on one, and waits for a trailing partial line to complete. `Log`
and `End` never become `BuildEventKind` variants: a plugin hook must not
receive them.

## Versioning

Three things are versioned separately, and each is bumped only on an
incompatible change:

- **`meta.json` `format`** versions the directory contract: file names, the
  lock, the id scheme, cleanup. `show`/`attach`/`inflight` refuse a newer
  format naming both versions; `list` shows a placeholder row ("format N,
  needs newer heph") and exits 0. `show` run by a heph older than
  `meta.heph_version` warns that the session may be shown incomplete;
  `list`, `attach` and `inflight` do not warn.
- **`SessionSnapshot.version`** versions `status.json` and `show --json`.
  It is still 1 with the list cap and `totals`, because no heph has shipped a
  snapshot yet: version 1 is what the first release carries, cap included.
  The cap is not an additive change, since it changes what the existing lists
  mean (the first 100 entries, not all of them), so a reader that ignored
  `totals` would misread a cut list as whole. From the first release on, a
  change of meaning like that bumps the version; only new fields come without
  one.
- **`events.ndjson`** only grows additively: new record kinds and fields,
  never a changed meaning.

New fields are `#[serde(default)]` and unknown ones are ignored. Two things
are frozen for **every** format: the `lock` file name and its `flock`
meaning. That is what lets cleanup, which never parses `meta.json`, work on a
session of a format it cannot read: a held lock with an unreadable `meta.json`
is a live session of unknown format.

### `--json` stability

`list --json` is `{version, sessions: [{id, owner_pid, argv, workspace_root,
cwd, state, started_unix_ms, last_event_unix_ms, exit_code, panicked,
progress, now, blocked_on, format}]}`, sorted by id, times in absolute unix
ms:

- `progress` is PROGRESS as numbers, `{done, total, total_complete,
  running_ops, failed}`, `null` before the session wrote its first
  `status.json`:
  - `total` is the matched targets: `null` before the matcher reported,
    provisional until `total_complete`.
  - `done` is `counts.matched_done` (matched targets that finished) once
    `total` is known, and `counts.finished` (every finished target, matched or
    not) before that.
  - `running_ops` is the targets with an operation open (executing, reading
    or writing a cache), counted in full. It is not `counts.in_flight`, which
    also counts a target between its result's start and end with no operation
    open, such as one waiting on a lock or on its deps.
- `now` is NOW for a live session (the lock or pool it waits on, else its
  oldest running target); `null` once it ended. It is human text and may
  change wording: an agent reads `blocked_on` and `progress` instead.
- `panicked` is true when the session ended in a panic, whatever its
  `exit_code`.
- `blocked_on` entries are `{kind: "lock", addr, holder_pid, holder_session}`
  or `{kind: "admission", addr, pool}`. From `status.json`, so at most the
  first 100 of each kind.
- `format` appears only on the row of a session in a newer directory format,
  which this heph reads no further: that row has `owner_pid: null`, empty
  `argv`, `null` progress and `state: "unknown"`. `owner_pid` is never `null`
  otherwise.

`show --json` is the `SessionSnapshot` plus `id`, `state`, `blocked_on`,
`chain_root`, and `next`. The snapshot is folded from the event log, every
list in full, unless the log reached its 64 MiB cap: then it is `status.json`,
each list capped at 100 entries, and `totals` says which were cut.
`blocked_on` has `list --json`'s shape, but from that same snapshot, so it is
in full where `list`'s, which comes from `status.json`, has at most 100
entries of each kind. `chain_root` is the id of the session at the root of
the first lock chain, `null` when no chain leads to a session. `next` is
`{kind, command, argv, cwd, reason}`; `kind` is one of `attach`, `show_holder`, `inflight`,
`inspect_failure`, `rerun`, `list`, `none` and may gain values.

- `command` is display text for a person: each word shell-quoted where it
  needs it, so it pastes back into a shell, then escaped for the terminal; a
  `rerun` names its program by file name. Don't parse it.
- `argv` is what an agent runs: the exact words, unquoted and unescaped. For
  `rerun` it is the recorded `argv`, the program as recorded; for the others,
  `heph` and its arguments.
- `cwd` is the directory to run `argv` in: the recorded `cwd` for `rerun`,
  `inspect_failure` and `list`, which depend on it; `null` for the commands
  that take a session id and run from anywhere, or when no `cwd` was recorded.
- All three are `null` for `none`.

The text `Next:` line prints `command` alone, its reason on the line below.

`inflight --json` is `{id, report}`.

Field names are permanent once released: a field may be added, never renamed
or removed without a version bump. `state` is computed by the viewer and may
gain values; a consumer treats one it does not know as unknown, not as an
error. `state` also depends on the viewer's `--stall-notice` (the `quiet`
threshold), so two viewers can disagree on it; `last_event_unix_ms` is the
stable signal for "has it gone silent".

## Invariants

| # | Rule |
| --- | --- |
| I1 | Watching never changes the build. The recorder reaches no hash and no target's argv, env or files; a recording failure never fails, blocks or slows a build task. The session id and directory are **never exported to the environment**: a session runner or `pass_env = ["*"]` would carry them into targets. |
| I2 | A session is alive exactly while its lock is held. The fd is `O_CLOEXEC`, so a child outliving heph cannot keep it alive. |
| I3 | Only the owning process writes a session directory, and only it cleans. A viewer's **only** write is `inflight.request`. |
| I4 | `events.ndjson` is append-only, one complete record per line; a reader ignores a trailing partial line until it is complete. |
| I5 | A record's `kind` is a `BuildEvent` exactly as on the plugin wire; `meta.json` carries its format, and a viewer refuses a newer one by name. |
| I6 | A session holds nothing the terminal would not have shown (argv, log lines, failing log tails), user-only. |
| I7 | A viewer **never signals** a process. `inflight` drops `inflight.request`; the owner's diag sweeper (one `stat` per 200ms tick) writes `inflight.txt`, removes the request and records `InflightWritten`, which the viewer waits for. `meta.json`'s `capabilities` must list `inflight`. |

### Recording

The process's session is a `SessionSlot` that `main` owns: there is no
process-wide instance. `main` configures it and hands it to whatever records
into it — every command, as its own argument beside the sink;
`bootstrap::new_engine` (the workspace for `meta.json`); the `LogSink`, which
keeps a reference only to tee `info!` lines into the open session; the diag
sweeper; and the error renderer. `tui::run_app` takes the session as an
explicit argument (a debug build asserts it is the sink's slot) and opens it
lazily (a viewer's `App::records_session()` is false), and `main` finishes it with the exit code
just before it returns its `ExitCode`, ahead of anything slow on the way out
(telemetry, the profiler). Both
backends consume a tapped receiver that moves each event to the recorder right
after the view folds it, so what is recorded is what was shown, paused TUI
included. The recorder is its own OS thread behind an unbounded channel:

- It writes `End`, flushes, writes `status.json`, **then** releases the lock,
  so "lock free and no `End`" means exactly "crashed". A panic unwinding
  through `run_app` writes `End {panicked: true}`. The join at exit is bounded
  (3s).
- On a write failure it latches "stopped" before its one `warn!` (which the
  log tap would otherwise feed back to it) and drops its receiver; it stops the
  same way past 500k pending events. A stopped recorder keeps its lock until
  the process exits, so the session reads as live while it is.
- `events.ndjson` stops at **64 MiB** with a `Truncated` record; `status.json`
  keeps updating. Past it, only what a viewer needs at the end is appended:
  `End`, `InflightWritten`, `Finished`, the failure report (`Output`), a
  `Label` change, and an `Approval` that clears the prompt. An `Output`
  record keeps at most the first 1 MiB of its text, then a line saying how
  many bytes were left out, so the report a process prints (at most twice)
  overshoots the cap by a bounded amount.

What a run loses: a process wedged before its first flush loses at most 250ms
of events; a second Ctrl-C, `SIGTERM`/`SIGHUP` or `SIGKILL` loses what came
after the last flush and writes no `End` (`lost`).

### The run's own lock-wait notice

A run waiting on a lock names the holder in its own progress output, on both
backends: `held by session <id> (pid N)` (the TUI row says `locked by …`) when a
live session other than its own is recorded for the pid, and `held by pid N`
otherwise. The events carry only the pid, so the engine stays unaware of
sessions: the pid is resolved with the same rule as `list`/`show` (a live
session, not this one, whose `meta.json` names the pid). The backend's event
receiver does it, once per distinct pid, on a detached thread, before it folds
the event that first carries the pid; that event waits for the scan, later
events queue behind it, and the build itself never waits. The wait is at most
200ms: past it (a hung `~/.heph` on NFS) the event is folded with the pid, the
pid is not scanned again, and an answer that comes later still names it from
then on. The thread is detached rather than on the blocking pool because a
runtime's shutdown waits for its blocking tasks: a hung scan never holds up the
exit. The scan reads each
session's `meta.json` and probes the matching one's lock, never `status.json`.
The view only reads the cached answer, so neither the fold nor a render does
IO. The directory comes from `HOME` alone, so a run
with `HEPH_NO_SESSION` still names other sessions; with no `HOME`, or any read
error, the notice is the pid.

## Attach

`heph session attach` draws the run's own `TuiProgressView`: the same type,
render functions and layout, fed the recorded events. The view has one seam,
`Role { Live, Attached }` in `crates/tui/src/tui/progress.rs`, and these are
the only things that differ between the two:

| | Live (the run) | Attached (a viewer) |
| --- | --- | --- |
| Approval queue | the engine's; `y`/`n`/Enter answer it | none: the keys are inert |
| Approval banner | shared `approval_banner`, with the keys and the `enter view` preview | the same banner from the recorded `Approval`, with no keys and no preview |
| `q` while running | nothing (Ctrl-C cancels the run) | detaches (Ctrl-C detaches too) |
| Help row | the keys | the keys, then ` · session <id> · q detach` |
| Bottom label | the header model's | the latest recorded `Label`, else `meta.json`'s |
| Summary clock (`set_finished`) | the run's teardown | the recorded `Finished`, else `End` |

Anything else that differs is a bug: `session/view_tests.rs` records a run's
view, replays the recording into an attached one, and compares the two frames
row for row with the prompt shown and after it clears.

`tui::session::SessionFeed::route` is where a record becomes what the viewer
does with it: an event is folded into the view, `Label`/`Approval`/`Finished`
update what the view reads, and `Log`/`Output` are printed above the
viewport. What was recorded after `Finished` (the failure report) is printed
after the viewer's own teardown, below its final summary, where the run
printed it.

Replayed text keeps the run's colours: a well-formed SGR sequence (`ESC[`,
digits, `;` and `:`, then `m`) is kept when the viewer's stderr takes colour
and dropped when it does not (`NO_COLOR`, not a terminal). Everything else is
escaped to visible text (`\u{1b}[2J`): cursor moves, clears, OSC titles, any
other CSI including private modes like `ESC[?1049h` and an intermediate byte
like `ESC[31 m`, C1 controls, DEL and NUL, the bidi embedding, override and
isolate controls (`U+202A`–`U+202E`, `U+2066`–`U+2069`, which reorder how a
line displays), and a truncated or malformed sequence. A parameter list over
128 bytes is malformed too: no real SGR comes near it (a truecolour
foreground and background with attributes is under 60 bytes), so its `ESC`
is escaped and the rest prints as text. The scan for the `m` stops at 129
bytes, so the check stays linear whatever the input. This reverses spec amendment A11 (drop every
SGR on output) for `attach`'s replayed `Log` and `Output` text only; `list`,
`show` and `inflight` still drop SGR (`escape_replayed` vs `escape_text` in
`src/commands/session/mod.rs`).

Lock holders are named as the run named them (above): the viewer scans the
same sessions directory (`SessionSource::holder_names`), excluding the
attached session itself. The residual: the viewer resolves a pid when it
folds that wait, possibly long after the run did, so a holder that has
exited by then is shown as `pid N` where the run showed its session.

On a terminal, `attach` always draws the TUI, even for a session the run
recorded with the line backend (`meta.json` `interactive: false`): the viewer's
terminal decides, not the run's. Without a terminal it follows in line form,
which for a line-backend session is its recorded log lines alone.

## Retention

Cleanup runs only at session start, on the new session's recorder thread;
`list` and every other viewer stay read-only. It decides on lock state and
modification times only, **never** on `meta.json`. An ended session (lock
free) is kept while it was last written under **10 minutes** ago and is among
the **20** most recent ended sessions; then the oldest ended ones go until
the ended sessions total at most **256 MiB**. A live session is never removed
or counted. A directory with no `lock` yet, younger than 10 minutes, is a
session being created and is left alone. Removal takes the session's lock
exclusively, so two cleanups racing never both remove one session: the
second finds the lock taken and moves on. A removal renames the directory to
a tomb (`.removing-<id>-<pid>`) and then deletes it; a tomb older than 10
minutes, left by a removal that failed or died, is deleted outright with no
lock. Two cleanups can race on that one, both deleting the same tomb, which is
harmless: one finishes, the other sees it gone.

## The daemon

A future daemon that holds the graph and runs builds replaces the transport,
not the commands, the snapshot format or the viewer. Under it one process has
many sessions (a session is a request, and a pid can no longer name it), lock
waits become joined memoized work, and the CLI talks to the daemon over a
socket — while each CLI client still receives its event stream and keeps
writing these files. That is why the id is opaque and carries no pid, why
the commands read through a `SessionSource` trait (`list`, `snapshot`,
`subscribe`, `inflight`; the files are its first implementation), why one
versioned `SessionSnapshot` serves `status.json`, `--json` and later the
daemon's reply, and why `inflight` is defined per session rather than as a
signal to a pid. Nothing here assumes one session per process beyond the
file source itself.
