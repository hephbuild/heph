//! Build-progress event stream types emitted by the engine core and consumed by
//! the TUI + telemetry. Shared here (the lowest crate) so neither consumer needs
//! to depend on the engine.
//!
//! Events are serde-serializable. They carry the target address as a `String`
//! (`//pkg:name`), never the internal `Arc`-backed `Addr`. The server (engine)
//! stamps every event with a wall-clock timestamp at emit time (`at_unix_ms`).
//! The `emit_scope` helper that pairs Start/End events lives in the engine (it
//! needs the engine's `RequestState`).
//!
//! # This is a live wire format
//!
//! Not a "future" one. `StableRemoteHook` serde-JSON-encodes **every** event
//! across the plugin seam to out-of-process hooks
//! (`crates/plugin-stabby/src/load_stable.rs`), and a plugin cdylib such as
//! `plugin-gha-cdylib` ships as its own release artifact, pinned by manifest URL
//! independently of which `heph` binary the user runs. **Host/plugin version
//! skew is therefore a normal, reachable state**, and a skewed pair still loads
//! successfully at `dlopen` — the ABI check covers the `StreamItem` envelope,
//! not this payload.
//!
//! The decode path has **no slack**: a frame that fails to deserialize is
//! treated as end-of-stream by the plugin SDK's pull loop, which then calls
//! `on_close()`. A single undecodable event silently truncates that hook's whole
//! stream while the hook believes the build finished normally.
//!
//! So, when changing anything here:
//!
//! - **Never change an existing field's type or remove one.** Add new fields
//!   alongside, each `#[serde(default)]` — a bare `Option<T>` is *not*
//!   absent-tolerant under `serde_derive`; a missing key is an error without it.
//!   Unknown fields are already ignored on the way in (no
//!   `deny_unknown_fields`), so new-host → old-plugin is safe.
//! - **New variants are safe** only because of [`BuildEventKind::Unknown`]'s
//!   `#[serde(other)]`. Do not remove it.
//! - **Renaming a variant is a break**, not a refactor: the name *is* the `type`
//!   tag on the wire. An old consumer decodes the renamed variant as `Unknown`
//!   and silently loses that event — it degrades rather than truncating the
//!   stream, which is survivable but is still a loss. `RequestConfig` was renamed
//!   from `MaxWorkers` deliberately, pre-1.0, with that cost accepted.
//!
//! `scripts/abi-check.sh` watches this file and will flag changes for review.
//!
//! # It is also an on-disk format
//!
//! Every heph process records its session to `~/.heph/sessions/<id>/events.ndjson`
//! (`crates/tui/src/tui/session/`), and a viewer from a **different heph version**
//! reads it back: `heph session show|attach` of a run started by an older or newer
//! binary. Each line is a record envelope `{at_unix_ms, kind?, session?}` whose
//! `kind` is a [`BuildEventKind`] encoded byte-for-byte as on the plugin wire, so
//! the rules above bind the file exactly as they bind the seam. Session-only
//! records (log lines, the final exit, truncation) live under the envelope's
//! `session` key, never as `BuildEventKind` variants: they are not build events,
//! and a plugin hook must never receive one.
//!
//! The session reader is the one consumer allowed — required — to skip a line it
//! cannot decode and carry on, counting it; the stop-on-first-error rule above
//! is the plugin SDK's and does not apply to a file a later heph appends to.
//!
//! # Counting cache hits
//!
//! The four tier events — [`LocalCacheHit`](BuildEventKind::LocalCacheHit),
//! [`LocalCacheMiss`](BuildEventKind::LocalCacheMiss),
//! [`RemoteCacheHit`](BuildEventKind::RemoteCacheHit),
//! [`RemoteCacheMiss`](BuildEventKind::RemoteCacheMiss) — say what each cache
//! *tier* answered, and one target produces several of them: a remote hit is a
//! `LocalCacheMiss` then a `RemoteCacheHit`, a full miss is a `LocalCacheMiss`
//! then a `RemoteCacheMiss`. They are for timelines and tier statistics.
//! **Never fold them into a hit count or a hit rate**: doing so counts every
//! remote hit as a miss and every full miss twice.
//!
//! Per-target numbers come from [`CacheOutcome`](BuildEventKind::CacheOutcome)
//! (at most one per cacheable addr per request; none when the resolution fails
//! before it settles — cancellation, a lock or local mirror error) and
//! [`CacheHitRebuilt`](BuildEventKind::CacheHitRebuilt) (a hit taken back), and
//! nothing else. [`CacheCounts`] is that fold; every surface heph ships (the
//! TUI, the GitHub Actions report, telemetry) uses it, and a hook should too.
//! Local hits + remote hits + misses is the number of cacheable targets the
//! request resolved.
//!
//! A hook sees every request on its engine, so a process that runs two
//! requests sees an outcome for the same addr in each.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::capacity::Unit;

/// An amount of a pool, as a person reads it, by the pool's unit: `cpu` in
/// CPUs (`4`, `0.5`), `bytes` in binary units (`2.0 GiB`), `count` as the
/// integer. Shared by every renderer of the admission events.
pub fn display_amount(unit: Unit, amount: u64) -> String {
    match unit {
        Unit::Cpu => unit.canonical(amount),
        Unit::Bytes => crate::units::human_bytes(amount),
        Unit::Count => amount.to_string(),
    }
}

/// The unit of `pool` as a `RequestConfig`'s `units` names it; a pool it
/// does not name (a host that predates `units`) has its default unit: a builtin's own, else `count`.
pub fn unit_of(units: &BTreeMap<String, String>, pool: &str) -> Unit {
    units
        .get(pool)
        .and_then(|u| Unit::from_name(u))
        .unwrap_or_else(|| Unit::default_for(pool))
}

