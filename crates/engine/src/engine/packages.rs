use crate::engine::Engine;
use crate::engine::provider::ListPackagesRequest;
use crate::engine::request_state::RequestState;
use enclose::enclose;
use hcore::hmemoizer::unwrap_arc_err;
use hmodel::htmatcher;
use hmodel::htpkg::PkgBuf;
use rustc_hash::FxHashSet;
use std::sync::Arc;

/// Walk the matcher and pick the most specific `Package` / `PackagePrefix`
/// constraint that any path-to-MatchYes must satisfy. Used to narrow the
/// `list_packages` scan; the per-addr matcher still runs afterwards, so
/// over-broad prefixes only cost perf, never correctness.
///
/// `And` arms are intersected — pick the longest narrowing arm. `Or` / `Not`
/// can't be narrowed in general (any arm could match a different prefix),
/// so they fall back to empty.
fn narrowing_prefix(m: &htmatcher::Matcher) -> PkgBuf {
    match m {
        htmatcher::Matcher::Package(p) | htmatcher::Matcher::PackagePrefix(p) => p.clone(),
        // An exact addr lives in exactly one package.
        htmatcher::Matcher::Addr(a) => a.package.clone(),
        htmatcher::Matcher::And(ms) => ms
            .iter()
            .map(narrowing_prefix)
            .max_by_key(|p| p.as_str().len())
            .unwrap_or_else(|| PkgBuf::from("")),
        _ => PkgBuf::from(""),
    }
}

