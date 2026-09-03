use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use enclose::enclose;
use futures::future::Either;
use futures::{StreamExt, TryStreamExt, stream};
use hmodel::htaddr::Addr;

use crate::engine::Engine;
use crate::engine::request_state::RequestState;

impl Engine {
    /// The shortest chain of targets linking `a` and `b`, whichever way round
    /// the edges run — `a → … → b` if `b` is a dependency of `a`, else
    /// `b → … → a`, else `None`. The returned chain always reads from the
    /// dependent to the dependency, so the caller need not know which of the two
    /// is upstream.
    ///
    /// Both directions are searched concurrently and share the request's def
    /// memoizer, so a target resolved by one walk is free for the other. The
    /// first walk to find a chain wins and the other is dropped — a DAG cannot
    /// link the pair both ways, so there is nothing to arbitrate. A direction
    /// that fails to resolve some target does not sink the other's answer, but
    /// it does outrank a bare "not connected": a walk that broke never proved
    /// the pair unconnected.
    pub async fn dep_path_between(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        a: Addr,
        b: Addr,
        no_transitive: bool,
    ) -> anyhow::Result<Option<Vec<Addr>>> {
        let forward = Arc::clone(&self).dep_path(rs.clone(), a.clone(), b.clone(), no_transitive);
        let backward = self.dep_path(rs, b, a, no_transitive);
        futures::pin_mut!(forward, backward);

        let (first, rest) = match futures::future::select(forward, backward).await {
            Either::Left((first, rest)) => (first, Either::Right(rest)),
            Either::Right((first, rest)) => (first, Either::Left(rest)),
        };
        // The chain answers the question; the other direction is dropped
        // wherever its walk had got to.
        if let Ok(Some(chain)) = first {
            return Ok(Some(chain));
        }

        let second = match rest {
            Either::Left(f) => f.await,
            Either::Right(f) => f.await,
        };
        match (first, second) {
            (Ok(Some(chain)), _) | (_, Ok(Some(chain))) => Ok(Some(chain)),
            (Err(e), _) | (_, Err(e)) => Err(e),
            _ => Ok(None),
        }
    }

    /// The shortest chain of targets leading from `from` to `to` — `from` first,
    /// `to` last, one hop per element. Returns `None` when `to` is not reachable
    /// from `from`.
    ///
    /// Edges are a target's resolved inputs (`get_def`), so a dep pulled in by
    /// another dep's transitive sandbox is a hop like any other — the graph the
    /// engine actually builds, not just what the build files spell out. With
    /// `no_transitive`, only the directly declared inputs are followed
    /// (`get_direct_def`), which can leave targets that are linked only through
    /// a transitive contribution looking unconnected.
    ///
    /// The walk is breadth-first: a whole level is resolved before the next one
    /// starts, so the first chain reaching `to` has the fewest hops. Within a
    /// level the resolutions run concurrently but are consumed in frontier
    /// order, keeping the chosen chain deterministic when several are equally
    /// short.
    pub async fn dep_path(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        from: Addr,
        to: Addr,
        no_transitive: bool,
    ) -> anyhow::Result<Option<Vec<Addr>>> {
        if from == to {
            return Ok(Some(vec![from]));
        }

        // Cap in-flight def resolutions; the engine's own semaphores gate the
        // real work, this just bounds the level's orchestration set.
        let concurrency = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .saturating_mul(2);

        // `parents[dep] = node` — the node that first reached `dep`, i.e. the
        // predecessor on a shortest chain from `from`.
        let mut parents: HashMap<Addr, Addr> = HashMap::new();
        let mut seen: HashSet<Addr> = HashSet::from([from.clone()]);
        let mut frontier = vec![from.clone()];

        while !frontier.is_empty() {
            // Each future carries its node through so the level stays paired
            // with the addr it was resolved from without re-cloning the frontier.
            let level: Vec<(Addr, Vec<Addr>)> = stream::iter(frontier.into_iter().map(|node| {
                enclose!((self => engine, rs) async move {
                    let deps = deps_of(engine, rs, &node, no_transitive).await?;
                    anyhow::Ok((node, deps))
                })
            }))
            // `buffered` (not `buffer_unordered`) yields in frontier order, so
            // ties between equally short chains resolve the same way each run.
            .buffered(concurrency)
            .try_collect()
            .await?;

            let mut next = Vec::new();
            for (node, deps) in level {
                for dep in deps {
                    if !seen.insert(dep.clone()) {
                        continue;
                    }
                    parents.insert(dep.clone(), node.clone());
                    if dep == to {
                        return Ok(Some(chain_to(&parents, &from, &to)));
                    }
                    next.push(dep);
                }
            }
            frontier = next;
        }

        Ok(None)
    }
}

