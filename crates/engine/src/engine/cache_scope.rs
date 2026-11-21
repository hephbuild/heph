//! The engine's side of provider cache scopes.
//!
//! A root request mints a [`CacheScope`]; every state derived from it or
//! nested inside it shares that scope (`RequestStateData::cache_scope`), and
//! every provider request the engine builds carries it (`with_cache_scope`).
//! When the last holder drops, the scope ends and the engine tells every
//! provider through `Provider::scope_ended`, so a provider can drop what it
//! cached for that scope (see `hplugin::scope`).
//!
//! The end is delivered off the dropping thread: the scope's end hook runs
//! synchronously inside a `Drop` — of a request state, on whatever thread let
//! go of it last — so it only counts the job on the request's `bg_pending` and
//! spawns it on the engine's runtime. The process-exit drain waits on that
//! counter, so a hook in flight is not cut off by exit; each provider's call is
//! bounded by `SCOPE_ENDED_TIMEOUT`, so a hung one cannot hold exit either.
//!
//! Nothing about a scope reaches a hash, a target's env or its argv.

use crate::engine::Engine;
use crate::engine::engine::Provider;
use crate::engine::sandbox_cleaner::PendingCounter;
use futures::FutureExt;
use hplugin::scope::{CacheScope, ScopeEnded, ScopeEndedAck};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;

impl Engine {
    /// A new cache scope for a root request whose background work is counted
    /// on `bg_pending`.
    ///
    /// Captures the engine weakly (a request state must not keep its engine
    /// alive) and the counter strongly (the drain must see the hook).
    pub(crate) fn mint_cache_scope(self: &Arc<Self>, bg_pending: &PendingCounter) -> CacheScope {
        let engine = Arc::downgrade(self);
        let bg_pending = Arc::clone(bg_pending);
        let scope = CacheScope::mint(move |ended, touched| {
            end_scope(&engine, &bg_pending, ended, touched);
        });
        tracing::debug!(scope = scope.id().get(), "cache scope started");
        scope
    }
}

/// A scope's last holder dropped. Runs synchronously inside that `Drop`, so it
/// never calls a provider itself: it spawns the calls and returns.
fn end_scope(engine: &Weak<Engine>, bg_pending: &PendingCounter, ended: ScopeEnded, touched: bool) {
    let scope = ended.scope.get();
    // No request was ever built with the scope: no provider can hold anything
    // for it. Its id already left the live set, so the watermark the next
    // touched scope sends covers it.
    if !touched {
        tracing::trace!(scope, "cache scope ended untouched; no provider told");
        return;
    }
    let Some(engine) = engine.upgrade() else {
        tracing::debug!(
            scope,
            "cache scope ended after its engine; no provider told"
        );
        return;
    };
    // The provider list, not the engine: the spawned task must not keep the
    // engine alive.
    let providers = engine.providers.clone();
    let runtime = engine.runtime.clone();
    drop(engine);
    if providers.is_empty() {
        return;
    }
    let slot = PendingSlot::take(bg_pending);
    // A runtime that is already shut down drops the task unpolled; the slot
    // drops with it, so the counter cannot stay up.
    drop(runtime.spawn(async move {
        let _slot = slot;
        send_scope_ended(&providers, ended).await;
    }));
}

/// How long one provider's `scope_ended` may take before the hook gives up on
/// it. The hook is counted on the request's background counter, which process
/// exit drains, so a provider whose `scope_ended` never returns would otherwise
/// hold `heph query` (and every other command) open at exit.
const SCOPE_ENDED_TIMEOUT: Duration = Duration::from_secs(5);