/// What an `ExecuteStart` was clamped by, as `cpu 16→4` (pools joined by
/// `, `), or `None` when every declared pool was granted in full. Only the
/// declared keys count: an undeclared pool was never asked for. `unit`
/// names each pool's unit.
pub fn clamp_text(
    declared: Option<&BTreeMap<String, u64>>,
    reserved: Option<&BTreeMap<String, u64>>,
    unit: impl Fn(&str) -> Unit,
) -> Option<String> {
    let (declared, reserved) = (declared?, reserved?);
    let parts: Vec<String> = declared
        .iter()
        .filter_map(|(pool, &want)| {
            let got = reserved.get(pool).copied().unwrap_or(0);
            (got < want).then(|| {
                let u = unit(pool);
                format!(
                    "{pool} {}→{}",
                    display_amount(u, want),
                    display_amount(u, got)
                )
            })
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// The text of an admission-wait row, after its `⧗`:
/// `//a:b waits for cpu 4 (2 free, 6 ahead, 41s)`. `waited` is the elapsed
/// time as the renderer spells it; `unit` is `pool`'s. A waiter that fits
/// what is free and is held back for the oldest one (`yielding_to`, with the
/// unit of *its* pool, which need not be `pool`) says so in place of the
/// queue: `//a:b waits for cpu 1 (3 free, yielding to a cpu 8 run, 41s)`.
/// With removals pending (`removals_pending` above 0, only ever set on a
/// `sandbox` wait), the cause is named:
/// `… — cleanup is behind: 3 removals pending`.
#[expect(
    clippy::too_many_arguments,
    reason = "one per field of the row; a struct for them would be built at each call site only \
              to be taken apart here"
)]
pub fn admission_wait_text(
    addr: &str,
    pool: &str,
    unit: Unit,
    need: u64,
    free: u64,
    ahead: u64,
    yielding_to: Option<(&YieldingToData, Unit)>,
    removals_pending: Option<u64>,
    waited: &str,
) -> String {
    let queue = match yielding_to {
        Some((to, to_unit)) => format!(
            "yielding to a {} {} run",
            to.pool,
            display_amount(to_unit, to.need)
        ),
        None => format!("{ahead} ahead"),
    };
    let text = format!(
        "{addr} waits for {pool} {} ({} free, {queue}, {waited})",
        display_amount(unit, need),
        display_amount(unit, free)
    );
    match removals_pending {
        Some(n @ 1..) => {
            let noun = if n == 1 { "removal" } else { "removals" };
            format!("{text} — cleanup is behind: {n} {noun} pending")
        }
        _ => text,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildEvent {
    /// Server-stamped at emit time (`SystemTime::now()` since epoch, milliseconds).
    pub at_unix_ms: u64,
    pub kind: BuildEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BuildEventKind {
    /// Declares how this request is configured. Emitted once at the start of a
    /// `result` batch.
    ///
    /// This is the one-shot "here is how the build is set up" announcement, and
    /// the home for anything a consumer would otherwise have to *discover* about
    /// the run. It was `MaxWorkers { count }` until it grew a second field and
    /// the name started lying; further request-level configuration (which caches
    /// are wired up, whether they are read-only) belongs here rather than in new
    /// variants.
    RequestConfig {
        /// The cpu capacity, in CPUs (rounded up): how many CPUs the running
        /// targets may reserve at once. Lets a client render a fixed
        /// worker-slot indicator. Equal to `resources["cpu"]` in whole CPUs.
        max_workers: usize,
        /// Whether this request stops at the first target failure.
        ///
        /// On the stream because a consumer must not have to guess it. The GHA
        /// hook previously scanned `std::env::args()` for `--fail-fast`, which is
        /// wrong twice over: a plugin is handed what it needs rather than reading
        /// the ambient environment — it may not even share the host's process —
        /// and a naive argv scan false-positives on a flag *value* that happens
        /// to look like the flag (`--define X=--ff`).
        ///
        /// It matters to a report because a one-failure summary under fail-fast
        /// reads as "one thing is broken" when the truth is "we stopped looking".
        fail_fast: bool,
        /// Whether this run was asked to ignore carried-over scratch state
        /// (`--no-scratch`). Scratch caches are still resolved, locked, created
        /// and announced through their environment variables — only the stored
        /// contents are withheld, so every target that uses one runs cold.
        ///
        /// On the stream because it explains *two* anomalies a consumer would
        /// otherwise have to guess at: a build that is inexplicably slow, and a
        /// report showing no cache reuse. A summary saying "0 cached" with no
        /// cause reads like a cache outage rather than a deliberate audit.
        ///
        /// Phrased negatively on purpose. `#[serde(default)]` fills a missing
        /// key with `false`, and for a frame from a host older than this field
        /// the true answer *is* "not disabled" — scratch was on. The positive
        /// spelling (`scratch_enabled`) would default those same frames to
        /// "scratch was off", which is both wrong and alarming.
        #[serde(default)]
        scratch_disabled: bool,
        /// Every admission pool and its capacity, in the base of the pool's
        /// unit (micro-CPUs for `cpu`, bytes, a count). `None` (`null` on the
        /// wire) is an **unbounded** pool: a custom pool declared with no
        /// value, or `memory` when the host's could not be read — every
        /// request is granted at once. Empty from a host that predates
        /// resources.
        #[serde(default)]
        resources: BTreeMap<String, Option<u64>>,
        /// Every pool in `resources` and its unit's name (`count`, `cpu`,
        /// `bytes`), so a renderer shows an amount the way it was written.
        /// Read through [`unit_of`]: a pool missing here (a host that
        /// predates the field) has its default unit.
        #[serde(default)]
        units: BTreeMap<String, String>,
    },
    /// Incremental notice of matched top-level targets. `addrs` are newly
    /// matched addresses to add to the set; `complete` is false while the
    /// matcher is still resolving (client renders a provisional `X / ~N`) and
    /// true on the final event once the full set is known (drops the `~`).
    Matched {
        addrs: Vec<String>,
        complete: bool,
    },
    /// The selector walk could not resolve part of the workspace and carried on
    /// without it, so the matched set may be missing targets. Emitted once, at
    /// the end of a command whose walk skipped something.
    ///
    /// Every field defaults, so a frame from a host that trims this down still
    /// decodes; a consumer older than the variant decodes it as `Unknown`.
    SelectionIncomplete {
        /// Exact number of distinct things skipped: targets, packages and
        /// provider listings, across every group.
        #[serde(default)]
        skipped: usize,
        #[serde(default)]
        groups: Vec<SelectionGap>,
    },
    ResultStart {
        addr: String,
    },
    /// What a finished target produced, by output group: sent right before
    /// the successful `ResultEnd` of a target that ran. Every output group
    /// the run produced, not only the ones this request asked for; log and
    /// support files are not outputs and are left out. Not sent for a cache
    /// hit (a warm build pays nothing for it; a consumer reads a hit's
    /// groups from `Engine::output_listing`), for a failure, nor to a request
    /// nobody listens to.
    ///
    /// Per-file rows are not here: an artifact is a group's packed tree, and
    /// listing it reads the cache entry, which a consumer asks for off its
    /// render path (`Engine::output_listing`). An older consumer reads it as
    /// [`BuildEventKind::Unknown`].
    ResultOutputs {
        addr: String,
        #[serde(default)]
        groups: Vec<OutputGroupSummary>,
    },
    ResultEnd {
        addr: String,
        /// The failure, flattened by `format!("{e:#}")` — the whole cause chain
        /// on one line. Unchanged in type and meaning; the fields below add
        /// structure alongside it rather than replacing it, because retyping
        /// this field would break a version-skewed plugin (see the module docs).
        error: Option<String>,
        /// Set when this target failed *only* because a dependency did, naming
        /// the root. Lets a consumer separate the one thing a human has to fix
        /// from the collateral cone — which at 20k targets can be thousands of
        /// targets reporting the same underlying failure.
        #[serde(default)]
        upstream_of: Option<String>,
        /// The process's exit status as the OS reported it (e.g. `exit status:
        /// 1`), when the failure was a subprocess.
        ///
        /// A string, not an `i32`, because a string is what the engine actually
        /// has: `ProcessFailed::status` is already formatted. Parsing an integer
        /// back out of it in a consumer would be the same guesswork this field
        /// exists to remove.
        #[serde(default)]
        exit_status: Option<String>,
        /// The last lines of the target's process log. Only ever attached to a
        /// failing result, so a green build pays nothing for it.
        #[serde(default)]
        log_tail: Option<LogTailData>,
        /// Set when the target failed because its run outlasted its `timeout`,
        /// so a consumer can tell a timeout from any other failure without
        /// reading `error`. `None` from a host that predates timeouts.
        #[serde(default)]
        timed_out: Option<TimedOutData>,
    },
    ExecuteStart {
        addr: String,
        driver: String,
        cache: bool,
        /// What the target declared (`resources` on its def), per pool in the
        /// pool's unit; only the declared keys. `None` from a host that
        /// predates resources.
        #[serde(default)]
        declared: Option<BTreeMap<String, u64>>,
        /// What the engine granted, after clamping to each pool's capacity.
        /// Always carries `cpu` (0 for a zero-cost target, which reserves
        /// nothing and is admitted at once). `None` from a host that predates
        /// resources: such a target counts as one CPU.
        #[serde(default)]
        reserved: Option<BTreeMap<String, u64>>,
        /// How long the target waited for admission, in milliseconds — set
        /// for every admitted run, so a wait under the `AdmissionWait*`
        /// notice threshold is still answerable from the stream. `None`
        /// from a host that predates it, and for a run that reserved nothing
        /// (an entrypoint).
        #[serde(default)]
        queued_ms: Option<u64>,
        /// How the run was admitted: on arrival, as the oldest waiter, or
        /// ahead of an older one. It answers "why did this start before
        /// that". `None` from a host that predates it, and for a run that
        /// reserved nothing (an entrypoint): absent never means `arrival`.
        /// Treat [`AdmittedKind::Unknown`] like `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        admitted: Option<AdmittedKind>,
        /// The output group names the target declares, in declaration order:
        /// what it will write, before it has. Empty from a host that predates
        /// it, and for a target that declares no output.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        output_groups: Vec<String>,
    },
    ExecuteEnd {
        addr: String,
        error: Option<String>,
    },
    /// The local tier had the revision's manifest. A **tier event, not a target
    /// outcome**: count targets with [`CacheOutcome`](Self::CacheOutcome).
    LocalCacheHit {
        addr: String,
    },
    /// The local tier did not have the revision. A **tier event, not a target
    /// outcome**: the target may still hit the remote tier right after, so a
    /// hit rate folded from this counts every remote hit as a miss. Count
    /// targets with [`CacheOutcome`](Self::CacheOutcome).
    LocalCacheMiss {
        addr: String,
    },
    /// The final cache outcome of one cacheable target: at most one per
    /// cacheable addr per request, whatever output variants of it the request
    /// resolved — none when the resolution fails before it settles
    /// (cancellation, a lock or local mirror error). Not emitted
    /// for a target that could not hit — caching off, `--force`,
    /// `--no-scratch`, a shell — nor for the engine's hash-only nested
    /// recomputes. See the module docs ("Counting cache hits").
    CacheOutcome {
        addr: String,
        outcome: CacheOutcomeKind,
    },
    /// A target whose [`CacheOutcome`](Self::CacheOutcome) was a hit is being
    /// rebuilt after all: the revision's manifest outlived its blobs. At most
    /// once per addr per request, and only after a hit outcome. Move one count
    /// from `was` to misses.
    CacheHitRebuilt {
        addr: String,
        was: CacheHitKind,
    },
    /// Start of writing a target's artifacts to the local cache. Paired with
    /// `LocalCacheWriteEnd` via `emit_scope` (fires on completion, `?`, or
    /// cancellation). The client groups this span under the target alongside
    /// `Execute*` as one entry in the per-target operation timeline.
    LocalCacheWriteStart {
        addr: String,
    },
    LocalCacheWriteEnd {
        addr: String,
        error: Option<String>,
    },
    /// Acquiring the per-addr result lock has been blocked past the notice
    /// threshold. `holder_pid` is a process that stamped the lock and still held
    /// it when it was probed — a snapshot, and `None` whenever the holder cannot
    /// be named. Paired one-to-one with
    /// `ResultLockWaitEnd` (which fires on acquire **or** cancellation), so a
    /// consumer can show the notice for exactly the duration of the wait.
    ResultLockWaitStart {
        addr: String,
        holder_pid: Option<u32>,
        /// With no `holder_pid`: the wait is on *readers* of the cached revision
        /// being rebuilt or deleted — another command still using that output,
        /// which releases it once done with it. Readers are not stamped, so they can
        /// be detected but not named. `false` from a host that predates it.
        #[serde(default)]
        in_use_by_readers: bool,
    },
    /// The execute-lock wait ended (lock acquired or the wait was cancelled).
    ResultLockWaitEnd {
        addr: String,
    },
    /// A target's admission has waited past the notice threshold for what it
    /// reserves to fit. Names the first pool it does not fit in (or, when it
    /// fits everywhere but waits behind others, the first it needs), as of the
    /// moment the notice fired. Paired one-to-one with `AdmissionWaitEnd`
    /// (granted or cancelled). Nothing is emitted for a wait shorter than the
    /// threshold.
    AdmissionWaitStart {
        addr: String,
        /// The pool waited on (`cpu`, `memory`, or a custom pool).
        pool: String,
        /// What the target needs of `pool`, in the pool's unit.
        need: u64,
        /// What `pool` had free when the notice fired.
        free: u64,
        /// How many requests were queued ahead of it.
        ahead: u64,
        /// Set only when `pool` is `sandbox`: how many finished runs' sandboxes
        /// were still waiting to be removed (each holding its slot until then)
        /// when the notice fired. Above 0, the wait is on cleanup, not on runs.
        /// `None` from a host that predates it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        removals_pending: Option<u64>,
        /// Set when the target fits what is free and still waits: the oldest
        /// waiter has been overtaken as far as it may be, and starting this
        /// one would take what that one needs. `None` otherwise, and from a
        /// host that predates it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        yielding_to: Option<YieldingToData>,
    },
    /// The admission wait ended (granted, or the wait was cancelled).
    AdmissionWaitEnd {
        addr: String,
    },
    /// One bounded pool's use, as the pool itself counts it: what a gauge
    /// draws. Sent by a per-request sampler on a 250 ms tick only when the
    /// pool changed since its last send, and once more before the request
    /// ends if it changed since; never from inside the pool. A request that
    /// admits nothing (an all-hit build) sends none. A consumer keeps the
    /// latest per `pool`.
    ///
    /// `held` counts every grant still outstanding, so a `sandbox` slot shows
    /// held from its run's admission until its sandbox has been removed, not
    /// until the run ends; summing `ExecuteStart.reserved` cannot see that.
    /// Amounts are in the base of the pool's unit (`RequestConfig.units`).
    /// An older consumer reads it as [`BuildEventKind::Unknown`].
    PoolUsage {
        pool: String,
        /// Granted and not yet returned.
        #[serde(default)]
        held: u64,
        #[serde(default)]
        capacity: u64,
        /// Queued requests that need some of this pool.
        #[serde(default)]
        waiting: u64,
        /// Finished runs whose sandbox removal has not returned, each holding
        /// one `sandbox` slot: set on the `sandbox` pool only, 0 elsewhere.
        #[serde(default)]
        removals_pending: u64,
    },
    /// Acquiring a scratch slot has been blocked past the notice threshold.
    ///
    /// Deliberately *not* folded into `ResultLockWait*`, because the two name
    /// different problems with different fixes. A blocked result lock means
    /// another process is building this exact target — wait for it, or kill it.
    /// A blocked scratch slot means targets are serialized on a shared cache,
    /// which is a declaration-level choice (`access`) the user can change.
    ///
    /// Emitted per *consumer*, so nothing is lost for a machine reader. A
    /// renderer must collapse them by `scratch` before display: one `exclusive`
    /// cache with hundreds of consumers produces that many simultaneous
    /// waiters, and that many identical rows is not a diagnostic.
    ///
    /// Paired one-to-one with `ScratchLockWaitEnd` (which fires on acquire
    /// **or** cancellation).
    ScratchLockWaitStart {
        /// The consumer waiting for the slot.
        addr: String,
        /// The scratch *declaration* being waited on (`//build:gocache`). The
        /// subject of the diagnostic — the slot hash stays off the wire, being
        /// internal and already resolvable via `heph tool scratch path`.
        scratch: String,
        /// `"exclusive"` or `"shared"`, echoing the word used in the BUILD file
        /// rather than a bool the user would have to translate back.
        access: String,
        /// A process holding the slot when it was probed, when one can be named.
        ///
        /// Only an *exclusive* holder in **another** process stamps a nameable
        /// pid, so this is `None` whenever the wait is on shared readers
        /// draining, or on this build serializing against itself — the same
        /// limitation, for the same reason, as `ResultLockWaitStart::holder_pid`.
        holder_pid: Option<u32>,
    },
    /// The scratch-slot wait ended (slot acquired or the wait was cancelled).
    ScratchLockWaitEnd {
        addr: String,
        scratch: String,
    },
    /// Start of preparing a scratch directory for a consumer: everything done
    /// under the slot guard — resolving the lineage, seeding a new scope from a
    /// fallback, creating the directory, and pulling from the remote when the
    /// lineage is cold.
    ///
    /// One span rather than one per step: they share a guard, run in fixed
    /// order, and a user cannot act on them separately. What they *can* act on
    /// is the outcome, which is why the detail rides on the `End`.
    ///
    /// Only ever emitted on an execute (i.e. after a cache miss), so a fully
    /// cached run emits none of these.
    ScratchPrepareStart {
        addr: String,
        scratch: String,
    },
    ScratchPrepareEnd {
        addr: String,
        scratch: String,
        /// What preparing the directory actually did — `"warm"`, `"seeded"`,
        /// `"pulled"`, `"cold"`, `"audit"`, or `"interrupted"`.
        ///
        /// A string, not an enum, and for a wire reason: an enum variant a
        /// skewed plugin has never heard of fails to deserialize, and the SDK
        /// treats a decode failure as end-of-stream. An unrecognised string just
        /// prints. Same call, for the same reason, as `ResultEnd::exit_status`.
        #[serde(default)]
        outcome: String,
        /// Bytes pulled from the remote, when the outcome was `"pulled"`.
        #[serde(default)]
        bytes: u64,
        /// The snapshot was restored at a different path than it was produced
        /// at.
        ///
        /// Worth its own field rather than a log line: a cache whose entries
        /// embed absolute paths restores perfectly and is then *inert* — the
        /// pull reports success, the bytes are spent, and every future build
        /// still runs cold. It looks exactly like a hit, so nothing else in the
        /// stream can distinguish it.
        #[serde(default)]
        path_mismatch: bool,
        error: Option<String>,
    },
    /// A remote tier had the revision. A **tier event, not a target outcome**:
    /// count targets with [`CacheOutcome`](Self::CacheOutcome).
    RemoteCacheHit {
        addr: String,
    },
    /// A remote tier was consulted and did not have the revision. A **tier
    /// event, not a target outcome**: it follows a [`LocalCacheMiss`]
    /// (Self::LocalCacheMiss) for the same target, so adding the two counts a
    /// full miss twice. Count targets with [`CacheOutcome`](Self::CacheOutcome).
    RemoteCacheMiss {
        addr: String,
    },
    /// Start of pulling a target's revision from the remote cache(s) into the
    /// local cache (one span per target, covering all of its blobs from the one
    /// cache that had the manifest). Surfaced as one `↓` op in the per-target
    /// timeline, marked slow if it runs long. Paired with `RemoteCacheReadEnd`.
    RemoteCacheReadStart {
        addr: String,
    },
    RemoteCacheReadEnd {
        addr: String,
        error: Option<String>,
    },
    /// Start of pushing a target's artifacts to the remote cache(s). Runs on a
    /// background task after the build's critical path, so it appears in the
    /// per-target op timeline (and is surfaced as "slow" if it runs long).
    /// Paired with `RemoteCacheWriteEnd`.
    RemoteCacheWriteStart {
        addr: String,
    },
    RemoteCacheWriteEnd {
        addr: String,
        error: Option<String>,
    },
    /// One target finished a cache sweep: how many cache revisions it dropped
    /// and how many bytes those revisions freed (summed from manifest artifact
    /// sizes). Emitted once per target visited by `heph tool gc` or by
    /// `heph tool clean` — including targets that dropped nothing — so a consumer can
    /// show both the count of targets explored and the total data reclaimed.
    GcTargetSwept {
        revisions_removed: usize,
        bytes_removed: u64,
    },
    /// `heph tool gc` swept `<home>/failures/`: how many last-failure slots it
    /// removed and the bytes they held. Emitted once per sweep. An older
    /// consumer reads it as [`BuildEventKind::Unknown`].
    GcFailureLogsSwept {
        #[serde(default)]
        removed: usize,
        #[serde(default)]
        bytes_removed: u64,
    },
    /// `heph tool gc`'s pass over the local cache database (`cache.db`): its
    /// size once the pass is done and what the pass did. Emitted once per
    /// sweep. An older consumer reads it as [`BuildEventKind::Unknown`].
    GcCacheDb {
        /// The database file's size on disk.
        #[serde(default)]
        file_bytes: u64,
        /// Its `-wal` file's size on disk.
        #[serde(default)]
        wal_bytes: u64,
        /// Bytes in pages holding data.
        #[serde(default)]
        live_bytes: u64,
        /// Bytes in free pages, which only a vacuum returns to the disk.
        #[serde(default)]
        free_bytes: u64,
        /// `PRAGMA auto_vacuum`: `none`, `incremental` or `full`.
        #[serde(default)]
        mode: String,
        /// `converted`, `vacuumed`, `skipped`, `unchanged` or `failed`.
        #[serde(default)]
        action: String,
        /// Bytes the file (and its WAL) shrank by.
        #[serde(default)]
        freed_bytes: u64,
        /// `skipped` for want of space: the free bytes the conversion needs on
        /// the filesystem that is shortest, and what it has.
        #[serde(default)]
        needed_bytes: u64,
        #[serde(default)]
        available_bytes: u64,
        /// Why it was skipped or failed, for a person.
        #[serde(default)]
        detail: Option<String>,
    },
    /// A path the codegen index excluded from a lookup (an fs `file` target,
    /// a glob) and that the user may have expected to see: `status` is the
    /// index's verdict (`generated`, `modified`), `reason` why (`recorded`,
    /// `pending`, `section`, `edited`, …), `addr` the target its row names,
    /// if any. An older consumer reads it as [`BuildEventKind::Unknown`].
    CodegenExcluded {
        path: String,
        status: String,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        addr: Option<String>,
    },
    /// `heph tool codegen import` recorded `files` stamped files and skipped
    /// `skipped` whose stamp it could not read.
    CodegenImported {
        #[serde(default)]
        files: usize,
        #[serde(default)]
        skipped: usize,
    },
    /// An event kind this build of the consumer does not know.
    ///
    /// **Load-bearing for cross-version safety, not a placeholder.** Events ride
    /// to out-of-process hooks as serde-JSON, and a plugin cdylib ships as its
    /// own release artifact pinned independently of the `heph` binary — so a
    /// consumer meeting an event kind newer than itself is a reachable state.
    ///
    /// Without this catch-all, that event fails to deserialize, and the plugin
    /// SDK's pull loop treats a decode failure as end-of-stream
    /// (`crates/plugin-sdk/src/serve.rs`: `decode_event_frame(..)` → `None` →
    /// `break`) and then calls `on_close()`. One unknown event would silently
    /// truncate that hook's **entire** stream while the hook believed the build
    /// had ended normally — no error surfaced anywhere.
    ///
    /// `#[serde(other)]` makes the unknown kind decode to this variant instead,
    /// so consumers skip it and keep receiving everything after it.
    #[serde(other)]
    Unknown,
}

/// One group of [`BuildEventKind::SelectionIncomplete`]: what one stage of the
/// walk could not resolve for one provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SelectionGap {
    /// `packages`, `probe`, `list`, `spec` or `def`.
    #[serde(default)]
    pub stage: String,
    /// Empty when the failure did not say which provider it came from.
    #[serde(default)]
    pub provider: String,
    /// Exact number of distinct things skipped in this group.
    #[serde(default)]
    pub count: usize,
    /// Up to five of them, the smallest by name, each with one line of cause.
    #[serde(default)]
    pub examples: Vec<SelectionGapExample>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SelectionGapExample {
    /// The addr, package (`//pkg`) or provider listing that was skipped.
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub cause: String,
}

/// A bounded slice of a target's process log, carried on the event stream.
///
/// A mirror of `hplugin::error::LogTail` rather than that type itself: it has no
/// serde derives, and `crates/plugin` depends on this crate, not the reverse.
/// `hcore` is deliberately the lowest crate so the TUI and telemetry need no
/// engine or plugin dependency to read events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogTailData {
    pub text: String,
    /// 1-based line number of `text`'s first line within the full log, so a
    /// renderer can show true file positions rather than renumbering from 1.
    pub start_line: usize,
}

/// One output group of a finished target, carried on `ResultOutputs`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputGroupSummary {
    /// The group's name; empty for the target's unnamed group.
    pub group: String,
    /// The group's bytes as stored (an artifact's packed size, summed over
    /// the group's artifacts). `None` when any of them does not know its
    /// size (an uncached passthrough file).
    #[serde(default)]
    pub size: Option<u64>,
    /// The group's content hash (`hashout`), when the group is one artifact,
    /// which it is unless a driver splits one.
    #[serde(default)]
    pub hash: Option<String>,
    /// How many artifacts the group is: 1 unless a driver splits one.
    #[serde(default)]
    pub artifacts: u32,
}

/// A run stopped at its target's `timeout`, carried on `ResultEnd`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimedOutData {
    /// The target's `timeout`, in milliseconds.
    pub timeout_ms: u64,
    /// The run did not stop within the grace after it was cancelled, and heph
    /// stopped waiting for it: its process may still be running.
    #[serde(default)]
    pub abandoned: bool,
}

/// How a run was admitted, carried on [`BuildEventKind::ExecuteStart`].
/// Diagnostic: it explains the order runs started in, and decides nothing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AdmittedKind {
    /// On arrival, with nobody waiting (a zero-cost run always is).
    Arrival,
    /// As the oldest waiter.
    Head,
    /// Ahead of an older waiter, in the slot a run of its own shape just
    /// released.
    Handoff,
    /// Ahead of an older waiter, in capacity that was free.
    Pass,
    /// A way this build does not know, from a newer host. A consumer treats
    /// it like an absent `admitted`. Without it, one such value would fail
    /// the whole frame and end a hook's stream (see the module docs).
    #[serde(other)]
    Unknown,
}

