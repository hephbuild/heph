//! A provider for tests that fails on demand at each stage of discovery.
//!
//! Tests about discovery, force and failure handling need a provider that
//! misbehaves in one specific way — a `get` that builds another target first,
//! a package that probes to an error, a listing that never ends. Before this,
//! each test file hand-rolled its own `Provider` impl for that; this is the one
//! to reach for, so a new failure mode is one field here rather than another
//! eighty-line impl.
//!
//! Built for the engine's own tests, and for the in-process e2e suite through
//! the `test-support` feature. Never compiled into a release binary.

use crate::engine::discovery::Stage;
use crate::engine::error::{CancelledError, MultiError};
use crate::engine::provider::{
    ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
    ListPackagesRequest, ListRequest, ListResponse, ListedFacts, ProbeRequest, ProbeResponse,
    Provider, SourceIndex, SpecSource, State,
};
use futures::future::BoxFuture;
use hbuiltins::pluginstatictarget;
use hcore::hasync::Cancellable;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// How a [`FaultProvider`] misbehaves. Everything defaults to healthy.
#[derive(Debug, Default)]
pub struct Faults {
    /// The provider's registered name. Default `faulty`.
    pub name: Option<&'static str>,
    /// Packages it resolves but never lists, from `list_packages` or `list`:
    /// a get-only package, like `@heph/go/...`.
    pub hidden_packages: Vec<String>,
    /// Targets it resolves but never lists, inside a listed package: go's
    /// `_lint-analyze`.
    pub unlisted: Vec<Addr>,
    /// Targets it lists (in addition to its static ones) and fails to `get`:
    /// a go package whose `go list` fails.
    pub broken: Vec<Addr>,
    /// Static targets whose `get` fails.
    pub fail_get: Vec<Addr>,
    /// `get` of `.0` builds `.1` through the executor first, failing if that
    /// does: the go provider's `_golist`.
    pub builds: Vec<(Addr, Addr)>,
    /// Packages whose probe fails.
    pub fail_probe: Vec<String>,
    /// Packages whose `list` fails.
    pub fail_list: Vec<String>,
    /// Packages whose `list` panics.
    pub panic_list: Vec<String>,
    /// `list_packages` fails.
    pub fail_list_packages: bool,
    /// `list` of this package sleeps this long first.
    pub slow_list: Option<(String, Duration)>,
    /// Signalled when the slow `list` starts. Every failing `get` and
    /// `fail_list` `list` waits for it (or for the request to be cancelled),
    /// so the slow package is provably in flight when the walk meets its first
    /// error, however loaded the machine is. Set it only with `slow_list`
    /// naming a package the walk lists, or the failing calls wait until
    /// cancelled.
    pub gate: Option<Arc<tokio::sync::Notify>>,
    /// At this stage, for this package or addr (ignored for
    /// [`Stage::Packages`]): signal the `Notify`, wait for the request to be
    /// cancelled, then fail the way a call with two dependencies in flight
    /// does — a `MultiError` of two cancellations.
    pub cancel_at: Option<(Stage, String, Arc<tokio::sync::Notify>)>,
    /// `list` reports no facts, like a plugin built before ABI 0.13, so a
    /// selector that reads one resolves every candidate's spec to decide.
    pub facts_unknown: bool,
    /// Labels its `list` reports for these static targets instead of their
    /// real ones: a listing that lies about what `get` will return.
    pub listed_labels: Vec<(Addr, Vec<String>)>,
    /// The facts `list` reports for an entry, as a function of its addr and
    /// the states the `list` call received; `None` keeps what it would report
    /// otherwise. Applied last, to every entry.
    pub listed_facts: Option<FactsFn>,
    /// Provider states its probe of each package declares, inherited down the
    /// tree like `provider_state(...)`: what `list` and `get` receive.
    pub states: Vec<(
        String,
        std::collections::HashMap<String, hcore::htvalue::Value>,
    )>,
    /// Targets it lists with these labels and that `get` says do not exist:
    /// go listing `test` in a package that turns out to have no tests.
    pub vanished: Vec<(Addr, Vec<String>)>,
    /// Every `get` sleeps this long first, so overlapping calls are visible
    /// in [`FaultProvider::max_concurrent_gets`].
    pub slow_get: Option<Duration>,
    /// A `get` of any of these addrs waits at the barrier before doing
    /// anything else, so it returns only once every one of them is in flight
    /// at the same time: a test of concurrent resolution that cannot pass by
    /// timing. Size the barrier to the addr count; resolving them one at a
    /// time hangs, so wrap the call in a timeout.
    pub rendezvous: Option<(Vec<Addr>, Arc<tokio::sync::Barrier>)>,
    /// The [`SpecSource`] `get` reports for each of these addrs; every other
    /// spec has none, as the static provider reports.
    pub sources: Vec<(Addr, SpecSource)>,
    /// What [`Provider::source_index`] answers for a file; `None` keeps the
    /// trait's default (no index), as a cdylib provider does.
    pub source_index: Option<SourceIndexFn>,
}

