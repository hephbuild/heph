//! The broker: resolve each distinct descriptor once per run, refresh on
//! expiry, and know every live value so the redactor can mask them.
//!
//! # Why this is not the engine's `Memoizer`
//!
//! The engine's `Memoizer` gives the dedup half for free, but *not* the refresh
//! half: `Memoizer::once` is compute-once for the life of the `RequestState`
//! and has no expiry or cell replacement. A credential has a clock on it and a
//! build is a long thing made of short things, so the broker owns a small cache
//! of its own with real TTL semantics — seeded by whichever of the four expiry
//! sources actually knew ([`crate::expiry`]).
//!
//! # Scope: within a run, not across runs
//!
//! Within one run the broker deduplicates: every target naming
//! `//infra/creds:ecr` shares one STS call. Across runs it does not — a CI
//! matrix, or two worktrees, or two terminals on one laptop each mint
//! independently. For an STS-class endpoint that is unremarkable. For a
//! rate-limited IdP on a wide matrix it is a stampede, and the obvious
//! mitigation — a short-lived on-disk cache keyed by descriptor — directly
//! contradicts "nothing durable is written". That trade is a decision to make
//! deliberately rather than discover under load, and it is deliberately not
//! made here.
//!
//! # One rule: never interactive during a build
//!
//! A build that opens a browser at target 400 of 900 is an ambush for a human
//! and a silent hang for an agent. That is enforced structurally rather than by
//! convention: a helper's stdin is never [`hproc::proc_exec::StdioSpec::Inherit`]
//! (see [`crate::provider`]), so a helper that tries to prompt reads EOF and
//! fails at once instead of blocking on a human who is not watching.

use crate::descriptor::Descriptor;
use crate::provider::{EnvLookup, MintCtx, ProviderRegistry, SharedEnvLookup};
use crate::redact::{Entry, Redactor};
use crate::value::{Credential, SecretValue};
use hcore::hasync::Cancellable;
use hmodel::htaddr::Addr;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::Mutex;

/// What the caller must supply for a mint that this crate cannot discover.
pub struct BrokerCtx<'a> {
    pub now: SystemTime,
    pub env: EnvLookup<'a>,
    pub ctoken: &'a (dyn Cancellable + Send + Sync),
    pub request_id: &'a str,
    /// The `runner` address on the selected acquire entry, already resolved by
    /// the caller — turning an address into a built runner target needs the
    /// engine, which this crate does not have.
    ///
    /// **The caller must resolve it from the same [`Descriptor::select`] the
    /// broker will make**, or the helper runs under a runner belonging to a
    /// different `acquire` entry. Nothing here can detect that mismatch; when
    /// the wiring lands it should pass the [`crate::descriptor::Selection`]
    /// itself so the choice is made exactly once.
    pub runner: Option<&'a Addr>,
    /// Working directory for an `exec` helper — the workspace root, not
    /// whatever directory heph was invoked from. See [`MintCtx::cwd`].
    pub cwd: &'a std::path::Path,
    /// Where this user's sessions live. See [`MintCtx::auth_home`].
    pub auth_home: Option<&'a std::path::Path>,
}

/// A record of one mint, for `heph auth show` and for the grant event.
///
/// Never the value: this is the shape that is allowed to reach a log.
#[derive(Debug, Clone)]
pub struct Grant {
    pub addr: String,
    /// Which `acquire` entry ran, and what selected it. The route taken is
    /// otherwise invisible in the output.
    pub acquire_index: usize,
    pub selected_by: Option<String>,
    pub provider: crate::descriptor::ProviderKind,
    pub expires_at: SystemTime,
    pub expiry_source: crate::expiry::ExpirySource,
}

/// How often the background sweep looks for credentials to renew.
///
/// Well under [`crate::expiry::MIN_HANDOUT_LIFETIME`], so a credential crossing
/// the headroom line is picked up long before a consumer could be handed it.
/// One wakeup a minute for a whole run is not a cost worth tuning.
pub const REFRESH_TICK: Duration = Duration::from_secs(60);

/// Everything the background sweep needs, owned rather than borrowed.
///
/// [`BrokerCtx`] borrows, which is right for a call made on a consumer's
/// behalf and impossible for a task that outlives every individual call. The
/// environment lookup is the process environment either way, so an owned handle
/// to it means the same thing.
#[derive(Clone)]
pub struct RefreshConfig {
    pub env: SharedEnvLookup,
    pub ctoken: Arc<dyn Cancellable + Send + Sync>,
    pub request_id: String,
    pub cwd: std::path::PathBuf,
    /// Owned, unlike [`BrokerCtx::auth_home`]: the sweep outlives every
    /// individual call, so it cannot borrow one's context.
    pub auth_home: Option<std::path::PathBuf>,
}

