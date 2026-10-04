use crate::engine::Engine;
use crate::engine::discovery::{Discovery, Gaps, Stage, is_cancellation, provider_of};
use crate::engine::error::{CancelledError, CycleError, TargetNotFoundError};
use crate::engine::listed::ListedLabelsMismatch;
use crate::engine::packages::merge_packages;
use crate::engine::provider::ListRequest;
use crate::engine::request_state::RequestState;
use crate::engine::spec::EngineTargetSpec;
use enclose::enclose;
use futures::{Stream, StreamExt, TryStreamExt};
use hcore::hmemoizer::downcast_chain_ref;
use hmodel::htaddr::Addr;
use hmodel::htmatcher;
use hmodel::htmatcher::MatchResult;
use hmodel::htpkg::PkgBuf;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

/// What one package's discovery task found: its candidates, in provider order,
/// and under [`Discovery::KeepGoing`] whatever it could not resolve.
#[derive(Default)]
struct PackageScan {
    candidates: Vec<Candidate>,
    skipped: Vec<Skip>,
}

/// One listed target, with what its provider said about its labels.
struct Candidate {
    addr: Addr,
    /// Index of the listing provider in `Engine::providers`.
    provider: usize,
    labels: Option<Arc<[String]>>,
}

/// A target the walk selected.
#[derive(Debug)]
pub(crate) struct Selected {
    pub(crate) addr: Addr,
    /// Selected on its listed labels alone: nothing has resolved its spec, so
    /// it may not exist (a provider can list more than it can resolve). Only a
    /// [`ListedMatch::Trust`] walk yields one.
    pub(crate) unconfirmed: bool,
}

/// What a walk does with a candidate whose listed labels match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListedMatch {
    /// Resolve it on the walk's serial arm before yielding it, so every
    /// yielded addr exists.
    Confirm,
    /// Yield it unresolved, as [`Selected::unconfirmed`]; the caller resolves
    /// it. See [`Engine::select`].
    Trust,
}

/// The labels the walk matches a listed addr against: one set when every
/// provider that listed it agrees, `None` (unknown — the spec decides) when
/// they differ or any of them does not know.
///
/// Two providers can list one addr and only one of them resolve it: go lists
/// a bare `build` in every package and declines it in a library, where a BUILD
/// file may define its own. First-listing-wins would match against go's set
/// and silently miss the BUILD target's labels.
fn merge_listings(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut at: FxHashMap<Addr, usize> = FxHashMap::default();
    let mut merged: Vec<Candidate> = Vec::with_capacity(candidates.len());
    for c in candidates {
        match at.get(&c.addr) {
            Some(&i) => {
                if let Some(first) = merged.get_mut(i)
                    && first.labels.as_deref() != c.labels.as_deref()
                {
                    first.labels = None;
                }
            }
            None => {
                at.insert(c.addr.clone(), merged.len());
                merged.push(c);
            }
        }
    }
    merged
}

/// Whether evaluating `m` can read a target's labels at all. Listings are only
/// worth recording — an `Addr` clone and a map entry per candidate — for a
/// walk that will act on them.
fn mentions_label(m: &htmatcher::Matcher) -> bool {
    use htmatcher::Matcher as M;
    match m {
        M::Label(_) => true,
        M::Or(ms) | M::And(ms) => ms.iter().any(mentions_label),
        M::Not(m) => mentions_label(m),
        M::Addr(_) | M::Package(_) | M::PackagePrefix(_) | M::TreeOutputTo(_) => false,
    }
}

/// One skipped scope, carried from a package task to the consumer, which owns
/// the sink.
struct Skip {
    stage: Stage,
    provider: String,
    scope: String,
    error: anyhow::Error,
}