/// [`Faults::source_index`]: the index of a file, called as the engine
/// calls [`Provider::source_index`].
#[derive(Clone)]
pub struct SourceIndexFn(pub Arc<SourceIndexFnInner>);

/// The function behind a [`SourceIndexFn`].
pub type SourceIndexFnInner = dyn Fn(&str) -> Option<Arc<dyn SourceIndex>> + Send + Sync;

impl SourceIndexFn {
    pub fn new(f: impl Fn(&str) -> Option<Arc<dyn SourceIndex>> + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }
}

impl std::fmt::Debug for SourceIndexFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SourceIndexFn(..)")
    }
}

/// [`Faults::listed_facts`]: what `list` says about an entry.
#[derive(Clone)]
pub struct FactsFn(pub Arc<FactsFnInner>);

/// The function behind a [`FactsFn`].
pub type FactsFnInner = dyn Fn(&Addr, &[State]) -> Option<ListedFacts> + Send + Sync;

impl FactsFn {
    pub fn new(f: impl Fn(&Addr, &[State]) -> Option<ListedFacts> + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }
}

impl std::fmt::Debug for FactsFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FactsFn(..)")
    }
}

/// The static provider, misbehaving as [`Faults`] says.
pub struct FaultProvider {
    inner: pluginstatictarget::Provider,
    faults: Faults,
    list_packages_calls: Arc<AtomicUsize>,
    list_calls: Arc<AtomicUsize>,
    gets: Arc<GetLog>,
}

/// What reached `get`, readable after the provider is handed to the engine.
#[derive(Debug, Default)]
pub struct GetLog {
    addrs: parking_lot::Mutex<Vec<Addr>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl GetLog {
    /// Every addr `get` was called for, in call order.
    pub fn addrs(&self) -> Vec<Addr> {
        self.addrs.lock().clone()
    }

    /// The most `get`s that were ever in flight at once.
    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }
}

/// Decrements `in_flight` however the `get` ends.
struct InFlight<'a>(&'a GetLog);

impl<'a> InFlight<'a> {
    fn enter(log: &'a GetLog, addr: &Addr) -> Self {
        log.addrs.lock().push(addr.clone());
        let now = log.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        log.max_in_flight.fetch_max(now, Ordering::SeqCst);
        Self(log)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl FaultProvider {
    pub fn new(targets: Vec<pluginstatictarget::Target>, faults: Faults) -> anyhow::Result<Self> {
        Ok(Self {
            inner: pluginstatictarget::Provider::new(targets)?,
            faults,
            list_packages_calls: Arc::new(AtomicUsize::new(0)),
            list_calls: Arc::new(AtomicUsize::new(0)),
            gets: Arc::default(),
        })
    }

    /// How many times `list` was called, readable after the provider has been
    /// handed to the engine.
    pub fn list_calls(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.list_calls)
    }

    /// What reached `get`, readable after the provider has been handed to
    /// the engine.
    pub fn gets(&self) -> Arc<GetLog> {
        Arc::clone(&self.gets)
    }

    /// A provider named `broken` that lists `addrs` and fails to resolve every
    /// one of them, and has nothing else.
    pub fn unresolvable(addrs: Vec<Addr>) -> anyhow::Result<Self> {
        Self::new(
            vec![],
            Faults {
                name: Some("broken"),
                broken: addrs,
                ..Default::default()
            },
        )
    }

    /// How many times `list_packages` was called, readable after the provider
    /// has been handed to the engine.
    pub fn list_packages_calls(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.list_packages_calls)
    }