impl Engine {
    /// Every package matching `m`'s narrowing prefix, deduped, in an order that
    /// does not depend on the order any provider returned: each provider's block
    /// is sorted, then the blocks are concatenated in provider-registration
    /// order.
    ///
    /// The order is load-bearing, not cosmetic. It reaches a def hash by more
    /// than one route — `query` → `pluginquery`'s `deps` → `plugingroup` folds
    /// them in order, and `EngineProviderExecutor::states_under` accumulates
    /// package-major so it also drives `ListRequest::states` order — and it
    /// reaches the sandbox as list-file line order. A provider is meanwhile free
    /// to hand back `HashSet` iteration order, i.e. a per-process hash seed.
    /// In-tree that was a live bug (fixed at the buildfile provider in #218); a
    /// third-party cdylib provider is outside our reach entirely, so the engine
    /// enforces rather than documenting a contract it cannot check.
    ///
    /// Sorted per provider, not globally, because `query` walks `self.providers`
    /// *inside* its per-package loop: the addr order it produces is
    /// package-major / provider-minor and so already rests on registration
    /// order. That order is config-declared and deterministic for a given
    /// workspace config — the always-on `query` and `fs`, then `plugins:`
    /// entries in the order written — so a global sort would buy no determinism
    /// the per-provider sort doesn't already give, while re-interleaving every
    /// provider's block, re-keying every multi-provider query target for
    /// nothing. (Reordering `plugins:` therefore re-keys those targets. That is
    /// a user editing the build, not a per-process seed.)
    ///
    /// Deliberately no `debug_assert!` / `warn!` when a provider hands back an
    /// unsorted list: `plugingo` legitimately returns a pre-order DFS, which is
    /// deterministic but not lexicographic once a sibling name is a
    /// punctuation-extended prefix of another (`-` is 0x2D, `/` is 0x2F). So
    /// sortedness is the wrong predicate; the invariant worth checking is
    /// *determinism*, which a single (memoized) call cannot observe.
    pub async fn packages(
        &self,
        m: &htmatcher::Matcher,
        rs: &Arc<RequestState>,
    ) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<String>> + Send>> {
        let prefix = narrowing_prefix(m);

        // Each provider's `list_packages` is an independent workspace walk — the
        // buildfile provider's recursive BUILD-file scan and the go provider's
        // `collect_go_packages` cover the same tree and never touched the same
        // state, yet ran one after the other. Overlap them: the walks are the
        // producer for the whole pipeline, so their latency is on the critical
        // path of every `query` / `validate` / `labels` / `revdeps`.
        //
        // `join_all`, not `try_join_all`: the walks are few (one per registered
        // provider) and already running, so short-circuiting saves nothing — and
        // `try_join_all` would return whichever walk failed *first in time*,
        // making "which provider does heph blame" a race on a flaky mount. The
        // serial loop always blamed the earliest-registered failing provider;
        // taking the lowest-index error below keeps that.
        let results: Vec<anyhow::Result<Arc<Vec<String>>>> =
            futures::future::join_all(self.providers.iter().map(|provider| {
                let req = ListPackagesRequest {
                    prefix: prefix.clone(),
                };
                let key = format!("{}:{}", provider.name, prefix);

                async move {
                    rs.data
                        .mem_packages
                        .once(
                            key,
                            enclose!((provider, rs) move || async move {
                                let it = provider
                                    .provider
                                    .list_packages(req, rs.ctoken())
                                    .await?;
                                let mut pkgs = Vec::new();
                                for res in it {
                                    pkgs.push(res?.pkg.to_string());
                                }
                                // Canonicalize the provider's block rather than
                                // trusting it to be ordered — see this method's
                                // docs for why the order matters and why the
                                // sort is per provider. Inside the memoizer, so
                                // it is paid once per (provider, prefix) per
                                // request, not once per call.
                                //
                                // This is the canonical package ordering for the
                                // whole engine, and it must stay a
                                // byte-lexicographic `String` compare. A
                                // collation-aware or case-folding comparator
                                // would make `LC_COLLATE` an undeclared hash
                                // input: two machines would order the same tree
                                // differently and could never share a
                                // remote-cache entry for a query-backed target.
                                //
                                // `dedup` is free once sorted (adjacent, no
                                // hashing) and shrinks the `Arc<Vec<String>>`
                                // shared for the rest of the request. The
                                // cross-provider dedup below still needs its own
                                // set: `Vec::dedup` only collapses adjacent
                                // equals, and the blocks are memoized apart.
                                //
                                // The sort lives inside the per-provider
                                // memoizer cell, which is also why overlapping
                                // the walks cannot disturb it: each block is
                                // canonicalized by its own future, and the merge
                                // below re-imposes registration order.
                                pkgs.sort_unstable();
                                pkgs.dedup();
                                Ok(Arc::new(pkgs))
                            }),
                        )
                        .await
                        .map_err(unwrap_arc_err)
                }
            }))
            .await;

        let mut per_provider: Vec<Arc<Vec<String>>> = Vec::with_capacity(results.len());
        for res in results {
            per_provider.push(res?);
        }

        let mut all_packages = Vec::new();
        // Different providers can list the same package; dedup so callers
        // (e.g. `query`) don't scan a package more than once. A package listed
        // by two providers keeps the position of the first one that listed it.
        //
        // Overlapping the walks above must not be allowed to reorder this
        // merge: the fold runs over `per_provider` in provider-registration
        // order, exactly as the serial loop did, regardless of which walk
        // finished first.
        let mut seen: FxHashSet<String> = FxHashSet::default();

        for pkgs in &per_provider {
            for p in pkgs.iter() {
                if seen.insert(p.clone()) {
                    all_packages.push(p.clone());
                }
            }
        }

        Ok(Box::new(all_packages.into_iter().map(Ok)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::provider::{
        ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
        ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse,
    };
    use futures::future::BoxFuture;
    use hcore::hasync::Cancellable;
    use hmodel::htmatcher::Matcher;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn pkg(s: &str) -> PkgBuf {
        PkgBuf::from(s)
    }

    /// Fake provider: a name plus the exact package sequence it lists.
    ///
    /// `rotating` makes it rotate that sequence by one more position on every
    /// call — a deterministic stand-in for a provider whose order is not stable
    /// between calls (`HashSet` iteration order being the real-world shape).
    /// `calls` is shared with the test so it can assert the provider really was
    /// re-asked, rather than trusting the memoizer to be per-request.
    struct ListsPkgs {
        name: &'static str,
        pkgs: &'static [&'static str],
        rotating: bool,
        calls: Arc<AtomicUsize>,
    }

    impl ListsPkgs {
        fn new(name: &'static str, pkgs: &'static [&'static str]) -> Self {
            Self {
                name,
                pkgs,
                rotating: false,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn rotating(
            name: &'static str,
            pkgs: &'static [&'static str],
            calls: Arc<AtomicUsize>,
        ) -> Self {
            Self {
                name,
                pkgs,
                rotating: true,
                calls,
            }
        }

        fn listing(&self) -> Vec<&'static str> {
            let n = self.calls.fetch_add(1, Ordering::Relaxed);
            let mut v = self.pkgs.to_vec();
            if self.rotating && !v.is_empty() {
                let by = n % v.len();
                v.rotate_left(by);
            }
            v
        }
    }

    impl crate::engine::provider::Provider for ListsPkgs {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: self.name.to_string(),
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
                Ok(Box::new(std::iter::empty())
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
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
        > {
            let listing = self.listing();
            Box::pin(async move {
                let items: Vec<anyhow::Result<ListPackageResponse>> = listing
                    .into_iter()
                    .map(|p| {
                        Ok(ListPackageResponse {
                            pkg: PkgBuf::from(p),
                        })
                    })
                    .collect();
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
                        provider: "dp".to_string(),
                        state: Default::default(),
                    }],
                })
            })
        }
    }

