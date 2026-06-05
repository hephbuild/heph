//! Workspace validation helpers.
//!
//! `codegen_copy_overlaps` detects when two *different* targets declare a
//! `codegen = copy` output on the *same* tree path — those outputs would clobber
//! each other when materialized into the source tree, so they must be rejected.
//!
//! The overlap key is the normalized, root-anchored output path produced by
//! [`crate::engine::gitignore::content_to_pattern`]. This matches identical
//! *declared* paths; it does not resolve glob-vs-file fuzzy overlap (a `*.go`
//! glob and a concrete `foo.go` are treated as distinct keys), mirroring the
//! precision of the gitignore enumeration that shares this normalization.

use std::collections::HashMap;
use std::sync::Arc;

use enclose::enclose;
use futures::TryStreamExt;

use crate::engine::Engine;
use crate::engine::driver::targetdef::path::CodegenMode;
use crate::engine::error::TargetNotFoundError;
use crate::engine::gitignore::content_to_pattern;
use crate::engine::request_state::RequestState;
use crate::hmemoizer::downcast_chain_ref;
use crate::htaddr::Addr;
use crate::htmatcher::Matcher;

/// A single overlap: one normalized output path produced by two or more targets.
#[derive(Debug, Clone)]
pub struct CodegenOverlap {
    /// Normalized, root-anchored output path (the collision key).
    pub path: String,
    /// The (≥2) distinct targets producing it, sorted by formatted address.
    pub addrs: Vec<Addr>,
}