/// Tell every provider that `ended` ended, concurrently. One provider erroring,
/// panicking or overrunning [`SCOPE_ENDED_TIMEOUT`] is logged and does not stop
/// the others.
async fn send_scope_ended(providers: &[Arc<Provider>], ended: ScopeEnded) {
    let scope = ended.scope.get();
    futures::future::join_all(providers.iter().map(|provider| async move {
        // The call itself inside the guarded future, so a provider that
        // panics before returning its future is caught too.
        let outcome = tokio::time::timeout(
            SCOPE_ENDED_TIMEOUT,
            AssertUnwindSafe(async { provider.provider.scope_ended(ended).await }).catch_unwind(),
        )
        .await;
        let name = provider.name.as_str();
        let Ok(outcome) = outcome else {
            tracing::warn!(
                scope,
                provider = name,
                timeout_secs = SCOPE_ENDED_TIMEOUT.as_secs(),
                "provider {name} did not finish ending cache scope {scope} in time; gave up \
                 waiting, it may keep that scope's caches"
            );
            return;
        };
        match outcome {
            Ok(Ok(ScopeEndedAck::Unsupported)) => {
                tracing::debug!(
                    scope,
                    provider = name,
                    ack = "unsupported",
                    "cache scope ended"
                );
                if !provider
                    .scope_ended_unsupported_reported
                    .swap(true, Ordering::Relaxed)
                {
                    tracing::debug!(
                        provider = name,
                        "provider {name} does not implement scope_ended; caches it keeps are \
                         never told to drop"
                    );
                }
            }
            Ok(Ok(_)) => {
                tracing::debug!(
                    scope,
                    provider = name,
                    ack = "released",
                    "cache scope ended"
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    scope,
                    provider = name,
                    error = %format!("{e:#}"),
                    "provider failed to end a cache scope; it may keep that scope's caches"
                );
            }
            Err(_) => {
                tracing::warn!(
                    scope,
                    provider = name,
                    "provider panicked ending a cache scope; it may keep that scope's caches"
                );
            }
        }
    }))
    .await;
}

/// One job counted on a request's background counter, retired on drop — when
/// the hook finishes, or when its task is dropped unpolled.
struct PendingSlot(PendingCounter);

impl PendingSlot {
    fn take(pending: &PendingCounter) -> Self {
        pending.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(pending))
    }
}

