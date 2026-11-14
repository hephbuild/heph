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
or suffix of an id; an ambiguous one lists the candidates.

## Commands and exit codes

| Command | Does | Exit |
| --- | --- | --- |
| `list` | One row per session of this workspace (`--all`: every workspace; outside a workspace, every one), live first: ID, WORKTREE, COMMAND, STATE, EXIT, PROGRESS, NOW. Reads `meta.json` and `status.json` only. | 0 |
| `show <id>` | The TUI's frame printed once as text, then a "Stuck on" block: lock waits naming the session that holds the lock (followed to the root of the chain, falling back to the pid), admission waits, targets running over 30s, and one `Next:` line. | 0 |
| `attach <id>` | The interactive TUI on the session, following it until it ends; `q` or Ctrl-C detaches and never cancels it. Without a terminal it follows in line form. | the session's exit code; 0 on detach |
| `inflight <id>` | The session's in-flight report (what the `SIGQUIT` dump carries), asked for through the session directory; waits 5s. | 0 |

Every error of `heph session` itself exits **125**: no such session, an
ambiguous id, a directory format newer than this heph, a lost session for
`attach`, a session whose `End` carries no exit code, an `inflight` that is
not answered or is asked of an ended session. 125 is outside what a session
can mirror for a build heph ran, so an agent waiting with `attach` tells "the
run failed" from "the wait failed".

STATE is computed by the viewer, never stored: `running`; `waiting` (every
open target is in a lock or admission wait); `stalled` (no event for longer
than the stall notice, 60s when it is off); `ok` or `failed` once `End` is
written; `lost` when the lock is free and there is no `End` (killed, or
crashed).

`HEPH_NO_SESSION=1` records nothing: no directory, no start line. It is read
once, by the front end, beside `HOME`; with no `HOME` or an unwritable
`~/.heph` the run records nothing and says so once with a `warn!`.

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
| `status.json` | temp + rename, at most once a second, only when changed | a `SessionSnapshot` |
| `inflight.request` | by a viewer, temp + rename | nothing; the owner consumes it |
| `inflight.txt` | by the owner, temp + rename | the last in-flight report |

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
skipped. A reader skips and counts a complete line that does not decode and
never stops on one, and waits for a trailing partial line to complete. `Log`
and `End` never become `BuildEventKind` variants: a plugin hook must not
receive them.

## Versioning

Three things are versioned separately, and each is bumped only on an
incompatible change:

- **`meta.json` `format`** versions the directory contract: file names, the
  lock, the id scheme, cleanup. `show`/`attach`/`inflight` refuse a newer
  format naming both versions; `list` shows a placeholder row ("format N,
  needs newer heph") and exits 0. A viewer older than `meta.heph_version`
  warns that the session may be shown incomplete.
- **`SessionSnapshot.version`** versions `status.json` and `show --json`.
- **`events.ndjson`** only grows additively: new record kinds and fields,
  never a changed meaning.

New fields are `#[serde(default)]` and unknown ones are ignored. Two things
are frozen for **every** format: the `lock` file name and its `flock`
meaning. That is what lets cleanup, which never parses `meta.json`, work on a
session of a format it cannot read: a held lock with an unreadable `meta.json`
is a live session of unknown format.

### `--json` stability

`list --json` is `{version, sessions: [{id, owner_pid, argv, workspace_root,
cwd, state, started_unix_ms, last_event_unix_ms, exit_code, blocked_on}]}`,
sorted by id, times in absolute unix ms; `blocked_on` entries are
`{kind: "lock", addr, holder_pid, holder_session}` or `{kind: "admission",
addr, pool}`. `show --json` is the `SessionSnapshot` plus `id` and `state`.
`inflight --json` is `{id, report}`.

Field names are permanent once released: a field may be added, never renamed
or removed without a version bump. `state` is computed by the viewer and may
gain values; a consumer treats one it does not know as unknown, not as an
error.

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

`tui::run_app` opens the process's session lazily (`tui::session::process()`,
configured by `main`; a viewer's `App::records_session()` is false), and
`main` finishes it with the exit code just before `std::process::exit`. Both
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
  keeps updating.

What a run loses: a process wedged before its first flush loses at most 250ms
of events; a second Ctrl-C, `SIGTERM`/`SIGHUP` or `SIGKILL` loses what came
after the last flush and writes no `End` (`lost`).

## Retention

Cleanup runs only at session start, on the new session's recorder thread;
`list` and every other viewer stay read-only. It decides on lock state and
modification times only, **never** on `meta.json`. An ended session (lock
free) is kept while it was last written under **10 minutes** ago and is among
the **20** most recent ended sessions; then the oldest ended ones go until
the ended sessions total at most **256 MiB**. A live session is never removed
or counted. A directory with no `lock` yet, younger than 10 minutes, is a
session being created and is left alone. Removal takes the session's lock
exclusively, so two cleanups racing never both act on one directory.

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