impl Engine {
    /// Find `codegen = copy` outputs that collide across targets.
    ///
    /// `matcher` scopes which targets are inspected — whole-workspace callers
    /// pass [`Matcher::TreeOutputTo`] with an empty package (the selector the
    /// gitignore enumeration uses to reach every codegen target); scoped callers
    /// pass the user's matcher. Each target's def is fetched concurrently via the
    /// per-request memoizer, so this shares parse work with any other walk in the
    /// same request.
    pub async fn codegen_copy_overlaps(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        matcher: &Matcher,
    ) -> anyhow::Result<Vec<CodegenOverlap>> {
        // Drain the addr stream first, then fan the def fetches out in parallel.
        let addrs: Vec<Addr> = {
            let stream = Arc::clone(&self).query(rs.clone(), matcher);
            tokio::pin!(stream);
            let mut v = Vec::new();
            while let Some(addr) = stream.try_next().await? {
                v.push(addr);
            }
            v
        };

        let fail_fast = rs.fail_fast();
        let futs = addrs.iter().map(|addr| {
            enclose!((self => engine, rs, addr) async move {
                // A provider may `list` an addr it cannot `get` standalone (e.g. go
                // per-platform variants resolved only as in-context deps). Such a
                // target produces no real tree output here, so skip it on a
                // self-addr NotFound — matching the query resolver. Any other
                // failure (parse error, missing dep) propagates.
                let def = match engine.get_def(rs, &addr).await {
                    Ok(def) => def,
                    Err(e)
                        if downcast_chain_ref::<TargetNotFoundError>(&e)
                            .is_some_and(|nf| nf.addr == addr) =>
                    {
                        return Ok((addr, Vec::new()));
                    }
                    Err(e) => return Err(e),
                };
                let paths: Vec<String> = def
                    .target_def
                    .outputs
                    .iter()
                    .flat_map(|output| output.paths.iter())
                    .filter(|p| p.codegen_tree == CodegenMode::Copy)
                    .map(|p| content_to_pattern(&p.content))
                    .collect();
                Ok::<(Addr, Vec<String>), anyhow::Error>((addr, paths))
            })
        });
        let per_target = crate::engine::fanout::join_all_failable(futs, fail_fast).await?;

        // Group addrs by output path. A path declared twice by the *same* target
        // is not a conflict, so dedupe per path (Addr interning makes `==` cheap).
        let mut by_path: HashMap<String, Vec<Addr>> = HashMap::new();
        for (addr, paths) in per_target {
            for path in paths {
                let entry = by_path.entry(path).or_default();
                if !entry.iter().any(|a| a == &addr) {
                    entry.push(addr.clone());
                }
            }
        }

        let mut overlaps: Vec<CodegenOverlap> = by_path
            .into_iter()
            .filter(|(_, addrs)| addrs.len() > 1)
            .map(|(path, mut addrs)| {
                addrs.sort_by_key(|a| a.format());
                CodegenOverlap { path, addrs }
            })
            .collect();
        overlaps.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(overlaps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::htpkg::PkgBuf;
    use crate::pluginstatictarget;
    use std::collections::HashMap;

    /// Build a static target declaring an `out` default-group with the given
    /// paths and codegen mode.
    fn codegen_target(addr: &str, outs: &[&str], codegen: &str) -> pluginstatictarget::Target {
        let mut out = HashMap::new();
        out.insert(
            String::new(),
            outs.iter().map(|o| (*o).to_string()).collect(),
        );
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some("true".to_string()),
            out,
            codegen: Some(codegen.to_string()),
            deps: HashMap::new(),
            labels: vec![],
        }
    }

    fn engine_with(
        targets: Vec<pluginstatictarget::Target>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        // exec driver parses specs into defs (with codegen-stamped outputs); the
        // fs provider/driver resolves any synthesized inputs; the static provider
        // supplies the specs and the `list_packages` query needs.
        engine.register_managed_driver(Box::new(crate::pluginexec::Driver::new_exec()))?;
        engine.register_provider(|_| Box::new(crate::pluginfs::Provider))?;
        engine.register_driver(Box::new(crate::pluginfs::Driver))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    fn all() -> Matcher {
        Matcher::TreeOutputTo(PkgBuf::from(""))
    }

    #[tokio::test]
    async fn detects_overlapping_copy_outputs() -> anyhow::Result<()> {
        // Two targets in the same package both emitting `gen.go` as copy.
        let (engine, _root) = engine_with(vec![
            codegen_target("//a:t1", &["gen.go"], "copy"),
            codegen_target("//a:t2", &["gen.go"], "copy"),
        ])?;
        let rs = engine.new_state();

        let overlaps = Arc::clone(&engine)
            .codegen_copy_overlaps(rs, &all())
            .await?;

        assert_eq!(
            overlaps.len(),
            1,
            "exactly one overlapping path: {overlaps:?}"
        );
        assert_eq!(overlaps[0].path, "/a/gen.go");
        let addrs: Vec<String> = overlaps[0].addrs.iter().map(|a| a.format()).collect();
        assert_eq!(addrs, vec!["//a:t1".to_string(), "//a:t2".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn distinct_copy_outputs_do_not_overlap() -> anyhow::Result<()> {
        let (engine, _root) = engine_with(vec![
            codegen_target("//a:t1", &["a.go"], "copy"),
            codegen_target("//a:t2", &["b.go"], "copy"),
        ])?;
        let rs = engine.new_state();

        let overlaps = Arc::clone(&engine)
            .codegen_copy_overlaps(rs, &all())
            .await?;

        assert!(overlaps.is_empty(), "no overlap expected: {overlaps:?}");
        Ok(())
    }

    #[tokio::test]
    async fn same_target_duplicate_path_is_not_a_conflict() -> anyhow::Result<()> {
        // One target listing the same copy output twice is not a self-conflict.
        let (engine, _root) = engine_with(vec![codegen_target(
            "//a:t1",
            &["dup.go", "dup.go"],
            "copy",
        )])?;
        let rs = engine.new_state();

        let overlaps = Arc::clone(&engine)
            .codegen_copy_overlaps(rs, &all())
            .await?;

        assert!(
            overlaps.is_empty(),
            "self-dup is not a conflict: {overlaps:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn non_copy_outputs_are_ignored() -> anyhow::Result<()> {
        // Same path but not `codegen = copy` -> not a tree collision.
        let (engine, _root) = engine_with(vec![
            codegen_target("//a:t1", &["gen.go"], "in_place"),
            codegen_target("//a:t2", &["gen.go"], "in_place"),
        ])?;
        let rs = engine.new_state();

        let overlaps = Arc::clone(&engine)
            .codegen_copy_overlaps(rs, &all())
            .await?;

        assert!(
            overlaps.is_empty(),
            "non-copy outputs ignored: {overlaps:?}"
        );
        Ok(())
    }

    /// A provider that *lists* an addr but returns `NotFound` from `get` —
    /// mimicking the go provider's per-platform variants that resolve only as
    /// in-context deps. `codegen_copy_overlaps` must skip such a target rather
    /// than surface its `get_def` failure as a validation error.
    struct GhostProvider;

    impl crate::engine::provider::Provider for GhostProvider {
        fn config(
            &self,
            _req: crate::engine::provider::ConfigRequest,
        ) -> anyhow::Result<crate::engine::provider::ConfigResponse> {
            Ok(crate::engine::provider::ConfigResponse {
                name: "ghost".to_string(),
            })
        }
        fn list<'a>(
            &'a self,
            req: crate::engine::provider::ListRequest,
            _ctoken: &'a (dyn crate::hasync::Cancellable + Send + Sync),
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
            Box::pin(async move {
                let items: Vec<anyhow::Result<crate::engine::provider::ListResponse>> =
                    if pkg.as_str() == "virt" {
                        vec![Ok(crate::engine::provider::ListResponse {
                            addr: Addr::new(pkg, "ghost".to_string(), Default::default()),
                        })]
                    } else {
                        vec![]
                    };
                Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn list_packages<'a>(
            &'a self,
            _req: crate::engine::provider::ListPackagesRequest,
            _ctoken: &'a (dyn crate::hasync::Cancellable + Send + Sync),
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
                let items: Vec<anyhow::Result<crate::engine::provider::ListPackageResponse>> =
                    vec![Ok(crate::engine::provider::ListPackageResponse {
                        pkg: PkgBuf::from("virt"),
                    })];
                Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn get<'a>(
            &'a self,
            _req: crate::engine::provider::GetRequest,
            _ctoken: &'a (dyn crate::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            Result<crate::engine::provider::GetResponse, crate::engine::provider::GetError>,
        > {
            // Listed but unresolvable standalone.
            Box::pin(async { Err(crate::engine::provider::GetError::NotFound) })
        }
        fn probe<'a>(
            &'a self,
            _req: crate::engine::provider::ProbeRequest,
            _ctoken: &'a (dyn crate::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<'a, anyhow::Result<crate::engine::provider::ProbeResponse>>
        {
            Box::pin(async { Ok(crate::engine::provider::ProbeResponse { states: vec![] }) })
        }
    }

    #[tokio::test]
    async fn listed_but_unresolvable_target_is_skipped() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(|_| Box::new(GhostProvider))?;
        let engine = Arc::new(engine);
        let rs = engine.new_state();

        // `PackagePrefix` yields the listed addr straight to the overlap fanout
        // (no spec resolution), so the per-target `get_def` NotFound is what the
        // skip must absorb. Result: no error, no overlaps.
        let overlaps = Arc::clone(&engine)
            .codegen_copy_overlaps(rs, &Matcher::PackagePrefix(PkgBuf::from("virt")))
            .await?;

        assert!(overlaps.is_empty(), "ghost target skipped: {overlaps:?}");
        Ok(())
    }
}