/// Stops the background sweep when dropped.
///
/// The task holds a weak reference to the broker, so it never keeps a run
/// alive on its own; this guard is what stops it promptly rather than at the
/// next tick after everything else has gone.
pub struct RefreshGuard {
    stop: hcore::hasync::StdCancellationToken,
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Per-run credential broker.
pub struct Broker {
    registry: Arc<ProviderRegistry>,
    /// Descriptor address → its single in-flight-or-cached slot. The outer lock
    /// is held only long enough to clone an `Arc`; the mint itself happens
    /// under the *slot's* lock, so two different descriptors never serialize
    /// against each other.
    slots: Mutex<BTreeMap<String, Arc<Mutex<Option<Cached>>>>>,
    /// Every value minted so far, for the redactor. Separate from `slots`
    /// because the redactor must be buildable without touching a slot that is
    /// mid-mint — which would deadlock the very error path that needs it.
    live: Mutex<LiveValues>,
    grants: Mutex<Vec<Grant>>,
}

/// How many superseded values per secret stay maskable after a re-mint.
///
/// Not unbounded, which is what it was. An unannotated `raw` descriptor
/// re-mints roughly every [`crate::expiry::DEFAULT_TTL`] minus the refresh
/// margin, so a long build accumulates hundreds of dead values per secret — a
/// monotonically growing pattern set rebuilt on every mint (quadratic), every
/// dead token held in memory for the run, and, worst of the three, a
/// `first_bytes` table saturating toward the whole base64 alphabet, which
/// destroys the pruning the per-chunk cost of `hold_back` depends on.
///
/// A few generations is enough: a superseded value can still be sitting in a
/// stream's carry buffer or an in-flight log line, but not four re-mints later.
const MAX_LIVE_PER_SECRET: usize = 4;

/// A minted credential, plus everything needed to mint it again without a
/// caller.
///
/// The background sweep has no `BrokerCtx` of its own and no consumer asking,
/// so the descriptor, the consumer's name and the resolved runner are kept from
/// the first mint. Storing them is what makes a refresh possible at all.
struct Cached {
    cred: Credential,
    desc: Arc<Descriptor>,
    /// The consumer's name for it, which is what `«redacted:NAME»` shows.
    name: String,
    /// The already-resolved runner from the first mint.
    runner: Option<Addr>,
}

#[derive(Default)]
struct LiveValues {
    /// `(secret name, value)`, oldest first, at most [`MAX_LIVE_PER_SECRET`]
    /// per name.
    ///
    /// `SecretValue`, not `String`: `value.rs` calls itself "the one type in
    /// the tree allowed to hold a live value", and a bare `String` here made
    /// that false — the day someone derives `Debug` on `Broker` for a
    /// diagnostic, every live token would print.
    entries: Vec<(String, SecretValue)>,
    /// Rebuilt whenever `entries` changes. Cheap: a handful of short patterns.
    redactor: Redactor,
}

/// Counts and addresses, never values. Hand-written for the same reason
/// [`SecretValue`]'s is: a derived one would print every live credential the
/// moment someone reached for a diagnostic.
impl std::fmt::Debug for Broker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `try_lock`, never `lock`: `Debug` is sync and may be called from
        // anywhere, including from inside a mint that already holds these.
        let slots = self
            .slots
            .try_lock()
            .map(|s| s.keys().cloned().collect::<Vec<_>>());
        let live = self.live.try_lock().map(|l| l.entries.len());
        f.debug_struct("Broker")
            .field("descriptors", &slots.ok())
            .field("live_values", &live.ok())
            .finish()
    }
}

/// The closure and cancellation token are not `Debug`, so this reports the
/// fields a diagnostic would actually want.
impl std::fmt::Debug for BrokerCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerCtx")
            .field("request_id", &self.request_id)
            .field("runner", &self.runner.map(hmodel::htaddr::Addr::format))
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

impl Broker {
    pub fn new(registry: Arc<ProviderRegistry>) -> Self {
        Self {
            registry,
            slots: Mutex::new(BTreeMap::new()),
            live: Mutex::new(LiveValues::default()),
            grants: Mutex::new(Vec::new()),
        }
    }

    /// The redactor covering everything minted so far.
    ///
    /// A snapshot, deliberately: a stream that started before a later mint
    /// keeps the older automaton until it next asks. That is correct, because a
    /// value that did not exist yet cannot have been printed yet.
    pub async fn redactor(&self) -> Redactor {
        self.live.lock().await.redactor.clone()
    }

    /// Every grant made this run, for the event stream and `heph auth show`.
    pub async fn grants(&self) -> Vec<Grant> {
        self.grants.lock().await.clone()
    }

