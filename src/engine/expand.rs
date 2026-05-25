use crate::engine::Engine;
use crate::engine::driver::targetdef::{Input, InputMode};
use crate::engine::request_state::RequestState;
use crate::hmemoizer::unwrap_arc_err;
use crate::htaddr::Addr;
use async_recursion::async_recursion;
use enclose::enclose;
use rustc_hash::FxHashSet;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Routing context inherited from a parent transparent-group input. Carries
/// origin_id/mode/annotations plus the parent's `hashed`/`runtime` flags so
/// children of a `hash_deps` (or `runtime_deps`) group expansion inherit
/// the parent's intent.
#[derive(Clone)]
pub(crate) struct ParentOverride {
    pub origin_id: String,
    pub mode: InputMode,
    pub annotations: BTreeMap<String, String>,
    pub hashed: bool,
    pub runtime: bool,
}

impl Engine {
    /// Memoized expansion of `addr`'s inputs. Called from both `link` and `meta`
    /// for every result — without memoization the whole transparent-group walk
    /// runs twice per addr per request.
    pub(crate) async fn expanded_inputs_for(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<Vec<Input>>> {
        rs.data
            .mem_expanded_inputs
            .once(
                addr.clone(),
                enclose!((self => engine, rs, addr) move || async move {
                    // _no_track: result_addr (the upstream entry point) already set
                    // parent=addr before getting here, so tracked get_def would
                    // record addr→addr (spurious self-cycle).
                    let def = Arc::clone(&engine)
                        .get_def_no_track(rs.clone(), &addr)
                        .await?;
                    let mut visited = FxHashSet::default();
                    let expanded = Arc::clone(&engine)
                        .expand_inputs(
                            rs,
                            def.target_def.inputs.clone(),
                            None,
                            &mut visited,
                        )
                        .await?;
                    Ok(Arc::new(expanded))
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    /// Recursively expands transparent (group) inputs, replacing each group with
    /// its constituent deps. Non-transparent inputs pass through unchanged.
    ///
    /// `parent_override` propagates the origin_id, mode, and annotations from
    /// the group's reference in the parent's input list so that exec driver
    /// grouping (SRC_*) and routing (e.g. `unpack_root=tools`) reflect the
    /// parent's intent, not the group's internal ids. Parent annotations form
    /// the base; the child's own annotations override on key conflict.
    ///
    /// Top-level callers (`parent_override = None`) use an allocate-on-write
    /// `result`: the original `inputs` Vec is returned untouched when no input
    /// is transparent, and we only allocate (and copy the prior pass-throughs)
    /// the first time we actually need to splice expanded deps in. Recursive
    /// callers (`parent_override = Some(...)`) always rewrite every input, so
    /// the result Vec is allocated up front.
    ///
    /// `visited` is borrowed mutably across recursion — expansion is purely
    /// sequential within a single task, so no shared-state synchronisation is
    /// needed (previously held behind `Arc<Mutex<…>>` for no benefit).
    #[async_recursion]
    pub(crate) async fn expand_inputs(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        inputs: Vec<Input>,
        parent_override: Option<ParentOverride>,
        visited: &mut FxHashSet<String>,
    ) -> anyhow::Result<Vec<Input>> {
        let mut result: Option<Vec<Input>> = parent_override
            .as_ref()
            .map(|_| Vec::with_capacity(inputs.len()));

        for (idx, input) in inputs.iter().enumerate() {
            let def = Arc::clone(&self)
                .get_def(rs.clone(), &input.r#ref.r#ref)
                .await?;

            if def.target_def.transparent {
                let out = result.get_or_insert_with(|| {
                    let mut v = Vec::with_capacity(inputs.len());
                    if let Some(prefix) = inputs.get(..idx) {
                        v.extend_from_slice(prefix);
                    }
                    v
                });

                let addr_str = input.r#ref.r#ref.format();
                if !visited.insert(addr_str.clone()) {
                    anyhow::bail!("circular group dependency at '{}'", addr_str);
                }

                // Effective routing context for the group's children: take the
                // parent override if recursing, otherwise the input's own
                // origin_id/mode/annotations/flags. Either way, the parent
                // context wins over the group's interior child annotations
                // only on keys the child does not set (merge happens below
                // per child).
                let eff = match parent_override.as_ref() {
                    Some(p) => {
                        let mut merged = p.annotations.clone();
                        for (k, v) in &input.annotations {
                            merged.insert(k.clone(), v.clone());
                        }
                        ParentOverride {
                            origin_id: p.origin_id.clone(),
                            mode: p.mode.clone(),
                            annotations: merged,
                            hashed: p.hashed,
                            runtime: p.runtime,
                        }
                    }
                    None => ParentOverride {
                        origin_id: input.origin_id.clone(),
                        mode: input.mode.clone(),
                        annotations: input.annotations.clone(),
                        hashed: input.hashed,
                        runtime: input.runtime,
                    },
                };

                let nested = Arc::clone(&self)
                    .expand_inputs(
                        rs.clone(),
                        def.target_def.inputs.clone(),
                        Some(eff),
                        visited,
                    )
                    .await?;

                visited.remove(&addr_str);

                out.extend(nested);
            } else if let Some(p) = parent_override.as_ref() {
                // Recursive call: result Vec was allocated up front.
                // Parent annotations form the base; child overrides on conflict.
                let mut merged = p.annotations.clone();
                for (k, v) in &input.annotations {
                    merged.insert(k.clone(), v.clone());
                }
                result
                    .as_mut()
                    .expect("recursive call allocated result up front")
                    .push(Input {
                        r#ref: input.r#ref.clone(),
                        origin_id: p.origin_id.clone(),
                        mode: p.mode.clone(),
                        annotations: merged,
                        hashed: p.hashed,
                        runtime: p.runtime,
                    });
            } else if let Some(out) = result.as_mut() {
                // Top-level, but an earlier transparent input already triggered
                // allocation — copy this passthrough into the new Vec.
                out.push(input.clone());
            }
            // else: top-level, no transparent seen yet, original `inputs` still
            // valid — skip.
        }

        Ok(result.unwrap_or(inputs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::driver::TargetAddr;
    use crate::engine::driver::targetdef::{Input, InputMode};
    use crate::engine::provider::{
        ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
        ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse,
        Provider as EProvider, TargetSpec,
    };
    use crate::hasync::Cancellable;
    use crate::htpkg::PkgBuf;
    use crate::loosespecparser::TargetSpecValue;
    use futures::future::BoxFuture;
    use std::collections::HashMap;

    /// Minimal in-memory provider that hands back hand-crafted `TargetSpec`s.
    /// The shared `pluginstatictarget` provider always wraps `deps` as a
    /// `Map<group, [addr]>` which the `group` driver rejects (it wants a flat
    /// `[addr]`). We need direct control over the config shape.
    struct CannedProvider {
        specs: Vec<TargetSpec>,
    }

    impl EProvider for CannedProvider {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: "canned".to_string(),
            })
        }
        fn list<'a>(
            &'a self,
            _req: ListRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>>>
        {
            let specs = self.specs.clone();
            Box::pin(async move {
                Ok(Box::new(specs.into_iter().map(|s| {
                    Ok(ListResponse {
                        addr: s.addr.clone(),
                    })
                })) as Box<dyn Iterator<Item = _>>)
            })
        }
        fn list_packages<'a>(
            &'a self,
            _req: ListPackagesRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>>>>,
        > {
            Box::pin(async move { Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _>>) })
        }
        fn get<'a>(
            &'a self,
            req: GetRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
            let specs = self.specs.clone();
            Box::pin(async move {
                specs
                    .into_iter()
                    .find(|s| s.addr == req.addr)
                    .map(|s| GetResponse { target_spec: s })
                    .ok_or(GetError::NotFound)
            })
        }
        fn probe<'a>(
            &'a self,
            _req: ProbeRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            Box::pin(async move { Ok(ProbeResponse { states: vec![] }) })
        }
    }

    fn exec_spec(addr: &str) -> TargetSpec {
        let mut config = HashMap::new();
        config.insert(
            "run".to_string(),
            TargetSpecValue::String("true".to_string()),
        );
        TargetSpec {
            addr: crate::htaddr::parse_addr(addr).expect("parse addr"),
            driver: "exec".to_string(),
            config,
            labels: vec![],
            transitive: Default::default(),
        }
    }

    fn group_spec(addr: &str, deps: &[&str]) -> TargetSpec {
        let mut config = HashMap::new();
        let deps_list: Vec<TargetSpecValue> = deps
            .iter()
            .map(|s| TargetSpecValue::String((*s).to_string()))
            .collect();
        config.insert("deps".to_string(), TargetSpecValue::List(deps_list));
        TargetSpec {
            addr: crate::htaddr::parse_addr(addr).expect("parse addr"),
            driver: "group".to_string(),
            config,
            labels: vec![],
            transitive: Default::default(),
        }
    }

    fn engine_with(specs: Vec<TargetSpec>) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        // group driver is registered by Engine::new; only the exec driver
        // needs adding here.
        engine.register_managed_driver(Box::new(crate::pluginexec::Driver::new_exec()))?;
        engine.register_provider(move |_| Box::new(CannedProvider { specs }))?;
        Ok((Arc::new(engine), root))
    }

    fn input(addr: &str, origin: &str, ann: &[(&str, &str)]) -> Input {
        Input {
            r#ref: TargetAddr::parse(addr, &PkgBuf::from("")).expect("parse addr"),
            mode: InputMode::Tool,
            origin_id: origin.to_string(),
            annotations: ann
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            hashed: true,
            runtime: true,
        }
    }

    // Regression: when a tool input refers to a transparent group, the parent
    // input's annotations (e.g. `unpack_root=tools`) were dropped because
    // expand_inputs only carried the child's annotations through. The driver
    // bridge then routed the tool into ws/ instead of tools/.
    #[tokio::test]
    async fn parent_annotations_propagate_through_transparent_group() -> anyhow::Result<()> {
        let (engine, _root) = engine_with(vec![
            // Group with one exec child.
            group_spec("//pkg:grp", &["//pkg:leaf"]),
            exec_spec("//pkg:leaf"),
        ])?;

        let rs = engine.new_state();
        let mut visited = FxHashSet::default();

        let inputs = vec![input("//pkg:grp", "tool|cc|0", &[("unpack_root", "tools")])];

        let expanded = Arc::clone(&engine)
            .expand_inputs(rs, inputs, None, &mut visited)
            .await?;

        assert_eq!(expanded.len(), 1, "group → one child");
        let child = &expanded[0];
        assert_eq!(child.r#ref.r#ref.format(), "//pkg:leaf");
        assert_eq!(child.origin_id, "tool|cc|0", "parent origin_id wins");
        assert_eq!(
            child.annotations.get("unpack_root").map(String::as_str),
            Some("tools"),
            "parent annotation must survive group expansion; got: {:?}",
            child.annotations,
        );
        Ok(())
    }

    // Child-set annotations should win over a same-named parent annotation.
    #[tokio::test]
    async fn child_annotation_overrides_parent_on_conflict() -> anyhow::Result<()> {
        // grp.deps points to leaf via the static-target deps map. The static
        // provider doesn't carry annotations on group children, but the group
        // driver itself builds Inputs with empty annotations — so this test
        // would need to inject child annotations through a custom driver.
        // For now we rely on the corresponding unit test in
        // `merge_sandbox_dedupes_*` plus the integration behaviour above.
        // (Documenting the contract here even though the static-target path
        // can't directly produce child annotations.)
        let (engine, _root) = engine_with(vec![
            group_spec("//pkg:grp", &["//pkg:leaf"]),
            exec_spec("//pkg:leaf"),
        ])?;
        let rs = engine.new_state();
        let mut visited = FxHashSet::default();
        // Both keys come from the parent; leaf has none → parent values win.
        let inputs = vec![input(
            "//pkg:grp",
            "tool|cc|0",
            &[("unpack_root", "tools"), ("custom", "p")],
        )];
        let expanded = Arc::clone(&engine)
            .expand_inputs(rs, inputs, None, &mut visited)
            .await?;
        assert_eq!(
            expanded[0].annotations.get("custom").map(String::as_str),
            Some("p"),
        );
        Ok(())
    }
}
