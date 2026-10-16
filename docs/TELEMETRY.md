# Telemetry

heph reports anonymous, coarse usage data to PostHog (EU region) to guide its
development. It is **on by default** and switched off by either of two opt-outs
(below). This page covers what is sent, when, how to see what is pending, and
how to turn it off.

## What is collected

One event per command (`cli_command`), and nothing else. It carries:

- **The command's shape**: the subcommand path (`run`, `inspect deps`, …), the
  *names* of the flags that were set (never their values), whether the selector
  was an address or a label + package matcher (never the address or label
  itself), success, a coarse failure class (`cancelled`, `target_failure`,
  `error`), and wall time.
- **Aggregate counters for the run**: targets resolved, local and remote cache
  hits/misses/writes, executes and their approximate p99 time, artifact counts
  and sizes, approval prompts, sandbox shells entered, sandbox backend split,
  cache spills, the size of a whole-graph query, the shape of a `-e` query (node
  counts, never the expression), whether the TUI was used.
- **The environment**: OS and version, CPU architecture, heph version, whether
  it looks like CI, how many config profiles are layered, the enabled plugin
  *type* names, and how many remote caches are configured and of which kind
  (`s3`, `gcs`, …, never the URI).
- **Two identifiers, neither personal**: a random per-install UUID
  (`~/.config/heph/telemetry-id`, derived from nothing; the constant `ci` on CI
  runners), and a repo fingerprint — a one-way hash of the git root commit, so
  events can be grouped by project without naming it.

Never sent: target addresses, labels, file paths, flag values, user names, host
names, IP-derived location (GeoIP is disabled), or a person profile (every event
is personless).

## When it is sent

A command never waits on the network. At exit it appends its event to a local
spool — `$XDG_CONFIG_HOME/heph/telemetry-spool.db`, or
`~/.config/heph/telemetry-spool.db` — and returns. The spool holds at most 200
events; past that the oldest are dropped.

A flush is **due** when there is something spooled, no flusher was started in
the last **60 seconds**, and either the oldest event is **5 minutes** old or
**50** events are waiting. The command that finds a flush due starts a detached
background process and does not wait for it:

```
heph __telemetry-flush /path/to/the/workspace
```

That is the line you may see in `ps`. It runs in its own session, with its
input and output on `/dev/null` and its working directory at `/`, sends the
spool in batches of 50, deletes exactly what PostHog confirmed, and exits. The
path is the workspace of the command that started it, whose config the flusher
re-checks for an opt-out before sending. It gives up sending after **30
seconds** (whatever is unsent stays spooled for the next one), and a watchdog
ends the process at 35 seconds whatever it is doing. Only one flusher runs at a
time; one that cannot reach PostHog leaves the spool due, and the 60-second
floor keeps an offline machine at one attempt per minute rather than one per
command.

**On CI the flush is synchronous**: every command sends at exit, waiting up to
**10 seconds** in total. A CI runner kills background processes with the job and
throws its spool away with the machine, so a detached flusher would lose the
data.

Two consequences worth knowing:

- **Short-lived containers can lose up to 5 minutes of events.** A container
  that runs a few heph commands and is destroyed before a flush came due takes
  its spool with it. Nothing is sent late from elsewhere; those events are gone.
- **A container whose PID 1 does not reap children keeps a zombie.** The
  flusher outlives the command that started it, so it is reparented to PID 1
  when that command exits. An init that reaps (`tini`, `dumb-init`,
  `docker run --init`, systemd) cleans it up when it exits; a PID 1 that never
  calls `wait` — a bare shell script, an application not written as an init —
  leaves a `<defunct>` entry until the container exits. It holds no memory or
  files, only a process-table slot.

## Opting out

Either switch turns telemetry off:

- **The environment**: `HEPH_DISABLE_TELEMETRY` set to anything, for one shell,
  one machine, or a CI job.
- **The workspace config**: in `.hephconfig`

  ```yaml
  telemetry:
    enabled: false
  ```

With telemetry off, a command records nothing.