impl AdmittedKind {
    /// The value as it is spelled on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Arrival => "arrival",
            Self::Head => "head",
            Self::Handoff => "handoff",
            Self::Pass => "pass",
            Self::Unknown => "unknown",
        }
    }
}

/// What a waiter that fits is held back for, carried on
/// [`BuildEventKind::AdmissionWaitStart`]: the oldest waiter's need of the
/// first pool where both cannot fit.
///
/// `pool` and `need` are permanent: no later host drops or renames them. A
/// field added here later must be `#[serde(default)]`, because a frame that
/// fails to decode ends a hook's stream. An inner field this build does not
/// know is ignored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct YieldingToData {
    /// The pool the oldest waiter needs. Not always the pool the event
    /// names as waited on.
    pub pool: String,
    /// What the oldest waiter needs of `pool`, in that pool's unit.
    pub need: u64,
}

/// Which cache tier a hit came from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CacheHitKind {
    Local,
    Remote,
    /// A tier this build does not know, from a newer host. Ignored by
    /// [`CacheCounts`]. Without it, one such value would fail the whole frame
    /// and end a hook's stream (see the module docs).
    #[serde(other)]
    Unknown,
}

/// A cacheable target's final cache outcome — see
/// [`BuildEventKind::CacheOutcome`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CacheOutcomeKind {
    LocalHit,
    RemoteHit,
    Miss,
    /// An outcome this build does not know, from a newer host. Ignored by
    /// [`CacheCounts`].
    #[serde(other)]
    Unknown,
}