    /// Obtain a value for one descriptor, minting only if there is no live one.
    ///
    /// `name` is the consumer's name for it, which is what appears in
    /// `«redacted:NAME»` and in `$SECRET_<NAME>`.
    pub async fn mint(
        &self,
        desc: &Descriptor,
        name: &str,
        ctx: &BrokerCtx<'_>,
    ) -> anyhow::Result<Credential> {
        let slot = {
            let mut slots = self.slots.lock().await;
            Arc::clone(
                slots
                    .entry(desc.addr.clone())
                    .or_insert_with(|| Arc::new(Mutex::new(None))),
            )
        };

        // Single-flight: concurrent consumers of one descriptor queue here, and
        // the second one finds a live value rather than making a second STS
        // call. This is the whole reason a broker exists rather than each
        // driver minting for itself.
        let mut guard = slot.lock().await;

        // Reuse only a credential a target can actually *do something with*.
        //
        // `stale_at` alone was the bug: it answers "is this still valid?", and
        // a value one second inside the skew margin passes that and then fails
        // every target that runs for longer than a second. A handout wants
        // headroom, so a still-valid credential with too little life left is
        // re-minted now rather than handed over to fail later.
        //
        // Unless re-minting cannot help — a credential whose whole lifetime is
        // shorter than the headroom would be re-minted on every single handout,
        // paying a mint per target to buy nothing. That is worth saying once,
        // not fixing silently.
        if let Some(existing) = guard.as_ref() {
            let e = &existing.cred.expiry;
            if !e.stale_at(ctx.now)
                && (e.has_handout_headroom(ctx.now) || !e.refresh_would_help(ctx.now))
            {
                if !e.has_handout_headroom(ctx.now) {
                    tracing::warn!(
                        secret = %desc.addr,
                        usable_lifetime_s = e.usable_lifetime().as_secs(),
                        min_s = crate::expiry::MIN_HANDOUT_LIFETIME.as_secs(),
                        "credential's whole lifetime is shorter than the headroom a target \
                         wants; re-minting cannot help, so a long target may outlive it. Raise \
                         the ttl, or give the target a process credential."
                    );
                }
                return Ok(existing.cred.clone());
            }
        }

        let cred = self.mint_into(desc, name, ctx.runner, ctx).await?;

        *guard = Some(Cached {
            cred: cred.clone(),
            desc: Arc::new(desc.clone()),
            name: name.to_string(),
            runner: ctx.runner.cloned(),
        });
        Ok(cred)
    }

    /// Select a route, call its provider, and record the grant.
    ///
    /// Shared by the on-demand path and the background sweep, so a refreshed
    /// credential is registered with the redactor and recorded as a grant on
    /// exactly the same terms as a freshly minted one. Deliberately does *not*
    /// touch the slot: its caller owns the lock and decides what to store.
    async fn mint_into(
        &self,
        desc: &Descriptor,
        name: &str,
        runner: Option<&Addr>,
        ctx: &BrokerCtx<'_>,
    ) -> anyhow::Result<Credential> {
        let selection = desc.select(ctx.env)?;
        let provider = self.registry.get(selection.entry.source.kind())?;

        let redactor = self.redactor().await;
        let mint_ctx = MintCtx {
            addr: &desc.addr,
            now: ctx.now,
            env: ctx.env,
            ctoken: ctx.ctoken,
            request_id: ctx.request_id,
            runner,
            cwd: ctx.cwd,
            redactor: &redactor,
            auth_home: ctx.auth_home,
        };

        let cred = provider
            .mint(&mint_ctx, &desc.identity, selection.entry)
            .await?;

        // Register before returning, so the value is maskable from the moment
        // it exists rather than from the moment it is first delivered.
        self.register_live(name, &cred).await;

        self.grants.lock().await.push(Grant {
            addr: desc.addr.clone(),
            acquire_index: selection.index,
            selected_by: selection.matched.clone(),
            provider: selection.entry.source.kind(),
            expires_at: cred.expiry.at,
            expiry_source: cred.expiry.source,
        });

        tracing::debug!(
            secret = %desc.addr,
            provider = ?selection.entry.source.kind(),
            acquire_index = selection.index,
            expiry_source = cred.expiry.source.as_str(),
            "minted a credential"
        );
        Ok(cred)
    }

    /// Start refreshing credentials in the background for the life of the run.
    ///
    /// A build that runs for hours would otherwise refresh only when a consumer
    /// asks, which puts the mint latency on whichever target happens to be
    /// unlucky and leaves a window where a target starting just then is handed
    /// the short end of a credential's life. The sweep moves that work off the
    /// critical path entirely: by the time a target asks, the value is warm.
    ///
    /// This is *not* the answer to a credential expiring **inside** a single
    /// long-running target. Nothing outside that process can replace a value it
    /// has already read; that case needs a process credential the tool re-reads
    /// (`credential_process`, `GOAUTH=command`, a git `credential.helper`),
    /// which is a separate piece of work.
    pub fn spawn_refresher(self: &Arc<Self>, cfg: RefreshConfig) -> RefreshGuard {
        self.spawn_refresher_every(cfg, REFRESH_TICK)
    }

