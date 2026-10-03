use std::sync::Arc;

use enclose::enclose;
use futures::{Stream, TryStreamExt};
use hcore::hmemoizer::downcast_chain_ref;
use hmodel::htaddr::Addr;
use hmodel::htmatcher::Matcher;

use crate::engine::Engine;
use crate::engine::discovery::{Discovery, Gaps, Stage, is_cancellation, provider_of};
use crate::engine::error::CycleError;
use crate::engine::query::skip_unresolvable;
use crate::engine::request_state::RequestState;

impl Engine {
    /// Stream every target in `scope` that declares `target` as a direct
    /// dependency — the reverse of `get_direct_def`'s inputs. This answers
    /// "which targets reference this one", the precise inverse of the build
    /// graph edge.
    ///
    /// Candidates are consumed straight off the query stream and resolved
    /// concurrently (bounded by `concurrency`); dependents are yielded as they
    /// are found, so nothing is ever materialized in bulk — the whole graph can
    /// be many thousands of addrs. Output order follows completion order, not a
    /// stable sort; a caller that needs determinism sorts the collected result.
    pub fn revdeps<'a>(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        target: Addr,
        scope: &'a Matcher,
        discovery: Discovery,
    ) -> impl Stream<Item = anyhow::Result<Addr>> + 'a {
        // Cap in-flight def resolutions; the engine's own semaphores gate the
        // real work, this just bounds the orchestration set held off the stream.
        let concurrency = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .saturating_mul(2);
        // The walk itself rarely resolves anything for a package scope; the
        // candidate's def is resolved here, so this is where most of a revdeps
        // run's skips happen.
        let gaps = discovery.gaps().cloned();

        Arc::clone(&self)
            .query(rs.clone(), scope, discovery)
            .map_err(|e| e.context("listing candidate targets"))
            .map_ok(move |candidate| {
                enclose!((self => engine, rs, target, gaps) async move {
                    resolve_dependent(&engine, &rs, candidate, &target, gaps.as_deref()).await
                })
            })
            .try_buffer_unordered(concurrency)
            // Drop the non-dependents (`None`); real errors still propagate.
            .try_filter_map(|dependent| async move { Ok(dependent) })
    }
}