/// The change one event makes to [`CacheCounts`], so a consumer that keeps
/// its counters elsewhere (telemetry's atomics) applies exactly the same
/// mapping. Each field is `-1`, `0` or `1`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheDelta {
    pub local_hits: i64,
    pub remote_hits: i64,
    pub misses: i64,
    pub rebuilt: i64,
}

/// Per-target cache accounting, folded from
/// [`CacheOutcome`](BuildEventKind::CacheOutcome) and
/// [`CacheHitRebuilt`](BuildEventKind::CacheHitRebuilt) only. The tier events
/// are ignored on purpose (module docs, "Counting cache hits").
///
/// Four integers and no per-addr state: the engine already guarantees one
/// outcome per addr per request, so the fold needs no set to dedupe.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheCounts {
    local_hits: u64,
    remote_hits: u64,
    misses: u64,
    rebuilt: u64,
}

impl CacheCounts {
    /// What `kind` changes, or `None` for an event that is not cache
    /// accounting. A rebuilt hit moves one count from its tier to misses and
    /// counts itself in `rebuilt`, which is a subset of misses.
    pub fn delta(kind: &BuildEventKind) -> Option<CacheDelta> {
        let d = match kind {
            BuildEventKind::CacheOutcome { outcome, .. } => match outcome {
                CacheOutcomeKind::LocalHit => CacheDelta {
                    local_hits: 1,
                    ..CacheDelta::default()
                },
                CacheOutcomeKind::RemoteHit => CacheDelta {
                    remote_hits: 1,
                    ..CacheDelta::default()
                },
                CacheOutcomeKind::Miss => CacheDelta {
                    misses: 1,
                    ..CacheDelta::default()
                },
                CacheOutcomeKind::Unknown => return None,
            },
            BuildEventKind::CacheHitRebuilt { was, .. } => {
                let mut d = CacheDelta {
                    misses: 1,
                    rebuilt: 1,
                    ..CacheDelta::default()
                };
                match was {
                    CacheHitKind::Local => d.local_hits = -1,
                    CacheHitKind::Remote => d.remote_hits = -1,
                    CacheHitKind::Unknown => return None,
                }
                d
            }
            _ => return None,
        };
        Some(d)
    }

    /// Fold one event. Saturating, so a stream joined mid-way (a rebuild whose
    /// hit was never seen) cannot underflow.
    pub fn apply(&mut self, kind: &BuildEventKind) {
        if let Some(d) = Self::delta(kind) {
            self.local_hits = self.local_hits.saturating_add_signed(d.local_hits);
            self.remote_hits = self.remote_hits.saturating_add_signed(d.remote_hits);
            self.misses = self.misses.saturating_add_signed(d.misses);
            self.rebuilt = self.rebuilt.saturating_add_signed(d.rebuilt);
        }
    }

    /// Counts kept elsewhere, read back for display (telemetry's snapshot).
    pub fn from_parts(local_hits: u64, remote_hits: u64, misses: u64, rebuilt: u64) -> Self {
        Self {
            local_hits,
            remote_hits,
            misses,
            rebuilt,
        }
    }

    pub fn local_hits(&self) -> u64 {
        self.local_hits
    }

    pub fn remote_hits(&self) -> u64 {
        self.remote_hits
    }

    pub fn hits(&self) -> u64 {
        self.local_hits.saturating_add(self.remote_hits)
    }

    /// Targets that had to be built, rebuilt hits included.
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// The part of [`misses`](Self::misses) that was announced as a hit and
    /// then rebuilt because its blobs were gone — cache health, not input
    /// changes.
    pub fn rebuilt(&self) -> u64 {
        self.rebuilt
    }

    /// Cacheable targets the cache was consulted for.
    pub fn consulted(&self) -> u64 {
        self.hits().saturating_add(self.misses)
    }

    /// Hits over consulted targets, or `None` when nothing was consulted —
    /// `0%` there would be a lie.
    pub fn hit_rate(&self) -> Option<f64> {
        let consulted = self.consulted();
        (consulted > 0).then(|| self.hits() as f64 / consulted as f64)
    }
}

