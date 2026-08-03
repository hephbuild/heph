use crate::engine::Engine;
use crate::engine::error::{CancelledError, CycleError, TargetNotFoundError};
use crate::engine::provider::ListRequest;
use crate::engine::request_state::RequestState;
use enclose::enclose;
use futures::{Stream, StreamExt};
use hcore::hmemoizer::downcast_chain_ref;
use hmodel::htaddr::Addr;
use hmodel::htmatcher;
use hmodel::htmatcher::MatchResult;
use hmodel::htpkg::PkgBuf;
use rustc_hash::FxHashSet;
use std::sync::Arc;

impl Engine {
    pub fn query<'a>(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        m: &'a htmatcher::Matcher,
    ) -> impl Stream<Item = anyhow::Result<Addr>> + 'a {
        // A whole-graph selector (`//...` — a `PackagePrefix` rooted at the empty
        // package) enumerates every target, so its final match count is the total
        // graph size. Recorded for telemetry only when the stream is driven to
        // completion: an early-dropped or errored stream never saw the full graph.
        // Centralized here so every whole-graph caller (query, unscoped validate)
        // is covered without per-command code.
        let whole_graph = matches!(m, htmatcher::Matcher::PackagePrefix(p) if p.is_empty());
        async_stream::try_stream! {
            // Multiple providers can surface the same addr (or the same package
            // from `packages()`), so dedup before yielding.
            let mut seen: FxHashSet<Addr> = FxHashSet::default();
            // Callback surface handed to each `list` so a provider can gather
            // config beyond the package ancestry (e.g. the go module variant
            // universe via `states_under`). `for_list`, so a reentrant
            // `executor.query()` called from inside a `list()` is caught rather
            // than silently nested — see its doc comment in `result.rs`.
            let executor: Arc<dyn hplugin::provider::ProviderExecutor> = Arc::new(
                crate::engine::result::EngineProviderExecutor::for_list(Arc::downgrade(&self), rs.clone()),
            );
            let pkgs: Vec<String> = self.packages(m, &rs).await?.collect::<anyhow::Result<_>>()?;

            // Set when the walk is abandoning. `Buffered` refills its queue from
            // the underlying iterator on every poll, so a drain still *visits*
            // all N packages — what this flag removes is the cost of each: a
            // package that has not started falls straight through instead of
            // paying a whole-package Starlark evaluation for a walk whose answer
            // nobody wants. O(N) cheap poll-throughs, not O(N) evaluations.
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

            // Discovery splits in two, and the split is not cosmetic.
            //
            // **Enumeration** — `probe` + `list` per package — is overlapped, `K
            // ≈ 2 * cores` packages in flight. `list` is a whole-package
            // Starlark evaluation for the buildfile provider, the single
            // heaviest synchronous unit in a build, and it was the producer for
            // the entire pipeline running strictly one package at a time.
            //
            // **Matcher evaluation** stays strictly serial, below, and this is
            // load-bearing rather than incidental. The `MatchShrug` arm
            // resolves a candidate's spec/def on a *speculative*
            // `RequestState`, and a speculative chain detects cycles by walking
            // its own breadcrumb list rather than the shared `DepDag`
            // (`RequestState::speculative`). That is only sound while one such
            // chain exists at a time. Run two concurrently and they are
            // mutually invisible, with two consequences, both of which reach a
            // build definition:
            //
            //   1. `mem_spec`/`mem_def` are shared across chains and keyed by
            //      addr alone, and the memoized closure captures whichever
            //      chain created the cell. `get_spec_inner` skips a provider
            //      whose `get` cycles and falls through to the next one, so a
            //      chain-dependent cycle means the *winning* chain decides
            //      which provider resolves the addr — and `hashin` folds
            //      `def.driver`. Whoever wins the race would pick the cache key.
            //   2. Two chains that resolve each other close a cycle neither can
            //      see: the `DepDag` is bypassed, the breadcrumbs are
            //      per-chain, and the memoizer's own cycle detection is off
            //      unless `HEPH_DEBUG_MEMOIZER_CYCLE=1`. That is a hang where
            //      the serial code reported an error.
            //
            // What guarantees one chain at a time is *structural*, not a claim
            // about which matchers reach the arm: the arm lives in the consumer
            // of the fan-out, inside this one linear generator body, so while it
            // awaits `get_spec`/`get_def` the stream below is not polled at all.
            // (Do not weaken this to "the shrug arm is rare". It is not —
            // `Matcher::Label` shrugs from `matches_addr` unconditionally, and
            // `Matcher::TreeOutputTo` shrugs at both the addr and the spec
            // level, so `heph validate`, `heph tool gen-gitignore` and
            // `heph query 'tree_output()'` drive `get_spec` *and* `get_def` for
            // every target in the workspace through it.)
            //
            // The guarantee is per **walk**, not per engine: `heph validate`
            // runs three `Engine::query` walks concurrently on one
            // `RequestState` (`src/commands/validate.rs`), two of them with
            // `TreeOutputTo`, so speculative chains from different walks can
            // still overlap. That exposure predates this change — the loops here
            // neither create nor close it — but it is the reason widening the
            // arm needs the speculative cycle check to become shared state
            // first, which is a separate change.
            //
            // `buffered`, never `buffer_unordered`: the emission order here is a
            // build input. It carries through `pluginquery`'s `deps` into
            // `plugingroup`, which folds `deps` in order into its def hash, so
            // under `buffer_unordered` the def hash of a query group target
            // would become a function of which BUILD file the OS scheduler
            // finished first. `buffered` yields in submission order, so the
            // sequence — and every candidate's position in it — is exactly the
            // one the serial loop produced.
            let per_pkg = futures::stream::iter(pkgs.into_iter()
                // Ends the source once the walk is abandoning, rather than
                // letting `Buffered` keep refilling from it. `Buffered` pulls a
                // replacement on every poll, and each pull *spawns*, so a plain
                // drain over a 20k-package workspace would spawn 20k tasks just
                // to have each one bail. Ending the iterator makes the drain
                // await only the <=K handles already spawned.
                .take_while(enclose!((stop) move |_| !stop.load(std::sync::atomic::Ordering::Relaxed)))
                .map(|pkg_str| {
                let pkg = PkgBuf::from(pkg_str);

                // No package-scope prune here: `packages()` above already
                // returns only packages `m` can match, whatever the provider
                // did with `ListPackagesRequest::prefix`. That matters because
                // `list` is a whole-package Starlark evaluation for the
                // buildfile provider — a scoped selector (`//foo/...`,
                // `label(l) && //foo/...`, a bare `//foo:bar`) must not pay to
                // evaluate every BUILD file in the repo only to throw the
                // results away at the addr check below.
                //
                // Spawned here rather than through a later `.map()`: handing the
                // async block to a generic fn through a combinator makes its
                // captured lifetimes late-bound and trips "implementation of
                // `FnOnce` is not general enough".
                hcore::hmemoizer::spawn_with_cycle_ctx(enclose!((self => engine, rs, executor, stop) async move {
                    // An abandoned walk must not start evaluating packages it has
                    // not reached yet — that is what keeps the drains below (and
                    // in `Engine::result`) to a cheap poll per remaining package
                    // instead of a package evaluation per remaining package.
                    //
                    // `Err`, never `Ok(vec![])`: this sequence becomes
                    // `pluginquery`'s `deps` and is folded in order into a def
                    // hash, so returning "no candidates here" would silently
                    // hash a *truncated* graph whose length depends on when the
                    // cancel landed. A short answer is not an answer.
                    if rs.ctoken().is_cancelled()
                        || stop.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        return Err(anyhow::Error::new(CancelledError));
                    }
                    let mut candidates: Vec<Addr> = Vec::new();
                    let states = Arc::clone(&engine).probe_segments(&rs, &pkg).await?;

                    for provider in &engine.providers {
                        let it = provider.provider.list(ListRequest {
                            request_id: rs.request_id().to_string(),
                            package: pkg.clone(),
                            states: states
                                .iter()
                                .filter(|s| s.provider == provider.name)
                                .cloned()
                                .collect(),
                            executor: Arc::clone(&executor),
                        }, rs.ctoken()).await?;
                        // The iterator is not `Send`; drain it before the next await.
                        let raw: Vec<_> = it.collect::<anyhow::Result<Vec<_>>>()?;

                        for item in raw {
                            if item.addr.package == pkg {
                                candidates.push(item.addr);
                            }
                        }
                    }

                    anyhow::Ok(candidates)
                }))
            }))
            // Each package runs as its own task, not merely as a future inside
            // `Buffered`. That is what makes the serial consumer below safe.
            //
            // Both `query` walks must be polled inside a tokio runtime because of
            // it. Every caller today is (the commands, `Engine::result`'s walk
            // task, `pluginquery::get`, and the stabby host's `DynFuture`, which
            // host workers poll) — but the surrounding code is otherwise
            // runtime-agnostic, and `HostExecutor::note_dep` already drives an
            // engine future with `block_on` across the synchronous seam, so state
            // the precondition rather than leave it to be rediscovered.
            //
            // `pluginbuildfile::probe`/`list` reach `run_pkg`, which takes a
            // `PKG_EVAL_SLOTS` permit (a global semaphore sized `cores`). When
            // this walk was written the permit was held across `run_pkg`'s
            // `blocking::run(..).await`, and as plain futures the holders
            // advanced only when the consumer polled this stream — while the
            // consumer stops polling it the moment it awaits
            // `get_spec`/`get_def` in the `MatchShrug` arm, which itself can
            // need a permit for another package. `Semaphore` is FIFO: the
            // consumer queued behind `cores` futures that could not advance.
            // Deadlock. `run_pkg` has since moved the permit *into* the
            // blocking job (released on the pool thread, no poll required —
            // see its comment), which makes that specific wedge impossible on
            // its own. Spawning stays as the structural half of the fix:
            // `run_pkg` is reachable from arbitrary provider code, and this
            // walk cannot know what those bodies acquire and hold across an
            // await — spawned, they are polled by the runtime regardless of
            // what the consumer is doing, so no such resource can wedge the
            // walk again. It also keeps packages progressing while the
            // consumer parks in the `MatchShrug` arm.
            // `discovery_fanout_does_not_starve_the_matcher_consumer` models
            // the original shape.
            //
            // `spawn_with_cycle_ctx`, not bare `tokio::spawn`, for the reason
            // `Engine::result` uses it one level up: the body calls
            // `Memoizer::once`, and without the inherited frame those calls get
            // no wait-for edge, so a cycle through them hangs instead of
            // reporting `MemoizerCycleError`.
            //
            // `buffered` over `JoinHandle`s still yields in submission order, so
            // the emitted sequence is unchanged.
            .buffered(crate::engine::fanout::discovery_concurrency())
            .map(|joined| match joined {
                Ok(res) => res,
                Err(e) => Err(anyhow::Error::new(e).context("package discovery task panicked")),
            });
            futures::pin_mut!(per_pkg);

            loop {
                let candidates = match per_pkg.next().await {
                    None => break,
                    Some(Ok(candidates)) => candidates,
                    // Never `?` straight out: inside `try_stream!` that yields the
                    // error and *returns*, leaving up to K-1 package tasks running
                    // with an `Arc<RequestState>` each. They would finish and
                    // release it — spawning means they are no longer strandable —
                    // but not before `Engine::result` has returned, so the request
                    // would deregister late and `drain_bg`, which waits on
                    // `bg_pending` rather than on detached tasks, would race it.
                    // Flag the walk as abandoning (which ends the source, so
                    // nothing further is spawned) and join what is already running
                    // before propagating.
                    Some(Err(e)) => {
                        stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        while per_pkg.next().await.is_some() {}
                        Err(e)?;
                        // Not reached: `Err(e)?` in a `try_stream!` body yields
                        // the error and returns. Present because the match arm
                        // still has to produce a value.
                        break;
                    }
                };
                for addr in candidates {
                    match m.matches_addr(&addr) {
                        MatchResult::MatchYes => {
                            if seen.insert(addr.clone()) { yield addr; }
                        }
                        MatchResult::MatchNo => {}
                        MatchResult::MatchShrug => {
                            // Speculative inspection: resolve the candidate's spec/def only
                            // to evaluate the matcher, on a speculative rs so a rejected
                            // candidate records no edge in the shared dep DAG (which would
                            // otherwise close a false cycle later). One chain at a time
                            // *within this walk* — see the note above the fan-out, which
                            // also records the engine-level exposure across walks.
                            let spec_rs = rs.speculative();
                            let spec = match Arc::clone(&self).get_spec(spec_rs.clone(), &addr).await {
                                Ok(spec) => Ok(spec),
                                Err(e) if downcast_chain_ref::<TargetNotFoundError>(&e).is_some() => continue,
                                Err(e) if downcast_chain_ref::<CycleError>(&e).is_some() => continue,
                                res => res,
                            }?;

                            match crate::engine::matcher_spec::match_spec(m, &spec) {
                                MatchResult::MatchYes => {
                                    if seen.insert(addr.clone()) { yield addr; }
                                }
                                MatchResult::MatchNo => {}
                                MatchResult::MatchShrug => {
                                    let def = match Arc::clone(&self).get_def(spec_rs.clone(), &addr).await {
                                        Ok(def) => def,
                                        // Cycle means this candidate transitively depends on the
                                        // query caller — it cannot be a result. Skip it.
                                        Err(e) if downcast_chain_ref::<CycleError>(&e).is_some() => continue,
                                        Err(e) => Err(e)?,
                                    };

                                    if crate::engine::matcher_target::match_target(
                                        m,
                                        &def.target_def,
                                    ) == MatchResult::MatchYes
                                        && seen.insert(addr.clone())
                                    {
                                        yield addr;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if whole_graph {
                htelemetry::telemetry::record_graph_size(seen.len() as u64);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use futures::TryStreamExt;
    use hbuiltins::pluginstatictarget;
    use hmodel::htmatcher::Matcher;
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn target(pkg: &str, name: &str, labels: &[&str]) -> pluginstatictarget::Target {
        pluginstatictarget::Target {
            addr: format!("//{pkg}:{name}"),
            driver: "exec".to_string(),
            run: None,
            out: HashMap::new(),
            codegen: None,
            deps: HashMap::new(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn make_engine(targets: Vec<pluginstatictarget::Target>) -> anyhow::Result<Arc<Engine>> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok(Arc::new(engine))
    }

    #[tokio::test]
    async fn query_dedups_repeated_addrs() -> anyhow::Result<()> {
        use crate::engine::provider::{
            ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
            ListPackagesRequest, ListResponse, ProbeRequest, ProbeResponse,
        };
        use futures::future::BoxFuture;
        use hcore::hasync::Cancellable;

        // Provider that surfaces the same addr twice in one package, and the
        // same package twice from `list_packages`.
        struct Dup;
        impl crate::engine::provider::Provider for Dup {
            fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
                Ok(ConfigResponse {
                    name: "dup".to_string(),
                })
            }
            fn list<'a>(
                &'a self,
                _req: ListRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<
                'a,
                anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
            > {
                Box::pin(async {
                    let mk = || {
                        Ok(ListResponse {
                            addr: Addr::new(
                                PkgBuf::from("foo"),
                                "a".to_string(),
                                Default::default(),
                            ),
                        })
                    };
                    let items: Vec<anyhow::Result<ListResponse>> = vec![mk(), mk()];
                    Ok(Box::new(items.into_iter())
                        as Box<
                            dyn Iterator<Item = anyhow::Result<ListResponse>> + Send,
                        >)
                })
            }
            fn list_packages<'a>(
                &'a self,
                _req: ListPackagesRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<
                'a,
                anyhow::Result<
                    Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>,
                >,
            > {
                Box::pin(async {
                    let items: Vec<anyhow::Result<ListPackageResponse>> = vec![
                        Ok(ListPackageResponse {
                            pkg: PkgBuf::from("foo"),
                        }),
                        Ok(ListPackageResponse {
                            pkg: PkgBuf::from("foo"),
                        }),
                    ];
                    Ok(Box::new(items.into_iter())
                        as Box<
                            dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                        >)
                })
            }
            fn get<'a>(
                &'a self,
                _req: GetRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
                Box::pin(async { Err(GetError::NotFound) })
            }
            fn probe<'a>(
                &'a self,
                req: ProbeRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
                let pkg = req.package.clone();
                Box::pin(async move {
                    Ok(ProbeResponse {
                        states: vec![crate::engine::provider::State {
                            package: pkg,
                            provider: "dup".to_string(),
                            state: Default::default(),
                        }],
                    })
                })
            }
        }

        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(move |_| Box::new(Dup))?;
        let engine = Arc::new(engine);

        let rs = engine.new_state();
        let addrs: Vec<Addr> = engine
            .query(rs, &Matcher::Package(PkgBuf::from("foo")))
            .try_collect()
            .await?;

        assert_eq!(addrs.len(), 1, "duplicate addrs collapsed");
        assert_eq!(addrs[0].name, "a");
        Ok(())
    }

    #[tokio::test]
    async fn query_by_package() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            target("foo/bar", "a", &[]),
            target("foo/bar", "b", &[]),
            target("other", "c", &[]),
        ])?;

        let rs = engine.new_state();
        let addrs: Vec<Addr> = engine
            .query(rs, &Matcher::Package(PkgBuf::from("foo/bar")))
            .try_collect()
            .await?;

        assert_eq!(addrs.len(), 2);
        assert!(addrs.iter().any(|a| a.name == "a"));
        assert!(addrs.iter().any(|a| a.name == "b"));
        Ok(())
    }

    #[tokio::test]
    async fn whole_graph_query_records_graph_size() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            target("foo/bar", "a", &[]),
            target("foo/bar", "b", &[]),
            target("other", "c", &[]),
        ])?;

        let rs = engine.new_state();
        let addrs: Vec<Addr> = engine
            .query(rs, &Matcher::PackagePrefix(PkgBuf::from("")))
            .try_collect()
            .await?;
        assert_eq!(addrs.len(), 3);

        // The whole-graph enumeration must land in the telemetry counter. The
        // collector is process-global and keeps the largest seen value, so with
        // other tests running in parallel only a lower bound is stable.
        assert!(
            htelemetry::telemetry::snapshot().graph_size >= 3,
            "whole-graph query must record the graph size"
        );
        Ok(())
    }

    #[tokio::test]
    async fn query_by_addr() -> anyhow::Result<()> {
        let engine = make_engine(vec![target("foo", "a", &[]), target("foo", "b", &[])])?;

        let rs = engine.new_state();
        let target_addr = Addr::new(PkgBuf::from("foo"), "a".to_string(), Default::default());
        let addrs: Vec<Addr> = engine
            .query(rs, &Matcher::Addr(target_addr))
            .try_collect()
            .await?;

        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].name, "a");
        Ok(())
    }

    #[tokio::test]
    async fn query_by_label_calls_get_spec() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            target("foo", "a", &["//labels:lint"]),
            target("foo", "b", &[]),
        ])?;

        let rs = engine.new_state();
        let addrs: Vec<Addr> = engine
            .query(rs, &Matcher::Label("//labels:lint".to_string()))
            .try_collect()
            .await?;

        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].name, "a");
        Ok(())
    }

    #[tokio::test]
    async fn list_request_receives_probed_states() -> anyhow::Result<()> {
        use crate::engine::provider::{
            ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
            ListPackagesRequest, ListResponse, ProbeRequest, ProbeResponse, State,
        };
        use futures::future::BoxFuture;
        use hcore::hasync::Cancellable;
        use std::sync::Mutex;

        struct Recorder {
            list_states: Arc<Mutex<Vec<Vec<State>>>>,
        }
        impl crate::engine::provider::Provider for Recorder {
            fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
                Ok(ConfigResponse {
                    name: "rec".to_string(),
                })
            }
            fn list<'a>(
                &'a self,
                req: ListRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<
                'a,
                anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
            > {
                let states = req.states.clone();
                let rec = Arc::clone(&self.list_states);
                Box::pin(async move {
                    rec.lock().unwrap().push(states);
                    Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
                })
            }
            fn list_packages<'a>(
                &'a self,
                _req: ListPackagesRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<
                'a,
                anyhow::Result<
                    Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>,
                >,
            > {
                Box::pin(async {
                    let items: Vec<anyhow::Result<ListPackageResponse>> =
                        vec![Ok(ListPackageResponse {
                            pkg: PkgBuf::from("a/b/c"),
                        })];
                    Ok(Box::new(items.into_iter())
                        as Box<
                            dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                        >)
                })
            }
            fn get<'a>(
                &'a self,
                _req: GetRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
                Box::pin(async { Err(GetError::NotFound) })
            }
            fn probe<'a>(
                &'a self,
                req: ProbeRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
                let pkg = req.package.clone();
                Box::pin(async move {
                    Ok(ProbeResponse {
                        states: vec![State {
                            package: pkg,
                            provider: "rec".to_string(),
                            state: Default::default(),
                        }],
                    })
                })
            }
        }

        let root = tempdir()?;
        let list_states = Arc::new(Mutex::new(Vec::<Vec<State>>::new()));
        let list_states_clone = Arc::clone(&list_states);
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(move |_| {
            Box::new(Recorder {
                list_states: Arc::clone(&list_states_clone),
            })
        })?;
        let engine = Arc::new(engine);
        let rs = engine.new_state();

        let _: Vec<Addr> = engine
            .query(rs, &Matcher::Package(PkgBuf::from("a/b/c")))
            .try_collect()
            .await?;

        let recorded = list_states.lock().unwrap();
        // The built-in `fs` provider also advertises its `@heph/fs` package, so
        // `list` may run for that too; assert the call for the queried package.
        let abc = recorded
            .iter()
            .find(|states| {
                states
                    .first()
                    .is_some_and(|s| s.package.as_str() == "a/b/c")
            })
            .expect("list called for queried package a/b/c");
        let pkgs: Vec<String> = abc.iter().map(|s| s.package.as_str().to_string()).collect();
        assert_eq!(pkgs, vec!["a/b/c", "a/b", "a", ""]);
        Ok(())
    }

    /// A provider over a fixed package set that records every package `list`
    /// was actually asked for — the cost a scoped query must not pay.
    struct ListSpy {
        pkgs: Vec<&'static str>,
        listed: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl crate::engine::provider::Provider for ListSpy {
        fn config(
            &self,
            _req: crate::engine::provider::ConfigRequest,
        ) -> anyhow::Result<crate::engine::provider::ConfigResponse> {
            Ok(crate::engine::provider::ConfigResponse {
                name: "spy".to_string(),
            })
        }
        fn list<'a>(
            &'a self,
            req: ListRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            anyhow::Result<
                Box<
                    dyn Iterator<Item = anyhow::Result<crate::engine::provider::ListResponse>>
                        + Send,
                >,
            >,
        > {
            let pkg = req.package.clone();
            let listed = Arc::clone(&self.listed);
            Box::pin(async move {
                listed.lock().unwrap().push(pkg.as_str().to_string());
                let items: Vec<anyhow::Result<crate::engine::provider::ListResponse>> =
                    vec![Ok(crate::engine::provider::ListResponse {
                        addr: Addr::new(pkg, "t".to_string(), Default::default()),
                    })];
                Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn list_packages<'a>(
            &'a self,
            _req: crate::engine::provider::ListPackagesRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            anyhow::Result<
                Box<
                    dyn Iterator<
                            Item = anyhow::Result<crate::engine::provider::ListPackageResponse>,
                        > + Send,
                >,
            >,
        > {
            // Deliberately ignores `req.prefix`, like every real provider in
            // the tree: the engine must do the narrowing itself.
            let items: Vec<anyhow::Result<crate::engine::provider::ListPackageResponse>> = self
                .pkgs
                .iter()
                .map(|p| {
                    Ok(crate::engine::provider::ListPackageResponse {
                        pkg: PkgBuf::from(*p),
                    })
                })
                .collect();
            Box::pin(async move {
                Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn get<'a>(
            &'a self,
            _req: crate::engine::provider::GetRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            Result<crate::engine::provider::GetResponse, crate::engine::provider::GetError>,
        > {
            Box::pin(async { Err(crate::engine::provider::GetError::NotFound) })
        }
        fn probe<'a>(
            &'a self,
            req: crate::engine::provider::ProbeRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<'a, anyhow::Result<crate::engine::provider::ProbeResponse>>
        {
            let pkg = req.package.clone();
            Box::pin(async move {
                Ok(crate::engine::provider::ProbeResponse {
                    states: vec![crate::engine::provider::State {
                        package: pkg,
                        provider: "spy".to_string(),
                        state: Default::default(),
                    }],
                })
            })
        }
    }

    async fn listed_packages_for(m: Matcher) -> anyhow::Result<Vec<String>> {
        let root = tempdir()?;
        let listed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let spy_listed = Arc::clone(&listed);
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(move |_| {
            Box::new(ListSpy {
                pkgs: vec!["foo", "foo/deep", "bar", "bar/deep", "unrelated"],
                listed: Arc::clone(&spy_listed),
            })
        })?;
        let engine = Arc::new(engine);
        let rs = engine.new_state();

        let _: Vec<Addr> = engine.query(rs, &m).try_collect().await?;

        let mut out = listed.lock().unwrap().clone();
        out.sort();
        Ok(out)
    }

    #[tokio::test]
    async fn scoped_query_does_not_list_packages_outside_its_scope() -> anyhow::Result<()> {
        // `label(lint) && //foo/...`: the label arm can never prune, but the
        // package arm must keep `list` (a whole-package Starlark evaluation
        // for the buildfile provider) off every package outside `foo`.
        let listed = listed_packages_for(Matcher::And(vec![
            Matcher::Label("lint".to_string()),
            Matcher::PackagePrefix(PkgBuf::from("foo")),
        ]))
        .await?;
        assert_eq!(listed, vec!["foo".to_string(), "foo/deep".to_string()]);

        // Arm order must not change what gets paid for.
        let flipped = listed_packages_for(Matcher::And(vec![
            Matcher::PackagePrefix(PkgBuf::from("foo")),
            Matcher::Label("lint".to_string()),
        ]))
        .await?;
        assert_eq!(flipped, listed);
        Ok(())
    }

    #[tokio::test]
    async fn addr_query_lists_only_the_owning_package() -> anyhow::Result<()> {
        let listed = listed_packages_for(Matcher::Addr(Addr::new(
            PkgBuf::from("bar"),
            "t".to_string(),
            Default::default(),
        )))
        .await?;
        assert_eq!(listed, vec!["bar".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn unprunable_matcher_still_scans_everything() -> anyhow::Result<()> {
        // A bare label has no package information — pruning must not invent any.
        // The always-on built-in `fs` provider contributes `@heph/fs`, and every
        // provider is listed for every surviving package.
        let listed = listed_packages_for(Matcher::Label("lint".to_string())).await?;
        assert_eq!(
            listed,
            vec![
                "@heph/fs".to_string(),
                "bar".to_string(),
                "bar/deep".to_string(),
                "foo".to_string(),
                "foo/deep".to_string(),
                "unrelated".to_string(),
            ]
        );
        Ok(())
    }

    /// A provider whose per-package `list` is slow, with a per-package delay the
    /// test chooses. Tracks peak in-flight `list` calls so a test can distinguish
    /// an overlapped walk from a serial one without trusting wall-clock alone.
    struct SlowList {
        pkgs: Vec<String>,
        /// Sleep for the package at index `i` in `pkgs`.
        delay: Box<dyn Fn(usize) -> std::time::Duration + Send + Sync>,
        inflight: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::engine::provider::Provider for SlowList {
        fn config(
            &self,
            _req: crate::engine::provider::ConfigRequest,
        ) -> anyhow::Result<crate::engine::provider::ConfigResponse> {
            Ok(crate::engine::provider::ConfigResponse {
                name: "slow".to_string(),
            })
        }
        fn list<'a>(
            &'a self,
            req: ListRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            anyhow::Result<
                Box<
                    dyn Iterator<Item = anyhow::Result<crate::engine::provider::ListResponse>>
                        + Send,
                >,
            >,
        > {
            use std::sync::atomic::Ordering::SeqCst;
            let pkg = req.package.clone();
            let idx = self.pkgs.iter().position(|p| p == pkg.as_str());
            // Packages this provider does not own (e.g. the built-in `@heph/fs`)
            // cost nothing and list nothing.
            let Some(idx) = idx else {
                return Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<_>) });
            };
            let d = (self.delay)(idx);
            let (inflight, peak) = (Arc::clone(&self.inflight), Arc::clone(&self.peak));
            Box::pin(async move {
                let now = inflight.fetch_add(1, SeqCst) + 1;
                peak.fetch_max(now, SeqCst);
                tokio::time::sleep(d).await;
                inflight.fetch_sub(1, SeqCst);
                let items: Vec<anyhow::Result<crate::engine::provider::ListResponse>> =
                    vec![Ok(crate::engine::provider::ListResponse {
                        addr: Addr::new(pkg, "t".to_string(), Default::default()),
                    })];
                Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn list_packages<'a>(
            &'a self,
            _req: crate::engine::provider::ListPackagesRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            anyhow::Result<
                Box<
                    dyn Iterator<
                            Item = anyhow::Result<crate::engine::provider::ListPackageResponse>,
                        > + Send,
                >,
            >,
        > {
            let items: Vec<anyhow::Result<crate::engine::provider::ListPackageResponse>> = self
                .pkgs
                .iter()
                .map(|p| {
                    Ok(crate::engine::provider::ListPackageResponse {
                        pkg: PkgBuf::from(p.as_str()),
                    })
                })
                .collect();
            Box::pin(async move {
                Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn get<'a>(
            &'a self,
            _req: crate::engine::provider::GetRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            Result<crate::engine::provider::GetResponse, crate::engine::provider::GetError>,
        > {
            Box::pin(async { Err(crate::engine::provider::GetError::NotFound) })
        }
        fn probe<'a>(
            &'a self,
            req: crate::engine::provider::ProbeRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<'a, anyhow::Result<crate::engine::provider::ProbeResponse>>
        {
            let pkg = req.package.clone();
            Box::pin(async move {
                Ok(crate::engine::provider::ProbeResponse {
                    states: vec![crate::engine::provider::State {
                        package: pkg,
                        provider: "slow".to_string(),
                        state: Default::default(),
                    }],
                })
            })
        }
    }

    fn slow_engine(
        pkgs: Vec<String>,
        delay: impl Fn(usize) -> std::time::Duration + Send + Sync + 'static,
        inflight: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        let delay = Arc::new(delay);
        engine.register_provider(move |_| {
            Box::new(SlowList {
                pkgs: pkgs.clone(),
                delay: {
                    let d = Arc::clone(&delay);
                    Box::new(move |i| d(i))
                },
                inflight: Arc::clone(&inflight),
                peak: Arc::clone(&peak),
            })
        })?;
        Ok((Arc::new(engine), root))
    }

    /// Discovery is the producer for the whole pipeline and used to be strictly
    /// serial — `for pkg { probe; for provider { list } }` — so a workspace's
    /// wall-clock discovery time was the *sum* of every package's evaluation.
    ///
    /// The assertion is on observed concurrency, not only elapsed time: a serial
    /// loop peaks at one in-flight `list` on any machine, so this cannot pass
    /// against it however fast or slow the host is.
    #[tokio::test]
    async fn query_overlaps_per_package_listing() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

        const N: usize = 12;
        let delay = std::time::Duration::from_millis(60);
        let pkgs: Vec<String> = (0..N).map(|i| format!("p{i:02}")).collect();
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (engine, _root) = slow_engine(
            pkgs,
            move |_| delay,
            Arc::clone(&inflight),
            Arc::clone(&peak),
        )?;

        let rs = engine.new_state();
        let start = std::time::Instant::now();
        let addrs: Vec<Addr> = Arc::clone(&engine)
            .query(rs, &Matcher::PackagePrefix(PkgBuf::from("")))
            .try_collect()
            .await?;
        let elapsed = start.elapsed();

        assert_eq!(addrs.len(), N);

        let k = crate::engine::fanout::discovery_concurrency();
        let peak = peak.load(SeqCst);
        // A serial `for pkg { … list.await … }` peaks at exactly one in-flight
        // `list`, on every machine — that is what this cannot pass against.
        // Not asserted as an equality: `min(N, K)` only holds if the runtime
        // polls all K before the first sleep expires, which a descheduled
        // worker on a loaded runner can break. The exact cap is asserted in
        // `query_keeps_at_most_k_packages_in_flight`, where it is the point.
        assert!(peak > 1, "package listings must overlap; serial peaks at 1");
        assert!(
            peak <= k,
            "in-flight listings must stay within K={k}, saw {peak}"
        );
        // Wall clock, derived from the actual K rather than a fixed fraction:
        // the serial cost is `N * delay`, the overlapped cost about
        // `ceil(N/K) * delay`. Allowing 2x that still fails against serial for
        // every K >= 2, which is the floor `discovery_concurrency()` can return.
        let waves = N.div_ceil(k.max(1)) as u32;
        assert!(
            elapsed < delay * waves * 2,
            "overlapped discovery of {N} packages at K={k} must beat serial \
             ({:?}), took {elapsed:?}",
            delay * (N as u32)
        );
        Ok(())
    }

    /// The in-flight *cap* is the memory half of the change — what keeps a
    /// 20k-package workspace from holding 20k live package futures. The test
    /// above cannot see it, because `K >= N` on most dev machines. Here N is a
    /// multiple of K, so the bound is the thing being measured.
    #[tokio::test]
    async fn query_keeps_at_most_k_packages_in_flight() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

        let k = crate::engine::fanout::discovery_concurrency();
        let n = k * 4;
        let pkgs: Vec<String> = (0..n).map(|i| format!("p{i:04}")).collect();
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (engine, _root) = slow_engine(
            pkgs,
            |_| std::time::Duration::from_millis(5),
            Arc::clone(&inflight),
            Arc::clone(&peak),
        )?;

        let rs = engine.new_state();
        let addrs: Vec<Addr> = Arc::clone(&engine)
            .query(rs, &Matcher::PackagePrefix(PkgBuf::from("")))
            .try_collect()
            .await?;

        assert_eq!(addrs.len(), n);
        let peak = peak.load(SeqCst);
        assert!(
            peak <= k,
            "at most K={k} packages may be in flight at once, saw {peak} with N={n}"
        );
        assert!(peak > 1, "package listings must still overlap, saw {peak}");
        Ok(())
    }

    /// The addr sequence `query` emits is a build input: it carries through
    /// `pluginquery`'s `deps` into `plugingroup`, which folds `deps` in order
    /// into its def hash. Overlapping the packages must therefore keep the
    /// *submission* order (`buffered`) and never adopt completion order
    /// (`buffer_unordered`), or a query group's identity becomes a function of
    /// which BUILD file the scheduler happened to finish first.
    ///
    /// The delays here invert completion order relative to package order, so a
    /// `buffer_unordered` implementation returns the exact reverse.
    #[tokio::test]
    async fn query_emits_packages_in_listing_order_not_completion_order() -> anyhow::Result<()> {
        use std::sync::atomic::AtomicUsize;

        const N: usize = 8;
        let pkgs: Vec<String> = (0..N).map(|i| format!("p{i:02}")).collect();
        let (engine, _root) = slow_engine(
            pkgs.clone(),
            // First package listed sleeps longest, last sleeps least.
            |i| std::time::Duration::from_millis(((N - i) * 15) as u64),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        )?;

        let rs = engine.new_state();
        let addrs: Vec<Addr> = Arc::clone(&engine)
            .query(rs, &Matcher::PackagePrefix(PkgBuf::from("")))
            .try_collect()
            .await?;

        let got: Vec<String> = addrs
            .iter()
            .map(|a| a.package.as_str().to_string())
            .collect();
        assert_eq!(
            got, pkgs,
            "query must emit packages in listing order even though they complete \
             in the reverse order"
        );
        Ok(())
    }

    #[tokio::test]
    async fn query_empty_when_no_match() -> anyhow::Result<()> {
        let engine = make_engine(vec![target("foo", "a", &[])])?;

        let rs = engine.new_state();
        let addrs: Vec<Addr> = engine
            .query(rs, &Matcher::Package(PkgBuf::from("nonexistent")))
            .try_collect()
            .await?;

        assert!(addrs.is_empty());
        Ok(())
    }
}
