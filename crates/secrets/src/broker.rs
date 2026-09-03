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
use crate::provider::{EnvLookup, MintCtx, ProviderRegistry};
use crate::redact::{Entry, Redactor};
use crate::value::{Credential, SecretValue};
use hcore::hasync::Cancellable;
use hmodel::htaddr::Addr;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;
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

/// Per-run credential broker.
pub struct Broker {
    registry: Arc<ProviderRegistry>,
    /// Descriptor address → its single in-flight-or-cached slot. The outer lock
    /// is held only long enough to clone an `Arc`; the mint itself happens
    /// under the *slot's* lock, so two different descriptors never serialize
    /// against each other.
    slots: Mutex<BTreeMap<String, Arc<Mutex<Option<Credential>>>>>,
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

        if let Some(existing) = guard.as_ref()
            && !existing.expiry.stale_at(ctx.now)
        {
            return Ok(existing.clone());
        }

        let selection = desc.select(ctx.env)?;
        let provider = self.registry.get(selection.entry.provider)?;

        let redactor = self.redactor().await;
        let mint_ctx = MintCtx {
            addr: &desc.addr,
            now: ctx.now,
            env: ctx.env,
            ctoken: ctx.ctoken,
            request_id: ctx.request_id,
            runner: ctx.runner,
            cwd: ctx.cwd,
            redactor: &redactor,
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
            provider: selection.entry.provider,
            expires_at: cred.expiry.at,
            expiry_source: cred.expiry.source,
        });

        tracing::debug!(
            secret = %desc.addr,
            provider = ?selection.entry.provider,
            acquire_index = selection.index,
            expiry_source = cred.expiry.source.as_str(),
            "minted a credential"
        );

        *guard = Some(cred.clone());
        Ok(cred)
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
    use crate::descriptor::{Acquire, Identity, Protocol, ProviderKind, WhenEnv};
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
                provider: ProviderKind::StaticEnv,
                var: Some("X".into()),
                vars: BTreeMap::new(),
                helper: Vec::new(),
                protocol: None,
                runner: None,
                exchange: None,
                timeout: None,
                ttl: None,
            }],
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
        };
        let mut d = descriptor("//c:x");
        d.acquire = vec![Acquire {
            when_env: Some(WhenEnv::Set("NEVER_SET".into())),
            protocol: Some(Protocol::Raw),
            ..d.acquire.first().cloned().expect("one")
        }];

        let err = b.mint(&d, "x", &ctx).await.expect_err("no route");
        assert!(err.to_string().contains("NEVER_SET is unset"), "{err}");
    }
}