    fn hidden(&self, pkg: &str) -> bool {
        self.faults.hidden_packages.iter().any(|h| h == pkg)
    }

    /// The cancellation failure, if `cancel_at` names this call.
    async fn cancelled(
        &self,
        stage: Stage,
        scope: &str,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Option<anyhow::Error> {
        let (at, target, started) = self.faults.cancel_at.as_ref()?;
        if *at != stage || (stage != Stage::Packages && target != scope) {
            return None;
        }
        started.notify_one();
        ctoken.cancelled().await;
        Some(
            MultiError(vec![
                anyhow::Error::new(CancelledError),
                anyhow::Error::new(CancelledError),
            ])
            .into(),
        )
    }
}

impl Provider for FaultProvider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: self.faults.name.unwrap_or("faulty").to_string(),
        })
    }

    fn list<'a>(
        &'a self,
        req: ListRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>
    {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let pkg = req.package.as_str().to_string();
            let states = req.states.clone();
            if let Some(e) = self.cancelled(Stage::List, &pkg, ctoken).await {
                return Err(e);
            }
            if let Some((slow, d)) = &self.faults.slow_list
                && *slow == pkg
            {
                if let Some(gate) = &self.faults.gate {
                    gate.notify_one();
                }
                tokio::time::sleep(*d).await;
            }
            #[expect(
                clippy::panic,
                reason = "`panic_list` exists to make a package task panic"
            )]
            if self.faults.panic_list.contains(&pkg) {
                panic!("list blew up in {pkg}");
            }
            if self.faults.fail_list.contains(&pkg) {
                if let Some(gate) = &self.faults.gate {
                    tokio::select! {
                        // Pass the permit on, as a failing `get` does.
                        () = gate.notified() => gate.notify_one(),
                        () = ctoken.cancelled() => {}
                    }
                }
                anyhow::bail!("evaluating {pkg}/BUILD: syntax error");
            }
            if self.hidden(&pkg) {
                return Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>);
            }
            let broken: Vec<_> = self
                .faults
                .broken
                .iter()
                .filter(|a| a.package == req.package)
                .map(|a| Ok(ListResponse::addr_only(a.clone())))
                .collect();
            let vanished: Vec<_> = self
                .faults
                .vanished
                .iter()
                .filter(|(a, _)| a.package == req.package)
                .map(|(a, labels)| {
                    Ok(ListResponse::with_facts(
                        a.clone(),
                        ListedFacts::default().with_labels(labels.iter().cloned()),
                    ))
                })
                .collect();
            let listed: Vec<_> = self
                .inner
                .list(req, ctoken)
                .await?
                .filter(|res| !matches!(res, Ok(t) if self.faults.unlisted.contains(&t.addr)))
                .map(|res| {
                    res.map(|t| {
                        match self.faults.listed_labels.iter().find(|(a, _)| *a == t.addr) {
                            Some((_, lie)) => ListResponse::with_facts(
                                t.addr,
                                t.facts.with_labels(lie.iter().cloned()),
                            ),
                            None if self.faults.facts_unknown => ListResponse::addr_only(t.addr),
                            None => t,
                        }
                    })
                })
                .collect();
            let facts = self.faults.listed_facts.clone();
            Ok(
                Box::new(
                    broken
                        .into_iter()
                        .chain(vanished)
                        .chain(listed)
                        .map(move |res| match &facts {
                            Some(FactsFn(f)) => res.map(|t| match f(&t.addr, &states) {
                                Some(facts) => ListResponse::with_facts(t.addr, facts),
                                None => t,
                            }),
                            None => res,
                        }),
                ) as Box<dyn Iterator<Item = _> + Send>,
            )
        })
    }

    fn list_packages<'a>(
        &'a self,
        req: ListPackagesRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    > {
        self.list_packages_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if let Some(e) = self.cancelled(Stage::Packages, "", ctoken).await {
                return Err(e);
            }
            if self.faults.fail_list_packages {
                anyhow::bail!("walking the workspace: permission denied");
            }
            let broken: Vec<_> = self
                .faults
                .broken
                .iter()
                .chain(self.faults.vanished.iter().map(|(a, _)| a))
                .map(|a| {
                    Ok(ListPackageResponse {
                        pkg: a.package.clone(),
                    })
                })
                .collect();
            let listed: Vec<_> = self
                .inner
                .list_packages(req, ctoken)
                .await?
                .filter(|res| !matches!(res, Ok(p) if self.hidden(p.pkg.as_str())))
                .collect();
            Ok(Box::new(broken.into_iter().chain(listed)) as Box<dyn Iterator<Item = _> + Send>)
        })
    }

    fn get<'a>(
        &'a self,
        req: GetRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
        Box::pin(async move {
            let _in_flight = InFlight::enter(&self.gets, &req.addr);
            if let Some((addrs, barrier)) = &self.faults.rendezvous
                && addrs.contains(&req.addr)
            {
                barrier.wait().await;
            }
            if let Some(d) = self.faults.slow_get {
                tokio::time::sleep(d).await;
            }
            if let Some(e) = self
                .cancelled(Stage::Spec, &req.addr.format(), ctoken)
                .await
            {
                return Err(GetError::Other(e));
            }
            if self.faults.vanished.iter().any(|(a, _)| *a == req.addr) {
                return Err(GetError::NotFound);
            }
            if self.faults.broken.contains(&req.addr) || self.faults.fail_get.contains(&req.addr) {
                if let Some(gate) = &self.faults.gate {
                    tokio::select! {
                        // Pass the permit on: `notify_one` stores one, and a
                        // second failing `get` would otherwise wait forever.
                        () = gate.notified() => gate.notify_one(),
                        () = ctoken.cancelled() => {}
                    }
                }
                return Err(GetError::Other(anyhow::anyhow!("go list: exit status 1")));
            }
            for (target, through) in &self.faults.builds {
                if req.addr == *target {
                    req.executor
                        .result(through)
                        .await
                        .map_err(GetError::Other)?;
                }
            }
            let addr = req.addr.clone();
            let mut res = self.inner.get(req, ctoken).await?;
            if let Some((_, source)) = self.faults.sources.iter().find(|(a, _)| *a == addr) {
                res.target_spec.source = Some(source.clone());
            }
            Ok(res)
        })
    }

    fn source_index(&self, file: &str) -> Option<Arc<dyn SourceIndex>> {
        self.faults.source_index.as_ref().and_then(|f| (f.0)(file))
    }

    fn probe<'a>(
        &'a self,
        req: ProbeRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async move {
            let pkg = req.package.as_str().to_string();
            if let Some(e) = self.cancelled(Stage::Probe, &pkg, ctoken).await {
                return Err(e);
            }
            if self.faults.fail_probe.contains(&pkg) {
                anyhow::bail!("BUILD: syntax error");
            }
            let declared: Vec<State> = self
                .faults
                .states
                .iter()
                .filter(|(p, _)| *p == pkg)
                .map(|(p, state)| State {
                    package: PkgBuf::from(p.as_str()),
                    provider: self.faults.name.unwrap_or("faulty").to_string(),
                    state: state.clone(),
                })
                .collect();
            let mut res = self.inner.probe(req, ctoken).await?;
            res.states.extend(declared);
            Ok(res)
        })
    }
}