    /// [`Self::spawn_refresher`] with the tick chosen by the caller, for tests.
    pub fn spawn_refresher_every(
        self: &Arc<Self>,
        cfg: RefreshConfig,
        tick: Duration,
    ) -> RefreshGuard {
        let stop = hcore::hasync::StdCancellationToken::new();
        // `Weak`, so a forgotten guard leaks a sleeping task rather than
        // pinning the whole broker — and every live credential in it — for the
        // life of the process.
        let weak = Arc::downgrade(self);
        let stop_for_task = stop.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = stop_for_task.cancelled() => return,
                    () = cfg.ctoken.cancelled() => return,
                    () = tokio::time::sleep(tick) => {}
                }
                let Some(broker) = weak.upgrade() else { return };
                let ctx = BrokerCtx {
                    now: SystemTime::now(),
                    env: cfg.env.as_ref(),
                    ctoken: cfg.ctoken.as_ref(),
                    request_id: &cfg.request_id,
                    runner: None,
                    cwd: &cfg.cwd,
                    auth_home: cfg.auth_home.as_deref(),
                };
                let n = broker.refresh_due(&ctx).await;
                if n > 0 {
                    tracing::debug!(count = n, "refreshed credentials ahead of demand");
                }
            }
        });

        RefreshGuard { stop }
    }

    /// Re-mint every credential that a handout would now refuse.
    ///
    /// This is the background half. A run that lasts hours would otherwise
    /// leave every credential to be refreshed by the next consumer that asks —
    /// which makes some unlucky target pay the mint latency, and leaves a gap
    /// where a target starting just then gets the short end of a credential's
    /// life. Sweeping ahead of demand keeps values warm, so a handout is a
    /// clone rather than a network round trip.
    ///
    /// Separate from the loop that calls it so the *logic* is testable without
    /// spawning anything or waiting on a clock.
    ///
    /// Returns the number re-minted. Failures are logged and left in place: the
    /// old value may still be usable, and a background sweep is the wrong place
    /// to fail a build — the consumer that actually needs it will surface the
    /// error with its own target's name attached.
    pub async fn refresh_due(&self, ctx: &BrokerCtx<'_>) -> usize {
        let slots: Vec<(String, Arc<Mutex<Option<Cached>>>)> = {
            let slots = self.slots.lock().await;
            slots
                .iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect()
        };

        let mut refreshed = 0usize;
        for (addr, slot) in slots {
            // `try_lock`: a slot mid-mint is already being refreshed by whoever
            // holds it, and a sweep must never queue behind a helper
            // subprocess — that would stall every later slot behind one slow
            // one.
            let Ok(mut guard) = slot.try_lock() else {
                continue;
            };
            let Some(cached) = guard.as_ref() else {
                continue;
            };
            let e = &cached.cred.expiry;
            if e.has_handout_headroom(ctx.now) || !e.refresh_would_help(ctx.now) {
                continue;
            }

            let (desc, name, runner) = (
                Arc::clone(&cached.desc),
                cached.name.clone(),
                cached.runner.clone(),
            );
            match self.mint_into(&desc, &name, runner.as_ref(), ctx).await {
                Ok(cred) => {
                    *guard = Some(Cached {
                        cred,
                        desc,
                        name,
                        runner,
                    });
                    refreshed = refreshed.saturating_add(1);
                }
                Err(e) => tracing::warn!(
                    secret = %addr,
                    error = %e,
                    "background credential refresh failed; the existing value is kept and the \
                     next consumer will report this with its own target attached"
                ),
            }
        }
        refreshed
    }

    async fn register_live(&self, name: &str, cred: &Credential) {
        let mut live = self.live.lock().await;
        let mut changed = false;
        for value in cred.values() {
            if value.is_empty() {
                continue;
            }
            if live
                .entries
                .iter()
                .any(|(n, existing)| n == name && existing == value)
            {
                continue;
            }
            live.entries.push((name.to_string(), value.clone()));
            changed = true;
        }
        if !changed {
            return;
        }

        // Evict the oldest generations of this secret. Bounded rather than
        // monotonic — see `MAX_LIVE_PER_SECRET`.
        let mine = live.entries.iter().filter(|(n, _)| n == name).count();
        if let Some(excess) = mine.checked_sub(MAX_LIVE_PER_SECRET).filter(|e| *e > 0) {
            let mut dropped = 0usize;
            live.entries.retain(|(n, _)| {
                if n == name && dropped < excess {
                    dropped = dropped.saturating_add(1);
                    return false;
                }
                true
            });
        }

        let entries: Vec<Entry<'_>> = live
            .entries
            .iter()
            .map(|(n, v)| Entry {
                name: n.as_str(),
                value: v.expose(),
            })
            .collect();
        let (redactor, too_short) = Redactor::new(&entries);
        for name in too_short {
            // Warn rather than fail: the credential still works, and a
            // redactor that shredded a build log would be worse than one that
            // misses. The author can see this and choose.
            tracing::warn!(
                secret = %name,
                min = crate::redact::MIN_PATTERN_LEN,
                "credential value is too short to mask safely; it will appear in logs verbatim"
            );
        }
        live.redactor = redactor;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{Acquire, Identity, ProviderKind, Source, WhenEnv};
    use crate::expiry::{Expiry, ExpirySource};
    use crate::provider::SecretProvider;
    use hcore::hasync::StdCancellationToken;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Counts calls, so dedup and refresh are observable rather than inferred.
    struct CountingProvider {
        calls: AtomicUsize,
        ttl: Duration,
    }

    #[async_trait::async_trait]
    impl SecretProvider for CountingProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::StaticEnv
        }
        async fn mint(
            &self,
            ctx: &MintCtx<'_>,
            _identity: &Identity,
            _acquire: &Acquire,
        ) -> anyhow::Result<Credential> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Credential::single(
                format!("token_number_{n}_padded_out"),
                Expiry {
                    at: ctx.now + self.ttl,
                    source: ExpirySource::DeclaredTtl,
                    issued_at: ctx.now,
                },
            ))
        }
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| {
            owned
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    fn descriptor(addr: &str) -> Descriptor {
        Descriptor {
            addr: addr.to_string(),
            identity: Identity::default(),
            acquire: vec![Acquire {
                when_env: None,
                source: Source::StaticEnv {
                    vars: BTreeMap::from([("token".to_string(), "X".to_string())]),
                },
                exchange: Vec::new(),
                ttl: None,
            }],
            allow: None,
        }
    }

    fn broker(ttl: Duration) -> (Broker, Arc<CountingProvider>) {
        let p = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
            ttl,
        });
        let mut reg = ProviderRegistry::default();
        reg.register(p.clone()).expect("register");
        (Broker::new(Arc::new(reg)), p)
    }

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// The point of the broker: every target naming one descriptor shares one
    /// call, so a warm-ish build makes one STS request rather than four hundred.
    #[tokio::test]
    async fn one_descriptor_is_minted_once_per_run() {
        let (b, p) = broker(Duration::from_secs(3600));
        let token = StdCancellationToken::new();
        let env = env_of(&[]);
        let ctx = BrokerCtx {
            now: t(0),
            env: &env,
            ctoken: &token,
            request_id: "req",
            runner: None,
            cwd: std::path::Path::new("."),
            auth_home: None,
        };
        let d = descriptor("//infra/creds:ecr");

        for _ in 0..5 {
            b.mint(&d, "ecr", &ctx).await.expect("mint");
        }
        assert_eq!(p.calls.load(Ordering::SeqCst), 1);
    }

    /// A provider that blocks until every expected caller has arrived, so
    /// single-flight is *observed* rather than inferred.
    struct GateProvider {
        calls: AtomicUsize,
        /// Highest number of callers ever inside `mint` at once.
        peak: AtomicUsize,
        inside: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl SecretProvider for GateProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::StaticEnv
        }
        async fn mint(
            &self,
            ctx: &MintCtx<'_>,
            _identity: &Identity,
            _acquire: &Acquire,
        ) -> anyhow::Result<Credential> {
            let now = self.inside.fetch_add(1, Ordering::SeqCst).saturating_add(1);
            self.peak.fetch_max(now, Ordering::SeqCst);
            // A real await, so the runtime is free to poll another caller into
            // this function if the slot lock does not stop it.
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.inside.fetch_sub(1, Ordering::SeqCst);
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Credential::single(
                format!("token_number_{n}_padded_out"),
                Expiry {
                    at: ctx.now + Duration::from_secs(3600),
                    source: ExpirySource::DeclaredTtl,
                    issued_at: ctx.now,
                },
            ))
        }
    }

    /// The property the type exists for, tested under *actual* concurrency.
    ///
    /// The sequential version of this test passes against a broker with no slot
    /// lock at all — every caller simply finds the cached value left by the one
    /// before it. What has to be shown is that N callers arriving *before the
    /// first mint returns* still produce one provider call, which is the
    /// difference between one STS request per run and four hundred.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_consumers_of_one_descriptor_mint_exactly_once() {
        let p = Arc::new(GateProvider {
            calls: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            inside: AtomicUsize::new(0),
        });
        let mut reg = ProviderRegistry::default();
        reg.register(p.clone()).expect("register");
        let b = Arc::new(Broker::new(Arc::new(reg)));
        let d = Arc::new(descriptor("//infra/creds:ecr"));

        let mut tasks = Vec::new();
        for _ in 0..16 {
            let (b, d) = (Arc::clone(&b), Arc::clone(&d));
            tasks.push(tokio::spawn(async move {
                let token = StdCancellationToken::new();
                let env = env_of(&[]);
                let ctx = BrokerCtx {
                    now: t(0),
                    env: &env,
                    ctoken: &token,
                    request_id: "req",
                    runner: None,
                    cwd: std::path::Path::new("."),
                    auth_home: None,
                };
                b.mint(&d, "ecr", &ctx).await.map(|_| ())
            }));
        }
        for task in tasks {
            task.await.expect("join").expect("mint");
        }

        assert_eq!(p.calls.load(Ordering::SeqCst), 1, "minted more than once");
        assert_eq!(
            p.peak.load(Ordering::SeqCst),
            1,
            "two callers were inside the provider at the same time; the slot lock did not hold"
        );
    }

    /// The other half of the lock claim: the outer map lock is held only long
    /// enough to clone an `Arc`, so two *different* descriptors must overlap
    /// rather than serialize.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn distinct_descriptors_mint_concurrently() {
        let p = Arc::new(GateProvider {
            calls: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            inside: AtomicUsize::new(0),
        });
        let mut reg = ProviderRegistry::default();
        reg.register(p.clone()).expect("register");
        let b = Arc::new(Broker::new(Arc::new(reg)));

        let mut tasks = Vec::new();
        for i in 0..4 {
            let b = Arc::clone(&b);
            tasks.push(tokio::spawn(async move {
                let d = descriptor(&format!("//c:s{i}"));
                let token = StdCancellationToken::new();
                let env = env_of(&[]);
                let ctx = BrokerCtx {
                    now: t(0),
                    env: &env,
                    ctoken: &token,
                    request_id: "req",
                    runner: None,
                    cwd: std::path::Path::new("."),
                    auth_home: None,
                };
                b.mint(&d, "s", &ctx).await.map(|_| ())
            }));
        }
        for task in tasks {
            task.await.expect("join").expect("mint");
        }

        assert_eq!(p.calls.load(Ordering::SeqCst), 4);
        assert!(
            p.peak.load(Ordering::SeqCst) > 1,
            "four independent descriptors serialized against each other"
        );
    }

    /// A failed mint must not be cached: the next caller retries rather than
    /// inheriting a failure, and nothing is recorded for an attempt that
    /// produced no credential.
    #[tokio::test]
    async fn a_failed_mint_is_not_cached_and_records_nothing() {
        struct FlakyProvider {
            calls: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl SecretProvider for FlakyProvider {
            fn kind(&self) -> ProviderKind {
                ProviderKind::StaticEnv
            }
            async fn mint(
                &self,
                ctx: &MintCtx<'_>,
                _identity: &Identity,
                _acquire: &Acquire,
            ) -> anyhow::Result<Credential> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    anyhow::bail!("helper not logged in");
                }
                Ok(Credential::single(
                    "recovered_token_value",
                    Expiry {
                        at: ctx.now + Duration::from_secs(3600),
                        source: ExpirySource::DeclaredTtl,
                        issued_at: ctx.now,
                    },
                ))
            }
        }

        let p = Arc::new(FlakyProvider {
            calls: AtomicUsize::new(0),
        });
        let mut reg = ProviderRegistry::default();
        reg.register(p.clone()).expect("register");
        let b = Broker::new(Arc::new(reg));

        let token = StdCancellationToken::new();
        let env = env_of(&[]);
        let ctx = BrokerCtx {
            now: t(0),
            env: &env,
            ctoken: &token,
            request_id: "req",
            runner: None,
            cwd: std::path::Path::new("."),
            auth_home: None,
        };
        let d = descriptor("//c:x");

        let err = b.mint(&d, "x", &ctx).await.expect_err("first fails");
        assert!(err.to_string().contains("not logged in"), "{err}");
        assert!(
            b.grants().await.is_empty(),
            "a failed mint recorded a grant"
        );
        assert!(
            b.redactor().await.is_inert(),
            "a failed mint registered a value"
        );

        // The failure was not cached, so the retry actually re-enters.
        b.mint(&d, "x", &ctx).await.expect("second succeeds");
        assert_eq!(p.calls.load(Ordering::SeqCst), 2);
        assert_eq!(b.grants().await.len(), 1);
    }

    // ---- preemptive refresh ----

    /// The bug this pins, in the user's words: a target must not start with a
    /// token that is valid for one second.
    ///
    /// `stale_at` alone answers "is this still valid?", and a credential just
    /// inside the skew margin passes that while being useless to anything that
    /// runs for longer than a moment. A handout wants *headroom*, so a
    /// still-valid credential with too little life left is re-minted before it
    /// is given away.
    #[tokio::test]
    async fn a_handout_refreshes_a_still_valid_but_nearly_dead_credential() {
        // 1h credentials, so a refresh genuinely buys more life.
        let (b, p) = broker(Duration::from_secs(3600));
        let token = StdCancellationToken::new();
        let env = env_of(&[]);
        let d = descriptor("//c:x");
        let mk = |now| BrokerCtx {
            now,
            env: &env,
            ctoken: &token,
            request_id: "req",
            runner: None,
            cwd: std::path::Path::new("."),
            auth_home: None,
        };

        b.mint(&d, "x", &mk(t(0))).await.expect("first");
        assert_eq!(p.calls.load(Ordering::SeqCst), 1);

        // Still valid — well inside the 60s skew margin of a 3600s expiry — but
        // with under five minutes of usable life. The old rule reused it.
        let nearly_dead = t(3600 - 60 - 30);
        b.mint(&d, "x", &mk(nearly_dead)).await.expect("handout");
        assert_eq!(
            p.calls.load(Ordering::SeqCst),
            2,
            "a credential with 30s of usable life was handed to a target"
        );
    }

    /// …but not when re-minting cannot help. A credential whose whole lifetime
    /// is shorter than the headroom would otherwise be re-minted on every
    /// single handout, paying a mint per target to buy nothing.
    #[tokio::test]
    async fn a_short_lived_credential_is_not_re_minted_on_every_handout() {
        // 90s credentials: 30s usable after the margin, always under the
        // handout headroom, and a refresh would land in exactly the same place.
        let (b, p) = broker(Duration::from_secs(90));
        let token = StdCancellationToken::new();
        let env = env_of(&[]);
        let d = descriptor("//c:x");
        let mk = |now| BrokerCtx {
            now,
            env: &env,
            ctoken: &token,
            request_id: "req",
            runner: None,
            cwd: std::path::Path::new("."),
            auth_home: None,
        };

        b.mint(&d, "x", &mk(t(0))).await.expect("first");
        for at in [1, 2, 3, 4, 5] {
            b.mint(&d, "x", &mk(t(at))).await.expect("handout");
        }
        assert_eq!(
            p.calls.load(Ordering::SeqCst),
            1,
            "a credential too short-lived to refresh was re-minted anyway"
        );
    }

    /// The background half: a long run keeps its credentials warm without any
    /// consumer paying the mint latency.
    #[tokio::test]
    async fn the_sweep_renews_ahead_of_demand() {
        let (b, p) = broker(Duration::from_secs(3600));
        let token = StdCancellationToken::new();
        let env = env_of(&[]);
        let d = descriptor("//c:x");
        let mk = |now| BrokerCtx {
            now,
            env: &env,
            ctoken: &token,
            request_id: "req",
            runner: None,
            cwd: std::path::Path::new("."),
            auth_home: None,
        };

        b.mint(&d, "x", &mk(t(0))).await.expect("first");

        // Nothing due yet.
        assert_eq!(b.refresh_due(&mk(t(60))).await, 0);
        assert_eq!(p.calls.load(Ordering::SeqCst), 1);

        // Inside the headroom window: the sweep renews it before anyone asks.
        assert_eq!(b.refresh_due(&mk(t(3600 - 60 - 30))).await, 1);
        assert_eq!(p.calls.load(Ordering::SeqCst), 2);

        // And the consumer that arrives next is served without a mint.
        b.mint(&d, "x", &mk(t(3600 - 60 - 29))).await.expect("warm");
        assert_eq!(
            p.calls.load(Ordering::SeqCst),
            2,
            "the consumer paid a mint the sweep had already done"
        );
    }

    /// A background failure must not fail a build: the old value is kept, and
    /// the consumer that actually needs it reports the error with its own
    /// target attached.
    #[tokio::test]
    async fn a_failed_sweep_keeps_the_existing_value() {
        struct FailAfterFirst {
            calls: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl SecretProvider for FailAfterFirst {
            fn kind(&self) -> ProviderKind {
                ProviderKind::StaticEnv
            }
            async fn mint(
                &self,
                ctx: &MintCtx<'_>,
                _identity: &Identity,
                _acquire: &Acquire,
            ) -> anyhow::Result<Credential> {
                if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
                    anyhow::bail!("idp unreachable");
                }
                Ok(Credential::single(
                    "the_first_token_value",
                    Expiry {
                        at: ctx.now + Duration::from_secs(3600),
                        source: ExpirySource::DeclaredTtl,
                        issued_at: ctx.now,
                    },
                ))
            }
        }

        let p = Arc::new(FailAfterFirst {
            calls: AtomicUsize::new(0),
        });
        let mut reg = ProviderRegistry::default();
        reg.register(p.clone()).expect("register");
        let b = Broker::new(Arc::new(reg));
        let token = StdCancellationToken::new();
        let env = env_of(&[]);
        let d = descriptor("//c:x");
        let mk = |now| BrokerCtx {
            now,
            env: &env,
            ctoken: &token,
            request_id: "req",
            runner: None,
            cwd: std::path::Path::new("."),
            auth_home: None,
        };

        b.mint(&d, "x", &mk(t(0))).await.expect("first");
        assert_eq!(b.refresh_due(&mk(t(3600 - 60 - 30))).await, 0, "it failed");
        // The value is still there, and still masked.
        assert_eq!(
            b.redactor().await.redact_str("saw the_first_token_value"),
            "saw «redacted:x»"
        );
    }

    /// The loop itself, not just the sweep it calls: a spawned refresher must
    /// actually renew on its own, and must stop when its guard is dropped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_spawned_refresher_renews_and_then_stops() {
        // Short-lived enough that the very first tick finds work to do.
        let (b, p) = broker(Duration::from_secs(3600));
        let b = Arc::new(b);
        let token = StdCancellationToken::new();
        let env = env_of(&[]);
        let d = descriptor("//c:x");

        b.mint(
            &d,
            "x",
            &BrokerCtx {
                // Minted far enough in the past that it is already inside the
                // headroom window by the time the sweep runs against `now()`.
                now: SystemTime::now() - Duration::from_secs(3600 - 60 - 10),
                env: &env,
                ctoken: &token,
                request_id: "req",
                runner: None,
                cwd: std::path::Path::new("."),
                auth_home: None,
            },
        )
        .await
        .expect("first");
        assert_eq!(p.calls.load(Ordering::SeqCst), 1);

        let guard = b.spawn_refresher_every(
            RefreshConfig {
                auth_home: None,
                env: Arc::new(|_: &str| None),
                ctoken: Arc::new(StdCancellationToken::new()),
                request_id: "req".to_string(),
                cwd: std::path::PathBuf::from("."),
            },
            Duration::from_millis(20),
        );

        // Wait for the sweep to do its work rather than assuming a duration.
        for _ in 0..100 {
            if p.calls.load(Ordering::SeqCst) > 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            p.calls.load(Ordering::SeqCst) > 1,
            "the background refresher never renewed anything"
        );

        // Dropping the guard stops it: the count settles.
        drop(guard);
        tokio::time::sleep(Duration::from_millis(80)).await;
        let settled = p.calls.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(
            p.calls.load(Ordering::SeqCst),
            settled,
            "the refresher kept running after its guard was dropped"
        );
    }

    /// A superseded value stays maskable for a few generations and then stops,
    /// so a long build does not accumulate every dead token it ever held.
    #[tokio::test]
    async fn the_live_value_set_is_bounded_across_re_mints() {
        let (b, _) = broker(Duration::from_secs(120));
        let token = StdCancellationToken::new();
        let env = env_of(&[]);
        let d = descriptor("//c:x");

        let mut first = String::new();
        for generation in 0..12u64 {
            let ctx = BrokerCtx {
                now: t(generation.saturating_mul(120)),
                env: &env,
                ctoken: &token,
                request_id: "req",
                runner: None,
                cwd: std::path::Path::new("."),
                auth_home: None,
            };
            let c = b.mint(&d, "x", &ctx).await.expect("mint");
            if generation == 0 {
                first = c.resolve_pointer("$.").expect("v").expose().to_string();
            }
        }

        let live = b.live.lock().await;
        assert!(
            live.entries.len() <= MAX_LIVE_PER_SECRET,
            "live values grew unbounded: {} held",
            live.entries.len()
        );
        drop(live);
        // The first generation is long superseded and no longer masked.
        assert_eq!(b.redactor().await.redact_str(&first), first);
    }

    /// Two descriptors are two identities and must not share a slot.
    #[tokio::test]
    async fn distinct_descriptors_do_not_share_a_slot() {
        let (b, p) = broker(Duration::from_secs(3600));
        let token = StdCancellationToken::new();
        let env = env_of(&[]);
        let ctx = BrokerCtx {
            now: t(0),
            env: &env,
            ctoken: &token,
            request_id: "req",
            runner: None,
            cwd: std::path::Path::new("."),
            auth_home: None,
        };
        b.mint(&descriptor("//c:a"), "a", &ctx).await.expect("a");
        b.mint(&descriptor("//c:b"), "b", &ctx).await.expect("b");
        assert_eq!(p.calls.load(Ordering::SeqCst), 2);
    }

    /// The half `Memoizer::once` structurally cannot do.
    #[tokio::test]
    async fn an_expired_value_is_re_minted() {
        let (b, p) = broker(Duration::from_secs(600));
        let token = StdCancellationToken::new();
        let env = env_of(&[]);
        let d = descriptor("//c:x");

        let mk = |now| BrokerCtx {
            now,
            env: &env,
            ctoken: &token,
            request_id: "req",
            runner: None,
            cwd: std::path::Path::new("."),
            auth_home: None,
        };

        b.mint(&d, "x", &mk(t(0))).await.expect("first");
        b.mint(&d, "x", &mk(t(100))).await.expect("still fresh");
        assert_eq!(p.calls.load(Ordering::SeqCst), 1);

        // Past the refresh margin before the 600s expiry.
        b.mint(&d, "x", &mk(t(560))).await.expect("re-mint");
        assert_eq!(p.calls.load(Ordering::SeqCst), 2);
    }

    /// A value must be maskable from the moment it exists, not from the moment
    /// it is first delivered — the window between is where a mid-mint
    /// diagnostic would print it.
    #[tokio::test]
    async fn a_minted_value_is_immediately_redactable() {
        let (b, _) = broker(Duration::from_secs(3600));
        let token = StdCancellationToken::new();
        let env = env_of(&[]);
        let ctx = BrokerCtx {
            now: t(0),
            env: &env,
            ctoken: &token,
            request_id: "req",
            runner: None,
            cwd: std::path::Path::new("."),
            auth_home: None,
        };
        assert!(b.redactor().await.is_inert());

        let c = b
            .mint(&descriptor("//c:x"), "gh", &ctx)
            .await
            .expect("mint");
        let value = c.resolve_pointer("$.").expect("v").expose().to_string();

        let masked = b.redactor().await.redact_str(&format!("saw {value} here"));
        assert_eq!(masked, "saw «redacted:gh» here");
    }

    /// The route taken is invisible in a build's output, so it has to be
    /// recorded — and recorded without the value.
    #[tokio::test]
    async fn a_grant_records_the_route_but_never_the_value() {
        let (b, _) = broker(Duration::from_secs(3600));
        let token = StdCancellationToken::new();
        let env = env_of(&[("GITHUB_ACTIONS", "true")]);
        let ctx = BrokerCtx {
            now: t(0),
            env: &env,
            ctoken: &token,
            request_id: "req",
            runner: None,
            cwd: std::path::Path::new("."),
            auth_home: None,
        };
        let mut d = descriptor("//c:x");
        d.acquire.insert(
            0,
            Acquire {
                when_env: Some(WhenEnv::Set("NOT_SET_ANYWHERE".into())),
                ..d.acquire.first().cloned().expect("one entry")
            },
        );
        d.acquire.insert(
            0,
            Acquire {
                when_env: Some(WhenEnv::Set("GITHUB_ACTIONS".into())),
                ..d.acquire.first().cloned().expect("one entry")
            },
        );

        let c = b.mint(&d, "x", &ctx).await.expect("mint");
        let grants = b.grants().await;
        let g = grants.first().expect("one grant");
        assert_eq!(g.addr, "//c:x");
        assert_eq!(g.acquire_index, 0);
        assert_eq!(g.selected_by.as_deref(), Some("GITHUB_ACTIONS is set"));

        let rendered = format!("{grants:?}");
        let value = c.resolve_pointer("$.").expect("v").expose();
        assert!(
            !rendered.contains(value),
            "grant leaked a value: {rendered}"
        );
    }

    /// A protocol is required for exec, and the broker must surface the
    /// descriptor's own validation rather than a provider-level surprise.
    #[tokio::test]
    async fn a_descriptor_with_no_matching_entry_fails_with_the_guard_report() {
        let (b, _) = broker(Duration::from_secs(3600));
        let token = StdCancellationToken::new();
        let env = env_of(&[]);
        let ctx = BrokerCtx {
            now: t(0),
            env: &env,
            ctoken: &token,
            request_id: "req",
            runner: None,
            cwd: std::path::Path::new("."),
            auth_home: None,
        };
        let mut d = descriptor("//c:x");
        d.acquire = vec![Acquire {
            when_env: Some(WhenEnv::Set("NEVER_SET".into())),
            ..d.acquire.first().cloned().expect("one")
        }];

        let err = b.mint(&d, "x", &ctx).await.expect_err("no route");
        assert!(err.to_string().contains("NEVER_SET is unset"), "{err}");
    }
}