/// Resolve one candidate: is `target` among its direct inputs? Returns the
/// candidate's addr when it is, `None` when it isn't or when the candidate can't
/// be resolved standalone — or, with `gaps`, when it could not be resolved at
/// all, which is then recorded rather than failing the whole run.
async fn resolve_dependent(
    engine: &Arc<Engine>,
    rs: &Arc<RequestState>,
    candidate: Addr,
    target: &Addr,
    gaps: Option<&Gaps>,
) -> anyhow::Result<Option<Addr>> {
    let res = Arc::clone(engine)
        .get_direct_def(rs.clone(), &candidate)
        .await;
    // On top of the candidate that is not a target at all (`skip_unresolvable`):
    // a candidate that cycles back to the query target cannot be a resolvable
    // dependent either. Skip it the way the query resolver does.
    let res = match res {
        Err(e) if downcast_chain_ref::<CycleError>(&e).is_some() => return Ok(None),
        res => res,
    };
    let def = match (skip_unresolvable(&candidate, res), gaps) {
        (Ok(Some(def)), _) => def,
        (Ok(None), _) => return Ok(None),
        (Err(e), Some(gaps)) if !is_cancellation(rs, &e) => {
            gaps.record(Stage::Def, provider_of(&e), candidate.format(), &e);
            return Ok(None);
        }
        (Err(e), _) => return Err(e),
    };
    let uses = def
        .target_def
        .inputs
        .iter()
        .any(|input| input.r#ref.r#ref == *target);
    Ok(uses.then_some(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use hbuiltins::pluginstatictarget;
    use hmodel::htaddr::parse_addr;
    use hmodel::htpkg::PkgBuf;
    use std::collections::HashMap;

    /// A static `exec` target depending on `deps` (default group) — addr strings.
    fn target(addr: &str, deps: &[&str]) -> pluginstatictarget::Target {
        let mut dep_map = HashMap::new();
        if !deps.is_empty() {
            dep_map.insert(
                String::new(),
                deps.iter().map(|d| (*d).to_string()).collect(),
            );
        }
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some("true".to_string()),
            deps: dep_map,
            ..Default::default()
        }
    }

    fn make_engine(
        targets: Vec<pluginstatictarget::Target>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    fn all() -> Matcher {
        Matcher::PackagePrefix(PkgBuf::from(""))
    }

    /// Drain the revdeps stream and return the dependents' formatted addrs,
    /// sorted.
    async fn collect_sorted(
        engine: Arc<Engine>,
        rs: Arc<RequestState>,
        target: Addr,
        scope: Matcher,
    ) -> anyhow::Result<Vec<String>> {
        let dependents: Vec<Addr> = engine
            .revdeps(rs, target, &scope, Discovery::Complete)
            .try_collect()
            .await?;
        let mut dependents: Vec<String> = dependents.iter().map(|a| a.format()).collect();
        dependents.sort();
        Ok(dependents)
    }

    #[tokio::test]
    async fn finds_direct_users() -> anyhow::Result<()> {
        // //lib:core is used by //app:a and //app:b, not //other:c.
        let (engine, _root) = make_engine(vec![
            target("//lib:core", &[]),
            target("//app:a", &["//lib:core"]),
            target("//app:b", &["//lib:core"]),
            target("//other:c", &[]),
        ])?;
        let rs = engine.new_state();
        let core = parse_addr("//lib:core")?;

        let users = collect_sorted(Arc::clone(&engine), rs, core, all()).await?;
        assert_eq!(users, vec!["//app:a".to_string(), "//app:b".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn scope_restricts_search() -> anyhow::Result<()> {
        let (engine, _root) = make_engine(vec![
            target("//lib:core", &[]),
            target("//app:a", &["//lib:core"]),
            target("//tool:t", &["//lib:core"]),
        ])?;
        let rs = engine.new_state();
        let core = parse_addr("//lib:core")?;

        // Only search within //app — //tool:t must not appear.
        let scope = Matcher::PackagePrefix(PkgBuf::from("app"));
        let users = collect_sorted(Arc::clone(&engine), rs, core, scope).await?;
        assert_eq!(users, vec!["//app:a".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn no_users_is_empty() -> anyhow::Result<()> {
        let (engine, _root) = make_engine(vec![target("//lib:core", &[]), target("//app:a", &[])])?;
        let rs = engine.new_state();
        let core = parse_addr("//lib:core")?;

        let users = collect_sorted(Arc::clone(&engine), rs, core, all()).await?;
        assert!(users.is_empty());
        Ok(())
    }

    /// A package scope decides every candidate from its address, so revdeps
    /// meets an unresolvable candidate when it resolves the def, not in the
    /// walk. Kept going, that candidate is recorded and the real dependents
    /// are still found.
    #[tokio::test]
    async fn keep_going_records_a_candidate_whose_def_fails() -> anyhow::Result<()> {
        let (mut engine, _root) = make_engine(vec![
            target("//lib:core", &[]),
            target("//app:a", &["//lib:core"]),
        ])?;
        Arc::get_mut(&mut engine)
            .expect("unshared")
            .register_provider(|_| {
                Box::new(crate::engine::discovery::test_support::Unresolvable {
                    addrs: vec![parse_addr("//go/broken:x").expect("addr")],
                })
            })?;
        let core = parse_addr("//lib:core")?;
        let scope = all();

        let gaps = Gaps::new("//...");
        let users: Vec<Addr> = Arc::clone(&engine)
            .revdeps(
                engine.new_state(),
                core.clone(),
                &scope,
                Discovery::KeepGoing(gaps.clone()),
            )
            .try_collect()
            .await?;
        assert_eq!(users, [parse_addr("//app:a")?]);
        let report = gaps.report();
        assert_eq!(report.skipped, 1, "{report:?}");
        assert_eq!(report.groups[0].examples[0].scope, "//go/broken:x");

        let complete: anyhow::Result<Vec<Addr>> = Arc::clone(&engine)
            .revdeps(engine.new_state(), core, &scope, Discovery::Complete)
            .try_collect()
            .await;
        assert!(complete.is_err(), "a complete run still fails on it");
        Ok(())
    }
}