**Opting out sends what was already spooled, once, then deletes the spool.**
Events recorded before you opted out were recorded while telemetry was on, so
they are sent rather than dropped. Either switch does it the same way. The first
command that runs with telemetry off and finds a spool starts
`heph __telemetry-flush --opt-out` in the background and does not wait for it.
That flusher has the same limits as any other: it stops sending after 30
seconds, and its watchdog ends it at 35. On CI the send runs inside the command
instead, within the same 10-second limit. There is exactly **one attempt**.
Afterwards `telemetry-spool.db` and its `-wal`/`-shm` files are deleted, whether
the send worked or not. Nothing is kept for a retry, and nothing is sent again
later. A spool with no events in it is simply deleted.

A flusher that was already running when you opted out checks again before
sending (`HEPH_DISABLE_TELEMETRY`, or the config opt-out of its workspace). It
then behaves the same way: one send, then it deletes the spool. The spool is
machine-wide, so opting out in one workspace also sends and then removes events
that another workspace had spooled. `HEPH_TELEMETRY_ENDPOINT` applies to this
final send as to any other.

To drop the spool without sending it, delete it by hand before your next heph
command:

```
rm -f ~/.config/heph/telemetry-spool.db*
```

(`$XDG_CONFIG_HOME/heph/` if you set `XDG_CONFIG_HOME`.) Deleting
`telemetry-id` as well makes the next event come from a new, unrelated install
id.

## Seeing what is pending

`heph tool telemetry status` prints whether telemetry is on and which switch
turned it off, the endpoint, the spool path, how many events are pending and how
old the oldest is, how the last flush went, and when the next one is due. If
telemetry is off and events are still pending from before, it says that they
will be sent once as the command exits, and that the spool is deleted after.

`heph tool telemetry status --json` prints the same as one JSON object. Its keys
are a stable interface: fields may be added, never renamed or removed. Times are
milliseconds since the Unix epoch, ages milliseconds.

| Key | Type | Meaning |
|---|---|---|
| `enabled` | bool | Whether this process records and sends events |
| `disabled_by` | array of `"env"` / `"config"` | Every opt-out in force; empty when enabled |
| `ci` | bool | Whether this looks like a CI runner |
| `flush_mode` | `"detached"` / `"synchronous"` / `"off"` | How events are delivered: a background flusher, every command at exit (CI), or not at all |
| `endpoint` | string | Where events are sent |
| `endpoint_overridden` | bool | Whether `HEPH_TELEMETRY_ENDPOINT` is set |
| `endpoint_error` | string or null | Why that override was rejected (nothing is sent then) |
| `spool_path` | string or null | The spool file; null with neither `HOME` nor `XDG_CONFIG_HOME` |
| `pending_events` | integer | Events waiting to be sent |
| `oldest_event_ms` | integer or null | When the oldest pending event was recorded |
| `oldest_event_age_ms` | integer or null | Its age |
| `last_flush_ms` | integer or null | When the last flush finished |
| `last_flush_ok` | bool or null | Whether it sent everything; null before the first |
| `last_flush_error` | string or null | Why it failed |
| `last_flusher_spawn_ms` | integer or null | When a command last started a flusher |
| `next_flush_due_ms` | integer or null | When the next command will start a flusher if nothing more is spooled; at or before `now_ms` means the next command does. Null with nothing pending, on CI, and when disabled |
| `now_ms` | integer | The clock the rest was computed against |

## The endpoint override (for tests and benchmarks)

`HEPH_TELEMETRY_ENDPOINT` points the flush at another ingestion URL instead of
PostHog. It exists so heph's own tests and benchmarks can exercise the real send
path against a local sink without touching production.

It accepts **loopback URLs only** — `http://` or `https://` with host
`127.0.0.1` (any `127.x.y.z`), `[::1]` or `localhost`, any port and path; heph
posts the same PostHog batch request to `<url>/batch/`. It cannot redirect your
events off the machine. Any other value — empty, malformed, another host —
disables sending entirely (the flush fails and records why, visible in
`status`) rather than falling back to PostHog, since whoever set it meant
events not to go there. `heph tool telemetry status` shows the override and any
rejection.