/// A target's dependency addrs, deduplicated, in declared order. Several inputs
/// may reference the same target through different outputs; as a graph edge that
/// is one dep. `no_transitive` drops the inputs contributed by the deps'
/// transitive sandboxes, leaving only what the target declares itself.
async fn deps_of(
    engine: Arc<Engine>,
    rs: Arc<RequestState>,
    addr: &Addr,
    no_transitive: bool,
) -> anyhow::Result<Vec<Addr>> {
    let def = if no_transitive {
        engine.get_direct_def(rs, addr).await?
    } else {
        engine.get_def(rs, addr).await?
    };
    let inputs = &def.target_def.inputs;

    let mut seen = HashSet::with_capacity(inputs.len());
    Ok(inputs
        .iter()
        .map(|input| &input.r#ref.r#ref)
        .filter(|dep| seen.insert((*dep).clone()))
        .cloned()
        .collect())
}

/// Rebuild the chain `from → … → to` by walking `parents` backwards from `to`.
fn chain_to(parents: &HashMap<Addr, Addr>, from: &Addr, to: &Addr) -> Vec<Addr> {
    let mut chain = vec![to.clone()];
    let mut cur = to;
    while cur != from {
        // Every entry in `parents` was reached from `from`, so the walk
        // terminates there.
        let Some(parent) = parents.get(cur) else {
            break;
        };
        chain.push(parent.clone());
        cur = parent;
    }
    chain.reverse();
    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::driver::TargetAddr;
    use crate::engine::driver::sandbox::{Dep, Sandbox};
    use hbuiltins::pluginstatictarget;
    use hmodel::htaddr::parse_addr;
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
        engine.register_managed_driver(|_| {
            Box::new(hplugin_exec::pluginexec::Driver::new_exec().with_host_path())
        })?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    /// A target contributing `dep` to its dependents' defs through transitives —
    /// an edge that only exists once transitives are applied.
    fn target_with_transitive(addr: &str, dep: &str) -> anyhow::Result<pluginstatictarget::Target> {
        let mut transitive = Sandbox::default();
        transitive.push_dep(Dep {
            r#ref: TargetAddr {
                r#ref: parse_addr(dep)?,
                ..Default::default()
            },
            group: String::new(),
            runtime: true,
            hash: true,
            id: "t".to_string(),
            ..Default::default()
        });
        Ok(pluginstatictarget::Target {
            transitive,
            ..target(addr, &[])
        })
    }

    /// `dep_path` between two addr strings, formatted for comparison.
    async fn path(
        engine: Arc<Engine>,
        from: &str,
        to: &str,
    ) -> anyhow::Result<Option<Vec<String>>> {
        path_opts(engine, from, to, false).await
    }

    async fn path_opts(
        engine: Arc<Engine>,
        from: &str,
        to: &str,
        no_transitive: bool,
    ) -> anyhow::Result<Option<Vec<String>>> {
        let rs = engine.new_state();
        let chain = engine
            .dep_path(rs, parse_addr(from)?, parse_addr(to)?, no_transitive)
            .await?;
        Ok(chain.map(|c| c.iter().map(|a| a.format()).collect()))
    }

    #[tokio::test]
    async fn walks_the_intermediate_hops() -> anyhow::Result<()> {
        let (engine, _root) = make_engine(vec![
            target("//app:bin", &["//lib:mid"]),
            target("//lib:mid", &["//lib:leaf"]),
            target("//lib:leaf", &[]),
        ])?;

        assert_eq!(
            path(engine, "//app:bin", "//lib:leaf").await?,
            Some(vec![
                "//app:bin".to_string(),
                "//lib:mid".to_string(),
                "//lib:leaf".to_string(),
            ])
        );
        Ok(())
    }

    #[tokio::test]
    async fn unconnected_targets_have_no_path() -> anyhow::Result<()> {
        // //other:c is reachable from nothing the walk visits.
        let (engine, _root) = make_engine(vec![
            target("//app:bin", &["//lib:leaf"]),
            target("//lib:leaf", &[]),
            target("//other:c", &[]),
        ])?;

        assert_eq!(path(engine, "//app:bin", "//other:c").await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn edges_are_directed() -> anyhow::Result<()> {
        // bin → leaf exists; leaf → bin must not.
        let (engine, _root) = make_engine(vec![
            target("//app:bin", &["//lib:leaf"]),
            target("//lib:leaf", &[]),
        ])?;

        assert_eq!(path(engine, "//lib:leaf", "//app:bin").await?, None);
        Ok(())
    }

    /// `dep_path_between` over two addr strings, formatted for comparison.
    async fn between(engine: Arc<Engine>, a: &str, b: &str) -> anyhow::Result<Option<Vec<String>>> {
        let rs = engine.new_state();
        let chain = engine
            .dep_path_between(rs, parse_addr(a)?, parse_addr(b)?, false)
            .await?;
        Ok(chain.map(|c| c.iter().map(|a| a.format()).collect()))
    }

    #[tokio::test]
    async fn argument_order_does_not_matter() -> anyhow::Result<()> {
        // The chain always reads dependent → dependency, whichever way the pair
        // is given, so the caller need not know which end is upstream.
        let (engine, _root) = make_engine(vec![
            target("//app:bin", &["//lib:mid"]),
            target("//lib:mid", &["//lib:leaf"]),
            target("//lib:leaf", &[]),
        ])?;
        let expected = Some(vec![
            "//app:bin".to_string(),
            "//lib:mid".to_string(),
            "//lib:leaf".to_string(),
        ]);

        assert_eq!(
            between(Arc::clone(&engine), "//app:bin", "//lib:leaf").await?,
            expected
        );
        assert_eq!(between(engine, "//lib:leaf", "//app:bin").await?, expected);
        Ok(())
    }

    #[tokio::test]
    async fn a_broken_walk_does_not_sink_the_other_direction() -> anyhow::Result<()> {
        // Walking up from //lib:leaf breaks on its missing dep, but the walk down
        // from //app:bin reaches leaf without ever resolving it — the chain stands.
        let (engine, _root) = make_engine(vec![
            target("//app:bin", &["//lib:leaf"]),
            target("//lib:leaf", &["//nope:missing"]),
        ])?;

        assert_eq!(
            between(engine, "//lib:leaf", "//app:bin").await?,
            Some(vec!["//app:bin".to_string(), "//lib:leaf".to_string()])
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_broken_walk_outranks_not_connected() -> anyhow::Result<()> {
        // Neither direction finds a chain, but one of them broke on the way — that
        // is not proof the pair is unconnected, so the error surfaces.
        let (engine, _root) = make_engine(vec![
            target("//lib:leaf", &["//nope:missing"]),
            target("//other:c", &[]),
        ])?;

        let err = between(engine, "//lib:leaf", "//other:c")
            .await
            .expect_err("expected the broken walk to surface");
        assert!(format!("{err:#}").contains("//nope:missing"), "{err:#}");
        Ok(())
    }

    #[tokio::test]
    async fn unconnected_targets_have_no_path_either_way() -> anyhow::Result<()> {
        // Both walks must exhaust before `None` is the answer.
        let (engine, _root) = make_engine(vec![
            target("//app:bin", &["//lib:leaf"]),
            target("//lib:leaf", &[]),
            target("//other:c", &["//other:d"]),
            target("//other:d", &[]),
        ])?;

        assert_eq!(between(engine, "//app:bin", "//other:c").await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn picks_the_shortest_chain() -> anyhow::Result<()> {
        // bin reaches leaf directly and through a 2-hop detour; the direct edge
        // wins because breadth-first exhausts the nearer level first.
        let (engine, _root) = make_engine(vec![
            target("//app:bin", &["//lib:long", "//lib:leaf"]),
            target("//lib:long", &["//lib:mid"]),
            target("//lib:mid", &["//lib:leaf"]),
            target("//lib:leaf", &[]),
        ])?;

        assert_eq!(
            path(engine, "//app:bin", "//lib:leaf").await?,
            Some(vec!["//app:bin".to_string(), "//lib:leaf".to_string()])
        );
        Ok(())
    }

    #[tokio::test]
    async fn follows_transitively_contributed_edges() -> anyhow::Result<()> {
        // Nothing declares a dep on //lib:extra: //lib:mid only *contributes* it,
        // and applying transitives lands it directly in //app:bin's inputs — so
        // the edge the engine actually builds is bin → extra.
        let (engine, _root) = make_engine(vec![
            target("//app:bin", &["//lib:mid"]),
            target_with_transitive("//lib:mid", "//lib:extra")?,
            target("//lib:extra", &[]),
        ])?;

        assert_eq!(
            path(Arc::clone(&engine), "//app:bin", "//lib:extra").await?,
            Some(vec!["//app:bin".to_string(), "//lib:extra".to_string()])
        );
        // --no-transitive follows only what the build files declare, so the same
        // pair looks unconnected.
        assert_eq!(
            path_opts(engine, "//app:bin", "//lib:extra", true).await?,
            None
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_target_reaches_itself() -> anyhow::Result<()> {
        let (engine, _root) = make_engine(vec![target("//app:bin", &[])])?;

        assert_eq!(
            path(engine, "//app:bin", "//app:bin").await?,
            Some(vec!["//app:bin".to_string()])
        );
        Ok(())
    }

    #[tokio::test]
    async fn diamond_reports_one_chain_of_each_hop() -> anyhow::Result<()> {
        // Two equal-length chains bin→{l,r}→leaf: the first frontier entry wins,
        // deterministically (deps are consumed in declared order).
        let (engine, _root) = make_engine(vec![
            target("//app:bin", &["//lib:l", "//lib:r"]),
            target("//lib:l", &["//lib:leaf"]),
            target("//lib:r", &["//lib:leaf"]),
            target("//lib:leaf", &[]),
        ])?;

        assert_eq!(
            path(engine, "//app:bin", "//lib:leaf").await?,
            Some(vec![
                "//app:bin".to_string(),
                "//lib:l".to_string(),
                "//lib:leaf".to_string(),
            ])
        );
        Ok(())
    }
}