impl Engine {
    /// Every target matching `m`, streamed.
    ///
    /// `discovery` decides what a candidate that cannot be resolved costs the
    /// walk — see [`Discovery`]. Under [`Discovery::KeepGoing`] the six places a
    /// walk can fail (a provider's package listing, a package's probe, a
    /// provider's `list`, and a candidate's spec or def where the matcher needs
    /// them, plus `query_spec`'s spec) record into the sink and the walk
    /// carries on. Never skipped, in either mode: cancellation, a panicked
    /// package task, and a candidate that was never there (`TargetNotFound`, a
    /// cycle back to the caller), which is silently dropped as before.
    ///
    /// The nested walk behind query targets
    /// (`EngineProviderExecutor::query`) is a different function and is never
    /// parameterized: its output reaches a def hash, so it is always complete.
    pub fn query<'a>(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        m: &'a htmatcher::Matcher,
        discovery: Discovery,
    ) -> impl Stream<Item = anyhow::Result<Addr>> + 'a {
        self.select(rs, m, discovery, ListedMatch::Confirm)
            .map_ok(|selected| selected.addr)
    }

    /// [`query`](Self::query), optionally yielding candidates whose listed
    /// labels match without resolving them first.
    ///
    /// Listed labels always *reject* without a resolve: a candidate the
    /// listing says cannot match never reaches `get`. `listed` decides what
    /// happens to a listed *match*. [`ListedMatch::Confirm`] resolves it on the
    /// serial arm below like any shrug, so every addr `query` yields still
    /// exists. [`ListedMatch::Trust`] yields it as [`Selected::unconfirmed`]
    /// and the caller resolves it — which is what lets `Engine::result` resolve
    /// matches in parallel rather than one at a time here. A caller that trusts
    /// owns two things: dropping an unconfirmed addr that turns out not to
    /// exist, and never announcing one before it has resolved.
    pub(crate) fn select<'a>(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        m: &'a htmatcher::Matcher,
        discovery: Discovery,
        listed: ListedMatch,
    ) -> impl Stream<Item = anyhow::Result<Selected>> + 'a {
        let uses_labels = mentions_label(m);
        // A whole-graph selector (`//...` — a `PackagePrefix` rooted at the empty
        // package) enumerates every target, so its final match count is the total
        // graph size. Recorded for telemetry only when the stream is driven to
        // completion with nothing skipped: an early-dropped, errored or
        // incomplete stream never saw the full graph. Centralized here so every
        // whole-graph caller (query, unscoped validate) is covered without
        // per-command code.
        let whole_graph = matches!(m, htmatcher::Matcher::PackagePrefix(p) if p.is_empty());
        async_stream::try_stream! {
            let gaps = discovery.gaps().cloned();
            let keep_going = gaps.is_some();
            let mut skipped_any = false;
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
            let pkgs: Vec<String> = match &gaps {
                None => self.packages(m, &rs).await?.collect::<anyhow::Result<_>>()?,
                Some(gaps) => {
                    let (pkgs, skipped) = self.packages_keep_going(m, &rs, gaps).await?;
                    skipped_any |= skipped;
                    pkgs
                }
            };

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
            // `Matcher::Label` shrugs for every candidate whose provider did
            // not list its labels, and every listed match comes through here
            // too unless the caller passed `ListedMatch::Trust`; and
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
            // `buffered`, never `buffer_unordered`: the emission order is what
            // `heph query` prints and the order `Engine::result` admits targets
            // in, and it must not depend on which BUILD file the OS scheduler
            // finished first. `buffered` yields in submission order, so the
            // sequence — and every candidate's position in it — is exactly the
            // one the serial loop produced. (The nested walk in
            // `EngineProviderExecutor::query` makes the same choice for a
            // stronger reason: its order carries through `pluginquery`'s `deps`
            // into `plugingroup`, which folds them in order into a def hash.)
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
                    // `Err`, never `Ok` with nothing in it: an empty package and a
                    // package the walk never looked at must not read the same.
                    // Under `KeepGoing` a package that could not be scanned is
                    // a recorded skip the command reports; a cancelled one is
                    // neither, it ends the walk.
                    if rs.ctoken().is_cancelled()
                        || stop.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        return Err(anyhow::Error::new(CancelledError));
                    }
                    let mut scan = PackageScan::default();
                    let states = match Arc::clone(&engine).probe_segments(&rs, &pkg).await {
                        Ok(states) => states,
                        Err(e) if keep_going && !is_cancellation(&rs, &e) => {
                            scan.skipped.push(Skip {
                                stage: Stage::Probe,
                                provider: provider_of(&e).to_string(),
                                scope: format!("//{pkg}"),
                                error: e,
                            });
                            return Ok(scan);
                        }
                        Err(e) => return Err(e),
                    };

                    for (provider_idx, provider) in engine.providers.iter().enumerate() {
                        let listed = provider.provider.list(ListRequest {
                            request_id: rs.request_id().to_string(),
                            package: pkg.clone(),
                            states: states
                                .iter()
                                .filter(|s| s.provider == provider.name)
                                .cloned()
                                .collect(),
                            executor: Arc::clone(&executor),
                        }, rs.ctoken()).await
                            // Drained before the next await.
                            .and_then(|it| it.collect::<anyhow::Result<Vec<_>>>());
                        let raw = match listed {
                            Ok(raw) => raw,
                            // The other providers' candidates in this package
                            // are still good.
                            Err(e) if keep_going && !is_cancellation(&rs, &e) => {
                                scan.skipped.push(Skip {
                                    stage: Stage::List,
                                    provider: provider.name.clone(),
                                    scope: format!("//{pkg}"),
                                    error: e,
                                });
                                continue;
                            }
                            Err(e) => return Err(e),
                        };

                        for item in raw {
                            if item.addr.package == pkg {
                                scan.candidates.push(Candidate {
                                    addr: item.addr,
                                    provider: provider_idx,
                                    labels: item.labels,
                                });
                            }
                        }
                    }

                    anyhow::Ok(scan)
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

            // Every way out of the walk on an error goes through this one value,
            // never through `?` straight out: inside `try_stream!` that yields
            // the error and *returns*, leaving up to K-1 package tasks running
            // with an `Arc<RequestState>` each. They would finish and release it
            // — spawning means they are no longer strandable — but not before
            // `Engine::result` has returned, so the request would deregister
            // late and `drain_bg`, which waits on `bg_pending` rather than on
            // detached tasks, would race it. So the walk is flagged as
            // abandoning (which ends the source, so nothing further is spawned)
            // and what is already running is joined before propagating.
            let fatal: Option<anyhow::Error> = 'walk: loop {
                let scan = match per_pkg.next().await {
                    None => break 'walk None,
                    Some(Ok(scan)) => scan,
                    Some(Err(e)) => break 'walk Some(e),
                };
                if let Some(gaps) = &gaps {
                    for skip in scan.skipped {
                        skipped_any = true;
                        gaps.record(skip.stage, &skip.provider, skip.scope, &skip.error);
                    }
                }
                // Every provider's listing of a package is in this one scan, so
                // this is where they are reconciled. Not worth an `Addr` clone
                // per candidate when the matcher never reads a label.
                let candidates = if uses_labels {
                    merge_listings(scan.candidates)
                } else {
                    scan.candidates
                };
                for Candidate { addr, provider, labels } in candidates {
                    // Recorded before matching, so every later read of this
                    // addr's spec — here, in the caller, or as some other
                    // target's dep — is checked against the set the matcher used.
                    let labels = match labels {
                        Some(labels) if uses_labels => {
                            Some(rs.data.listed_labels.record(&addr, provider, &labels))
                        }
                        _ => None,
                    };
                    let mut verdict = m.matches_listed(&addr, labels.as_deref());
                    // A yes the addr alone would not have given: it rests on
                    // the listing, and nothing has shown the target exists.
                    let by_listing = verdict == MatchResult::MatchYes
                        && labels.is_some()
                        && m.matches_addr(&addr) != MatchResult::MatchYes;
                    if by_listing && listed == ListedMatch::Confirm {
                        verdict = MatchResult::MatchShrug;
                    }
                    match verdict {
                        MatchResult::MatchYes => {
                            if seen.insert(addr.clone()) {
                                yield Selected { addr, unconfirmed: by_listing };
                            }
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
                                Ok(spec) => spec,
                                // A candidate that was never there. (Any not-found in
                                // the chain reads as this one: `get_spec_no_track`
                                // rewrites it to the candidate's own addr.)
                                Err(e) if downcast_chain_ref::<TargetNotFoundError>(&e).is_some() => continue,
                                Err(e) if downcast_chain_ref::<CycleError>(&e).is_some() => continue,
                                // Not a gap to carry on past: the selection
                                // already acted on the listing it contradicts.
                                // (One raised under a plugin's own `get`, while
                                // it resolves a listed dependency through the
                                // executor, crosses the seam as a string and
                                // reads as an ordinary spec failure. Accepted:
                                // it still fails a complete walk, and the top
                                // level reads that same spec directly.)
                                Err(e) if downcast_chain_ref::<ListedLabelsMismatch>(&e).is_some() => {
                                    break 'walk Some(e)
                                }
                                Err(e) => match &gaps {
                                    Some(gaps) if !is_cancellation(&rs, &e) => {
                                        skipped_any = true;
                                        gaps.record(Stage::Spec, provider_of(&e), addr.format(), &e);
                                        continue;
                                    }
                                    _ => break 'walk Some(e),
                                },
                            };

                            match crate::engine::matcher_spec::match_spec(m, &spec) {
                                MatchResult::MatchYes => {
                                    if seen.insert(addr.clone()) {
                                        yield Selected { addr, unconfirmed: false };
                                    }
                                }
                                MatchResult::MatchNo => {}
                                MatchResult::MatchShrug => {
                                    let def = match Arc::clone(&self).get_def(spec_rs.clone(), &addr).await {
                                        Ok(def) => def,
                                        // Cycle means this candidate transitively depends on the
                                        // query caller — it cannot be a result. Skip it.
                                        Err(e) if downcast_chain_ref::<CycleError>(&e).is_some() => continue,
                                        Err(e) => match &gaps {
                                            Some(gaps) if !is_cancellation(&rs, &e) => {
                                                skipped_any = true;
                                                gaps.record(Stage::Def, &spec.provider, addr.format(), &e);
                                                continue;
                                            }
                                            _ => break 'walk Some(e),
                                        },
                                    };

                                    if crate::engine::matcher_target::match_target(
                                        m,
                                        &def.target_def,
                                    ) == MatchResult::MatchYes
                                        && seen.insert(addr.clone())
                                    {
                                        yield Selected { addr, unconfirmed: false };
                                    }
                                }
                            }
                        }
                    }
                }
            };

            if let Some(e) = fatal {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                while per_pkg.next().await.is_some() {}
                Err(e)?;
            }

            if whole_graph && !skipped_any {
                htelemetry::telemetry::record_graph_size(seen.len() as u64);
            }
        }
    }

    /// [`Engine::packages`] for a keep-going walk: every provider whose listing
    /// succeeded, merged the same way, with each failed listing recorded in
    /// `gaps`. The `bool` is whether anything was recorded.
    ///
    /// Reads the same memoized per-provider cells and never writes a short list
    /// into them, so `states_under` — which reaches a def hash — still sees the
    /// failure as an `Err`.
    async fn packages_keep_going(
        &self,
        m: &htmatcher::Matcher,
        rs: &Arc<RequestState>,
        gaps: &Gaps,
    ) -> anyhow::Result<(Vec<String>, bool)> {
        let mut listed = Vec::with_capacity(self.providers.len());
        let mut skipped = false;
        let lists = self.provider_packages(m, rs).await;
        for (provider, res) in self.providers.iter().zip(lists) {
            match res {
                Ok(pkgs) => listed.push(pkgs),
                Err(e) if is_cancellation(rs, &e) => return Err(e),
                Err(e) => {
                    skipped = true;
                    gaps.record(
                        Stage::Packages,
                        &provider.name,
                        "all packages".to_string(),
                        &e,
                    );
                }
            }
        }
        Ok((merge_packages(m, &listed), skipped))
    }

    /// [`Engine::query`] one tier up: the **spec** of every target matching `m`,
    /// with the candidates that don't resolve already dropped (see
    /// [`skip_unresolvable`]).
    ///
    /// This is what a walk that reads specs should use. Resolving `query`'s
    /// addrs by hand is the same four lines of `TargetNotFoundError` downcast at
    /// every call site, and getting them wrong turns one unresolvable candidate
    /// into a failed walk.
    ///
    /// Specs are resolved off the addr stream with a bounded in-flight set and
    /// yielded in completion order, so nothing is materialized in bulk: a
    /// whole-graph selector can be many thousands of addrs.
    ///
    /// Under [`Discovery::KeepGoing`] a matched candidate whose spec fails is
    /// recorded and dropped like any other skip, rather than ending the stream.
    pub fn query_spec<'a>(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        m: &'a htmatcher::Matcher,
        discovery: Discovery,
    ) -> impl Stream<Item = anyhow::Result<Arc<EngineTargetSpec>>> + 'a {
        // Cap in-flight spec resolutions; the engine's own semaphores gate the
        // real work, this just bounds the orchestration set held off the stream.
        let concurrency = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .saturating_mul(2);
        let gaps = discovery.gaps().cloned();

        Arc::clone(&self)
            .query(rs.clone(), m, discovery)
            .map_ok(move |addr| {
                enclose!((self => engine, rs, gaps) async move {
                    match skip_unresolvable(&addr, engine.get_spec(rs.clone(), &addr).await) {
                        Err(e) => match &gaps {
                            Some(gaps) if !is_cancellation(&rs, &e) => {
                                gaps.record(Stage::Spec, provider_of(&e), addr.format(), &e);
                                Ok(None)
                            }
                            _ => Err(e),
                        },
                        res => res,
                    }
                })
            })
            .try_buffer_unordered(concurrency)
            .try_filter_map(|spec| std::future::ready(Ok(spec)))
    }
}

