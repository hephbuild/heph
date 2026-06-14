//! Workspace validation helpers.
//!
//! `codegen_copy_overlaps` detects when two *different* targets declare
//! `codegen = copy` outputs that collide in the source tree — they would clobber
//! each other when materialized, so they must be rejected.
//!
//! Outputs are normalized to root-anchored paths via
//! [`crate::engine::gitignore::content_to_pattern`] (directories carry a trailing
//! `/`). Two outputs overlap when they are the *same* path, or when one is a
//! directory that *contains* the other (e.g. `/gen/` vs `/gen/a.go`). Glob
//! patterns are compared as their literal strings, so a glob lying inside a
//! declared directory is caught, but glob-vs-concrete-file fuzzy matching is not
//! resolved.

use std::sync::Arc;

use enclose::enclose;
use futures::TryStreamExt;

use crate::engine::Engine;
use crate::engine::driver::targetdef::path::CodegenMode;
use crate::engine::error::TargetNotFoundError;
use crate::engine::gitignore::content_to_pattern;
use crate::engine::request_state::RequestState;
use hcore::hmemoizer::downcast_chain_ref;
use hmodel::htaddr::Addr;
use hmodel::htmatcher::Matcher;

/// One `codegen = copy` output: its normalized, root-anchored path and the
/// target that emits it.
#[derive(Debug, Clone)]
pub struct CodegenOutput {
    pub path: String,
    pub addr: Addr,
}

/// A single collision between two `codegen = copy` outputs from *different*
/// targets — either the same path, or a directory containing the other.
#[derive(Debug, Clone)]
pub struct CodegenOverlap {
    pub a: CodegenOutput,
    pub b: CodegenOutput,
}

/// Split a normalized path into its `/`-separated components, ignoring a trailing
/// slash. Used as a sort key so a directory and its descendants stay adjacent
/// (component order, unlike raw byte order, never lets a sibling slip between).
fn path_components(p: &str) -> std::str::Split<'_, char> {
    p.trim_end_matches('/').split('/')
}

/// True when `ancestor` is a strict parent directory of `descendant` — both
/// already trailing-slash-trimmed. `descendant` must continue past `ancestor`
/// at a `/` boundary, so `/a` is an ancestor of `/a/b` but not of `/ab`.
fn is_ancestor(ancestor: &str, descendant: &str) -> bool {
    descendant.starts_with(ancestor) && descendant.as_bytes().get(ancestor.len()) == Some(&b'/')
}

/// True when two normalized output paths collide in the tree: the same path
/// (trailing slash ignored, so a file and a same-named directory still clash),
/// or one is an ancestor directory of the other (`/gen/` vs `/gen/a.go`).
fn paths_overlap(a: &str, b: &str) -> bool {
    let a = a.trim_end_matches('/');
    let b = b.trim_end_matches('/');
    a == b || is_ancestor(a, b) || is_ancestor(b, a)
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

        // Flatten to (path, addr) outputs, then sort by path *components* (ties by
        // addr) so every group of overlapping outputs is contiguous: equal paths
        // sit together and a directory's descendants sort immediately after it.
        // Component order (not raw string order) is what guarantees contiguity —
        // it keeps `/a` next to `/a/x` instead of letting a sibling like `/a-b`
        // (where `-` < `/`) slip between them. Dedupe identical (path, addr) — a
        // target listing the same output twice is not a self-conflict.
        let mut outs: Vec<CodegenOutput> = per_target
            .into_iter()
            .flat_map(|(addr, paths)| {
                paths.into_iter().map(move |path| CodegenOutput {
                    path,
                    addr: addr.clone(),
                })
            })
            .collect();
        outs.sort_by(|a, b| {
            path_components(&a.path)
                .cmp(path_components(&b.path))
                .then_with(|| a.addr.format().cmp(&b.addr.format()))
        });
        outs.dedup_by(|a, b| a.path == b.path && a.addr == b.addr);

        // Sorted scan: for each output, the only outputs that can overlap it are
        // the contiguous run that follows (equal paths, then — if it is a
        // directory — its contained children). The first non-overlapping entry
        // ends the run, since everything beyond sorts lexicographically clear of
        // it. Cross-target only: a target owning a dir *and* a file under it is
        // fine.
        let mut overlaps: Vec<CodegenOverlap> = Vec::new();
        for (i, a) in outs.iter().enumerate() {
            for b in outs.iter().skip(i + 1) {
                if !paths_overlap(&a.path, &b.path) {
                    break;
                }
                if a.addr != b.addr {
                    overlaps.push(CodegenOverlap {
                        a: a.clone(),
                        b: b.clone(),
                    });
                }
            }
        }
        Ok(overlaps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use hbuiltins::pluginstatictarget;
    use hmodel::htpkg::PkgBuf;
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
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    fn all() -> Matcher {
        Matcher::TreeOutputTo(PkgBuf::from(""))
    }

    /// True if `overlaps` contains the collision between the two given outputs,
    /// in either order.
    fn has_pair(overlaps: &[CodegenOverlap], p1: &str, a1: &str, p2: &str, a2: &str) -> bool {
        let want = |x: &CodegenOutput, p: &str, a: &str| x.path == p && x.addr.format() == a;
        overlaps.iter().any(|o| {
            (want(&o.a, p1, a1) && want(&o.b, p2, a2)) || (want(&o.a, p2, a2) && want(&o.b, p1, a1))
        })
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

        assert_eq!(overlaps.len(), 1, "one collision: {overlaps:?}");
        assert!(
            has_pair(&overlaps, "/a/gen.go", "//a:t1", "/a/gen.go", "//a:t2"),
            "{overlaps:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn directory_output_overlaps_file_inside_it() -> anyhow::Result<()> {
        // One target emits the directory `gen/`, another a file inside it.
        let (engine, _root) = engine_with(vec![
            codegen_target("//a:dir", &["gen/"], "copy"),
            codegen_target("//a:file", &["gen/sub/x.go"], "copy"),
        ])?;
        let rs = engine.new_state();

        let overlaps = Arc::clone(&engine)
            .codegen_copy_overlaps(rs, &all())
            .await?;

        assert_eq!(overlaps.len(), 1, "one collision: {overlaps:?}");
        assert!(
            has_pair(
                &overlaps,
                "/a/gen/",
                "//a:dir",
                "/a/gen/sub/x.go",
                "//a:file"
            ),
            "{overlaps:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn sibling_paths_with_shared_prefix_do_not_overlap() -> anyhow::Result<()> {
        // `gen/` must not be treated as a parent of the sibling `gen-extra.go`
        // just because the strings share a prefix.
        let (engine, _root) = engine_with(vec![
            codegen_target("//a:dir", &["gen/"], "copy"),
            codegen_target("//a:sib", &["gen-extra.go"], "copy"),
        ])?;
        let rs = engine.new_state();

        let overlaps = Arc::clone(&engine)
            .codegen_copy_overlaps(rs, &all())
            .await?;

        assert!(overlaps.is_empty(), "siblings do not overlap: {overlaps:?}");
        Ok(())
    }

    #[tokio::test]
    async fn same_target_dir_and_file_within_is_not_a_conflict() -> anyhow::Result<()> {
        // A single target owning a directory and a file inside it is fine.
        let (engine, _root) =
            engine_with(vec![codegen_target("//a:t", &["gen/", "gen/x.go"], "copy")])?;
        let rs = engine.new_state();

        let overlaps = Arc::clone(&engine)
            .codegen_copy_overlaps(rs, &all())
            .await?;

        assert!(overlaps.is_empty(), "same-target nesting ok: {overlaps:?}");
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
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
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
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
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