/// Fixtures shared by every consumer's tests, so they are all checked against
/// one event stream and one expected answer.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::{BuildEvent, BuildEventKind, CacheHitKind, CacheOutcomeKind};

    /// What [`cache_stream`] must fold to: `(local_hits, remote_hits, misses,
    /// rebuilt)`. Seven cacheable targets: two local hits (one with a rebuild
    /// of unknown `was`, which moves nothing), two remote hits (one of them
    /// rebuilt), a rebuilt local hit, a full miss with a remote, and a miss
    /// with no remote consulted.
    pub const CACHE_STREAM_EXPECTED: (u64, u64, u64, u64) = (2, 1, 4, 2);

    /// The events one request emits for the targets above, in the engine's
    /// order: tier events, the outcome, `ExecuteStart` for whatever builds,
    /// and an outcome kind from a newer host that must be ignored.
    pub fn cache_stream() -> Vec<BuildEvent> {
        fn ev(at_unix_ms: u64, kind: BuildEventKind) -> BuildEvent {
            BuildEvent { at_unix_ms, kind }
        }
        let a = |s: &str| s.to_string();
        let outcome = |addr: &str, outcome| BuildEventKind::CacheOutcome {
            addr: addr.to_string(),
            outcome,
        };
        let start = |addr: &str| BuildEventKind::ResultStart {
            addr: addr.to_string(),
        };
        let exec = |addr: &str| BuildEventKind::ExecuteStart {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            cache: true,
            declared: None,
            reserved: None,
            queued_ms: None,
            admitted: None,
            output_groups: Vec::new(),
        };
        let end = |addr: &str| BuildEventKind::ResultEnd {
            addr: addr.to_string(),
            error: None,
            upstream_of: None,
            exit_status: None,
            log_tail: None,
            timed_out: None,
        };
        vec![
            // Local hit.
            ev(1, start("//p:local")),
            ev(
                2,
                BuildEventKind::LocalCacheHit {
                    addr: a("//p:local"),
                },
            ),
            ev(2, outcome("//p:local", CacheOutcomeKind::LocalHit)),
            ev(3, end("//p:local")),
            // Remote hit: a local tier miss first — the CI bug.
            ev(4, start("//q:remote")),
            ev(
                5,
                BuildEventKind::LocalCacheMiss {
                    addr: a("//q:remote"),
                },
            ),
            ev(
                6,
                BuildEventKind::RemoteCacheHit {
                    addr: a("//q:remote"),
                },
            ),
            ev(6, outcome("//q:remote", CacheOutcomeKind::RemoteHit)),
            ev(7, end("//q:remote")),
            // Full miss, remote consulted: two tier misses, one target.
            ev(8, start("//q:cold")),
            ev(
                9,
                BuildEventKind::LocalCacheMiss {
                    addr: a("//q:cold"),
                },
            ),
            ev(
                10,
                BuildEventKind::RemoteCacheMiss {
                    addr: a("//q:cold"),
                },
            ),
            ev(10, outcome("//q:cold", CacheOutcomeKind::Miss)),
            ev(11, exec("//q:cold")),
            ev(12, end("//q:cold")),
            // Miss with no remote consulted.
            ev(13, start("//r:solo")),
            ev(
                14,
                BuildEventKind::LocalCacheMiss {
                    addr: a("//r:solo"),
                },
            ),
            ev(14, outcome("//r:solo", CacheOutcomeKind::Miss)),
            ev(15, exec("//r:solo")),
            ev(16, end("//r:solo")),
            // Local hit whose blobs are gone: rebuilt.
            ev(17, start("//r:gone")),
            ev(
                18,
                BuildEventKind::LocalCacheHit {
                    addr: a("//r:gone"),
                },
            ),
            ev(18, outcome("//r:gone", CacheOutcomeKind::LocalHit)),
            ev(
                19,
                BuildEventKind::CacheHitRebuilt {
                    addr: a("//r:gone"),
                    was: CacheHitKind::Local,
                },
            ),
            ev(19, exec("//r:gone")),
            ev(20, end("//r:gone")),
            // Remote hit whose blobs are gone: rebuilt.
            ev(21, start("//s:gone")),
            ev(
                22,
                BuildEventKind::LocalCacheMiss {
                    addr: a("//s:gone"),
                },
            ),
            ev(
                23,
                BuildEventKind::RemoteCacheHit {
                    addr: a("//s:gone"),
                },
            ),
            ev(23, outcome("//s:gone", CacheOutcomeKind::RemoteHit)),
            ev(
                24,
                BuildEventKind::CacheHitRebuilt {
                    addr: a("//s:gone"),
                    was: CacheHitKind::Remote,
                },
            ),
            ev(24, exec("//s:gone")),
            ev(25, end("//s:gone")),
            // From a newer host: ignored by every fold.
            ev(26, outcome("//s:future", CacheOutcomeKind::Unknown)),
            // A local hit whose rebuild names a hit kind this build does not
            // know: the rebuild moves nothing, so the hit stays a hit in every
            // fold, the package rollup and the TUI's cached set alike.
            ev(27, start("//t:odd")),
            ev(28, outcome("//t:odd", CacheOutcomeKind::LocalHit)),
            ev(
                29,
                BuildEventKind::CacheHitRebuilt {
                    addr: a("//t:odd"),
                    was: CacheHitKind::Unknown,
                },
            ),
            ev(30, end("//t:odd")),
        ]
    }
}

pub type EventSender = tokio::sync::mpsc::UnboundedSender<BuildEvent>;
pub type EventReceiver = tokio::sync::mpsc::UnboundedReceiver<BuildEvent>;

pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Wall-clock milliseconds since the Unix epoch. Stamped once at emit time.
#[cfg(test)]
mod tests {
    use super::*;