impl Engine {
    /// Resolve the spec of a [`Selected::unconfirmed`] addr, so the caller of
    /// a [`ListedMatch::Trust`] walk learns whether it exists: `Some(addr)` if
    /// it does, `None` for a candidate the provider listed but cannot resolve.
    /// A confirmed selection passes straight through.
    ///
    /// Drops what the walk's serial arm drops — a cycle back through the addr,
    /// and under [`Discovery::KeepGoing`] a spec failure, recorded as a skip —
    /// with one deliberate difference: only a not-found naming *this* addr
    /// (see [`skip_unresolvable`]). One naming a dependency is a real missing
    /// dependency and fails. A [`ListedLabelsMismatch`] is never dropped: the
    /// selection acted on the listing it contradicts.
    pub(crate) async fn confirm_selected(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        gaps: Option<Arc<Gaps>>,
        selected: Selected,
    ) -> anyhow::Result<Option<Addr>> {
        if !selected.unconfirmed {
            return Ok(Some(selected.addr));
        }
        let addr = selected.addr;
        match skip_unresolvable(&addr, self.get_spec(rs.clone(), &addr).await) {
            Ok(Some(_)) => Ok(Some(addr)),
            Ok(None) => {
                // Silent to the user by design; here for "why wasn't X selected?".
                tracing::debug!(
                    addr = %addr.format(),
                    "listed label match did not resolve; dropped from the selection"
                );
                Ok(None)
            }
            Err(e) if downcast_chain_ref::<ListedLabelsMismatch>(&e).is_some() => Err(e),
            Err(e) if downcast_chain_ref::<CycleError>(&e).is_some() => Ok(None),
            Err(e) => match &gaps {
                Some(gaps) if !is_cancellation(&rs, &e) => {
                    gaps.record(Stage::Spec, provider_of(&e), addr.format(), &e);
                    Ok(None)
                }
                _ => Err(e),
            },
        }
    }
}