impl Drop for PendingSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::discovery::Discovery;
    use crate::engine::fault_provider::{FaultProvider, Faults, ScopeLog};
    use crate::engine::request_state::RequestState;
    use futures::StreamExt;
    use hbuiltins::pluginstatictarget;
    use hmodel::htaddr::parse_addr;
    use hmodel::htmatcher::Matcher;
    use hmodel::htpkg::PkgBuf;
    use hplugin::provider::Provider as _;
    use hplugin::scope::ScopeId;
    use std::time::Duration;

    fn target(addr: &str) -> pluginstatictarget::Target {
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some("true".to_string()),
            ..Default::default()
        }
    }

    /// An engine with one fault provider per entry of `faults` (each needs its
    /// own `name`), and their scope logs in the same order.
    fn engine_with(
        faults: Vec<Faults>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir, Vec<Arc<ScopeLog>>)> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            parallelism: None,
            ..Config::for_tests(root.path())
        })?;
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        let mut logs = Vec::new();
        for faults in faults {
            let provider = FaultProvider::new(vec![target("//live:t")], faults)?;
            logs.push(provider.scopes());
            engine.register_provider(move |_| Box::new(provider))?;
        }
        Ok((Arc::new(engine), root, logs))
    }

    /// One healthy fault provider.
    fn engine() -> anyhow::Result<(Arc<Engine>, tempfile::TempDir, Arc<ScopeLog>)> {
        let (engine, root, mut logs) = engine_with(vec![Faults::default()])?;
        let log = logs.pop().expect("one provider");
        Ok((engine, root, log))
    }

    fn named(name: &'static str, faults: Faults) -> Faults {
        Faults {
            name: Some(name),
            ..faults
        }
    }

    /// Walk the whole workspace under `rs`, so every provider is called in its
    /// scope. Failures are the caller's business.
    async fn touch(engine: &Arc<Engine>, rs: &Arc<RequestState>) {
        let all = Matcher::PackagePrefix(PkgBuf::from(""));
        let _: Vec<_> = Arc::clone(engine)
            .query(
                rs.clone(),
                &all,
                Discovery::KeepGoing(crate::engine::Gaps::new("//...")),
            )
            .collect()
            .await;
    }

    /// Poll `cond` until it holds, failing after 30s.
    async fn eventually(what: &str, cond: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !cond() {
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// `scope` reached `log` exactly once, and stays that way a moment later.
    async fn ended_once(log: &ScopeLog, scope: ScopeId) {
        eventually("scope_ended reached the provider", || {
            log.ended_count(scope) >= 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(log.ended_count(scope), 1, "{:?}", log.ended());
    }

    /// I3: every state derived from a root, or nested inside it, runs in the
    /// root's scope — and never under its request id.
    #[tokio::test]
    async fn derived_states_share_the_root_scope() -> anyhow::Result<()> {
        let (engine, _root, _log) = engine()?;
        let root = engine.new_state();
        let id = root.cache_scope().id();
        let a = parse_addr("//live:t")?;

        let derived: Vec<(&str, Arc<RequestState>)> = vec![
            ("with_parent", root.with_parent(a.clone())),
            ("with_skip_provider", root.with_skip_provider("faulty")),
            ("speculative", root.speculative()),
            (
                "hash_only",
                engine.new_hash_only_state(&root, a.clone(), root.listed_facts_trust()),
            ),
            ("nested", engine.new_nested_state(&root)),
        ];
        for (how, rs) in &derived {
            assert_eq!(rs.cache_scope().id(), id, "{how} left the root's scope");
        }
        for (how, rs) in &derived[3..] {
            assert_ne!(
                rs.request_id(),
                root.request_id(),
                "{how} shares the root's request id; fs would answer it from the root's snapshot"
            );
        }
        assert_ne!(
            engine.new_state().cache_scope().id(),
            id,
            "a root request mints a scope of its own"
        );
        Ok(())
    }

    /// D12: gc's phase 1, clean's selection walk and failure-gc's resolution
    /// each run under a state of their own, and every provider call they make
    /// is in the caller's scope.
    #[tokio::test]
    async fn gc_reads_one_snapshot() -> anyhow::Result<()> {
        for which in ["gc", "clean", "failure_gc"] {
            let (engine, _root, log) = engine()?;
            let live = parse_addr("//live:t")?;
            let rs = engine.new_state();
            let scope = rs.cache_scope().id();
            match which {
                "gc" => {
                    crate::engine::cache_test_support::write_revision(
                        &engine,
                        &live,
                        "h1",
                        100,
                        &["o.tar"],
                    );
                    Arc::clone(&engine).gc_all(rs.clone()).await?;
                }
                "clean" => {
                    Arc::clone(&engine)
                        .clean(
                            rs.clone(),
                            &Matcher::Label("x".to_string()),
                            Discovery::Complete,
                        )
                        .await?;
                }
                _ => {
                    let now = std::time::SystemTime::now();
                    let ms = now.duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64;
                    crate::engine::failure_slot::write_test_slot(
                        engine.failure_slots().root(),
                        &live,
                        ms,
                        b"boom",
                    );
                    Arc::clone(&engine)
                        .gc_failure_slots_at(&rs, Duration::from_secs(3600), now)
                        .await;
                }
            }
            let seen = log.seen();
            assert!(!seen.is_empty(), "{which} called no provider");
            assert!(
                seen.iter().all(|s| *s == Some(scope)),
                "{which} called a provider outside the caller's scope {scope}: {seen:?}"
            );
        }
        Ok(())
    }

    /// I4/D11: the hook fires once, after the last holder — not when the root
    /// drops while a derived or nested state still holds the scope.
    #[tokio::test]
    async fn scope_ended_fires_once_after_the_last_state() -> anyhow::Result<()> {
        let (engine, _root, log) = engine()?;
        let root = engine.new_state();
        let scope = root.cache_scope().id();
        let bg = root.bg_pending();
        touch(&engine, &root).await;
        let child = root.with_parent(parse_addr("//live:t")?);
        let nested =
            engine.new_hash_only_state(&root, parse_addr("//live:t")?, root.listed_facts_trust());

        drop(root);
        bg.wait_drained().await;
        assert_eq!(log.ended_count(scope), 0, "ended while a child held it");
        drop(child);
        bg.wait_drained().await;
        assert_eq!(
            log.ended_count(scope),
            0,
            "ended while a nested state held it"
        );
        drop(nested);
        ended_once(&log, scope).await;
        Ok(())
    }

    /// D11 and the exemptions: however a request ends, each provider is told
    /// once; when the engine is gone, nobody is told and nothing hangs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn scope_ended_fires_once_in_every_ending() -> anyhow::Result<()> {
        // Race: the last holders drop on many threads at once.
        {
            let (engine, _root, log) = engine()?;
            let root = engine.new_state();
            let scope = root.cache_scope().id();
            touch(&engine, &root).await;
            let barrier = Arc::new(std::sync::Barrier::new(16));
            let threads: Vec<_> = (0..16)
                .map(|i| {
                    let state = if i % 2 == 0 {
                        root.with_parent(parse_addr("//live:t").expect("addr"))
                    } else {
                        root.speculative()
                    };
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        drop(state);
                    })
                })
                .collect();
            drop(root);
            for t in threads {
                t.join().expect("dropper");
            }
            ended_once(&log, scope).await;
        }

        // Cancel: the request is cancelled mid-flight.
        {
            let (engine, _root, log) = engine()?;
            let root = engine.new_state();
            let scope = root.cache_scope().id();
            touch(&engine, &root).await;
            root.ctoken().cancel();
            touch(&engine, &root).await;
            drop(root);
            ended_once(&log, scope).await;
        }

        // Failure: the walk itself fails.
        {
            let (engine, _root, mut logs) = engine_with(vec![Faults {
                fail_list_packages: true,
                ..Default::default()
            }])?;
            let log = logs.pop().expect("one provider");
            let root = engine.new_state();
            let scope = root.cache_scope().id();
            touch(&engine, &root).await;
            drop(root);
            ended_once(&log, scope).await;
        }

        // Panic: a provider call panics inside the request.
        {
            let (engine, _root, mut logs) = engine_with(vec![Faults {
                panic_list: vec!["live".to_string()],
                ..Default::default()
            }])?;
            let log = logs.pop().expect("one provider");
            let root = engine.new_state();
            let scope = root.cache_scope().id();
            touch(&engine, &root).await;
            drop(root);
            ended_once(&log, scope).await;
        }

        // One provider erroring and one panicking in `scope_ended` stop
        // neither the healthy one nor each other.
        {
            let (engine, _root, logs) = engine_with(vec![
                named(
                    "erroring",
                    Faults {
                        fail_scope_ended: true,
                        ..Default::default()
                    },
                ),
                named(
                    "panicking",
                    Faults {
                        panic_scope_ended: true,
                        ..Default::default()
                    },
                ),
                named("healthy", Faults::default()),
            ])?;
            let root = engine.new_state();
            let scope = root.cache_scope().id();
            let bg = root.bg_pending();
            touch(&engine, &root).await;
            drop(root);
            for log in &logs {
                ended_once(log, scope).await;
            }
            bg.wait_drained().await;
        }

        // A provider with the trait's default (`Unsupported`): the hook still
        // completes, and the engine reports it once, not once per scope.
        {
            let root_dir = tempfile::tempdir()?;
            let mut engine = Engine::new(Config {
                parallelism: None,
                ..Config::for_tests(root_dir.path())
            })?;
            let fault = FaultProvider::new(vec![target("//live:t")], Faults::default())?;
            let log = fault.scopes();
            engine.register_provider(move |_| Box::new(fault))?;
            let plain = pluginstatictarget::Provider::new(vec![target("//other:t")])?;
            engine.register_provider(move |_| Box::new(plain))?;
            let engine = Arc::new(engine);
            let reported = |name: &str| {
                engine.providers_by_name[name]
                    .scope_ended_unsupported_reported
                    .load(Ordering::Relaxed)
            };
            for _ in 0..2 {
                let root = engine.new_state();
                let scope = root.cache_scope().id();
                let bg = root.bg_pending();
                touch(&engine, &root).await;
                drop(root);
                ended_once(&log, scope).await;
                bg.wait_drained().await;
                assert!(
                    reported("pluginstatictarget"),
                    "default provider not reported"
                );
            }
        }

        // Engine gone: nobody to tell, and the drop neither panics nor leaves
        // the background counter up.
        {
            let (engine, _root, log) = engine()?;
            let root = engine.new_state();
            touch(&engine, &root).await;
            let bg = root.bg_pending();
            eventually("the walk released the engine", || {
                Arc::strong_count(&engine) == 1
            })
            .await;
            drop(engine);
            drop(root);
            assert_eq!(bg.load(Ordering::Acquire), 0, "a hook was counted");
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(log.ended().is_empty(), "{:?}", log.ended());
        }
        Ok(())
    }

    /// D19: a scope no provider request was built with ends without a hook.
    #[tokio::test]
    async fn untouched_scope_sends_no_hook() -> anyhow::Result<()> {
        let (engine, _root, log) = engine()?;
        let rs = engine.new_state();
        let bg = rs.bg_pending();
        drop(rs.with_skip_provider("faulty"));
        drop(rs);
        assert_eq!(
            bg.load(Ordering::Acquire),
            0,
            "an untouched scope counted a hook"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(log.ended().is_empty(), "{:?}", log.ended());
        Ok(())
    }

    /// D24: an untouched scope sends no hook, but its id still leaves the live
    /// set, so the next touched scope's watermark tells providers it ended.
    #[tokio::test]
    async fn watermark_advances_past_untouched_scopes() -> anyhow::Result<()> {
        let (engine, _root, log) = engine()?;
        let untouched = engine.new_state();
        let old = untouched.cache_scope().id().get();
        drop(untouched);
        // Other tests in this process may hold older scopes for a while; the
        // watermark passes `old` once they end, unless `old` itself leaked.
        eventually("the watermark passed the untouched scope", || {
            CacheScope::ended_below() > old
        })
        .await;

        let rs = engine.new_state();
        let scope = rs.cache_scope().id();
        touch(&engine, &rs).await;
        drop(rs);
        ended_once(&log, scope).await;
        let ended = log.ended();
        assert!(
            ended[0].ended_below > old,
            "providers were not told the untouched scope {old} ended: {ended:?}"
        );
        Ok(())
    }

    /// D25: a scope is registered live in the same critical section that
    /// mints it, so no watermark computed while its request is alive can pass
    /// it — under concurrent minting too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn minted_scope_is_live_before_its_id_is_visible() -> anyhow::Result<()> {
        let (engine, _root, _log) = engine()?;
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let engine = Arc::clone(&engine);
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        let rs = engine.new_state();
                        let id = rs.cache_scope().id().get();
                        let below = CacheScope::ended_below();
                        assert!(below <= id, "watermark {below} passed live scope {id}");
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("minter");
        }
        Ok(())
    }

    /// D11: the hook never runs inside the `Drop` that ended the scope — the
    /// dropping thread returns while the provider's `scope_ended` is still
    /// held, and the call runs on another thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scope_hook_runs_off_the_dropping_thread() -> anyhow::Result<()> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (engine, _root, mut logs) = engine_with(vec![Faults {
            scope_ended_gate: Some(Arc::clone(&gate)),
            ..Default::default()
        }])?;
        let log = logs.pop().expect("one provider");
        let rs = engine.new_state();
        let scope = rs.cache_scope().id();
        let bg = rs.bg_pending();
        touch(&engine, &rs).await;

        let dropper = std::thread::spawn(move || {
            drop(rs);
            std::thread::current().id()
        });
        let dropper_id = tokio::task::spawn_blocking(move || dropper.join())
            .await?
            .expect("dropper");
        eventually("the hook started", || log.ended_count(scope) == 1).await;
        assert_ne!(log.ended_threads()[0], dropper_id, "the hook ran inline");
        gate.add_permits(1);
        bg.wait_drained().await;
        Ok(())
    }

    /// D11: a hook in flight is counted on the request's background counter,
    /// which process exit drains — exit waits for it rather than cutting it
    /// off.
    #[tokio::test]
    async fn engine_shutdown_drains_scope_hooks() -> anyhow::Result<()> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (engine, _root, mut logs) = engine_with(vec![Faults {
            scope_ended_gate: Some(Arc::clone(&gate)),
            ..Default::default()
        }])?;
        let log = logs.pop().expect("one provider");
        let rs = engine.new_state();
        let scope = rs.cache_scope().id();
        let bg = rs.bg_pending();
        touch(&engine, &rs).await;
        drop(rs);

        assert!(bg.load(Ordering::Acquire) >= 1, "the hook is not counted");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), bg.wait_drained())
                .await
                .is_err(),
            "the drain finished while the hook was still held"
        );
        gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(30), bg.wait_drained())
            .await
            .expect("the drain finishes once the hook does");
        assert_eq!(log.ended_count(scope), 1);
        Ok(())
    }

    /// A provider whose `scope_ended` never returns cannot hold the drain: the
    /// hook gives up on it after `SCOPE_ENDED_TIMEOUT` and retires its slot on
    /// the background counter, so process exit (`heph query` among them) is
    /// not held open. The clock is paused once the walk is done, so the
    /// timeout elapses in virtual time, deterministically.
    #[tokio::test]
    async fn hung_scope_hook_times_out_and_retires_its_slot() -> anyhow::Result<()> {
        // Never given a permit: the provider's `scope_ended` hangs for good.
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (engine, _root, mut logs) = engine_with(vec![Faults {
            scope_ended_gate: Some(Arc::clone(&gate)),
            ..Default::default()
        }])?;
        let log = logs.pop().expect("one provider");
        let rs = engine.new_state();
        let scope = rs.cache_scope().id();
        let bg = rs.bg_pending();
        touch(&engine, &rs).await;

        tokio::time::pause();
        let started = tokio::time::Instant::now();
        drop(rs);
        assert!(bg.load(Ordering::Acquire) >= 1, "the hook is not counted");
        tokio::time::timeout(SCOPE_ENDED_TIMEOUT * 4, bg.wait_drained())
            .await
            .expect("a hung scope_ended must not hold the drain past its timeout");
        assert!(
            started.elapsed() >= SCOPE_ENDED_TIMEOUT,
            "the drain finished before the hook timed out: {:?}",
            started.elapsed()
        );
        assert_eq!(log.ended_count(scope), 1, "the provider was called");
        Ok(())
    }

    /// D17: a delegating provider forwards `scope_ended` to what it wraps and
    /// answers with its ack — `FaultProvider` over the static provider, whose
    /// default is `Unsupported` — and records the call.
    #[tokio::test]
    async fn delegating_providers_forward_scope_ended() -> anyhow::Result<()> {
        let provider = FaultProvider::new(vec![target("//live:t")], Faults::default())?;
        let log = provider.scopes();
        let scope = ScopeId::new(7).expect("non-zero");
        let ack = provider.scope_ended(ScopeEnded::new(scope, 3)).await?;
        assert_eq!(
            ack,
            ScopeEndedAck::Unsupported,
            "the wrapped ack was not forwarded"
        );
        assert_eq!(log.ended(), vec![ScopeEnded::new(scope, 3)]);
        Ok(())
    }
}