    /// A frame from a host *older* than this build: the sibling fields are absent
    /// from the JSON entirely.
    ///
    /// This must decode. Without `#[serde(default)]` on each new field, a missing
    /// key is a hard error — a bare `Option<T>` is not absent-tolerant under
    /// `serde_derive` — and the plugin SDK treats a decode error as end-of-stream,
    /// silently truncating everything after it.
    #[test]
    fn old_host_frame_decodes_against_new_fields() {
        let old = r#"{"at_unix_ms":1,"kind":{"type":"ResultEnd","addr":"//a:x","error":"boom"}}"#;
        let ev: BuildEvent = serde_json::from_str(old).expect("old frame must decode");
        match ev.kind {
            BuildEventKind::ResultEnd {
                addr,
                error,
                upstream_of,
                exit_status,
                log_tail,
                timed_out,
            } => {
                assert_eq!(addr, "//a:x");
                assert_eq!(error.as_deref(), Some("boom"));
                assert_eq!(upstream_of, None);
                assert_eq!(exit_status, None);
                assert_eq!(log_tail, None);
                assert_eq!(timed_out, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// The wait row names cleanup as the cause only when removals are
    /// pending: not for a non-`sandbox` wait (`None`), nor for a `sandbox`
    /// wait with none pending (`Some(0)`).
    #[test]
    fn admission_wait_text_names_pending_removals_only_when_there_are_some() {
        let text = |pending| {
            admission_wait_text(
                "//a:b",
                "sandbox",
                Unit::Count,
                1,
                0,
                2,
                None,
                pending,
                "41s",
            )
        };
        let plain = "//a:b waits for sandbox 1 (0 free, 2 ahead, 41s)";
        assert_eq!(text(None), plain);
        assert_eq!(text(Some(0)), plain);
        assert_eq!(
            text(Some(1)),
            format!("{plain} — cleanup is behind: 1 removal pending")
        );
        assert_eq!(
            text(Some(3)),
            format!("{plain} — cleanup is behind: 3 removals pending")
        );
    }

    /// `AdmissionWaitStart` from a host predating `removals_pending` decodes
    /// as `None`, and a frame without it serializes without the key.
    #[test]
    fn an_old_admission_wait_has_no_removals_pending() {
        let old = r#"{"at_unix_ms":1,"kind":{"type":"AdmissionWaitStart","addr":"//a:x",
            "pool":"cpu","need":1,"free":0,"ahead":0}}"#;
        let ev: BuildEvent = serde_json::from_str(old).expect("old frame must decode");
        match &ev.kind {
            BuildEventKind::AdmissionWaitStart {
                removals_pending, ..
            } => assert_eq!(*removals_pending, None),
            other => panic!("wrong variant: {other:?}"),
        }
        let json = serde_json::to_string(&ev).expect("encode");
        assert!(!json.contains("removals_pending"), "{json}");
    }

    /// A waiter held back for the oldest one names it in place of the queue,
    /// each amount in its own pool's unit; the cleanup cause still follows.
    #[test]
    fn the_wait_text_names_what_it_yields_to() {
        let to = |pool: &str, need| YieldingToData {
            pool: pool.into(),
            need,
        };
        let cpu = 1_000_000;
        assert_eq!(
            admission_wait_text(
                "//a:b",
                "cpu",
                Unit::Cpu,
                4 * cpu,
                2 * cpu,
                6,
                None,
                None,
                "41s"
            ),
            "//a:b waits for cpu 4 (2 free, 6 ahead, 41s)",
            "unchanged without it"
        );
        assert_eq!(
            admission_wait_text(
                "//a:b",
                "cpu",
                Unit::Cpu,
                cpu,
                3 * cpu,
                6,
                Some((&to("cpu", 8 * cpu), Unit::Cpu)),
                None,
                "41s"
            ),
            "//a:b waits for cpu 1 (3 free, yielding to a cpu 8 run, 41s)"
        );
        // The pool yielded for is not the one the row waits on.
        assert_eq!(
            admission_wait_text(
                "//a:b",
                "cpu",
                Unit::Cpu,
                cpu,
                3 * cpu,
                0,
                Some((&to("memory", 2 << 30), Unit::Bytes)),
                None,
                "41s"
            ),
            "//a:b waits for cpu 1 (3 free, yielding to a memory 2.0 GiB run, 41s)"
        );
        assert_eq!(
            admission_wait_text(
                "//a:b",
                "sandbox",
                Unit::Count,
                1,
                2,
                1,
                Some((&to("cpu", 8 * cpu), Unit::Cpu)),
                Some(3),
                "41s"
            ),
            "//a:b waits for sandbox 1 (2 free, yielding to a cpu 8 run, 41s) — cleanup is \
             behind: 3 removals pending"
        );
    }

    /// `ExecuteStart.admitted` and `AdmissionWaitStart.yielding_to` are
    /// additive in both directions: a frame from a host that predates them
    /// reads as absent, a value from a newer host reads as `unknown`, a
    /// consumer that predates them still reads a frame that carries them,
    /// and a frame without them is spelled as it was before.
    #[test]
    fn an_old_frame_without_the_new_fields_still_parses() {
        let old_start = r#"{"at_unix_ms":1,"kind":{"type":"ExecuteStart","addr":"//a:x",
            "driver":"exec","cache":true,"declared":{"cpu":1000000},
            "reserved":{"cpu":1000000},"queued_ms":3}}"#;
        let start: BuildEvent = serde_json::from_str(old_start).expect("old frame must decode");
        match &start.kind {
            BuildEventKind::ExecuteStart { admitted, .. } => {
                assert_eq!(*admitted, None, "absent, never `arrival`");
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let old_wait = r#"{"at_unix_ms":1,"kind":{"type":"AdmissionWaitStart","addr":"//a:x",
            "pool":"cpu","need":1,"free":0,"ahead":0}}"#;
        let wait: BuildEvent = serde_json::from_str(old_wait).expect("old frame must decode");
        match &wait.kind {
            BuildEventKind::AdmissionWaitStart { yielding_to, .. } => {
                assert_eq!(*yielding_to, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        // Absent stays absent on the wire.
        for ev in [&start, &wait] {
            let json = serde_json::to_string(ev).expect("encode");
            assert!(
                !json.contains("admitted") && !json.contains("yielding_to"),
                "{json}"
            );
        }

        // A way of being admitted this build does not know.
        let future = r#"{"at_unix_ms":1,"kind":{"type":"ExecuteStart","addr":"//a:x",
            "driver":"exec","cache":true,"admitted":"future"}}"#;
        let ev: BuildEvent = serde_json::from_str(future).expect("unknown value must decode");
        match ev.kind {
            BuildEventKind::ExecuteStart { admitted, .. } => {
                assert_eq!(admitted, Some(AdmittedKind::Unknown));
            }
            other => panic!("wrong variant: {other:?}"),
        }

        // What a new host puts on the wire.
        let new_start = serde_json::to_string(&BuildEvent {
            at_unix_ms: 2,
            kind: BuildEventKind::ExecuteStart {
                addr: "//a:x".into(),
                driver: "exec".into(),
                cache: true,
                declared: None,
                reserved: None,
                queued_ms: Some(3),
                admitted: Some(AdmittedKind::Handoff),
                output_groups: vec!["out".into()],
            },
        })
        .expect("encode");
        assert!(new_start.contains(r#""admitted":"handoff""#), "{new_start}");
        let new_wait = serde_json::to_string(&BuildEvent {
            at_unix_ms: 2,
            kind: BuildEventKind::AdmissionWaitStart {
                addr: "//a:x".into(),
                pool: "cpu".into(),
                need: 1_000_000,
                free: 3_000_000,
                ahead: 2,
                removals_pending: None,
                yielding_to: Some(YieldingToData {
                    pool: "cpu".into(),
                    need: 8_000_000,
                }),
            },
        })
        .expect("encode");
        assert!(
            new_wait.contains(r#""yielding_to":{"pool":"cpu","need":8000000}"#),
            "{new_wait}"
        );
        for kind in [
            AdmittedKind::Arrival,
            AdmittedKind::Head,
            AdmittedKind::Handoff,
            AdmittedKind::Pass,
            AdmittedKind::Unknown,
        ] {
            assert_eq!(
                serde_json::to_string(&kind).expect("encode"),
                format!("\"{}\"", kind.as_str())
            );
        }

        // An older consumer: these events as they were before the fields
        // existed, with the same serde attributes as `BuildEventKind`.
        #[derive(Debug, Deserialize)]
        struct OldEvent {
            kind: OldKind,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "type")]
        enum OldKind {
            ExecuteStart {
                addr: String,
                #[serde(default)]
                queued_ms: Option<u64>,
            },
            AdmissionWaitStart {
                addr: String,
                pool: String,
                need: u64,
                free: u64,
                ahead: u64,
                #[serde(default)]
                removals_pending: Option<u64>,
            },
            #[serde(other)]
            Unknown,
        }
        let old: OldEvent = serde_json::from_str(&new_start).expect("an old consumer must decode");
        match old.kind {
            OldKind::ExecuteStart { addr, queued_ms } => {
                assert_eq!((addr.as_str(), queued_ms), ("//a:x", Some(3)));
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let old: OldEvent = serde_json::from_str(&new_wait).expect("an old consumer must decode");
        match old.kind {
            OldKind::AdmissionWaitStart {
                addr,
                pool,
                need,
                free,
                ahead,
                removals_pending,
            } => {
                assert_eq!((addr.as_str(), pool.as_str()), ("//a:x", "cpu"));
                assert_eq!((need, free, ahead), (1_000_000, 3_000_000, 2));
                assert_eq!(removals_pending, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// A newer host may add a field inside `yielding_to`. This build ignores
    /// it and keeps the two it knows, so the frame decodes and a hook's
    /// stream goes on.
    #[test]
    fn an_unknown_field_inside_yielding_to_is_ignored() {
        let newer = r#"{"at_unix_ms":1,"kind":{"type":"AdmissionWaitStart","addr":"//a:x",
            "pool":"cpu","need":1,"free":3,"ahead":2,
            "yielding_to":{"pool":"cpu","need":1,"extra":2}}}"#;
        let ev: BuildEvent = serde_json::from_str(newer).expect("unknown inner fields are ignored");
        match ev.kind {
            BuildEventKind::AdmissionWaitStart { yielding_to, .. } => {
                assert_eq!(
                    yielding_to,
                    Some(YieldingToData {
                        pool: "cpu".into(),
                        need: 1,
                    })
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// `ResultLockWaitStart` from a host predating `in_use_by_readers` decodes,
    /// and reads as "not known to be readers" — the only thing it could say.
    #[test]
    fn an_old_lock_wait_defaults_to_not_readers() {
        let old = r#"{"at_unix_ms":1,"kind":{"type":"ResultLockWaitStart","addr":"//a:x",
            "holder_pid":null}}"#;
        let ev: BuildEvent = serde_json::from_str(old).expect("old frame must decode");
        match ev.kind {
            BuildEventKind::ResultLockWaitStart {
                holder_pid,
                in_use_by_readers,
                ..
            } => {
                assert_eq!(holder_pid, None);
                assert!(!in_use_by_readers);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// `RequestConfig` from a host predating `scratch_disabled`.
    ///
    /// The default must read as "scratch was on", which is the truth for every
    /// frame emitted before the field existed. The positive spelling would
    /// default these to "scratch was off" and have consumers report a
    /// deliberate-looking audit on runs that never asked for one.
    #[test]
    fn an_old_request_config_defaults_to_scratch_enabled() {
        let old = r#"{"at_unix_ms":1,"kind":{"type":"RequestConfig","max_workers":8,
            "fail_fast":false}}"#;
        let ev: BuildEvent = serde_json::from_str(old).expect("old frame must decode");
        match ev.kind {
            BuildEventKind::RequestConfig {
                scratch_disabled, ..
            } => assert!(
                !scratch_disabled,
                "a host with no such field had scratch enabled",
            ),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// `RequestConfig` from a host predating resources announces no pools.
    #[test]
    fn an_old_request_config_has_no_pools() {
        let old = r#"{"at_unix_ms":1,"kind":{"type":"RequestConfig","max_workers":8,
            "fail_fast":false,"scratch_disabled":false}}"#;
        let ev: BuildEvent = serde_json::from_str(old).expect("old frame must decode");
        match ev.kind {
            BuildEventKind::RequestConfig {
                max_workers,
                resources,
                ..
            } => {
                assert_eq!(max_workers, 8);
                assert!(resources.is_empty(), "{resources:?}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// `ExecuteStart` from a host predating resources declared and reserved
    /// nothing — `None`, which a consumer counts as one CPU — and a new frame
    /// round-trips both maps.
    #[test]
    fn an_old_execute_start_reserved_nothing() {
        let old = r#"{"at_unix_ms":1,"kind":{"type":"ExecuteStart","addr":"//a:x",
            "driver":"exec","cache":true}}"#;
        let ev: BuildEvent = serde_json::from_str(old).expect("old frame must decode");
        match ev.kind {
            BuildEventKind::ExecuteStart {
                declared,
                reserved,
                queued_ms,
                ..
            } => {
                assert_eq!(declared, None);
                assert_eq!(reserved, None);
                assert_eq!(queued_ms, None, "an old frame says nothing of a wait");
            }
            other => panic!("wrong variant: {other:?}"),
        }

        let new = BuildEvent {
            at_unix_ms: 2,
            kind: BuildEventKind::ExecuteStart {
                addr: "//a:x".into(),
                driver: "exec".into(),
                cache: true,
                declared: Some(BTreeMap::from([("cpu".into(), 16_000_000)])),
                reserved: Some(BTreeMap::from([("cpu".into(), 4_000_000)])),
                queued_ms: Some(1500),
                admitted: None,
                output_groups: Vec::new(),
            },
        };
        let back: BuildEvent =
            serde_json::from_str(&serde_json::to_string(&new).expect("encode")).expect("decode");
        match back.kind {
            BuildEventKind::ExecuteStart {
                reserved,
                queued_ms,
                ..
            } => {
                assert_eq!(reserved, Some(BTreeMap::from([("cpu".into(), 4_000_000)])));
                assert_eq!(queued_ms, Some(1500));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn amounts_display_in_the_pools_unit() {
        assert_eq!(display_amount(Unit::Cpu, 4_000_000), "4");
        assert_eq!(display_amount(Unit::Cpu, 500_000), "0.5");
        assert_eq!(display_amount(Unit::Cpu, 10), "0.00001");
        assert_eq!(display_amount(Unit::Bytes, 2 << 30), "2.0 GiB");
        assert_eq!(display_amount(Unit::Count, 2), "2");
    }

    /// A pool's unit comes from `units`; a pool missing there has its
    /// default, and an unknown unit name reads as the default too.
    #[test]
    fn unit_of_reads_the_announced_unit_else_the_default() {
        let units = BTreeMap::from([
            ("scratch_disk".to_owned(), "bytes".to_owned()),
            ("odd".to_owned(), "furlongs".to_owned()),
        ]);
        assert_eq!(unit_of(&units, "scratch_disk"), Unit::Bytes);
        assert_eq!(unit_of(&units, "memory"), Unit::Bytes);
        assert_eq!(unit_of(&units, "cpu"), Unit::Cpu);
        assert_eq!(unit_of(&units, "gpu"), Unit::Count);
        assert_eq!(unit_of(&units, "odd"), Unit::Count);
        assert_eq!(
            clamp_text(
                Some(&BTreeMap::from([("scratch_disk".into(), 8 << 30)])),
                Some(&BTreeMap::from([("scratch_disk".into(), 4 << 30)])),
                |p| unit_of(&units, p),
            )
            .as_deref(),
            Some("scratch_disk 8.0 GiB→4.0 GiB")
        );
    }

    /// The scratch spans decode with their detail fields absent — the shape an
    /// older host, or one that emits only the `Start` half, produces.
    #[test]
    fn scratch_prepare_end_decodes_without_its_detail_fields() {
        let frame = r#"{"at_unix_ms":1,"kind":{"type":"ScratchPrepareEnd",
            "addr":"//a:x","scratch":"//b:cache","error":null}}"#;
        let ev: BuildEvent = serde_json::from_str(frame).expect("must decode");
        match ev.kind {
            BuildEventKind::ScratchPrepareEnd {
                outcome,
                bytes,
                path_mismatch,
                ..
            } => {
                assert_eq!(outcome, "");
                assert_eq!(bytes, 0);
                assert!(!path_mismatch);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// `GcTargetSwept` keeps its shape, and `GcFailureLogsSwept` decodes with
    /// its fields absent.
    #[test]
    fn gc_event_old_frame_decodes() {
        let old = r#"{"at_unix_ms":1,"kind":{"type":"GcTargetSwept",
            "revisions_removed":2,"bytes_removed":8}}"#;
        let ev: BuildEvent = serde_json::from_str(old).expect("old frame must decode");
        assert!(matches!(
            ev.kind,
            BuildEventKind::GcTargetSwept {
                revisions_removed: 2,
                bytes_removed: 8
            }
        ));
        let bare = r#"{"at_unix_ms":1,"kind":{"type":"GcFailureLogsSwept"}}"#;
        let ev: BuildEvent = serde_json::from_str(bare).expect("bare frame must decode");
        assert!(matches!(
            ev.kind,
            BuildEventKind::GcFailureLogsSwept {
                removed: 0,
                bytes_removed: 0
            }
        ));
    }

    /// `GcCacheDb` decodes with every field absent (each is defaulted), and a
    /// full frame round-trips.
    #[test]
    fn gc_cache_db_frame_decodes_with_fields_absent() {
        let bare = r#"{"at_unix_ms":1,"kind":{"type":"GcCacheDb"}}"#;
        let ev: BuildEvent = serde_json::from_str(bare).expect("bare frame must decode");
        match ev.kind {
            BuildEventKind::GcCacheDb {
                file_bytes,
                wal_bytes,
                live_bytes,
                free_bytes,
                mode,
                action,
                freed_bytes,
                needed_bytes,
                available_bytes,
                detail,
            } => {
                assert_eq!(
                    (file_bytes, wal_bytes, live_bytes, free_bytes),
                    (0, 0, 0, 0)
                );
                assert_eq!((freed_bytes, needed_bytes, available_bytes), (0, 0, 0));
                assert_eq!((mode.as_str(), action.as_str()), ("", ""));
                assert_eq!(detail, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        let full = r#"{"at_unix_ms":1,"kind":{"type":"GcCacheDb","file_bytes":10,
            "wal_bytes":1,"live_bytes":7,"free_bytes":3,"mode":"none",
            "action":"skipped","freed_bytes":0,"needed_bytes":9,
            "available_bytes":4,"detail":"short"}}"#;
        let ev: BuildEvent = serde_json::from_str(full).expect("full frame must decode");
        let back = serde_json::to_string(&ev).expect("encode");
        let again: BuildEvent = serde_json::from_str(&back).expect("re-decode");
        assert!(matches!(
            again.kind,
            BuildEventKind::GcCacheDb {
                file_bytes: 10,
                needed_bytes: 9,
                available_bytes: 4,
                ..
            }
        ));
    }

    /// A frame from a host *newer* than this build: it carries fields this build
    /// has never heard of. Serde ignores unknown fields by default, and nothing
    /// here sets `deny_unknown_fields` — adding it would break this.
    #[test]
    fn newer_host_frame_with_unknown_fields_decodes() {
        let newer = r#"{"at_unix_ms":1,"kind":{"type":"ResultEnd","addr":"//a:x",
            "error":null,"upstream_of":null,"exit_status":null,"log_tail":null,
            "some_field_from_the_future":{"nested":true}}}"#;
        let ev: BuildEvent = serde_json::from_str(newer).expect("unknown fields are ignored");
        assert!(matches!(ev.kind, BuildEventKind::ResultEnd { .. }));
    }

    /// An event *kind* from a newer host. Without `Unknown`'s `#[serde(other)]`
    /// this fails to deserialize, and one such event silently ends the whole
    /// stream for an out-of-process hook while it believes the build finished.
    #[test]
    fn unknown_event_kind_decodes_instead_of_ending_the_stream() {
        let future = r#"{"at_unix_ms":1,"kind":{"type":"SomethingInventedLater","x":1}}"#;
        let ev: BuildEvent = serde_json::from_str(future).expect("unknown kind must not be fatal");
        assert!(matches!(ev.kind, BuildEventKind::Unknown));
    }

    /// A whole stream survives an unknown kind in the middle — the property that
    /// actually matters, since the failure mode is truncation, not one lost event.
    #[test]
    fn a_stream_continues_past_an_unknown_kind() {
        let frames = [
            r#"{"at_unix_ms":1,"kind":{"type":"RequestConfig","max_workers":8,"fail_fast":false}}"#,
            r#"{"at_unix_ms":2,"kind":{"type":"FutureKind","whatever":true}}"#,
            r#"{"at_unix_ms":3,"kind":{"type":"ResultEnd","addr":"//a:x","error":null}}"#,
        ];
        let decoded: Vec<BuildEvent> = frames
            .iter()
            .map(|f| serde_json::from_str(f).expect("every frame decodes"))
            .collect();
        assert_eq!(decoded.len(), 3);
        assert!(matches!(decoded[1].kind, BuildEventKind::Unknown));
        assert!(matches!(decoded[2].kind, BuildEventKind::ResultEnd { .. }));
    }

    /// New fields round-trip when both sides know them.
    #[test]
    fn structured_failure_detail_round_trips() {
        let ev = BuildEvent {
            at_unix_ms: 7,
            kind: BuildEventKind::ResultEnd {
                addr: "//a:x".into(),
                error: Some("target failed".into()),
                upstream_of: Some("//base:proto".into()),
                exit_status: Some("exit status: 1".into()),
                log_tail: Some(LogTailData {
                    text: "FAIL".into(),
                    start_line: 88,
                }),
                timed_out: Some(TimedOutData {
                    timeout_ms: 600_000,
                    abandoned: true,
                }),
            },
        };
        let json = serde_json::to_string(&ev).expect("encode");
        let back: BuildEvent = serde_json::from_str(&json).expect("decode");
        match back.kind {
            BuildEventKind::ResultEnd {
                upstream_of,
                exit_status,
                log_tail,
                timed_out,
                ..
            } => {
                assert_eq!(upstream_of.as_deref(), Some("//base:proto"));
                assert_eq!(exit_status.as_deref(), Some("exit status: 1"));
                assert_eq!(log_tail.map(|l| l.start_line), Some(88));
                assert_eq!(
                    timed_out,
                    Some(TimedOutData {
                        timeout_ms: 600_000,
                        abandoned: true
                    })
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// `ResultOutputs` round-trips, a frame missing its groups decodes, a
    /// consumer built before it skips it as `Unknown` and reads on, and
    /// `ExecuteStart.output_groups` is absent from the wire when empty and
    /// read back when not, by a decoder that predates it too.
    #[test]
    fn result_outputs_round_trips_and_old_decoders_see_unknown() {
        let groups = vec![
            OutputGroupSummary {
                group: String::new(),
                size: Some(12),
                hash: Some("ab12".into()),
                artifacts: 1,
            },
            OutputGroupSummary {
                group: "rust".into(),
                size: None,
                hash: None,
                artifacts: 2,
            },
        ];
        let ev = BuildEvent {
            at_unix_ms: 4,
            kind: BuildEventKind::ResultOutputs {
                addr: "//a:x".into(),
                groups: groups.clone(),
            },
        };
        let json = serde_json::to_string(&ev).expect("encode");
        let back: BuildEvent = serde_json::from_str(&json).expect("decode");
        assert!(
            matches!(
                &back.kind,
                BuildEventKind::ResultOutputs { addr, groups: g } if addr == "//a:x" && *g == groups
            ),
            "{back:?}"
        );
        let sparse = r#"{"at_unix_ms":1,"kind":{"type":"ResultOutputs","addr":"//a:x"}}"#;
        let ev: BuildEvent = serde_json::from_str(sparse).expect("sparse frame decodes");
        assert!(
            matches!(ev.kind, BuildEventKind::ResultOutputs { ref groups, .. } if groups.is_empty())
        );

        let start = |output_groups: Vec<String>| {
            serde_json::to_string(&BuildEvent {
                at_unix_ms: 2,
                kind: BuildEventKind::ExecuteStart {
                    addr: "//a:x".into(),
                    driver: "exec".into(),
                    cache: true,
                    declared: None,
                    reserved: None,
                    queued_ms: None,
                    admitted: None,
                    output_groups,
                },
            })
            .expect("encode")
        };
        assert!(!start(Vec::new()).contains("output_groups"));
        let named = start(vec!["out".into(), "gen".into()]);
        let back: BuildEvent = serde_json::from_str(&named).expect("decode");
        assert!(
            matches!(
                &back.kind,
                BuildEventKind::ExecuteStart { output_groups, .. } if output_groups == &["out", "gen"]
            ),
            "{back:?}"
        );

        #[derive(serde::Deserialize)]
        struct OldEvent {
            kind: OldKind,
        }
        #[derive(serde::Deserialize, Debug)]
        #[serde(tag = "type")]
        enum OldKind {
            ResultStart {
                #[expect(dead_code, reason = "decoded only to prove the shape")]
                addr: String,
            },
            ExecuteStart {
                #[expect(dead_code, reason = "decoded only to prove the shape")]
                addr: String,
            },
            #[serde(other)]
            Unknown,
        }
        let stream = [
            json,
            named,
            r#"{"at_unix_ms":5,"kind":{"type":"ResultStart","addr":"//a:x"}}"#.into(),
        ];
        let old: Vec<OldKind> = stream
            .iter()
            .map(|l| {
                serde_json::from_str::<OldEvent>(l)
                    .expect("old decoder")
                    .kind
            })
            .collect();
        assert!(
            matches!(
                old.as_slice(),
                [
                    OldKind::Unknown,
                    OldKind::ExecuteStart { .. },
                    OldKind::ResultStart { .. }
                ]
            ),
            "{old:?}"
        );
    }

    /// `PoolUsage` round-trips, a frame missing its later fields decodes,
    /// and a consumer built before it skips it as `Unknown` and reads on.
    #[test]
    fn pool_usage_round_trips_and_old_decoders_see_unknown() {
        let ev = BuildEvent {
            at_unix_ms: 4,
            kind: BuildEventKind::PoolUsage {
                pool: "sandbox".into(),
                held: 9,
                capacity: 48,
                waiting: 2,
                removals_pending: 3,
            },
        };
        let json = serde_json::to_string(&ev).expect("encode");
        let back: BuildEvent = serde_json::from_str(&json).expect("decode");
        assert!(
            matches!(
                back.kind,
                BuildEventKind::PoolUsage {
                    ref pool,
                    held: 9,
                    capacity: 48,
                    waiting: 2,
                    removals_pending: 3,
                } if pool == "sandbox"
            ),
            "{back:?}"
        );
        let sparse = r#"{"at_unix_ms":1,"kind":{"type":"PoolUsage","pool":"cpu"}}"#;
        let ev: BuildEvent = serde_json::from_str(sparse).expect("sparse frame decodes");
        assert!(matches!(
            ev.kind,
            BuildEventKind::PoolUsage {
                held: 0,
                waiting: 0,
                ..
            }
        ));

        #[derive(serde::Deserialize)]
        struct OldEvent {
            kind: OldKind,
        }
        #[derive(serde::Deserialize, Debug)]
        #[serde(tag = "type")]
        enum OldKind {
            ResultStart {
                #[expect(dead_code, reason = "decoded only to prove the shape")]
                addr: String,
            },
            #[serde(other)]
            Unknown,
        }
        let stream = [
            json,
            r#"{"at_unix_ms":5,"kind":{"type":"ResultStart","addr":"//a:x"}}"#.into(),
        ];
        let old: Vec<OldKind> = stream
            .iter()
            .map(|l| {
                serde_json::from_str::<OldEvent>(l)
                    .expect("old decoder")
                    .kind
            })
            .collect();
        assert!(
            matches!(
                old.as_slice(),
                [OldKind::Unknown, OldKind::ResultStart { .. }]
            ),
            "{old:?}"
        );
    }

    /// `SelectionIncomplete` survives the wire, and a consumer built before it
    /// existed skips it as `Unknown` instead of ending its stream.
    #[test]
    fn selection_incomplete_round_trips_and_old_decoders_see_unknown() {
        let groups = vec![SelectionGap {
            stage: "spec".into(),
            provider: "go".into(),
            count: 412,
            examples: vec![SelectionGapExample {
                scope: "//svc/api:build".into(),
                cause: "go list: exit status 1".into(),
            }],
        }];
        let ev = BuildEvent {
            at_unix_ms: 9,
            kind: BuildEventKind::SelectionIncomplete {
                skipped: 412,
                groups: groups.clone(),
            },
        };
        let json = serde_json::to_string(&ev).expect("encode");
        let back: BuildEvent = serde_json::from_str(&json).expect("decode");
        match back.kind {
            BuildEventKind::SelectionIncomplete {
                skipped,
                groups: got,
            } => {
                assert_eq!(skipped, 412);
                assert_eq!(got, groups);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        // The shape of this enum before the variant existed: tagged the same
        // way, with the same catch-all.
        #[derive(Debug, Deserialize)]
        #[serde(tag = "type")]
        enum OlderKind {
            #[expect(dead_code, reason = "only the variant tag is under test")]
            Matched { addrs: Vec<String>, complete: bool },
            #[serde(other)]
            Unknown,
        }
        #[derive(Debug, Deserialize)]
        struct OlderEvent {
            kind: OlderKind,
        }
        let old: OlderEvent =
            serde_json::from_str(&json).expect("an older consumer must decode it");
        assert!(matches!(old.kind, OlderKind::Unknown), "{old:?}");

        // And a trimmed frame decodes with every field defaulted.
        let bare = r#"{"at_unix_ms":1,"kind":{"type":"SelectionIncomplete"}}"#;
        let ev: BuildEvent = serde_json::from_str(bare).expect("must decode");
        assert!(matches!(
            ev.kind,
            BuildEventKind::SelectionIncomplete { skipped: 0, ref groups } if groups.is_empty()
        ));
    }

    fn fold(kinds: &[BuildEventKind]) -> CacheCounts {
        let mut c = CacheCounts::default();
        for k in kinds {
            c.apply(k);
        }
        c
    }

    fn outcome(o: CacheOutcomeKind) -> BuildEventKind {
        BuildEventKind::CacheOutcome {
            addr: "//a:x".into(),
            outcome: o,
        }
    }

    fn rebuilt(was: CacheHitKind) -> BuildEventKind {
        BuildEventKind::CacheHitRebuilt {
            addr: "//a:x".into(),
            was,
        }
    }

    #[test]
    fn cache_counts_fold_the_shared_stream() {
        let kinds: Vec<_> = test_support::cache_stream()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        let c = fold(&kinds);
        assert_eq!(
            (c.local_hits(), c.remote_hits(), c.misses(), c.rebuilt()),
            test_support::CACHE_STREAM_EXPECTED
        );
        assert_eq!(c.consulted(), 7, "one count per cacheable target");
    }

    #[test]
    fn cache_counts_ignore_tier_events() {
        let c = fold(&[
            BuildEventKind::LocalCacheMiss {
                addr: "//a:x".into(),
            },
            BuildEventKind::RemoteCacheMiss {
                addr: "//a:x".into(),
            },
            BuildEventKind::LocalCacheHit {
                addr: "//a:y".into(),
            },
            BuildEventKind::RemoteCacheHit {
                addr: "//a:z".into(),
            },
        ]);
        assert_eq!(c, CacheCounts::default());
        assert_eq!(c.hit_rate(), None, "nothing consulted is not 0%");
    }

    #[test]
    fn cache_counts_rebuilt_moves_one_from_its_was_kind_to_misses() {
        let c = fold(&[
            outcome(CacheOutcomeKind::LocalHit),
            outcome(CacheOutcomeKind::RemoteHit),
            rebuilt(CacheHitKind::Remote),
        ]);
        assert_eq!(
            (c.local_hits(), c.remote_hits(), c.misses(), c.rebuilt()),
            (1, 0, 1, 1)
        );
        assert_eq!(c.hit_rate(), Some(0.5));

        let c = fold(&[
            outcome(CacheOutcomeKind::LocalHit),
            rebuilt(CacheHitKind::Local),
        ]);
        assert_eq!(
            (c.local_hits(), c.remote_hits(), c.misses(), c.rebuilt()),
            (0, 0, 1, 1)
        );
        assert_eq!(c.hit_rate(), Some(0.0));
    }

    /// A stream joined after the hit was emitted cannot underflow.
    #[test]
    fn cache_counts_saturate() {
        let c = fold(&[rebuilt(CacheHitKind::Local)]);
        assert_eq!((c.local_hits(), c.misses(), c.rebuilt()), (0, 1, 1));
    }

    #[test]
    fn cache_counts_ignore_unknown_kinds() {
        let c = fold(&[
            outcome(CacheOutcomeKind::Unknown),
            rebuilt(CacheHitKind::Unknown),
        ]);
        assert_eq!(c, CacheCounts::default());
    }

    /// Both new variants round-trip, a consumer that predates them skips them
    /// as `Unknown`, and an outcome value from a newer host decodes rather
    /// than ending the stream.
    #[test]
    fn new_cache_events_decode_as_unknown_in_old_shape() {
        for kind in [
            outcome(CacheOutcomeKind::RemoteHit),
            rebuilt(CacheHitKind::Local),
        ] {
            let ev = BuildEvent {
                at_unix_ms: 3,
                kind,
            };
            let json = serde_json::to_string(&ev).expect("encode");
            let back: BuildEvent = serde_json::from_str(&json).expect("decode");
            assert_eq!(
                serde_json::to_string(&back).expect("re-encode"),
                json,
                "round trip is stable"
            );

            #[derive(Debug, Deserialize)]
            #[serde(tag = "type")]
            enum OlderKind {
                #[expect(dead_code, reason = "only the variant tag is under test")]
                LocalCacheMiss { addr: String },
                #[serde(other)]
                Unknown,
            }
            #[derive(Debug, Deserialize)]
            struct OlderEvent {
                kind: OlderKind,
            }
            let old: OlderEvent = serde_json::from_str(&json).expect("older consumer decodes");
            assert!(matches!(old.kind, OlderKind::Unknown), "{old:?}");
        }

        let wire = r#"{"at_unix_ms":1,"kind":{"type":"CacheOutcome","addr":"//a:x","outcome":"remote_hit"}}"#;
        let ev: BuildEvent = serde_json::from_str(wire).expect("decode");
        assert!(matches!(
            ev.kind,
            BuildEventKind::CacheOutcome {
                outcome: CacheOutcomeKind::RemoteHit,
                ..
            }
        ));

        let future = r#"{"at_unix_ms":1,"kind":{"type":"CacheOutcome","addr":"//a:x","outcome":"peer_hit"}}"#;
        let ev: BuildEvent = serde_json::from_str(future).expect("an unknown outcome decodes");
        assert!(matches!(
            ev.kind,
            BuildEventKind::CacheOutcome {
                outcome: CacheOutcomeKind::Unknown,
                ..
            }
        ));
        let future =
            r#"{"at_unix_ms":1,"kind":{"type":"CacheHitRebuilt","addr":"//a:x","was":"peer"}}"#;
        let ev: BuildEvent = serde_json::from_str(future).expect("an unknown tier decodes");
        assert!(matches!(
            ev.kind,
            BuildEventKind::CacheHitRebuilt {
                was: CacheHitKind::Unknown,
                ..
            }
        ));
    }
}