    #[tokio::test]
    async fn packages_dedups_within_and_across_providers() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        // Each provider lists `foo` twice; two providers list it again.
        engine.register_provider(move |_| Box::new(ListsPkgs::new("p1", &["foo", "foo"])))?;
        engine.register_provider(move |_| Box::new(ListsPkgs::new("p2", &["foo", "foo"])))?;
        let engine = Arc::new(engine);
        let rs = engine.new_state();

        let pkgs: Vec<String> = engine
            .packages(&Matcher::PackagePrefix(pkg("")), &rs)
            .await?
            .collect::<anyhow::Result<Vec<_>>>()?;

        // The always-on built-in `fs` provider also advertises its `@heph/fs`
        // package; it sorts first because it is registered before `p1`/`p2`.
        // `foo` still appears exactly once despite four listings across providers.
        assert_eq!(pkgs, vec!["@heph/fs".to_string(), "foo".to_string()]);
        Ok(())
    }

    fn engine_with_builtins(root: &tempfile::TempDir) -> anyhow::Result<Engine> {
        Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })
    }

    /// Supersedes `packages_preserves_each_providers_listing_order`, which
    /// asserted the opposite: that the engine imposes no order and hands a
    /// provider's sequence through verbatim. That was only safe while every
    /// provider honored a *documented* ordering contract — which a third-party
    /// cdylib provider is under no obligation to do, and which the in-tree
    /// buildfile provider itself violated until #218. The engine now sorts each
    /// provider's block, so the output no longer depends on it.
    ///
    /// The order is not cosmetic: it reaches a def hash through `query` →
    /// `pluginquery`'s `deps` → `plugingroup`, which folds `deps` in order, and
    /// the sandbox's list-file line order.
    #[tokio::test]
    async fn packages_sorts_each_providers_listing() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut engine = engine_with_builtins(&root)?;
        // Deliberately reverse-sorted: the engine must impose its own order.
        engine.register_provider(move |_| {
            Box::new(ListsPkgs::new("p1", &["zeta", "mid", "alpha"]))
        })?;
        engine.register_provider(move |_| Box::new(ListsPkgs::new("p2", &["mid", "beta"])))?;
        let engine = Arc::new(engine);
        let rs = engine.new_state();

        let pkgs: Vec<String> = engine
            .packages(&Matcher::PackagePrefix(pkg("")), &rs)
            .await?
            .collect::<anyhow::Result<Vec<_>>>()?;

        // `@heph/fs` from the always-on built-in provider, registered first;
        // then p1's block sorted; then p2's only new package. Note `beta` lands
        // *after* `zeta`: the result is deliberately not one global sort. The
        // per-package loop in `query` already walks providers in registration
        // order, so sorting globally would re-interleave every provider's block
        // — re-keying every multi-provider query target — for no determinism
        // that sorting per provider doesn't already give.
        assert_eq!(pkgs, vec!["@heph/fs", "alpha", "mid", "zeta", "beta"]);
        Ok(())
    }

    /// Two engines given the same packages in different listing orders must
    /// produce byte-identical output. This is the property a documented contract
    /// could not enforce: a provider is free to return anything.
    #[tokio::test]
    async fn packages_order_is_independent_of_how_a_provider_lists() -> anyhow::Result<()> {
        async fn run(
            p1: &'static [&'static str],
            p2: &'static [&'static str],
        ) -> anyhow::Result<Vec<String>> {
            let root = tempfile::tempdir()?;
            let mut engine = engine_with_builtins(&root)?;
            engine.register_provider(move |_| Box::new(ListsPkgs::new("p1", p1)))?;
            engine.register_provider(move |_| Box::new(ListsPkgs::new("p2", p2)))?;
            let engine = Arc::new(engine);
            let rs = engine.new_state();
            engine
                .packages(&Matcher::PackagePrefix(pkg("")), &rs)
                .await?
                .collect::<anyhow::Result<Vec<_>>>()
        }

        let expected = vec!["@heph/fs", "alpha", "delta", "mid", "zeta", "beta"];

        // Reverse-sorted.
        assert_eq!(
            run(&["zeta", "mid", "delta", "alpha"], &["mid", "beta"]).await?,
            expected
        );
        // Shuffled, and the shared package listed from the other side first.
        assert_eq!(
            run(&["mid", "alpha", "zeta", "delta"], &["beta", "mid"]).await?,
            expected
        );
        // Already sorted — the compliant provider's result is unchanged.
        assert_eq!(
            run(&["alpha", "delta", "mid", "zeta"], &["beta", "mid"]).await?,
            expected
        );
        Ok(())
    }

    /// A provider whose order drifts between calls — the shape of the #218 bug,
    /// where `HashSet` iteration order made the same tree list differently on
    /// every run — must not make the engine's output drift with it. A fresh
    /// `RequestState` per iteration bypasses the per-request memoizer, so the
    /// provider really is re-asked and really does return a different order each
    /// time. The call count is asserted, not assumed: if `mem_packages` ever
    /// became engine-level, the loop would collapse to one call at rotation 0 and
    /// this would silently degrade into a copy of the test above.
    #[tokio::test]
    async fn packages_is_stable_across_requests_when_a_provider_reorders() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut engine = engine_with_builtins(&root)?;
        let calls = Arc::new(AtomicUsize::new(0));
        engine.register_provider(enclose!((calls) move |_| {
            Box::new(ListsPkgs::rotating("p1", &["zeta", "mid", "alpha", "delta"], calls))
        }))?;
        let engine = Arc::new(engine);

        for _ in 0..5 {
            let rs = engine.new_state();
            let pkgs: Vec<String> = engine
                .packages(&Matcher::PackagePrefix(pkg("")), &rs)
                .await?
                .collect::<anyhow::Result<Vec<_>>>()?;
            assert_eq!(pkgs, vec!["@heph/fs", "alpha", "delta", "mid", "zeta"]);
        }
        assert_eq!(
            calls.load(Ordering::Relaxed),
            5,
            "provider was not re-asked"
        );
        Ok(())
    }

    /// A provider whose `list_packages` is a slow workspace walk. Records how
    /// many walks were in flight at once, so a test can tell an overlapped fan-out
    /// from a serial loop without relying on wall-clock alone.
    struct SlowWalk {
        name: &'static str,
        pkgs: &'static [&'static str],
        delay: std::time::Duration,
        inflight: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::engine::provider::Provider for SlowWalk {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: self.name.to_string(),
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
                Ok(Box::new(std::iter::empty())
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
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
        > {
            use std::sync::atomic::Ordering::SeqCst;
            let items: Vec<anyhow::Result<ListPackageResponse>> = self
                .pkgs
                .iter()
                .map(|p| {
                    Ok(ListPackageResponse {
                        pkg: PkgBuf::from(*p),
                    })
                })
                .collect();
            let (delay, inflight, peak) = (
                self.delay,
                Arc::clone(&self.inflight),
                Arc::clone(&self.peak),
            );
            Box::pin(async move {
                let now = inflight.fetch_add(1, SeqCst) + 1;
                peak.fetch_max(now, SeqCst);
                tokio::time::sleep(delay).await;
                inflight.fetch_sub(1, SeqCst);
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
            let name = self.name.to_string();
            Box::pin(async move {
                Ok(ProbeResponse {
                    states: vec![crate::engine::provider::State {
                        package: pkg,
                        provider: name,
                        state: Default::default(),
                    }],
                })
            })
        }
    }

    /// Each provider's `list_packages` is an independent workspace walk (the
    /// buildfile BUILD-file scan and the go `collect_go_packages` scan cover the
    /// same tree). They used to run strictly one after another, so their
    /// latencies added; now they overlap.
    ///
    /// Asserted on observed concurrency rather than only wall-clock: a serial
    /// loop can never put two walks in flight at once, whatever the machine.
    ///
    /// The blocks are deliberately unsorted, so this also pins the composition
    /// with #230's per-provider sort: the sort lives inside the memoizer cell
    /// and must survive the fan-out, while the merge across blocks must stay in
    /// registration order rather than becoming one global sort.
    #[tokio::test]
    async fn packages_overlaps_the_provider_walks() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

        let root = tempfile::tempdir()?;
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let delay = std::time::Duration::from_millis(120);

        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        for (name, pkgs) in [
            ("w1", &["zeta", "alpha"][..]),
            ("w2", &["mid"][..]),
            ("w3", &["beta"][..]),
        ] {
            engine.register_provider(enclose!((inflight, peak) move |_| Box::new(SlowWalk {
                name,
                pkgs,
                delay,
                inflight: Arc::clone(&inflight),
                peak: Arc::clone(&peak),
            })))?;
        }
        let engine = Arc::new(engine);
        let rs = engine.new_state();

        let start = std::time::Instant::now();
        let pkgs: Vec<String> = engine
            .packages(&Matcher::PackagePrefix(pkg("")), &rs)
            .await?
            .collect::<anyhow::Result<Vec<_>>>()?;
        let elapsed = start.elapsed();

        assert_eq!(
            peak.load(SeqCst),
            3,
            "all three walks must be in flight at once; serial discovery peaks at 1"
        );
        assert!(
            elapsed < delay * 3,
            "three overlapped {delay:?} walks must beat three serial ones, took {elapsed:?}"
        );
        // Each block sorted (`alpha` before `zeta`, whichever walk finished
        // first), blocks concatenated in registration order (`mid` and `beta`
        // land *after* `zeta`, so this is not one global sort). That composite
        // is the order that reaches a def hash.
        assert_eq!(pkgs, vec!["@heph/fs", "alpha", "zeta", "mid", "beta"]);
        Ok(())
    }

    #[test]
    fn package_returns_pkg() {
        assert_eq!(
            narrowing_prefix(&Matcher::Package(pkg("foo/bar"))),
            pkg("foo/bar")
        );
    }

    #[test]
    fn package_prefix_returns_pkg() {
        assert_eq!(
            narrowing_prefix(&Matcher::PackagePrefix(pkg("foo"))),
            pkg("foo")
        );
    }

    #[test]
    fn addr_returns_its_package() {
        let a = hmodel::htaddr::Addr::new(pkg("foo/bar"), "baz".to_string(), Default::default());
        assert_eq!(narrowing_prefix(&Matcher::Addr(a)), pkg("foo/bar"));
    }

    #[test]
    fn label_returns_empty() {
        assert_eq!(narrowing_prefix(&Matcher::Label("l".into())), pkg(""));
    }

    #[test]
    fn and_picks_package_arm() {
        let m = Matcher::And(vec![
            Matcher::Package(pkg("foo/bar")),
            Matcher::Label("go_test_data".into()),
        ]);
        assert_eq!(narrowing_prefix(&m), pkg("foo/bar"));
    }

    #[test]
    fn and_picks_longest_prefix_arm() {
        let m = Matcher::And(vec![
            Matcher::PackagePrefix(pkg("foo")),
            Matcher::PackagePrefix(pkg("foo/bar")),
            Matcher::Label("l".into()),
        ]);
        assert_eq!(narrowing_prefix(&m), pkg("foo/bar"));
    }

    #[test]
    fn and_with_no_pkg_arms_returns_empty() {
        let m = Matcher::And(vec![Matcher::Label("a".into()), Matcher::Label("b".into())]);
        assert_eq!(narrowing_prefix(&m), pkg(""));
    }

    #[test]
    fn or_returns_empty() {
        // Or arms could each match different prefixes; can't narrow safely.
        let m = Matcher::Or(vec![
            Matcher::Package(pkg("foo")),
            Matcher::Package(pkg("bar")),
        ]);
        assert_eq!(narrowing_prefix(&m), pkg(""));
    }

    #[test]
    fn not_returns_empty() {
        let m = Matcher::Not(Box::new(Matcher::Package(pkg("foo"))));
        assert_eq!(narrowing_prefix(&m), pkg(""));
    }

    #[test]
    fn nested_and_descends() {
        let m = Matcher::And(vec![
            Matcher::Label("l".into()),
            Matcher::And(vec![
                Matcher::Label("m".into()),
                Matcher::Package(pkg("deep/inner")),
            ]),
        ]);
        assert_eq!(narrowing_prefix(&m), pkg("deep/inner"));
    }
}
