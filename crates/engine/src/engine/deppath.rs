use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use enclose::enclose;
use futures::{StreamExt, TryStreamExt, stream};
use hmodel::htaddr::Addr;

use crate::engine::Engine;
use crate::engine::request_state::RequestState;

impl Engine {
    /// The shortest chain of targets leading from `from` to `to`, following
    /// declared direct dependency edges — `from` first, `to` last, one hop per
    /// element. Returns `None` when `to` is not reachable from `from`.
    ///
    /// Edges are the *direct* inputs of each target (`get_direct_def`), so the
    /// hops are the ones actually written in the build files; transitive
    /// resolution would collapse intermediate targets into a single edge and
    /// hide them.
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
                    let deps = direct_deps(engine, rs, &node).await?;
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

/// A target's direct dependency addrs, deduplicated, in declared order. Several
/// inputs may reference the same target through different outputs; as a graph
/// edge that is one dep.
async fn direct_deps(
    engine: Arc<Engine>,
    rs: Arc<RequestState>,
    addr: &Addr,
) -> anyhow::Result<Vec<Addr>> {
    let def = engine.get_direct_def(rs, addr).await?;
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
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    /// `dep_path` between two addr strings, formatted for comparison.
    async fn path(
        engine: Arc<Engine>,
        from: &str,
        to: &str,
    ) -> anyhow::Result<Option<Vec<String>>> {
        let rs = engine.new_state();
        let chain = engine
            .dep_path(rs, parse_addr(from)?, parse_addr(to)?)
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
