# Sessions

Every heph process that runs a command under the TUI (`run`, `query`, `gc`, …:
anything going through `tui::run_app`) records what it is doing to a session
directory. Another terminal lists the sessions, prints a snapshot of one, or
attaches the same TUI to it. Nothing has to be decided before the run starts.

```
heph session list [--all] [--json]
heph session show <ref> [--json]
heph session attach <ref>
heph session inflight <id> [--json]
heph session export <ref> [-o PATH|-] [--no-logs]
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
prefix. `show`, `attach` and `export` also take a `.hephsession` file, by
path or `https://` URL (see [Opening a recording: `<ref>`](#opening-a-recording-ref)).

## Commands and exit codes

| Command | Does | Exit |
| --- | --- | --- |
| `list` | One row per session of this workspace (`--all`: every workspace; outside a workspace, every one, with a note on stderr), live first: ID, WORKTREE, COMMAND, STATE, EXIT, PROGRESS, NOW. Reads each recording's `session` and `status` rows only, never `records`. | 0 |
| `show <ref>` | The TUI's frame printed once as text, then a "Stuck on" block: lock waits naming the session that holds the lock (followed to the root of the chain, falling back to the pid), admission waits, targets running over 30s, and one `Next:` line. An ended session adds one line per bounded resource pool (see [Pools](#pools)). | 0 |
| `attach <ref>` | The interactive TUI on the session, following it until it ends; `q` or Ctrl-C detaches and never cancels it. Without a terminal it follows in line form. | the session's exit code; 0 on detach |
| `inflight <id>` | The session's in-flight report (what the `SIGQUIT` dump carries), asked for through the session directory; waits 5s. Local sessions only. | 0 |
| `export <ref>` | Writes the recording as one self-contained `.hephsession` file and prints its path (see [Export](#export-hephsession)). | 0 |

Every error of `heph session` itself exits **125**: no such session, an
ambiguous id, a file that is not a heph session, a URL refused or not
downloaded, a recording format newer than this heph, a lost session for
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

Under `--json` (`list`, `show`, `inflight`), an error of `heph session`
itself also prints one line on stdout, `{"error":{"kind":…,"message":…}}`,
beside the usual line on stderr; the exit stays 125. `kind` says which
failure it is, and is an open enum: a consumer reads a value it does not know
as a failure, with `message` saying what.

| `kind` | When |
| --- | --- |
| `not_found` | no session matches the `<ref>`; for digits, neither a live pid nor an id prefix or suffix |
| `ambiguous` | several sessions match; the message lists them |
| `not_a_session` | the file or download is not a heph recording (an HTML error page, a zip without exactly one `.hephsession`, a truncated file) |
| `format_too_new` | a heph recording in a format newer than this heph reads; the message names both formats |
| `fetch` | an `http://` URL, a URL carrying credentials, or a download that failed or passed its cap |
| `io` | a file could not be read or written |
| `other` | anything else; the message says what |

STATE is computed by the viewer, never stored: `running`; `waiting` (every
open target is in a lock or admission wait); `quiet` (no event for longer
than the stall notice, 60s when it is off: not a verdict, a long compile is
quiet too, and `show` says "no events for 1m05s; N targets still running");
`ok` or `failed` once `End` is
written; `lost` when the lock is free and there is no `End` (killed, or
crashed).

`HEPH_NO_SESSION=1` makes the session private: no start line, and nothing
another terminal can see. It is still recorded, into
`~/.heph/private-sessions/<id>/` instead of `sessions/`, because a closed full
screen replays its log lines from there (see [Recording](#recording)); `list`
never reads that directory, so `show` and `attach` cannot name it. The run
removes it when it finishes. A killed run's directory stays until the next
cleanup sweeps it, which runs at most once an hour (see
[Retention](#retention)), so it can linger for up to an hour.
It is read once, by the front end, beside `HOME`; with no `HOME` it records
nothing. This is a change of contract: before run logs, `HEPH_NO_SESSION=1`
wrote nothing under `~/.heph` at all. It now writes there while the run
lasts, so a caller that set it to keep `HOME` untouched (a read-only home, a
sandbox that captures it) should use `--raw`, which still records nothing,
or point `HOME` somewhere writable. With no `HOME` or an unwritable `~/.heph`
a public run records
nothing and says so once with a `warn!`.

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
  directory (its `session.db` included) into the target's outputs, and with
  it a fresh id and timestamps on every run. Set `HEPH_NO_SESSION=1`, or use
  `--raw`.

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

```
~/.heph/sessions/
  .cleanup                   its mtime: when the last cleanup was claimed (at most hourly)
  .tmp/                      readers' copies of lost recordings, downloaded and
                             decompressed .hephsession files (swept by the hourly
                             cleanup once older than 10 minutes)
  .removing-<id>-<pid>/      a removal in progress (a tomb)
  <id>/
    lock                     flock held while alive (frozen for every format)
    session.db               the recording (+ -wal, -shm while live)
    inflight.request         by a viewer, temp + rename; the owner consumes it
    inflight.txt             by the owner, temp + rename: the last in-flight report
```

| File | Written | Holds |
| --- | --- | --- |
| `lock` | `flock` taken at start, held until the recording is sealed or the process exits | nothing: held means alive |
| `session.db` | by the owner's recorder only, one transaction per 250ms flush tick and at exit | the whole recording: see [The recording](#the-recording-sessiondb) |
| `inflight.request` | by a viewer | nothing |
| `inflight.txt` | by the owner | the last in-flight report |

`session.db` is created under a temporary name (`.session.db.creating`) with
its schema, format stamp and session row in place (under a rollback journal
kept in memory, then switched to WAL, so the temporary name never has a
`-wal` and its close has nothing to checkpoint), closed, renamed, and
reopened by its final name, so no viewer ever opens a half-made file, the
file is in WAL mode from the moment it has its final name, and no `-wal` is
ever left named after the temporary name. A filesystem that
refuses WAL mode (some network filesystems) stops the recorder before the
rename, with its one `warn!` (see [Recording](#recording)).

The `status` row keeps at most the first 100 entries of each list
(`running`, `waiting`, `failed`, `lock_waits`, `admission_waits`), plus
`totals.<list> = {total, truncated}` with the full length. This reverses A17
(user decision), the design amendment that kept full lists: a 50k-target
build that fails wide would otherwise rewrite megabytes every second, and
`list` reads it for every session. The counts (`counts`, including
`running_unblocked`, which `waiting` is derived from) are over the full
lists. A snapshot without `running_unblocked` (a writer from before the
field) is read by comparing the `running` and `waiting` lists, as it was
before the cap. `list` reads only the `session` and `status` rows and stays
cheap; `show` (text and `--json`) and `attach` fold `records` and have every
entry. Past the recording's 256 MiB full cap (a `Truncated` record) a fold
is short of the run, so `show --json` prints the `status` row instead,
capped, and its `totals` say so.

The id is 10 Crockford base32 characters: 42 bits of start time in ms, then 8
random bits, so ids sort by start. Uniqueness comes from `create_dir` (not
`create_dir_all`), regenerated on `EEXIST`.

## The recording: `session.db`

One SQLite file per session. Its header says what it is:
`PRAGMA application_id` is `0x48455048` ("HEPH") on every heph recording,
`PRAGMA user_version` is its format (1), and bytes 18–19 say whether it is
still in WAL mode (live, or a seal cut short) or sealed (rollback mode).

| Table | Holds |
| --- | --- |
| `session` | exactly one row: id, `format`, heph version, owner pid, `host`, `user`, `actor_kind` (`human`, `agent`, `ci`), `actor_name` (`claude-code`, `github-actions`, `unknown`), `ci_run_url`, `command`, `argv` (a JSON array), `cwd` (`HEPH_CWD` when set), workspace root and home, `git_head` (always NULL for now), label, interactive, `capabilities`, `lossy` (argv or cwd lost bytes: a NUL or invalid UTF-8 stored as U+FFFD), `started_ms`, `ended_ms`, `exit_code`, `panicked`, `truncated` and `truncated_seq` (set once the recording reached its 1 GiB ceiling; passing only the 256 MiB full cap shows as the first `heph.truncated` span event and in `status`), `derivation`, `sealed`, `snapshot` (0 in a local recording; 1 in an export taken of a session still running) |
| `records` | the source of truth, in stream order: `seq`, `at_ms`, `kind` (the record's variant name), `addr`, and `body`, the record as JSON (see [Records](#records)) |
| `status` | one row: the latest `SessionSnapshot`, upserted at most once a second and only when changed, and the final one at exit |
| `notices`, `failure_logs` | empty in a local recording; filled only by an export (see [Export](#export-hephsession)) |
| `spans`, `span_events`, `metrics` | the explorer tables, derived from `records` (see [The explorer](#the-explorer)) |
| `logs`, `sessions`, `targets` | explorer views over `records`, `session` and `spans` |

Actor detection reads the environment only, never sets it: `CLAUDECODE=1` is
an agent (`claude-code`), before `GITHUB_ACTIONS=true` (CI,
`github-actions`, with `ci_run_url` from `GITHUB_SERVER_URL`,
`GITHUB_REPOSITORY` and `GITHUB_RUN_ID`, left NULL when any of the three is
missing, empty or holds a control character), before `CI` set to anything
but empty, `0`, `false` or `no` (CI, `unknown`; so `CI=off` is CI);
anything else is `human`. A variable whose name or value is not UTF-8 is
ignored. None
of the session row's fields reaches a hash, or a target's argv, env or files
(I1).

The six explorer names (`sessions`, `spans`, `span_events`, `metrics`,
`logs`, `targets`) are the public contract (I9, below). The tables behind
them (`session`, `records`, `status`, `notices`, `failure_logs`) are
internal and may change within a format.

### The explorer

`spans`, `span_events` and `metrics` are OpenTelemetry-shaped rows derived
from `records` as they arrive, by one deriver that serves the live recorder
and a re-derivation from `records` alone, so the two write the same rows
(I4). `session.derivation` names the version of the code that derived them.

```
heph.session                     root, span_id 1; trace id derived from the session id
├─ heph.startup                   session start -> first ResultStart
├─ heph.result  //p:t            ResultStart..ResultEnd; addr, driver, outcome
│  └─ phase spans                 from ResultEnd.spans: heph.deps, heph.resolve, ...
└─ heph.teardown                  last ResultEnd -> End
```

- **Spans stay flat.** Every `heph.result` is parented by the session span,
  never by a dependent, so a memoized result reached from several dependents
  is drawn once. `outcome` is `ran`, `hit`, `failed` or `cancelled` (a result
  still open at the end has none); `status` is `ok`, `error` or `unset`.
  Its attributes are listed under [Attribute keys](#attribute-keys).
- **Several results of one addr.** Two scopes in one process (two requests,
  or a request and the TUI's own) can resolve the same addr at once and
  share one run. Each `ResultStart` opens a `heph.result` span, and each
  `ResultEnd` closes the **oldest** span still open for that addr (FIFO);
  phase spans become children of the span it closes. The oldest open span
  carries the run: the resource and admission attributes, the cache tier
  events, `heph.cache` and `heph.cache.hit_rebuilt`. Every span open when
  `ExecuteStart` arrives (and one that opens later while any of them is
  still open) reads outcome `ran` with the driver: a waiter shares the run,
  so it is not a hit.
- **Phase spans** are the children of a result, read from the optional
  `ResultEnd.spans` list (`{name, start_unix_us, end_unix_us}`). They are
  read tolerantly from the record's body: a result from a host that does not
  send them simply has no children. `class` is assigned by name:
  `heph.process` is `target`, `heph.driver.run` is `driver`;
  `heph.resolve`, `heph.cache.read`, `heph.stage`, `heph.collect` and
  `heph.cache.write` are `heph`; `heph.deps`, `heph.lock_wait`,
  `heph.admission` and `heph.scratch` are `waiting`. A name this heph does
  not know is recorded with no class.
- **Span events**, each with the record's time:
  - On the result's span: `heph.cache.local_hit`, `heph.cache.local_miss`,
    `heph.cache.remote_hit` and `heph.cache.remote_miss`, one per cache tier
    probed (a probe, not the outcome: the span's `heph.cache` attribute is
    the canonical outcome, and a consumer reads that); `heph.cache.hit_rebuilt`
    (attribute `heph.cache.was`, the hit's tier) when a target whose outcome
    was a hit is rebuilt after all because the manifest outlived its blobs.
    `heph.cache` keeps the hit; this event on the span means it became a
    miss. `heph.cache.stale_output_changed` when a target rebuilt because
    its local entry was stale produced different outputs than that entry,
    so its dependents rebuild too.
  - `heph.codegen.excluded` on the result's span, or the session span when
    no result is open: a generated file kept out of a glob.
  - On the session span: `heph.lock.holder_seen`, once per pid the first
    time this run sees it holding a lock it waits on; `heph.truncated` at
    each cap the recording passed (full cap, then ceiling).
  - `heph.approval.prompt` and `heph.approval.cleared`, on the session span.
    An `Approval` record is written each time the approval banner's state
    changes: the pending prompt (its queue of targets) or the standing Y/N
    decision. `heph.approval.prompt` is a record with a prompt on screen (a
    new prompt, or one whose queue changed); `heph.approval.cleared` is a
    record with none (the prompt was answered, or only the standing decision
    changed with no prompt up).
- **Metrics** (see [Pools](#pools)): `heph.pool.capacity`, `heph.pool.held`,
  `heph.pool.waiting` and, for the `sandbox` pool, `heph.pool.removals_pending`,
  gauges from each `PoolUsage` event, attribute `heph.pool`; at the end, per
  pool, `heph.session.pool.held.mean` (time-weighted over the pool's in-use
  window: from its first `PoolUsage` row to the session's end, not from the
  session's start), `heph.session.pool.held.peak` and
  `heph.session.pool.waiting_us` (the time with at least one request
  waiting, not the sum of every request's wait), and `heph.session.results`
  per outcome (attribute `heph.outcome`). `heph.session.results` counts a target once per
  resolution, not once per `ResultEnd`: overlapping results of one addr count
  once, when the last of them closes, with the worst outcome among them
  (`failed`, then `ran`, `cancelled`, `hit`); the same addr resolved again
  after they all closed (a second request in the same process) counts again.
  A pool's unit comes from `RequestConfig.units`: a `cpu`
  pool in CPUs (`{cpu}`, the engine's micro-CPUs over 10^6), a `bytes` pool in
  `By`, anything else `1`.
- **Views**: `logs` (`Log` and `Output` records: `session_id`, `seq`,
  `time_us`, `severity`, `body`), `sessions` (the session row as resource
  attributes, plus `schema_version`), and `targets` (`session_id`, `addr`,
  `outcome`, `duration_us`: one row per target per session, cache hits
  included). Where an addr has several `heph.result` spans, `targets` keeps
  the one with the worst outcome (`failed`, then `ran`, `cancelled`, `hit`,
  then none), the oldest among equals (the run's carrier); `spans` keeps
  them all.

#### Attribute keys

| Key | On | Value |
| --- | --- | --- |
| `heph.cache` | `heph.result` span | the cache outcome: `local_hit`, `remote_hit`, `miss` (open) |
| `heph.cache.stale_host_path` | `heph.result` span | on a miss because the local entry was stale: the declared host path found gone, as the entry stored it |
| `heph.cache.reason` | `heph.result` span | why the outcome is what it is, as prose for a human (today only a stale miss); match on `heph.cache.stale_host_path`, not on this text |
| `heph.upstream_of` | `heph.result` span | the addr whose failure stopped this one |
| `heph.timed_out` | `heph.result` span | `true` when its run timed out |
| `heph.resources.declared.<pool>` | `heph.result` span | what the target declared of `<pool>`, in the pool's unit |
| `heph.resources.reserved.<pool>` | `heph.result` span | what admission reserved of `<pool>`, in the pool's unit |
| `heph.admission.queued_ms` | `heph.result` span | how long it waited for admission |
| `heph.cache.was` | `heph.cache.hit_rebuilt` event | the hit's tier |
| `heph.codegen.path`, `heph.codegen.status`, `heph.codegen.reason` | `heph.codegen.excluded` event | the file, why it was excluded (`reason` optional) |
| `heph.lock.holder_pid`, `heph.lock.holder_session` | `heph.lock.holder_seen` event | the holder's pid, and its session id when known |
| `heph.pool` | `heph.pool.*`, `heph.session.pool.*` metrics | the pool's name |
| `heph.outcome` | `heph.session.results` metric | `ran`, `hit`, `failed`, `cancelled` |

The trace id is the first 16 bytes of SHA-256 over `heph.trace.v1:`
followed by the session id; a span id is written as its 8-byte big-endian
value, never 0. Both are frozen, as are released span and metric names.
Enumerations (`class`, `status`, `outcome`, `metrics.kind`) are open: a
consumer handles a value it does not know.

Two recipes:

```sql
-- Where did time go, by class and phase (busy time, not wall time)?
SELECT class, name, count(*) AS n, sum(duration_us) / 1e6 AS busy_s
FROM spans WHERE class IS NOT NULL
GROUP BY class, name ORDER BY busy_s DESC;

-- How did //p:t end, and who ran it?
SELECT s.started_ms, s.user, s.actor_name, s.argv, r.outcome, r.duration_us / 1e3 AS ms
FROM spans r JOIN sessions s USING (session_id)
WHERE r.name = 'heph.result' AND r.addr = '//p:t';
```

There is no query command yet; `sqlite3` on a sealed recording (or a copy of
a live one) answers them.

### Pools

`show` on an ended session prints one line per bounded pool that had rows,
from the `heph.session.pool.*` metrics and the last `heph.pool.capacity`:

```
docker pool (capacity 4): held 3 mean while in use (75%), 4 peak; something was waiting for 1s
cpu pool (capacity 8): held 7.6 mean while in use (95%), 8 peak; nothing waited for it
```

"While in use": the mean is over the pool's in-use window, from its first
row to the session's end. "Something was waiting" is `waiting_us`, the time
with at least one request queued, not the sum of every request's wait.

A live session has none yet (the rows come at its end), and neither does a
build that admitted nothing (an all-hit build). These rows are read from
events the engine already emits; nothing flows back into admission, a
target, or a hash.

## Records

One row of `records` has a `body` like

```json
{"at_unix_ms": 1794666004822, "kind": {"type": "ExecuteStart", "addr": "//p:t", …}}
{"at_unix_ms": 1794666004822, "session": {"type": "End", "exit_code": 1, "panicked": false}}
```

`kind` is a `BuildEventKind` byte for byte as on the plugin wire
(`crates/core/src/events.rs`), with the same skew rules: an unknown variant
decodes as `Unknown`. `session` is a `SessionRecordKind`: `Log {level, msg,
seq}` (every line the terminal shows, at the terminal's own filter, so
`debug!` lines under `HEPH_LOG=debug` too, and diagnostics with an empty `level`; lossy
UTF-8, stored verbatim and escaped by the viewer on output; `seq` numbers the
sink's lines from 1), `End {exit_code,
panicked}`, `Truncated`, `InflightWritten`, `LockHolder {pid, session}` (the
run resolved a lock holder's pid to a session, or found none: see [The run's
own lock-wait notice](#the-runs-own-lock-wait-notice)), and `Unknown` for
anything newer. Both keys are optional; a body with neither is skipped.

What the run's terminal showed besides events and log lines is recorded too,
so `attach` draws it (see [Attach](#attach)):

- `Label {label}`: the TUI's bottom label changed (the session row holds the first).
- `Approval {prompt, standing}`: the approval banner changed; `prompt` is
  `{queue: [{seq, addr, queued_at_unix_ms, notices: [{name, sources: [{addr,
  origin}]}]}]}`, every pending request oldest first (the front one is what
  the banner names), or absent once nothing is pending. `origin` says where one artifact's bytes
  are kept, tagged by `kind`: `cache {addr, revision, hashout}` (an entry of
  the local cache under the run's home), `file {path, out_path, x, hashout}`
  (a source file read in place, `path` relative to the workspace root), or
  `view {source, paths}` (a `group` relocation of another origin, as
  `(source path, path)` pairs). Never the notice's content, and no notice is
  written anywhere or held whole in memory: the running process and every
  viewer read it from the store by `sources`, through the same reader
  (`ApprovalPromptNotice::read` with the engine's `read_notice_from_store`:
  read-only, no build, no fetch, no write), rooted at the session's home and
  at the session row's `workspace_root`. A `cache = False` input, whose revision would
  otherwise live only in the run's memory, is spilled into the durable local
  cache before the gate asks, under its own `{hashin}_{nanos}` key, which no
  lookup hits. The reader stops at 4 MiB (`NOTICE_READ_CAP`); past it the
  text ends with `… notice truncated: N of M bytes shown; full text: heph
  inspect outputs <addr>`. A source not where it was recorded — a revision
  gc'd since, a source file changed or gone — or a record with no `sources`
  reads as "notice unavailable". A record from an older heph, whose sources
  have another shape, does not decode: there is no fallback. `standing` is
  `approve` or `reject` once Y or N set a standing decision for the build,
  absent otherwise.
- `Finished`: the TUI was torn down; `at_unix_ms` is the instant its final
  summary was measured against.
- `Output {text}`: text the process wrote straight to its terminal, today the
  failure report printed after the teardown; its first 1 MiB, with a marker
  line when it was cut.

`RunLog {addr, hashin, file}` says where a running target's live log is:
`file` is relative to the session row's home (`runs/<id>/<n>.log`), written when
the run's first byte creates it, and past the full cap. A session with no
`RunLog` of an addr (a heph from before them, say) simply has no live log
of it. A reader (`tui::session::find_run_log`) trusts it
only for a session whose session row names its own home, while that
session's lock is held and no `ExecuteEnd` for the addr follows it; a file
gone under such a record means the run ended. `file` must be
`runs/<dir>/<name>`, reached through no symlink, or it reads as absent.

A heph that does not know a record kind decodes it as `Unknown` and skips
it. A reader skips and counts a body that does not decode and never stops on
one; `show` says how many it skipped, and `show --json` carries the count as
`skipped`. `Log`, `End` and `LockHolder` never become `BuildEventKind`
variants: a plugin hook must not receive them.

## Versioning

Three things are versioned separately, and each is bumped only on an
incompatible change:

- **The recording's format**, `PRAGMA user_version` in its header (the
  session row repeats it; the header wins), versions the directory contract
  and the recording: file names, the lock, the id scheme, cleanup, the
  schema. A reader decides from the header alone: a file whose
  `application_id` is not heph's is "not a heph session"; a newer format is
  refused by name, by `show`/`attach`/`inflight` naming both versions,
  while `list` shows a placeholder row ("format N, needs newer heph") and
  exits 0; every older format is read, back to 1. Each released format has a
  committed fixture that a test opens
  (`crates/tui/src/tui/session/testdata/format-N/session.db`), and a format
  bump adds one. `show` run by a heph older than the session row's
  `heph_version` warns that the session may be shown incomplete; `list`,
  `attach` and `inflight` do not warn.
- **`SessionSnapshot.version`** versions the `status` row on disk, and
  nothing else: `show --json` leaves it out (its stability is the CLI's,
  below).
  It is still 1 with the list cap and `totals`, because no heph has shipped a
  snapshot yet: version 1 is what the first release carries, cap included.
  The cap is not an additive change, since it changes what the existing lists
  mean (the first 100 entries, not all of them), so a reader that ignored
  `totals` would misread a cut list as whole. From the first release on, a
  change of meaning like that bumps the version; only new fields come without
  one.
- **Record bodies** only grow additively: new record kinds and fields,
  never a changed meaning.
- **The explorer contract** (I9) is `sessions.schema_version` (1). Within a
  format the six names only gain tables, columns, names and enum values; a
  derivation bump may add rows or attributes, and may correct a value that
  was wrong.
- **Within a format the schema only grows** (I5). Every table, view and
  index of the format's fixture stays, its columns in their order; new ones
  may be added beside them. The stored definitions of the six public names
  in a released recording never change: a recording carries its views, and
  no heph rewrites a sealed file. A later contract that grows them (a new
  column of `spans`, a new view) reaches older recordings through the TEMP
  views a later heph puts in front of them, as `heph session query` does
  (spec D20), not by editing what is stored. The fixture test checks exactly
  this: a subset, in order, everywhere, and the six names' columns unchanged.

New fields are `#[serde(default)]` and unknown ones are ignored. Two things
are frozen for **every** format: the `lock` file name and its `flock`
meaning. That is what lets cleanup, which reads at most a recording's
100-byte header, work on a session of a format it cannot read: a held lock
over an unreadable recording is a live session of unknown format.

There is no reader for the `meta.json` / `events.ndjson` / `status.json`
directories an earlier heph wrote (spec D9): it has no `session.db`, so
`list` skips it, `show` does not find it, and retention removes it by age.

## Reading

A viewer opens a local recording one of three ways, from the lock and the
header (`store::open_local`):

| Lock | Header | Opened as |
| --- | --- | --- |
| held | WAL | **live**: a read-only connection with a 1s `busy_timeout`. A follower re-reads `records WHERE seq > last` whenever `PRAGMA data_version` moves, and drops its connection as soon as a batch holds `End`, so the owner can seal. If the session row already says `sealed=1`, the holder (the owner after `End`, or a cleanup) is sealing: the viewer closes the connection, which would cut the seal short, and waits up to 3s for the header to turn rollback (read **sealed**) or the lock to free (read a **copy**); past that it reads in place, as ended. Never **live**. |
| any | rollback | **sealed**: `immutable=1` (no `-shm`, no locks, works on a read-only copy). A held lock over a rollback-mode header is a cleanup that just sealed it: read as ended. `sealed=1` alone never chooses `immutable=1`. |
| free | WAL | **copy**: a lost session, or a seal cut short. The viewer takes the session's `lock` **shared** while it copies `session.db` and its `-wal` into `<sessions root>/.tmp/`, releases it, and opens the copy (WAL recovery runs on the copy). A cleanup that finds the lock held shared does not wait for it: it leaves the session unsealed and uncounted, and the next cleanup (a session start, at most hourly) seals it. Every other probe still reads the session as not live. A viewer that finds the lock held exclusively (a cleanup sealing it) waits for that seal exactly as in the first row: up to 3s for the header to turn rollback or the lock to free, then in place, as ended. The copy is removed when the viewer is done; an orphan is swept by cleanup after 10 minutes. |

None of the three writes the original: a live WAL reader's only file write
is SQLite's `-shm` index (I3). No reader opens with `SQLITE_OPEN_CREATE`, so
none creates a recording.

## Liveness and sealing

A session is alive exactly while its `lock` is `flock`ed (I2). SQLite's own
locks are `fcntl` locks, released when any fd on the file closes, so they
never carry liveness.

At exit the owner writes `End`, the final `status`, and the session row's
`ended_ms`, `exit_code`, `panicked` and `sealed=1` in the last WAL
transaction; then it **seals** the file: `wal_checkpoint(TRUNCATE)`,
checking its busy column (a viewer mid-read makes it busy with rc OK), then
`journal_mode=DELETE`, checking the mode it returns. It retries while a
viewer is in the way for at most 400ms (less if the exit's 3s bound has
less than that left after a 250ms margin), then releases the lock. A
follower closes on `End` well inside that; a connection that stays open (a
`sqlite3` shell on the live file) cuts the seal short, so the process exits
on time and the next cleanup seals the recording. Nothing writes the file after the mode switch, so the
rollback-mode header is the only seal test: `sealed=1` in a WAL-mode file
means the seal was cut short, and readers take the copy path for it.
`sealed=1` therefore means `End` and the session row's end fields are
committed, not that the file is immutable: a cleanup that later seals such
a file only finishes the checkpoint and mode switch, and keeps the owner's
`ended_ms`, `exit_code` and `derivation`.

A session killed before its seal (`SIGKILL`, a second Ctrl-C) leaves a
WAL-mode recording with no `End` and a free lock: `lost`. The next cleanup
past [`STARTING`](#retention) (cleanup runs at most hourly, so up to an
hour later) **seals** it under its lock taken exclusively:
`ended_ms` is its last record's `at_ms`, `exit_code` stays NULL, and the
file is switched to rollback mode. Cleanup runs during a build, so it
repairs nothing (I1): the explorer rows stay what the crash left, without
the session-wide rows it never wrote, and `session.derivation` is set to
`0` (older than any real derivation) so that a session command re-derives
the recording before it reads explorer rows from it. It refuses beside a
`-journal`, and it never seals a format it does not know. A seal cut short
there leaves the file for the next cleanup.

### `--json`

`list`, `show` and `inflight` take `--json`, `--fields` and `--jq`, with the
rules every heph command follows (`docs/JSON_OUTPUT.md`): `--fields help`
lists each command's fields.

`list --json` is an array, one element per session, `[{id, owner_pid, argv,
workspace_root, cwd, state, started_unix_ms, last_event_unix_ms, exit_code,
panicked, progress, now, blocked_on, incomplete, format}]`, sorted by id,
times in absolute unix ms; `[]` when there is no session:

- `progress` is PROGRESS as numbers, `{done, total, total_complete,
  running_ops, failed}`, `null` before the session wrote its first
  `status` row:
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
  or `{kind: "admission", addr, pool}`. From the `status` row, so at most the
  first 100 of each kind.
- `incomplete` is true when the recording passed its size cap and so is
  short of the run (the table adds `incomplete` to PROGRESS); `null` when
  this heph cannot tell: a newer-format row, or a status row it cannot read
  (`state` is then `unknown`). `show --json`'s `incomplete_reason` says why.
- `format` appears only on the row of a session in a newer recording format,
  which this heph reads no further: that row has `owner_pid: null`, empty
  `argv`, `null` progress and `state: "unknown"`. `owner_pid` is never `null`
  otherwise.

`show --json` is one object: the viewer's `id`, `state`, `blocked_on`,
`chain_root` and `next`, then the snapshot field by field
(`approval_standing`, present only while a standing approval decision acts on
the build, `first_event_unix_ms`, `last_event_unix_ms`, `counts`, `running`,
`waiting`, `failed`, `lock_waits`, `admission_waits`, `totals`, `truncated`,
`end`), then `skipped`, `incomplete`, `incomplete_reason` and `pools`. The
snapshot's own `version` is not printed: it versions the `status` row on
disk, not this output. The snapshot is folded from `records`, every list in
full, unless the recording reached its 256 MiB full cap: then it is the
`status` row, each list capped at 100 entries, and `totals` says which were
cut. `blocked_on` has `list --json`'s shape, but from that same snapshot, so
it is in full where `list`'s, which comes from the `status` row, has at most
100 entries of each kind. `skipped` is the number of records that did not
decode and were skipped (0 normally). `incomplete` is true when the
recording is short of the run, and `incomplete_reason` says why, `null` when
it is not. It is an open enum: a consumer reads a value it does not know as
"incomplete" and the text as the reason. Today the only value is `size_cap`:
the recording passed its 256 MiB full cap, so output and log lines (and
progress-only events) were dropped from there, and if it then reached its
1 GiB ceiling, nothing but its `End` was recorded after that (see
[Recording](#recording)). The text says which cap, how far into the run, and
what was dropped. `pools` is `[{pool, unit, capacity, held_mean, held_peak,
waiting_us}]`, one
per bounded pool with rows (see [Pools](#pools)): empty while the session is
live, and for a build that admitted nothing. `capacity` is `null` when no
row gave one. The comparison with machine samples (`declared_above_use_us`
and the rest) comes later, as added fields. `chain_root` is the id of the
session at the root of
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

A recording covers the whole run when `incomplete` is `false` and `state` is
not `"lost"`, in `list --json` and `show --json` alike. `incomplete` speaks
only of the size caps: a `lost` session's process went away without
recording its end, so its last events may be missing whatever `incomplete`
says (see "Why is my session `lost`?" under [Recipes](#recipes)).

`inflight --json` is `{id, report}`.

A shipped field is never renamed or removed and never changes type; a change
that would is a new field (`docs/JSON_OUTPUT.md`, "Stability"). No output
carries a version to bump. `state` is computed by the viewer and may
gain values; a consumer treats one it does not know as unknown, not as an
error. `state` also depends on the viewer's `--stall-notice` (the `quiet`
threshold), so two viewers can disagree on it; `last_event_unix_ms` is the
stable signal for "has it gone silent".

## Invariants

| # | Rule |
| --- | --- |
| I1 | Watching never changes the build. The recorder reaches no hash and no target's argv, env or files; a recording failure never fails, blocks or slows a build task. The session id and directory are **never exported to the environment**: a session runner or `pass_env = ["*"]` would carry them into targets. |
| I2 | A session is alive exactly while its lock is held. The fd is `O_CLOEXEC`, so a child outliving heph cannot keep it alive. The `lock` name and meaning are frozen for every format; SQLite's locks never carry liveness. |
| I3 | While the lock is held only the owner writes `session.db`. After that, only a cleanup holding the lock exclusively writes it, to seal it or remove it. A viewer writes no row; its file writes are `inflight.request` and, for a live WAL reader, SQLite's `-shm` index. |
| I4 | `records` is the one source of truth. `status` and the explorer tables are derived from `records` plus the session row, and a re-derivation from `records` alone writes exactly what the recorder wrote live, a capped recording included. |
| I5 | A record's `kind` is a `BuildEvent` exactly as on the plugin wire. The recording names its format in its header; a reader refuses a newer one by name and reads every older one, and each released format has a committed fixture a test opens. |
| I6 | A session holds nothing the terminal would not have shown (argv, log lines, failing log tails), user-only. |
| I7 | A viewer **never signals** a process. `inflight` drops `inflight.request`; the owner's diag sweeper (one `stat` per 200ms tick) writes `inflight.txt`, removes the request and records `InflightWritten`, which the viewer waits for. The session row's `capabilities` must list `inflight`. |
| I9 | The six explorer names (`sessions`, `spans`, `span_events`, `metrics`, `logs`, `targets`) are the public contract. Within a format they only gain tables, columns, names and enum values; released span and metric names, the trace-id rule and the span-id encoding never change. |

### Recording

The process's session is a `SessionSlot` that `main` owns: there is no
process-wide instance. `main` configures it and hands it to whatever records
into it — every command, as its own argument beside the sink;
`bootstrap::new_engine` (the workspace for the session row); the `LogSink`, which
keeps a reference only to tee its lines into the open session; the diag
sweeper; and the error renderer. `tui::run_app` takes the session as an
explicit argument (a debug build asserts it is the sink's slot) and opens it
lazily (a viewer's `App::records_session()` is false), and `main` finishes it with the exit code
just before it returns its `ExitCode`, ahead of anything slow on the way out
(telemetry, the profiler). Both
backends consume a tapped receiver that moves each event to the recorder right
after the view folds it, so what is recorded is what was shown, paused TUI
included. The recorder is its own OS thread behind an unbounded channel:

- Records go into one transaction per flush tick, committed 250ms after the
  first uncommitted record, as soon as it holds 512 KiB of records and
  explorer rows, and at exit, with the explorer rows they complete
  (`journal_mode=WAL`, `synchronous=OFF`: a process crash loses nothing
  committed). The size bound keeps the commit the exiting process waits on
  small: a fast build fits its whole run in one tick, and committing that at
  exit cost 8 ms on a 12.5k-target no-op. The `status` row is upserted in the same transaction, at most
  once a second and only when something changed.
- At exit it writes `End`, the final `status` and the session row's end, and
  seals the file (see [Liveness and sealing](#liveness-and-sealing)),
  **then** releases the lock, so "lock free and no `End`" means exactly
  "crashed". A panic unwinding through `run_app` writes `End {panicked:
  true}`. The join at exit is bounded (3s). A seal cut short is only a
  `debug!`: `End` is committed, and the next cleanup seals the file. A join
  that times out leaves the lock with the recorder thread, which still holds
  the write connection: it releases the lock once its `End` and seal are done
  (or failed), and the process's exit releases it otherwise. The lock never
  goes free while a writer may still commit.
- On a write failure it rolls back the open transaction and latches
  "stopped" before its one `warn!` (which the log tap would otherwise feed
  back to it) and drops its receiver; it stops the same way past 500k pending
  events, and when the filesystem refuses WAL mode. A stopped recorder keeps
  its lock until the process exits, so the session reads as live while it is.
- **Caps.** The first **256 MiB** of record bodies is a full recording. Past
  it, a `Truncated` record marks the point and only the critical kinds are
  kept: every kind an explorer row is derived from (`RequestConfig`,
  `ResultStart`, `ResultEnd`, `ExecuteStart`, the cache-tier, cache-outcome,
  lock-wait, admission-wait and scratch events, `PoolUsage`,
  `CodegenExcluded`, `LockHolder`) and what a viewer needs at the end
  (`End`, `InflightWritten`, `Finished`, a `Label` change, every `Approval`,
  `RunLog`). `Log`, `Output` and progress-only kinds (`Matched`,
  `ExecuteEnd`, cache-write and remote read/write progress, …) are dropped.
  At the
  **1 GiB** ceiling (record bodies plus explorer rows) a second `Truncated`
  record stops the recording; derivation stops at that same record, and
  `truncated=1` and `truncated_seq` are set in the session row, so a
  re-derivation from `records` is exact (I4). Only `End` follows, and the
  `status` row keeps updating. `show` then says the recording is
  incomplete. An `Output` record keeps at most the first 1 MiB of its text,
  then a line saying how many bytes were left out. Both caps count record
  bodies (the ceiling adds an estimate of the explorer rows' bytes), not
  SQLite's pages, indexes or `-wal`: `session.db` on disk is larger than the
  bytes counted by its page overhead, under 1% with large records and more
  with many small ones, and `keepBytes` counts the files as they are on
  disk.
- The `LogSink` numbers every line it takes, a log line at the terminal's
  filter or a diagnostic, and records it as `Log {seq}`. Numbering, sending
  the record and handing the line to the printer are one critical section of
  the sink's lock, so the printer and `records` see the same numbers in
  the same order, none reused or skipped.
- `SessionTap::flush` waits (bounded) until every record sent before it is on
  disk, and fails at once on a stopped or finished recorder rather than
  waiting for it.
- While a full screen is up the printer keeps only line numbers. Closing it
  replays from disk (`LogSink::replay`): take the latest number, flush through
  it, then read `Log` records after the `seq` noted when the screen opened
  and print exactly those numbered after the last line printed and up to that
  latest, once each. Lines missing from disk (past the full cap, dropped by a
  stopped recorder) are counted, and one `WARN` says how many were skipped.
  This holds under `HEPH_NO_SESSION=1`, whose private session is what the
  replay reads.

What a run loses: a process wedged before its first flush loses at most 250ms
of events; a second Ctrl-C, `SIGTERM`/`SIGHUP` or `SIGKILL` loses what came
after the last commit and writes no `End` (`lost`, sealed later by cleanup).

### The run's own lock-wait notice

A run waiting on a lock names the holder in its own progress output, on both
backends: `held by session <id> (pid N)` (the TUI row says `locked by …`) when a
live session other than its own is recorded for the pid, and `held by pid N`
otherwise. The events carry only the pid, so the engine stays unaware of
sessions: the pid is resolved with the same rule as `list`/`show` (a live
session, not this one, whose session row names the pid). The backend's event
receiver does it, once per distinct pid, on a detached thread, before it folds
the event that first carries the pid; that event waits for the scan, later
events queue behind it, and the build itself never waits. The wait is at most
200ms: past it (a hung `~/.heph` on NFS) the event is folded with the pid, the
pid is not scanned again, and an answer that comes later still names it from
then on. The thread is detached rather than on the blocking pool because a
runtime's shutdown waits for its blocking tasks: a hung scan never holds up the
exit. The scan probes each
session's lock first and opens only the live ones' recordings, reading
their session row, never `records` or `status`; so it never copies an ended
recording. The view only reads the cached answer, so neither the fold nor a
render does IO. The directory comes from `HOME` alone, so a run
with `HEPH_NO_SESSION` still names other sessions; with no `HOME`, or any read
error, the notice is the pid.

The run records each answer once per pid as a `LockHolder {pid, session}`
record (`session` absent when no live session holds it, or when the scan did
not answer within the 200ms; a later answer is not recorded again), so a
viewer names holders from the recording instead of scanning (spec D6).

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
| Bottom label | the header model's | the latest recorded `Label`, else the session row's |
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
isolate controls and the implicit direction marks (`U+202A`–`U+202E`,
`U+2066`–`U+2069`, `U+200E`, `U+200F`, `U+061C`, which reorder how a line
displays), and a truncated or malformed sequence. A parameter list over
128 bytes is malformed too: no real SGR comes near it (a truecolour
foreground and background with attributes is under 60 bytes), so its `ESC`
is escaped and the rest prints as text. The scan for the `m` stops at 129
bytes, so the check stays linear whatever the input. This reverses spec amendment A11 (drop every
SGR on output) for `attach`'s replayed `Log` and `Output` text only; `list`,
`show` and `inflight` still drop SGR (`escape_replayed` vs `escape_text` in
`src/commands/session/mod.rs`).

Lock holders are named exactly as the run named them: `SessionFeed` fills
its holder names from the recorded `LockHolder` records as it routes them,
and no viewer scans for pids. A holder that has exited since is still shown
by the session the run saw.

`attach` follows the recording with a `RecordCursor`, in batches of up to
256 records, and drops its connection as soon as a batch holds `End`,
before it hands the batch on, so a slow terminal never holds up the owner's
seal.

On a terminal, `attach` always draws the TUI, even for a session the run
recorded with the line backend (session row `interactive: false`): the viewer's
terminal decides, not the run's. Without a terminal it follows in line form,
which for a line-backend session is its recorded log lines alone.

## Export: `.hephsession`

```
heph session export <ref> [-o PATH|-] [--no-logs]
```

A `.hephsession` file is one session's recording, made to be read on another
machine: a zstd frame whose content is a sealed `session.db` (format 1, the
same tables as a local recording). `export` takes a consistent copy with
`VACUUM INTO` a temp file, fills the two tables a local recording leaves
empty, compresses it, and renames it into place:

- `-o PATH` writes `PATH`, replacing a file already there; the default is
  `./heph-<id>.hephsession`. `-o -` writes the bytes to stdout, and is
  refused when stdout is a terminal (`-o ./-` is a file named `-`). On
  success it prints the path written on stdout.
- A **running** session exports too: the copy is a snapshot of it so far,
  its session row says `snapshot = 1`, `export` says so on stderr, and every
  viewer of the file says "snapshot of a running session".
- `notices` holds the text of each approval notice the session showed, read
  as the live banner would, at most 4 MiB per notice.
- `failure_logs` holds the failure log of each **root** failure (never the
  collateral ones failed by it), its last 1 MiB each and 32 MiB in all, with
  `cut_bytes` saying how much of its start was left out.
- `--no-logs` leaves both tables empty, for a file that leaves the machine
  without the text of a notice or a log. The records themselves (addresses,
  argv, cwd, the log lines the run printed) are still in it.

A `<ref>` that is already a file is written again: its own `notices` and
`failure_logs` are kept, or emptied by `--no-logs`.

A reader sniffs the leading bytes, never the name:

| Leading bytes | Read as |
| --- | --- |
| `28 B5 2F FD` (zstd) | decompressed into the sessions root's `.tmp/` (at most 4 GiB), then opened |
| `SQLite format 3\0` | opened in place |
| `PK 03 04` (zip: what a browser download of a CI artifact gives) | the one `*.hephsession` entry inside; zero or several is an error that lists the entries |
| anything else | an error, `not_a_session` |

Every file is opened read-only and `immutable=1`, with `trusted_schema` off,
and its text reaches the terminal escaped, as a live recording's does.

## Opening a recording: `<ref>`

`show`, `attach` and `export` take a `<ref>`, resolved once, in this order:

1. **`https://…`** is a URL: downloaded into the sessions root's `.tmp/`
   (at most 1 GiB), then sniffed. `http://` is refused, and so is a URL
   carrying credentials (`https://user:token@…`), so a token never reaches
   an error message. There is no URL authentication: for a private CI
   artifact, download it first and open the file (see [In CI](#in-ci)).
2. **Anything with a `/` or a `.`** is a path. Ids are Crockford base32 and
   pids are digits, so neither ever has one; `./02kf5` forces a path, and
   `02kf5.hephsession` is a path by its `.`. Nothing is probed on disk to
   decide: a bare `02kf5` is always an id, even beside a file of that name.
3. **Otherwise** a session of this user's sessions directory: the full id,
   the pid of a live session, or a unique prefix or suffix of an id. Digits
   are a live pid first, then part of an id; with neither, the error names
   both readings.

A recording opened by id is **local**. One opened by path or URL is
**foreign**, even when it was exported on this machine, and is read only from
itself: no lock probe, no `inflight`, no run-log or failure-slot lookup, no
read of a notice's source. Its state comes from its session row: `ok` or
`failed` when it was sealed with an exit code, otherwise incomplete (a
snapshot, or a recording copied out of a run that never finished).

- `show <file>` starts with `read from <origin>`, adding `: snapshot of a
  running session` or `: incomplete`; prints each recorded notice and each
  root failure log after the "Stuck on" block; and its `Next:` is `none`,
  since every other next step reads local state. `show --json` adds
  `foreign: {origin, partial, notices: [{addr, seq, name, text}],
  failure_logs: [{addr, log, cut_bytes}]}`, where `partial` is `snapshot`,
  `incomplete` or `null`; the field is absent for a local session. `state`
  is derived as for a local one, so a snapshot reads `lost`, and `partial`
  says why.
- `attach <file>` says `heph: replaying <origin>` on stderr, replays the
  recording once, and exits with its recorded code. A recording with no
  `End` exits 125 ("has no recorded end"), never `lost`.

Downloads and decompressed copies live in the sessions root's `.tmp/` while
the command runs, and are removed when it ends; one a killed command left
behind is swept by the first cleanup after it is 10 minutes old, and
cleanup runs at most once an hour (see
[Retention](#retention)).

## In CI

```
HEPH_SESSION_EXPORT_DIR=<absolute dir>
```

With the variable set, every session that seals also writes
`<dir>/<id>.hephsession`: one file per session, named by its id, so every heph
in the job adds its own, nested ones included, and none overwrites another. A
workflow sets it on the step that runs heph and uploads the directory with
`if: always()`, so a failed job keeps its recordings:

```yaml
- name: Build
  env:
    HEPH_SESSION_EXPORT_DIR: ${{ runner.temp }}/heph-sessions
  run: heph run //...
- name: Upload session recordings
  if: always()
  uses: actions/upload-artifact@v7
  with:
    name: heph-sessions
    path: ${{ runner.temp }}/heph-sessions
    if-no-files-found: ignore
```

To read one, download it and open the file (the artifact URL needs
authentication, which `<ref>` does not do):

```
gh run download <run-id> -n heph-sessions -D sessions
heph session attach sessions/<id>.hephsession
```

- The path must be **absolute**: a relative one is warned about once when
  the session opens and exports nothing; an empty one is off. The directory
  is created if missing.
- A browser download of an artifact is a zip; a zip holding one
  `.hephsession` opens as it is, one holding several must be extracted first.
- The export runs after the recording's write connection closes, with its
  own 30s bound outside the recorder's 3s join. Past it the temp file is
  removed and one `warn!` says so. It prints nothing on success, and a
  failure is one `warn!` that never changes the exit code.
- It includes the notices and root failure logs, as `export` does without
  `--no-logs`. A private session (`HEPH_NO_SESSION=1`) and `--raw` export
  nothing.
- It **never reaches a target**. heph strips it from `pass_env = ["*"]`,
  `runtime_pass_env = ["*"]` and a session runner's launch, and refuses it by
  name in `pass_env` (and in a dependency's `transitive` env passed with
  `hash: true`), where it would move the def hash with the directory a job
  picked. A target that wants it names it in `runtime_pass_env`, which is not
  hashed. An `entrypoint` target runs on the caller's whole environment, so a
  heph it starts writes its own file there, as intended.
- **Don't point it inside a target's outputs** (or anywhere a target reads):
  a heph run by that target would add a new file with a new id on every run,
  and the outputs would differ every time.

## Retention

Cleanup runs only at session start, on a thread of its own beside the new
session's recorder, so a slow seal never holds up the recording; `list` and
every other viewer stay read-only. It decides on lock state,
modification times and a recording's 100-byte header, **never** on its
contents.

It runs **at most once an hour per sessions root**, asynchronously (a user
decision, 2026-11-24: nothing about it needs to be inline). The root's
`.cleanup` stamp holds, in its modification time, when its last cleanup was
claimed:

- A start whose stamp is younger than an hour (`INTERVAL`) does one `stat`
  and nothing else: no thread, no scan. Its cost does not grow with history.
- Otherwise the cleanup thread claims the run and then cleans. The claim is
  atomic across hephs starting together: the first stamp is created with
  `O_EXCL`, so exactly one creator wins; an existing stamp is checked and
  touched under `flock(LOCK_EX | LOCK_NB)` on it, so a heph that finds it
  locked, or finds it freshly touched, goes. The lock covers the check and
  the touch, never the cleanup.
- The claim stamps the root *before* any work. A heph that exits or crashes
  mid-cleanup leaves the rest to the first start an hour later; every step
  is idempotent (a seal under the session's lock, a removal by rename), and
  a crashed claim leaves only the stamp, which expires like any other. A
  stamp more than an hour in the future (the clock went back) counts as
  expired.
- A public start that claims its root also claims, under that root's own
  stamp, the private root beside it (below).

So retention is enforced at the next hourly cleanup, not at once: a removal
past `session.keep` or `session.keepBytes`, a change to either key
(`keep: 0` included), and the sealing of a lost session all take effect up
to an hour later. Readers already handle that: a
lost recording not yet sealed is read through a copy (see
[Reading](#reading)), and a removed one was past retention either way.

| What | Kept | Then |
| --- | --- | --- |
| An ended recording (lock free) | while last written under **7 days** ago, and among the newest that total at most **2 GiB** | the session directory is removed |
| A live session (lock held) | always | never counted, never touched |
| A session still starting: a free `lock` younger than **60s** (`STARTING`), or no `lock` and written under 60s ago | always | neither counted, sealed nor removed |
| A tomb (`.removing-*`) or a reader copy, download or decompressed file in `.tmp/` | **10 minutes** (`SCRATCH_GRACE`) | deleted with no lock |

- There is no count cap. The byte budget counts only sealed recordings in a
  format this heph knows, measured after any seal: the oldest go until the
  rest fit.
- A **lost** recording (lock free, WAL mode, a known format) that age keeps
  is sealed first (see [Liveness and sealing](#liveness-and-sealing)), then
  counted. One that age removes is removed without the seal work.
- A recording heph cannot seal (an unknown format, not a heph recording,
  zero bytes or no schema, a seal that failed or was cut short, or a lock a
  viewer holds) is never written: it is removed by age only, and left out of
  the byte budget.
- Sealing and removal take the session's lock exclusively, so two cleanups
  racing never both seal or remove one session: the second finds the lock
  taken and moves on, and a session cannot start in a directory being
  removed (ids are never reused). A removal renames the directory to a tomb
  (`.removing-<id>-<pid>`) and then deletes it. Two cleanups can race on a
  stale tomb or reader copy, both deleting it, which is harmless.
- Each removal of an ended recording logs one `debug` line, `removed an
  ended session recording`, with its `id`, its `age` and the `reason`:
  `keep` (older than `session.keep`), `keep_bytes` (past
  `session.keepBytes`) or `private` (a private session). Cleanup runs while
  a build starts, so it must not change that build's output (I1): the
  default filter (`info`) neither shows nor records these lines. To see
  them, run any session-recording command with `HEPH_LOG=debug`, or only
  cleanup's with `HEPH_LOG=info,tui::tui::session::cleanup=debug`.
- A session command given an id that matches nothing says that an ended
  recording is removed past `session.keep`, or sooner past
  `session.keepBytes`, with the values in force for the viewing heph, and
  points here.

Both windows are set in `.hephconfig`:

```yaml
session:
  keep: 7d          # how long an ended recording is kept; a bare number is seconds
  keepBytes: 2G     # the most bytes of ended recordings kept, newest first
```

`keep` takes a duration (`7d`, `36h`, `90m`), `keepBytes` a size (`2G`,
`500MiB`, `1048576`). `0` is zero, never "unset" or "no limit": `keep: 0`
keeps no ended recording, so the next cleanup (at most an hour later)
removes every ended one past the 60s start grace however young, and `keepBytes: 0` removes every
sealed one it can count (one it cannot count still goes by age). Live and
starting sessions are untouched either way. A value that does not
parse (`-1`, `5x`, a number past 2^64) is a warning naming its key, and that
field keeps its default: retention is never a reason to fail a build.
`keep` above 90 days is clamped to 90 days, with one warning naming
`session.keep`. The keys are camelCase, like every other `.hephconfig` key;
`keep_bytes` is refused as unknown.
Profiles override each field separately.

Every cleanup also cleans `~/.heph/private-sessions/` with nothing kept
(`Retention::PRIVATE`), at most hourly under that root's own `.cleanup`
stamp: an ended private session (lock free, so a killed run's) goes
whatever its age and is never sealed, a live one is never touched, and one
still starting is left alone as above. A public start sweeps it only when it
has claimed its own root's cleanup; a private start claims the private root
directly. So a killed run's private directory can linger up to an hour,
while a run that ends normally removes its own at once.

## Recipes

The questions asked most, and what answers each. The SQL runs with `sqlite3`
on a sealed recording, or on a copy of a live one (see [Reading](#reading)).
It needs `sqlite3` 3.25 or newer: the `targets` view uses a window function,
and an older `sqlite3` cannot parse the recording's schema, so every query
fails with "malformed database schema".
[Exit codes](#commands-and-exit-codes) say which failures are `heph
session`'s own (125).

**A run just failed: what does an agent run first?** `heph session show <id>
--json`, where `<id>` is the run's start line, or the newest row of `heph
session list --json`. Its `state`, `exit_code` and `incomplete` say how the
run ended and whether the recording covers all of it; `next` is the command
to run after it, with `argv` and `cwd` to run it as is. For a failed run
that is `heph inspect failure <addr>` on its first failed target, which names
the target's log and kept outputs (`--json` for the paths alone). The failed targets and the one each failure came from:

```sql
SELECT t.addr, t.outcome,
       json_extract(r.attributes, '$."heph.upstream_of"') AS upstream_of
FROM targets t JOIN spans r ON r.name = 'heph.result' AND r.addr = t.addr
WHERE t.outcome IN ('failed', 'cancelled');
```

A row with `upstream_of` failed only because that target did; the rows
without one are what to fix.

**Why is my session `incomplete`?** It passed a size cap (`show --json`:
`incomplete_reason` is `size_cap`). Past the 256 MiB full cap, output and log
lines and progress-only events were dropped; past the 1 GiB ceiling, nothing
but its `End` was recorded. `show` names the cap and how far into the run it
was passed; the same, from the rows:

```sql
SELECT name, (time_us - (SELECT start_us FROM spans WHERE span_id = 1)) / 1e6
       AS seconds_into_run
FROM span_events WHERE name = 'heph.truncated';
```

The first row is the full cap, a second one the ceiling. The run itself was
not affected; a target's whole log is still in its
[run log](#run-logs) or `heph inspect failure`.

**Why is my session `lost`?** Its lock is free and it has no `End`: the
process was killed (`SIGKILL`, a second Ctrl-C, the OOM killer, the machine
going down) or crashed before it could finish. Everything it committed until
then is readable. The next cleanup, at most an hour later, seals it (see
[Liveness and sealing](#liveness-and-sealing)): `ended_ms` becomes its last
record's time and `exit_code` stays `null`, so a lost session never reads as
a pass. What it was doing when it went, from `records` alone (a lost
recording's explorer rows stop where it did, and it has no session-wide
rows): the targets that started and never ended, and how far into the run
each started:

```sql
SELECT addr, (min(at_ms) - (SELECT started_ms FROM session)) / 1e3
       AS started_s_into_run
FROM records WHERE kind IN ('ResultStart', 'ResultEnd')
GROUP BY addr HAVING sum(kind = 'ResultStart') > sum(kind = 'ResultEnd');
```

**My session id is gone.** An ended recording is removed once it is older
than `session.keep` (7 days), or sooner when all of them together pass
`session.keepBytes` (2 GiB), oldest first; `heph session show` on a missing id
says which values were in force, and each removal logs a `debug` line with
its reason, shown with `HEPH_LOG=info,tui::tui::session::cleanup=debug`
(see [Retention](#retention)). Raise either key in
`.hephconfig` to keep more.

**Are my `resources` declarations the limit, or the machine?** The pool rows
say how much of each pool was held and how long something waited for it:

```sql
SELECT json_extract(m.attributes, '$."heph.pool"') AS pool,
       (SELECT value FROM metrics c WHERE c.name = 'heph.pool.capacity'
          AND c.attributes = m.attributes ORDER BY c.time_us DESC LIMIT 1) AS capacity,
       max(CASE WHEN m.name = 'heph.session.pool.held.mean' THEN m.value END) AS held_mean,
       max(CASE WHEN m.name = 'heph.session.pool.held.peak' THEN m.value END) AS held_peak,
       max(CASE WHEN m.name = 'heph.session.pool.waiting_us' THEN m.value END) / 1e6 AS waiting_s
FROM metrics m WHERE m.name LIKE 'heph.session.pool.%'
GROUP BY pool;

-- Which targets waited longest for admission, and what they declared.
SELECT addr,
       json_extract(attributes, '$."heph.admission.queued_ms"') AS queued_ms,
       json_extract(attributes, '$."heph.resources.declared.cpu"') AS declared_cpu
FROM spans WHERE name = 'heph.result'
ORDER BY queued_ms DESC;
```

A pool held near its capacity with a long `waiting_s` held work back. Whether
the machine had room while it did needs the machine's own usage beside these
rows, which this heph does not record yet: until it does, compare with the
machine's load over the same window by hand. Nothing here flows back into
admission (see [Pools](#pools)).

## Run logs

A run's live log is not in the session directory. It lives in the workspace's
home, beside the cache and the failure slots: `<home>/runs/<id>/<n>.log`, one
directory per engine (a random id, not the session id) and one file per run
(`crates/engine/src/engine/run_log.rs`).

- **Created lazily.** A run's first output byte creates its file, and the
  first file the directory. A run that prints nothing, and a warm all-hit
  build, never touch `runs/`.
- **Its own lock.** The directory is built as `runs/.new-<id>-<pid>/` with its
  `lock` file already `flock`ed, then renamed into place, and the engine holds
  that lock for its whole life. Liveness is that lock, never the session's:
  with `HEPH_NO_SESSION=1` the session is private and nobody looks it up,
  and a live build's logs must still not be deleted.
- **Lifetime.** A run's file goes once the cache entry or failure slot that
  replaces it is in place. A `cache = false` success keeps its file until the
  engine exits. At a normal exit the engine removes its own directory, lock
  still held.
- **Cleanup.** `heph tool gc`, `heph tool clean` (whatever the selection) and the
  start of every engine sweep `runs/`, the last off the build's path. A
  directory whose `lock` is free or missing (a killed heph's, its
  `cache = false` leftovers with it) is locked, renamed to a tomb
  (`.tomb-<id>-<pid>`) and deleted; one whose lock is held is never touched. A
  `.new-` or tomb directory older than 10 minutes is a crashed set-up or
  removal and is deleted outright. `GcStats.run_log_dirs_removed` and
  `CleanStats.run_log_dirs_removed` count what went.

## The daemon

A future daemon that holds the graph and runs builds replaces the transport,
not the commands, the snapshot format or the viewer. Under it one process has
many sessions (a session is a request, and a pid can no longer name it), lock
waits become joined memoized work, and the CLI talks to the daemon over a
socket — while each CLI client still receives its event stream and keeps
writing its recording. That is why the id is opaque and carries no pid, why
the commands read through a `SessionSource` trait (`list`, `snapshot`,
`subscribe`, `inflight`, `pools`; the recordings are its first
implementation), why one versioned `SessionSnapshot` serves the `status`
row, `--json` and later the
daemon's reply, and why `inflight` is defined per session rather than as a
signal to a pid. Nothing here assumes one session per process beyond the
file source itself.