/// Each fault does what it says — and only for what it names. A fault that
/// silently did nothing would leave every "the walk survives X" test green.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::discovery::Discovery;
    use crate::engine::{Config, Engine, Gaps};
    use futures::TryStreamExt;
    use hmodel::htaddr::parse_addr;
    use hmodel::htmatcher::Matcher;
    use hmodel::htpkg::PkgBuf;

    fn target(addr: &str, run: &str) -> pluginstatictarget::Target {
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some(run.to_string()),
            ..Default::default()
        }
    }

    fn faulty_engine(
        faults: Faults,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir, Arc<AtomicUsize>)> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            parallelism: None,
            ..Config::for_tests(root.path())
        })?;
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        let provider = FaultProvider::new(
            vec![
                target("//a:ok", "true"),
                target("//a:other", "true"),
                target("//b:fails", "false"),
                target("//h:hidden", "true"),
            ],
            faults,
        )?;
        let calls = provider.list_packages_calls();
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root, calls))
    }

    fn addr(a: &str) -> Addr {
        parse_addr(a).expect("addr")
    }

    async fn walk(engine: &Arc<Engine>, discovery: Discovery) -> anyhow::Result<Vec<String>> {
        let all = Matcher::PackagePrefix(PkgBuf::from(""));
        Arc::clone(engine)
            .query(engine.new_state(), &all, discovery)
            .map_ok(|a| a.format())
            .try_collect()
            .await
    }

    async fn spec(engine: &Arc<Engine>, a: &str) -> anyhow::Result<()> {
        Arc::clone(engine)
            .get_spec(engine.new_state(), &addr(a))
            .await
            .map(drop)
    }

    #[tokio::test]
    async fn healthy_by_default() -> anyhow::Result<()> {
        let (engine, _root, calls) = faulty_engine(Faults::default())?;
        let got = walk(&engine, Discovery::Complete).await?;
        assert_eq!(got, ["//a:ok", "//a:other", "//b:fails", "//h:hidden"]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn hidden_and_unlisted_resolve_but_are_not_listed() -> anyhow::Result<()> {
        let (engine, _root, _) = faulty_engine(Faults {
            hidden_packages: vec!["h".to_string()],
            unlisted: vec![addr("//a:other")],
            ..Default::default()
        })?;
        assert_eq!(
            walk(&engine, Discovery::Complete).await?,
            ["//a:ok", "//b:fails"]
        );
        spec(&engine, "//h:hidden").await?;
        spec(&engine, "//a:other").await?;
        Ok(())
    }

    #[tokio::test]
    async fn broken_is_listed_and_fails_to_resolve() -> anyhow::Result<()> {
        let (engine, _root, _) = faulty_engine(Faults {
            broken: vec![addr("//x:broken")],
            ..Default::default()
        })?;
        assert!(
            walk(&engine, Discovery::Complete)
                .await?
                .contains(&"//x:broken".to_string())
        );
        let err = spec(&engine, "//x:broken").await.expect_err("broken");
        assert!(format!("{err:#}").contains("go list"), "{err:#}");
        Ok(())
    }

    #[tokio::test]
    async fn each_failure_hits_only_what_it_names() -> anyhow::Result<()> {
        let (engine, _root, _) = faulty_engine(Faults {
            fail_get: vec![addr("//a:other")],
            ..Default::default()
        })?;
        assert!(spec(&engine, "//a:other").await.is_err());
        spec(&engine, "//a:ok").await?;

        let (engine, _root, _) = faulty_engine(Faults {
            fail_probe: vec!["b".to_string()],
            ..Default::default()
        })?;
        assert!(
            spec(&engine, "//b:fails").await.is_err(),
            "the probe runs under get_spec"
        );
        spec(&engine, "//a:ok").await?;

        let (engine, _root, _) = faulty_engine(Faults {
            fail_list: vec!["b".to_string()],
            ..Default::default()
        })?;
        let gaps = Gaps::new("//...");
        let got = walk(&engine, Discovery::KeepGoing(Arc::clone(&gaps))).await?;
        assert_eq!(got, ["//a:ok", "//a:other", "//h:hidden"]);
        assert_eq!(gaps.report().skipped, 1);

        let (engine, _root, calls) = faulty_engine(Faults {
            fail_list_packages: true,
            ..Default::default()
        })?;
        assert!(walk(&engine, Discovery::Complete).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    /// `builds` really builds the other target: one that fails takes the
    /// `get` down with it.
    #[tokio::test]
    async fn builds_runs_the_other_target_first() -> anyhow::Result<()> {
        let (engine, _root, _) = faulty_engine(Faults {
            builds: vec![(addr("//a:ok"), addr("//b:fails"))],
            ..Default::default()
        })?;
        assert!(spec(&engine, "//a:ok").await.is_err());
        spec(&engine, "//a:other").await?;
        Ok(())
    }

    /// Two failing `get`s behind one gate both get through: the gate passes
    /// its permit on instead of leaving the second waiter parked forever.
    #[tokio::test]
    async fn a_gate_releases_every_failing_get() -> anyhow::Result<()> {
        let (engine, _root, _) = faulty_engine(Faults {
            // So the label walk has to `get` each candidate.
            facts_unknown: true,
            fail_get: vec![addr("//a:ok"), addr("//a:other")],
            gate: Some(Arc::new(tokio::sync::Notify::new())),
            slow_list: Some(("b".to_string(), Duration::from_millis(50))),
            ..Default::default()
        })?;
        let gaps = Gaps::new("//...");
        let label = Matcher::Label("none".to_string());
        let walk = Arc::clone(&engine)
            .query(
                engine.new_state(),
                &label,
                Discovery::KeepGoing(Arc::clone(&gaps)),
            )
            .try_collect::<Vec<Addr>>();
        tokio::time::timeout(Duration::from_secs(20), walk)
            .await
            .expect("a second failing get waited on the gate forever")?;
        assert_eq!(gaps.report().skipped, 2);
        Ok(())
    }

    /// An index that answers every lookup with what it holds.
    struct FixedIndex(crate::engine::provider::Located);

    impl SourceIndex for FixedIndex {
        fn locate(
            &self,
            _source: &SpecSource,
            _name: &str,
            _field: Option<&str>,
        ) -> crate::engine::provider::Located {
            self.0
        }
    }

    fn span(line: u32, col: u32) -> hcore::diagnostic::Span {
        hcore::diagnostic::Span {
            start_line: line,
            start_col: col,
            end_line: line,
            end_col: col + 1,
        }
    }

    fn source(file: &str, line: u32) -> SpecSource {
        SpecSource::checked(Arc::from(file), span(line, 1)).expect("a source")
    }

    /// An engine whose `//a:x` and `//b:x` name a driver no plugin registered,
    /// each declared in its own file (`a/BUILD`, `b/BUILD`) at line 2.
    fn located_engine(
        source_index: Option<SourceIndexFn>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        located_engine_with("nope", source_index, None)
    }

    /// [`located_engine`] with its targets naming `driver`, and `registered`
    /// registered.
    fn located_engine_with(
        driver: &str,
        source_index: Option<SourceIndexFn>,
        registered: Option<Box<dyn crate::engine::driver::Driver>>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config::for_tests(root.path()))?;
        if let Some(d) = registered {
            engine.register_driver(move |_| d)?;
        }
        let nope = |a: &str| pluginstatictarget::Target {
            addr: a.to_string(),
            driver: driver.to_string(),
            ..Default::default()
        };
        let provider = FaultProvider::new(
            vec![nope("//a:x"), nope("//b:x")],
            Faults {
                sources: vec![
                    (addr("//a:x"), source("a/BUILD", 2)),
                    (addr("//b:x"), source("b/BUILD", 2)),
                ],
                source_index,
                ..Default::default()
            },
        )?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    /// `targets`, which must fail, in a fresh request: the request, and the
    /// failures it recorded.
    async fn failures(
        engine: &Arc<Engine>,
        targets: &[&str],
    ) -> (
        Arc<crate::engine::request_state::RequestState>,
        Vec<Arc<hplugin::error::TargetFailure>>,
    ) {
        use crate::engine::result::{OutputMatcher, ResultOptions};
        let rs = engine.new_state_with_events(false, None);
        let m = Matcher::Or(targets.iter().map(|a| Matcher::Addr(addr(a))).collect());
        let res = Arc::clone(engine)
            .result(
                Arc::clone(&rs),
                &m,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await;
        assert!(res.map_or(true, |b| !b.errors.is_empty()), "must fail");
        let failures = rs.take_failures();
        (rs, failures)
    }

    /// `targets`, which must fail, in a fresh request: each failure's culprit
    /// and location, sorted, and how many indexes the request loaded.
    async fn locations(
        engine: &Arc<Engine>,
        targets: &[&str],
    ) -> (Vec<(String, Option<(String, u32, u32)>)>, usize) {
        let (rs, failures) = failures(engine, targets).await;
        let mut got: Vec<_> = failures
            .iter()
            .map(|f| {
                let d = &f.record.diagnostic;
                assert!(
                    !d.message.contains(crate::engine::provider::DeclAt::CHANGED),
                    "nothing changed: {}",
                    d.message
                );
                (
                    f.culprit().to_string(),
                    d.location.as_ref().map(|l| (l.file.clone(), l.line, l.col)),
                )
            })
            .collect();
        got.sort();
        (got, rs.source_index_loads())
    }

    /// A provider that reports where a target was declared but cannot index
    /// the file (a cdylib provider keeps the default): the diagnostic points
    /// at the declaring call, and says nothing of the file having changed.
    #[tokio::test]
    async fn a_source_without_an_index_points_at_the_call() -> anyhow::Result<()> {
        let (engine, _root) = located_engine(None)?;
        let (got, _) = locations(&engine, &["//a:x"]).await;
        assert_eq!(got, [("//a:x".into(), Some(("a/BUILD".into(), 2, 1)))]);
        Ok(())
    }

    /// A message `compact` already cut to the cap, in a file that changed
    /// since evaluation: the message makes room for the marker, so the record
    /// stays within the cap and still says the file changed (CQ r2).
    #[tokio::test]
    async fn a_changed_marker_keeps_the_message_within_its_cap() -> anyhow::Result<()> {
        use hcore::diagnostic::MESSAGE_CAP;
        let index = SourceIndexFn::new(|_| {
            Some(
                Arc::new(FixedIndex(crate::engine::provider::Located::Changed))
                    as Arc<dyn SourceIndex>,
            )
        });
        let (engine, _root) = located_engine_with(&"n".repeat(MESSAGE_CAP), Some(index), None)?;
        let (_, failures) = failures(&engine, &["//a:x"]).await;
        let [f] = failures.as_slice() else {
            panic!("one failure, got {}", failures.len());
        };
        let d = &f.record.diagnostic;
        let marker = format!(" ({})", crate::engine::provider::DeclAt::CHANGED);
        assert!(d.message.len() <= MESSAGE_CAP, "{} bytes", d.message.len());
        assert!(d.message.ends_with(&marker), "{}", d.message);
        assert!(d.message.contains("driver not found: nnn"), "{}", d.message);
        assert!(d.truncated, "the message was cut");
        Ok(())
    }

    /// A driver named `refuse` whose `parse` fails: with a cancellation when
    /// it holds `true`, else with a refusal.
    struct RefusingDriver(bool);

    #[async_trait::async_trait]
    impl crate::engine::driver::Driver for RefusingDriver {
        fn config(
            &self,
            _r: crate::engine::driver::ConfigRequest,
        ) -> anyhow::Result<crate::engine::driver::ConfigResponse> {
            Ok(crate::engine::driver::ConfigResponse {
                name: "refuse".to_string(),
            })
        }
        fn schema(&self) -> crate::engine::driver::DriverSchema {
            crate::engine::driver::DriverSchema::default()
        }
        async fn parse(
            &self,
            _r: crate::engine::driver::ParseRequest,
            _c: &(dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> anyhow::Result<crate::engine::driver::ParseResponse> {
            if self.0 {
                return Err(hplugin::error::CancelledError.into());
            }
            anyhow::bail!("refused")
        }
        async fn apply_transitive(
            &self,
            _r: crate::engine::driver::ApplyTransitiveRequest,
            _c: &(dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> anyhow::Result<crate::engine::driver::ApplyTransitiveResponse> {
            anyhow::bail!("not under test")
        }
        async fn run<'a, 'io>(
            &self,
            _r: crate::engine::driver::RunRequest<'a, 'io>,
            _c: &(dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> anyhow::Result<crate::engine::driver::RunResponse> {
            anyhow::bail!("not under test")
        }
        async fn run_shell<'a, 'io>(
            &self,
            _r: crate::engine::driver::RunRequest<'a, 'io>,
            _c: &(dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> anyhow::Result<crate::engine::driver::RunResponse> {
            anyhow::bail!("not under test")
        }
    }

    /// A parse refused by the driver loads the file's index, for the
    /// diagnostic about it; a cancelled parse does not, since no diagnostic
    /// is shown for it (FQ r2 N-2).
    #[tokio::test]
    async fn a_cancelled_parse_loads_no_index() -> anyhow::Result<()> {
        use crate::engine::result::{OutputMatcher, ResultOptions};
        let mut loads = Vec::new();
        for cancelled in [false, true] {
            let index = SourceIndexFn::new(|_| {
                Some(Arc::new(FixedIndex(crate::engine::provider::Located::Call))
                    as Arc<dyn SourceIndex>)
            });
            let (engine, _root) = located_engine_with(
                "refuse",
                Some(index),
                Some(Box::new(RefusingDriver(cancelled))),
            )?;
            let rs = engine.new_state_with_events(false, None);
            let res = Arc::clone(&engine)
                .result(
                    Arc::clone(&rs),
                    &Matcher::Addr(addr("//a:x")),
                    OutputMatcher::All,
                    &ResultOptions::default(),
                )
                .await;
            // A cancellation is not tallied as a failure.
            assert!(
                cancelled || res.map_or(true, |b| !b.errors.is_empty()),
                "must fail"
            );
            loads.push((cancelled, rs.source_index_loads()));
        }
        assert_eq!(loads, [(false, 1), (true, 0)], "(cancelled, index loads)");
        Ok(())
    }

    /// The engine asks for a file's index on the blocking pool, never on a
    /// reactor worker (the provider may read and parse the file), and two
    /// files' indexes load at the same time: each ask waits until the other
    /// is in flight, which a lock held across the ask would make impossible.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn source_indexes_load_on_the_blocking_pool_in_parallel() -> anyhow::Result<()> {
        let met = Arc::new((parking_lot::Mutex::new(0usize), parking_lot::Condvar::new()));
        let seen = Arc::new(parking_lot::Mutex::new(Vec::<(String, bool, bool)>::new()));
        let index = SourceIndexFn::new({
            let (met, seen) = (Arc::clone(&met), Arc::clone(&seen));
            move |file| {
                let blocking = hcore::blocking::in_blocking_job();
                let (count, cv) = &*met;
                let mut n = count.lock();
                *n += 1;
                cv.notify_all();
                let both = !cv
                    .wait_while_for(&mut n, |n| *n < 2, Duration::from_secs(10))
                    .timed_out();
                drop(n);
                seen.lock().push((file.to_owned(), blocking, both));
                Some(
                    Arc::new(FixedIndex(crate::engine::provider::Located::Field(span(
                        3, 7,
                    )))) as Arc<dyn SourceIndex>,
                )
            }
        });
        let (engine, _root) = located_engine(Some(index))?;
        let (got, loads) = locations(&engine, &["//a:x", "//b:x"]).await;
        assert_eq!(
            got,
            [
                ("//a:x".into(), Some(("a/BUILD".into(), 3, 7))),
                ("//b:x".into(), Some(("b/BUILD".into(), 3, 7))),
            ],
            "each failure points where its file's index put `driver`"
        );
        assert_eq!(loads, 2, "one load per file");
        let mut seen = seen.lock().clone();
        seen.sort();
        assert_eq!(
            seen,
            [
                ("a/BUILD".to_owned(), true, true),
                ("b/BUILD".to_owned(), true, true),
            ],
            "(file, on the blocking pool, met the other file's load)"
        );
        Ok(())
    }
}
