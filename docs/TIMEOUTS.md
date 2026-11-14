# Target timeouts

A target can say how long its run may take; heph stops it there and fails it.

```python
target(
    name = "integration",
    driver = "bash",
    run = "./run-integration-tests.sh",
    timeout = "10m",
)
```

`timeout` is a duration string (`"90s"`, `"10m"`, `"1h30m"`) or a whole
number of seconds (`600`). It must be above zero; leave it out for no limit.
Anything else (`"0s"`, `"soon"`, `1.5`, a `${read://…}` reference) fails the
driver's parse, naming the target.

## What happens at the timeout

The **engine** enforces it, around the driver's `run`; the driver only moves
the declaration onto the def (`TargetDef.timeout`). So a driver cannot opt out
by ignoring it:

1. At the timeout the engine cancels the run's own cancellation token. The
   driver's cancellation path runs: `exec`/`bash` send `SIGINT` to the run's
   process group, then `SIGKILL` to the group once the command has exited, or
   after a 2s grace if it has not. Anything still in the command's process
   group is killed too, even a process that ignores `SIGINT` (as a shell's `&`
   job does). A process that left the group escapes it: a tool that calls
   `setsid` or daemonizes, or a job started under `set -m`. The same holds
   for a target under a session runner (`docs/EXEC_RUNNERS.md`): its agent
   kills what is left of the target's group once the target exits.
2. The run has a grace period (10s) to return. If it does not, the engine stops
   waiting and drops it.
3. Either way the target fails:

   ```
   //p:integration timed out after 10m (its `timeout`)
   ```

   (the limit exactly as declared, `hcore::units::format_duration`; `inspect
   def --json` shows it as `timeout_ms`)

   with `; the run did not stop within 10s of being cancelled and was
   abandoned, so its process may still be running` appended when step 2 ran
   out.

A timeout is a **failure**, not a cancellation: the failure box shows the log
tail, the log and `failure_out` are kept in the failure slot, `--fail-fast`
reacts to it, and
nothing is cached, so the next build runs the target again. (A cancellation
gets no failure slot: when the request was cancelled, by `--fail-fast` or
Ctrl-C, a run's error is treated as that cancellation unless it is a timeout,
so a real failure racing the cancel is not slotted either.)

A machine tells a timeout from any other failure by the `ResultEnd` event's
`timed_out` field (`{timeout_ms, abandoned}`), not by reading `error`. It is
unset for every other outcome, and from a host that predates timeouts.

A cache hit shows nothing: nothing ran. The timeout is per target only; there
is no workspace-wide default yet.

## What the clock covers

The driver's `run`: not the wait for admission (`resources`), not building
dependencies. A target queued behind others does not burn its timeout while it
waits.

For `exec`/`bash` (managed drivers), `run` includes staging the inputs into the
sandbox and collecting the outputs, so those count. Heph's own staging and
collection stop within one chunk once the run is cancelled, so they never use
up the grace; only the driver's process can. Leftover jobs of an abandoned run
stop as soon as it is dropped, and each output tar is written under a name
unique to its run, so a leftover job cannot overwrite a later run's artifact.

An abandoned run's sandbox is **not removed**, even though a failed sandbox
otherwise is (see [FAILURE_OUTPUTS.md](FAILURE_OUTPUTS.md)): its process may
still be writing to the tree, and a removal racing that writer can neither
finish nor be counted as done. The tree is left on disk, its `sandbox` slot is
released at once, the target's next run reclaims it, and the failure box says
`sandbox: left (run abandoned after timeout)`. A credential-material target's
sandbox is removed all the same, writer or not.

`--shell` is never timed: it is a person at a prompt. An entrypoint
(`heph run //x`) is held to its `timeout` like a build, and a timed-out
entrypoint exits 125. On the caller's terminal (inherited stdio, not a PTY)
the tool shares heph's process group, so only its direct child is killed: a
process it forked can outlive it.

## Never hashed

A limit does not change what a run that finishes within it produces, so the
timeout reaches no def hash, input hash or cache key: changing it, adding it or
removing it never rebuilds anything. That holds only while the run cannot see
its limit: a driver must not expose the timeout to the run (argv, env, a
file), or the run's outputs can depend on a value no hash covers.

## Which drivers accept it

`exec` and `bash`. Every other in-tree driver refuses `timeout` as an unknown
attribute at parse, so a declaration is never silently ignored; that holds for
drivers whose spec is built on the `Spec` derive, which refuses unknown keys. A
driver adds
it by carrying a `timeout: Option<hplugin::driver::Timeout>` field on its spec
and moving it onto `TargetDef.timeout`; it must not fold it into its own def or
hash. A cdylib driver can do the same: its timeout is enforced exactly like an
in-process driver's.

A cdylib driver's timeout crosses the plugin ABI as `TargetDef.timeout_ms`
(since ABI 0.19.1). Cancelling a timed-out run stops that run alone: each run
call carries its own `ManagedRunRequest.cancel_key`, and the host cancels it by
that key, not by the request id every target of the request shares (which would
also stop every sibling run of the same driver). A cancelled request still
stops every run.

The run's token says why it was cancelled. The engine cancels a timed-out
run's token as timed out (`Cancellable::timed_out`), and the host sends a
cdylib's run `StableCancel::cancel_timed_out(cancel_key)` rather than
`cancel` (ABI 0.21.0), so the plugin's token reads the same. That is how the
managed layer keeps a timed-out run's `failure_out` and nothing of a
cancelled one's.

Skew, both ways. A 0.19.0 plugin cannot declare a timeout (its `TargetDef` has
none), so its runs are cancelled only with their request; the host cancels
them by request id, as before. A 0.19.1 plugin on a 0.19.0 host has its
timeout dropped at decode and its targets run unbounded: an accepted outcome,
by decision, not refused.
