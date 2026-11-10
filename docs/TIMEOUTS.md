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
   driver's cancellation path runs: `exec`/`bash` kill the process group.
2. The run has a grace period (10s) to return. If it does not, the engine stops
   waiting and drops it.
3. Either way the target fails:

   ```
   //p:integration timed out after 10m (its `timeout`)
   ```

   with `; the run did not stop within 10s of being cancelled and was
   abandoned, so its process may still be running` appended when step 2 ran
   out.

A timeout is a **failure**, not a cancellation: the failure box shows the log
tail, the sandbox is kept for diagnostics, `--fail-fast` reacts to it, and
nothing is cached, so the next build runs the target again.

## What the clock covers

Only the driver's `run`: not the wait for admission (`resources`), not building
dependencies, not preparing the sandbox. A target queued behind others does not
burn its timeout while it waits.

`--shell` is never timed: it is a person at a prompt. An entrypoint
(`heph run //x`) is held to its `timeout` like a build.

## Never hashed

A limit does not change what a run that finishes within it produces, so the
timeout reaches no def hash, input hash or cache key: changing it, adding it or
removing it never rebuilds anything.

## Which drivers accept it

`exec` and `bash`. Every other in-tree driver refuses `timeout` as an unknown
attribute at parse, so a declaration is never silently ignored. A driver adds
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

Skew, both ways. A 0.19.0 plugin cannot declare a timeout (its `TargetDef` has
none), so its runs are cancelled only with their request; the host cancels
them by request id, as before. A 0.19.1 plugin on a 0.19.0 host has its
timeout dropped at decode and its targets run unbounded: an accepted outcome,
by decision, not refused.