/// Drop a [`Engine::query`] candidate that cannot be resolved standalone.
///
/// A provider's `list` is a **candidate** set, not a target list: it may
/// advertise an addr `get` declines — go's per-package target set, for one, is
/// only known once `go list` has run, so `list` emits the superset and `get`
/// narrows it. A walk over query results must expect that and skip such a
/// candidate rather than fail: it is not a broken target, it is a target that
/// was never there.
///
/// Only a `TargetNotFound` naming **this same addr** counts. One naming a
/// *different* addr came from resolving a dependency of this target — a real
/// missing dependency, which still propagates.
///
/// [`Engine::query_spec`] applies this already; call it directly only where the
/// resolution isn't stream-shaped (a `join_all_failable` fan-out that must
/// report every failure, or a def-tier walk):
///
/// ```ignore
/// let Some(def) = skip_unresolvable(&addr, engine.get_def(rs, &addr).await)? else {
///     return Ok(Vec::new());
/// };
/// ```
pub fn skip_unresolvable<T>(addr: &Addr, res: anyhow::Result<T>) -> anyhow::Result<Option<T>> {
    match res {
        Ok(v) => Ok(Some(v)),
        Err(e)
            if downcast_chain_ref::<TargetNotFoundError>(&e).is_some_and(|nf| nf.addr == *addr) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
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

    /// A provider that lists one addr per package and can `get` none of them —
    /// the shape every real provider has some corner of (go's `list` is a
    /// candidate set narrowed at `get` time by what `go list` reports).
    struct PhantomLister;

    impl crate::engine::provider::Provider for PhantomLister {
        fn config(
            &self,
            _req: crate::engine::provider::ConfigRequest,
        ) -> anyhow::Result<crate::engine::provider::ConfigResponse> {
            Ok(crate::engine::provider::ConfigResponse {
                name: "phantom".to_string(),
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
            let addr = Addr::new(
                req.package.clone(),
                "phantom".to_string(),
                Default::default(),
            );
            Box::pin(async move {
                let items = vec![Ok(crate::engine::provider::ListResponse::addr_only(addr))];
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
            Box::pin(async {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
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
            _req: crate::engine::provider::ProbeRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<'a, anyhow::Result<crate::engine::provider::ProbeResponse>>
        {
            Box::pin(async { Ok(crate::engine::provider::ProbeResponse { states: vec![] }) })
        }
    }

    /// `list` is a candidate set: a provider may advertise an addr no provider
    /// can resolve. `query_spec` is where that gets absorbed — one phantom
    /// sibling must not take a whole-graph walk down with `target not found`,
    /// and every consumer that resolves query results (`labels`, `validate`,
    /// `revdeps`, the gitignore walk) leans on it.
    #[tokio::test]
    async fn query_spec_skips_candidates_that_cannot_be_resolved() -> anyhow::Result<()> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        let provider = pluginstatictarget::Provider::new(vec![target("foo", "a", &["lint"])])?;
        engine.register_provider(move |_| Box::new(provider))?;
        engine.register_provider(|_| Box::new(PhantomLister))?;
        let engine = Arc::new(engine);

        let rs = engine.new_state();
        let specs: Vec<Arc<EngineTargetSpec>> = engine
            .query_spec(
                rs,
                &Matcher::PackagePrefix(PkgBuf::from("")),
                Discovery::Complete,
            )
            .try_collect()
            .await?;

        let addrs: Vec<String> = specs.iter().map(|s| s.addr.format()).collect();
        assert_eq!(addrs, vec!["//foo:a".to_string()]);
        Ok(())
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
                        Ok(ListResponse::addr_only(Addr::new(
                            PkgBuf::from("foo"),
                            "a".to_string(),
                            Default::default(),
                        )))
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
            .query(
                rs,
                &Matcher::Package(PkgBuf::from("foo")),
                Discovery::Complete,
            )
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
            .query(
                rs,
                &Matcher::Package(PkgBuf::from("foo/bar")),
                Discovery::Complete,
            )
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
            .query(
                rs,
                &Matcher::PackagePrefix(PkgBuf::from("")),
                Discovery::Complete,
            )
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
            .query(rs, &Matcher::Addr(target_addr), Discovery::Complete)
            .try_collect()
            .await?;

        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].name, "a");
        Ok(())
    }

    #[tokio::test]
    async fn query_by_label_calls_get_spec() -> anyhow::Result<()> {
        let engine = make_engine(vec![target("foo", "a", &["lint"]), target("foo", "b", &[])])?;

        let rs = engine.new_state();
        let addrs: Vec<Addr> = engine
            .query(rs, &Matcher::Label("lint".to_string()), Discovery::Complete)
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
            .query(
                rs,
                &Matcher::Package(PkgBuf::from("a/b/c")),
                Discovery::Complete,
            )
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
                    vec![Ok(crate::engine::provider::ListResponse::addr_only(
                        Addr::new(pkg, "t".to_string(), Default::default()),
                    ))];
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

        let _: Vec<Addr> = engine
            .query(rs, &m, Discovery::Complete)
            .try_collect()
            .await?;

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
                    vec![Ok(crate::engine::provider::ListResponse::addr_only(
                        Addr::new(pkg, "t".to_string(), Default::default()),
                    ))];
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
            .query(
                rs,
                &Matcher::PackagePrefix(PkgBuf::from("")),
                Discovery::Complete,
            )
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
            .query(
                rs,
                &Matcher::PackagePrefix(PkgBuf::from("")),
                Discovery::Complete,
            )
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
            .query(
                rs,
                &Matcher::PackagePrefix(PkgBuf::from("")),
                Discovery::Complete,
            )
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
            .query(
                rs,
                &Matcher::Package(PkgBuf::from("nonexistent")),
                Discovery::Complete,
            )
            .try_collect()
            .await?;

        assert!(addrs.is_empty());
        Ok(())
    }

    // ─── Keep-going discovery ────────────────────────────────────────────────

    mod keep_going {
        use super::*;
        use crate::engine::discovery::{GapReport, Stage};
        use crate::engine::fault_provider::{FaultProvider, Faults};
        use std::time::Duration;

        /// `//p:b` and friends, for `Faults`' addr fields.
        fn addrs(addrs: &[&str]) -> Vec<Addr> {
            addrs
                .iter()
                .map(|a| hmodel::htaddr::parse_addr(a).expect("addr"))
                .collect()
        }

        /// An engine over one [`FaultProvider`] named `faulty`, plus any
        /// healthy static targets registered before it.
        fn faulty_engine(
            healthy: Vec<pluginstatictarget::Target>,
            targets: Vec<pluginstatictarget::Target>,
            faults: Faults,
        ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
            let root = tempdir()?;
            let mut engine = Engine::new(Config {
                root: root.path().to_path_buf(),
                home_dir: std::path::PathBuf::new(),
                parallelism: None,
                ..Default::default()
            })?;
            // For the def stage: a target naming any other driver fails `parse`.
            engine.register_managed_driver(|_| {
                Box::new(hplugin_exec::pluginexec::Driver::new_exec())
            })?;
            if !healthy.is_empty() {
                let provider = pluginstatictarget::Provider::new(healthy)?;
                engine.register_provider(move |_| Box::new(provider))?;
            }
            let provider = FaultProvider::new(targets, faults)?;
            engine.register_provider(move |_| Box::new(provider))?;
            Ok((Arc::new(engine), root))
        }

        async fn walk(
            engine: &Arc<Engine>,
            m: &Matcher,
            discovery: Discovery,
        ) -> anyhow::Result<Vec<String>> {
            let rs = engine.new_state();
            Arc::clone(engine)
                .query(rs, m, discovery)
                .map_ok(|a| a.format())
                .try_collect()
                .await
        }

        fn label(l: &str) -> Matcher {
            Matcher::Label(l.to_string())
        }

        /// (stage, provider, count, example scopes) per group.
        fn groups(report: &GapReport) -> Vec<(Stage, &str, usize, Vec<&str>)> {
            report
                .groups
                .iter()
                .map(|g| {
                    (
                        g.stage,
                        g.provider.as_str(),
                        g.count,
                        g.examples.iter().map(|e| e.scope.as_str()).collect(),
                    )
                })
                .collect()
        }

        #[tokio::test]
        async fn keep_going_skips_a_candidate_whose_spec_fails() -> anyhow::Result<()> {
            let (engine, _root) = faulty_engine(
                vec![],
                vec![
                    target("p", "a", &["x"]),
                    target("p", "b", &["x"]),
                    target("p", "c", &["x"]),
                ],
                Faults {
                    fail_get: addrs(&["//p:b"]),
                    ..Default::default()
                },
            )?;
            let gaps = Gaps::new("label(x)");
            let got = walk(&engine, &label("x"), Discovery::KeepGoing(gaps.clone())).await?;
            assert_eq!(got, ["//p:a", "//p:c"]);
            let report = gaps.report();
            assert_eq!(report.skipped, 1);
            assert_eq!(groups(&report), [(Stage::Spec, "faulty", 1, vec!["//p:b"])]);
            assert_eq!(report.groups[0].examples[0].cause, "go list: exit status 1");
            Ok(())
        }

        /// `Complete` keeps today's answer — the first error fails the walk —
        /// and, from the label-check arm too, joins the package tasks already
        /// running before it returns, so none outlives the walk holding the
        /// request.
        #[tokio::test]
        async fn complete_mode_fails_on_the_first_error_and_drains() -> anyhow::Result<()> {
            let (engine, _root) = faulty_engine(
                vec![],
                vec![target("p", "b", &["x"]), target("q", "slow", &["x"])],
                Faults {
                    fail_get: addrs(&["//p:b"]),
                    // `//p:b` fails only once `q`'s list has started, so `q` is
                    // in flight when the walk hits the error, however loaded
                    // the machine is.
                    gate: Some(Arc::new(tokio::sync::Notify::new())),
                    slow_list: Some(("q".to_string(), Duration::from_millis(300))),
                    ..Default::default()
                },
            )?;
            let rs = engine.new_state();
            let res: anyhow::Result<Vec<Addr>> = Arc::clone(&engine)
                .query(rs.clone(), &label("x"), Discovery::Complete)
                .try_collect()
                .await;
            let err = res.expect_err("the first failure fails a complete walk");
            assert!(
                format!("{err:#}").contains("go list: exit status 1"),
                "{err:#}"
            );
            assert_eq!(
                Arc::strong_count(&rs),
                1,
                "a package task was still running, holding the request"
            );
            Ok(())
        }

        #[tokio::test]
        async fn keep_going_keeps_other_providers_when_one_list_fails() -> anyhow::Result<()> {
            let (engine, _root) = faulty_engine(
                vec![target("p", "a", &[])],
                vec![target("p", "z", &[])],
                Faults {
                    fail_list: vec!["p".to_string()],
                    ..Default::default()
                },
            )?;
            let gaps = Gaps::new("//p");
            let m = Matcher::Package(PkgBuf::from("p"));
            let got = walk(&engine, &m, Discovery::KeepGoing(gaps.clone())).await?;
            assert_eq!(got, ["//p:a"]);
            assert_eq!(
                groups(&gaps.report()),
                [(Stage::List, "faulty", 1, vec!["//p"])]
            );
            Ok(())
        }

        #[tokio::test]
        async fn keep_going_skips_a_package_whose_probe_fails() -> anyhow::Result<()> {
            let (engine, _root) = faulty_engine(
                vec![],
                vec![target("p", "a", &[]), target("q", "b", &[])],
                Faults {
                    fail_probe: vec!["q".to_string()],
                    ..Default::default()
                },
            )?;
            let gaps = Gaps::new("//...");
            let m = Matcher::PackagePrefix(PkgBuf::from(""));
            let got = walk(&engine, &m, Discovery::KeepGoing(gaps.clone())).await?;
            assert_eq!(got, ["//p:a"]);
            assert_eq!(
                groups(&gaps.report()),
                [(Stage::Probe, "faulty", 1, vec!["//q"])]
            );
            Ok(())
        }

        #[tokio::test]
        async fn keep_going_survives_a_provider_whose_list_packages_fails() -> anyhow::Result<()> {
            let (engine, _root) = faulty_engine(
                vec![target("p", "a", &[])],
                vec![target("q", "b", &[])],
                Faults {
                    fail_list_packages: true,
                    ..Default::default()
                },
            )?;
            let gaps = Gaps::new("//...");
            let m = Matcher::PackagePrefix(PkgBuf::from(""));
            let got = walk(&engine, &m, Discovery::KeepGoing(gaps.clone())).await?;
            assert_eq!(got, ["//p:a"]);
            assert_eq!(
                groups(&gaps.report()),
                [(Stage::Packages, "faulty", 1, vec!["all packages"])]
            );
            // `Engine::packages` itself never answers short: it reads the same
            // memoized cell and still fails, so `states_under` cannot hash a
            // partial package set.
            let rs = engine.new_state();
            assert!(engine.packages(&m, &rs).await.is_err());
            Ok(())
        }

        /// Point 6: `//...` decides every candidate from its address, so the
        /// walk itself resolves no spec — `query_spec` does, after the match.
        #[tokio::test]
        async fn query_spec_keeps_going_past_a_matched_candidate_whose_spec_fails()
        -> anyhow::Result<()> {
            let (engine, _root) = faulty_engine(
                vec![],
                vec![
                    target("p", "a", &[]),
                    target("p", "b", &[]),
                    target("p", "c", &[]),
                ],
                Faults {
                    fail_get: addrs(&["//p:b"]),
                    ..Default::default()
                },
            )?;
            let gaps = Gaps::new("//...");
            let rs = engine.new_state();
            let m = Matcher::PackagePrefix(PkgBuf::from(""));
            let mut got: Vec<String> = Arc::clone(&engine)
                .query_spec(rs, &m, Discovery::KeepGoing(gaps.clone()))
                .map_ok(|s| s.addr.format())
                .try_collect()
                .await?;
            got.sort();
            assert_eq!(got, ["//p:a", "//p:c"]);
            assert_eq!(
                groups(&gaps.report()),
                [(Stage::Spec, "faulty", 1, vec!["//p:b"])]
            );
            Ok(())
        }

        /// A call cancelled with two dependencies in flight fails with a
        /// `MultiError` the typed downcast does not look inside; the request
        /// token says what it is. At every stage that can record a skip, it
        /// ends the walk instead, unrecorded — otherwise Ctrl-C would turn into
        /// thousands of skips and an "incomplete selection".
        #[tokio::test]
        async fn cancellation_is_never_recorded() -> anyhow::Result<()> {
            for (stage, at) in [
                (Stage::Packages, ""),
                (Stage::Probe, "p"),
                (Stage::List, "p"),
                (Stage::Spec, "//p:b"),
            ] {
                let started = Arc::new(tokio::sync::Notify::new());
                let (engine, _root) = faulty_engine(
                    vec![],
                    vec![target("p", "a", &["x"]), target("p", "b", &["x"])],
                    Faults {
                        cancel_at: Some((stage, at.to_string(), Arc::clone(&started))),
                        ..Default::default()
                    },
                )?;
                let gaps = Gaps::new("label(x)");
                let rs = engine.new_state();
                let walk = tokio::spawn({
                    let (engine, rs, gaps) = (Arc::clone(&engine), Arc::clone(&rs), gaps.clone());
                    async move {
                        let m = label("x");
                        engine
                            .query(rs, &m, Discovery::KeepGoing(gaps))
                            .try_collect::<Vec<Addr>>()
                            .await
                    }
                });
                started.notified().await;
                rs.ctoken().cancel();
                let res = tokio::time::timeout(Duration::from_secs(10), walk).await??;
                assert!(
                    res.is_err(),
                    "{stage:?}: a cancelled walk is not a complete one"
                );
                assert!(gaps.is_empty(), "{stage:?}: {:?}", gaps.report());
            }
            Ok(())
        }

        /// The sixth point: a candidate whose spec resolves but whose def does
        /// not, under a matcher that needs the def (`tree_output()`). Grouped
        /// under the provider that produced the spec.
        #[tokio::test]
        async fn keep_going_skips_a_candidate_whose_def_fails() -> anyhow::Result<()> {
            let codegen = |name: &str, driver: &str| pluginstatictarget::Target {
                addr: format!("//p:{name}"),
                driver: driver.to_string(),
                run: Some("true".to_string()),
                out: HashMap::from([(String::new(), vec![format!("{name}.go")])]),
                codegen: Some("copy".to_string()),
                ..Default::default()
            };
            let targets = || vec![codegen("a", "exec"), codegen("b", "no-such-driver")];
            let m = Matcher::TreeOutputTo(PkgBuf::from(""));

            let (engine, _root) = faulty_engine(vec![], targets(), Faults::default())?;
            let gaps = Gaps::new("tree_output()");
            let got = walk(&engine, &m, Discovery::KeepGoing(gaps.clone())).await?;
            assert_eq!(got, ["//p:a"]);
            assert_eq!(
                groups(&gaps.report()),
                [(Stage::Def, "faulty", 1, vec!["//p:b"])]
            );

            let (engine, _root) = faulty_engine(vec![], targets(), Faults::default())?;
            assert!(
                walk(&engine, &m, Discovery::Complete).await.is_err(),
                "a complete walk still fails on it"
            );
            Ok(())
        }

        /// A candidate whose `get` builds a dependency that does not exist (a
        /// `_golist` whose input is gone) is a real candidate that could not be
        /// checked: recorded, with the missing addr in its cause.
        #[tokio::test]
        async fn a_candidate_whose_dependency_is_missing_is_recorded() -> anyhow::Result<()> {
            let targets = || vec![target("p", "a", &["x"]), target("p", "b", &["x"])];
            let faults = || Faults {
                // `//p:b`'s `get` builds `//gone:dep`, which does not exist.
                builds: vec![(
                    addrs(&["//p:b"])[0].clone(),
                    addrs(&["//gone:dep"])[0].clone(),
                )],
                ..Default::default()
            };

            let (engine, _root) = faulty_engine(vec![], targets(), faults())?;
            let gaps = Gaps::new("label(x)");
            let got = walk(&engine, &label("x"), Discovery::KeepGoing(gaps.clone())).await?;
            assert_eq!(got, ["//p:a"]);
            let report = gaps.report();
            assert_eq!(groups(&report), [(Stage::Spec, "faulty", 1, vec!["//p:b"])]);
            assert!(
                report.groups[0].examples[0].cause.contains("//gone:dep"),
                "{report:?}"
            );

            let (engine, _root) = faulty_engine(vec![], targets(), faults())?;
            assert!(
                walk(&engine, &label("x"), Discovery::Complete)
                    .await
                    .is_err(),
                "a complete walk fails on it"
            );
            Ok(())
        }

        /// `probe_segments` probes a package's ancestors, so one broken
        /// `a/BUILD` takes every package under `a` with it — each counted once.
        #[tokio::test]
        async fn an_ancestor_probe_failure_skips_every_package_under_it() -> anyhow::Result<()> {
            let (engine, _root) = faulty_engine(
                vec![],
                vec![
                    target("a", "x", &[]),
                    target("a/b", "y", &[]),
                    target("a/b/c", "z", &[]),
                    target("d", "w", &[]),
                ],
                Faults {
                    fail_probe: vec!["a".to_string()],
                    ..Default::default()
                },
            )?;
            let gaps = Gaps::new("//...");
            let m = Matcher::PackagePrefix(PkgBuf::from(""));
            let got = walk(&engine, &m, Discovery::KeepGoing(gaps.clone())).await?;
            assert_eq!(got, ["//d:w"]);
            assert_eq!(
                groups(&gaps.report()),
                [(Stage::Probe, "faulty", 3, vec!["//a", "//a/b", "//a/b/c"])]
            );
            Ok(())
        }

        #[tokio::test]
        async fn panicked_package_task_stays_fatal_in_keep_going() -> anyhow::Result<()> {
            let (engine, _root) = faulty_engine(
                vec![],
                vec![target("p", "a", &[]), target("q", "b", &[])],
                Faults {
                    panic_list: vec!["q".to_string()],
                    ..Default::default()
                },
            )?;
            let gaps = Gaps::new("//...");
            let m = Matcher::PackagePrefix(PkgBuf::from(""));
            let err = walk(&engine, &m, Discovery::KeepGoing(gaps.clone()))
                .await
                .expect_err("a panic is a bug, not a gap");
            assert!(format!("{err:#}").contains("panicked"), "{err:#}");
            assert!(gaps.is_empty());
            Ok(())
        }

        #[tokio::test]
        async fn healthy_walk_output_is_byte_identical_under_keep_going() -> anyhow::Result<()> {
            let targets = || {
                vec![
                    target("a", "one", &["x"]),
                    target("a", "two", &[]),
                    target("a/b", "three", &["x"]),
                    target("c", "four", &["x"]),
                    target("c", "five", &["y"]),
                ]
            };
            for m in [
                label("x"),
                Matcher::PackagePrefix(PkgBuf::from("")),
                Matcher::And(vec![label("x"), Matcher::PackagePrefix(PkgBuf::from("a"))]),
            ] {
                let (engine, _root) = faulty_engine(vec![], targets(), Faults::default())?;
                let complete = walk(&engine, &m, Discovery::Complete).await?;
                let (engine, _root) = faulty_engine(vec![], targets(), Faults::default())?;
                let gaps = Gaps::new("");
                let kept = walk(&engine, &m, Discovery::KeepGoing(gaps.clone())).await?;
                assert_eq!(complete, kept);
                assert!(gaps.is_empty());
            }
            Ok(())
        }

        /// `heph validate` runs several walks on one request into one sink: a
        /// candidate every walk fails on counts once, and the report is the
        /// same however the runs went.
        #[tokio::test]
        async fn three_walks_report_each_skip_once_and_deterministically() -> anyhow::Result<()> {
            let mut reports = Vec::new();
            for reversed in [false, true] {
                let (engine, _root) = faulty_engine(
                    vec![],
                    (0..8)
                        .map(|i| target("p", &format!("t{i}"), &["x", "y"]))
                        .collect(),
                    Faults {
                        fail_get: addrs(&["//p:t3", "//p:t5", "//p:t7"]),
                        ..Default::default()
                    },
                )?;
                let gaps = Gaps::new("validate");
                let rs = engine.new_state();
                let mut matchers = vec![
                    label("x"),
                    label("y"),
                    Matcher::And(vec![label("x"), Matcher::PackagePrefix(PkgBuf::from(""))]),
                ];
                // The second engine runs the walks in the other order.
                if reversed {
                    matchers.reverse();
                }
                for m in matchers {
                    let _: Vec<Addr> = Arc::clone(&engine)
                        .query(rs.clone(), &m, Discovery::KeepGoing(gaps.clone()))
                        .try_collect()
                        .await?;
                }
                reports.push(gaps.report());
            }
            assert_eq!(reports[0], reports[1]);
            assert_eq!(reports[0].skipped, 3);
            assert_eq!(
                groups(&reports[0]),
                [(Stage::Spec, "faulty", 3, vec!["//p:t3", "//p:t5", "//p:t7"])]
            );
            Ok(())
        }
    }
}
