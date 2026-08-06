//! The `group` driver: re-export other targets' outputs, optionally filtered
//! and relocated.
//!
//! A group has two modes, and which one it is in follows from its config alone:
//!
//! * **Aggregate** (no transform configured) — the group is `transparent`, and
//!   the engine splices its members straight into each consumer's input list.
//!   It never executes and never produces an artifact of its own. This is the
//!   original behaviour and stays byte-for-byte what it was.
//!
//! * **Filter/relocate** (any of `include`/`exclude`/`strip_prefix`/`prefix`/
//!   `rename` set) — the group becomes a real target that produces
//!   [`ContentView`] artifacts: zero-copy windows onto its deps' artifacts with
//!   rewritten paths. It is marked uncacheable, so no revision, no blob, and no
//!   remote push is ever created for it; the deps stay cached exactly as
//!   before, and the group is re-derived from a path list on each build.
//!
//! The alternative — an exec target running `cp` — costs a sandbox, a
//! subprocess, and a second full copy of every byte in both the local and the
//! remote cache. This costs a few string operations.
//!
//! It cannot stay `transparent` in the second mode. A transparent target is
//! expanded away *before* `hashin` is computed, so its config never reaches a
//! consumer's cache key — membership changes are caught only because they
//! change which dep hashouts get folded in. A path transform changes no dep's
//! hashout, so a transparent one would be invisible to the cache and a consumer
//! would happily reuse an entry built against the pre-relocation layout. Being
//! a real target is what puts the transform in the consumer's key, via the
//! group's own def hash and its artifacts' derived hashouts.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::hartifactcontent::{
    Content as _, PathTransform, Rename, RenamePlan, SourcePaths, ViewContent,
};
use hcore::hasync::Cancellable;
use hplugin::driver::TargetAddr;
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, RunRequest, RunResponse, outputartifact,
    targetdef::{
        CacheConfig, Input, InputMode, Output, TargetDef,
        path::{CodegenMode, Content as PathContent, Path},
    },
};
use hplugin::htspec::Spec;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

pub const DRIVER_NAME: &str = "group";

/// Config for a `group` target. `#[derive(Spec)]` provides the parser and the
/// LSP schema.
#[derive(Spec)]
struct GroupSpec {
    /// Target addresses this group aggregates; the group re-exports their outputs.
    deps: Vec<String>,
    /// Glob patterns an output path must match to be re-exported, written the way
    /// the producing package declares them (`**/*.so`). Empty re-exports everything.
    include: Vec<String>,
    /// Glob patterns that drop an output path. Applied after `include`.
    exclude: Vec<String>,
    /// Leading directory stripped from re-exported paths, written the way the
    /// producing package declares it (`build/out`).
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    strip_prefix: Option<String>,
    /// Leading directory prepended to re-exported paths (e.g. `lib`).
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    prefix: Option<String>,
    /// Where selected outputs are placed. A string (`"bin/myserver"`) renames the
    /// single output surviving `include`/`exclude`, so you never name a source
    /// path; it cannot be combined with `strip_prefix`/`prefix`. A dict maps exact
    /// emitted paths to destinations.
    rename: Rename,
}

impl GroupSpec {
    fn transform(&self) -> anyhow::Result<PathTransform> {
        let t = PathTransform {
            include: self.include.clone(),
            exclude: self.exclude.clone(),
            strip_prefix: self.strip_prefix.clone(),
            prefix: self.prefix.clone(),
            rename: self.rename.clone(),
        };
        // A string `rename` fixes the destination outright, so a
        // `strip_prefix`/`prefix` alongside it would be computed and thrown
        // away. Rejecting the combination is clearer than silently ignoring
        // half the config. (The dict form is fine with them: it places the
        // paths it names, and the prefixes place the rest.)
        if t.prefixes_are_dead_config() {
            anyhow::bail!(
                "a string `rename` sets the destination outright and cannot be combined \
                 with `strip_prefix`/`prefix` — drop those, or use the dict form of \
                 `rename` so the prefixes still place everything else"
            );
        }
        Ok(t)
    }
}

#[derive(serde::Serialize)]
struct GroupDef {
    transform: PathTransform,
}

/// The package of the target that produced this input's artifact.
///
/// Handed to every [`ViewContent`] so transform patterns can be written the way
/// the *producing* BUILD file writes them (`build/out/server`) rather than the
/// way heph emits them (`app/build/out/server`) — see [`SourcePaths`]. An
/// author writing a group has no reason to know a dep's emitted layout, and
/// that layout changes if the dep ever moves package.
fn dep_package(input: &hplugin::driver::RunInput) -> Option<String> {
    let pkg = input.source_addr.package.as_str();
    (!pkg.is_empty()).then(|| pkg.to_string())
}

pub struct Driver;

#[async_trait]
impl hplugin::driver::Driver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        GroupSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let spec = GroupSpec::from(&req.target_spec.config).context("parse group config")?;
        let transform = spec.transform().context("group config")?;
        let deps = spec.deps;

        let pkg = req.target_spec.addr.package.clone();
        let inputs = deps
            .iter()
            .enumerate()
            .map(|(i, addr_str)| -> anyhow::Result<Input> {
                let r#ref = TargetAddr::parse(addr_str, &pkg)
                    .with_context(|| format!("parsing group dep '{addr_str}'"))?;
                Ok(Input {
                    r#ref,
                    mode: InputMode::Standard,
                    origin_id: format!("group:{i}"),
                    annotations: std::collections::BTreeMap::new(),
                    hashed: true,
                    runtime: true,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let mut h = Xxh3::new();
        h.update(req.target_spec.addr.format().as_bytes());
        for d in &deps {
            h.update(d.as_bytes());
        }
        // Folded into the def hash, which is folded into every consumer's
        // `hashin`. This is what makes changing a `prefix` invalidate the
        // targets that depend on this group — nothing else would, because a
        // transform changes no dep's hashout. Written only when non-identity so
        // the digest of an ordinary aggregate group is byte-identical to what
        // it was before transforms existed, and no cached revision is
        // invalidated by this feature landing.
        let relocating = !transform.is_identity();
        if relocating {
            use std::hash::Hash as _;
            struct Fold<'a>(&'a mut Xxh3);
            impl std::hash::Hasher for Fold<'_> {
                fn write(&mut self, bytes: &[u8]) {
                    self.0.update(bytes);
                }
                fn finish(&self) -> u64 {
                    self.0.digest()
                }
            }
            h.update(b"\0group-transform-v1\0");
            let mut fold = Fold(&mut h);
            transform.hash(&mut fold);
        }
        let hash = format!("{:016x}", h.digest()).into_bytes();

        // A relocating group is a real target (see the module docs for why it
        // cannot stay transparent), so it must declare an output group for
        // consumers to select. The declared paths are descriptive only — what
        // it actually emits is decided at run time from its deps' artifacts —
        // so they mirror the `include` patterns, or `**` when everything is
        // re-exported.
        let outputs = if relocating {
            let patterns = if transform.include.is_empty() {
                vec!["**".to_string()]
            } else {
                transform.include.clone()
            };
            vec![Output {
                group: String::new(),
                paths: patterns
                    .into_iter()
                    .map(|p| Path {
                        content: PathContent::Glob(p),
                        codegen_tree: CodegenMode::None,
                        collect: false,
                    })
                    .collect(),
            }]
        } else {
            vec![]
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(GroupDef { transform }),
                inputs,
                outputs,
                support_files: vec![],
                // Off in both modes, for different reasons: an aggregate group
                // has nothing of its own to cache, and a relocating one owns no
                // bytes — its artifacts borrow its deps'. See `is_passthrough`
                // in the engine.
                cache: CacheConfig::off(),
                pty: false,
                hash,
                transparent: !relocating,
            },
        })
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        Ok(ApplyTransitiveResponse {
            target_def: req.target_def,
        })
    }

    /// Build one zero-copy [`ViewContent`] per input artifact.
    ///
    /// No sandbox is created and no bytes are read: the transform is resolved
    /// against each artifact's path list (a header-only scan for a tar-backed
    /// cache artifact), and the resulting `Content` forwards file data by
    /// handle when a consumer eventually materializes it.
    async fn run<'a, 'io>(
        &self,
        req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        // `def` (in-process downcast), not `def_de`: `group` is a built-in
        // driver and its def never crosses the plugin ABI — a view could not
        // survive the trip anyway.
        let transform = &req.target.def::<GroupDef>().transform;
        if transform.is_identity() {
            anyhow::bail!(
                "group driver run() must never be called for an aggregate group — \
                 groups with no include/exclude/strip_prefix/prefix/rename are \
                 inlined before execution"
            );
        }

        // Typo/ambiguity check over the union of every dep's paths, not per
        // artifact: a `strip_prefix` that applies to one dep and not another is
        // legitimate (only one matching *nothing at all* is a mistake), and a
        // `rename` key is ambiguous only relative to every path in play.
        let mut sources = Vec::with_capacity(req.inputs.len());
        for input in &req.inputs {
            if matches!(input.artifact.r#type, hplugin::driver::inputartifact::Type::Dep) {
                sources.push(SourcePaths::new(
                    dep_package(input),
                    input
                        .artifact
                        .content
                        .entry_paths()
                        .with_context(|| format!("listing paths of dep '{}'", input.source_addr))?,
                ));
            }
        }
        let plan = transform
            .plan(&sources)
            .with_context(|| format!("group {}", req.target.addr.format()))?;

        let mut artifacts = Vec::with_capacity(req.inputs.len());
        for (i, input) in req.inputs.iter().enumerate() {
            match input.artifact.r#type {
                // Support files are an all-or-nothing per-dep set that
                // materializes *alongside* an output group rather than as part
                // of it, so the transform does not apply to them; they are
                // re-exported as they are.
                hplugin::driver::inputartifact::Type::Support => {
                    // Wrapped in an identity view rather than passed through
                    // raw: `Content::View` is what marks an artifact as
                    // borrowing bytes it does not own, which is what keeps it
                    // out of the cache. The transform is empty, so every path
                    // is preserved.
                    let view = Arc::new(ViewContent::new(
                        Arc::clone(&input.artifact.content),
                        PathTransform::default(),
                        dep_package(input),
                        RenamePlan::default(),
                    ));
                    // Read off the view, not the source: the engine swaps this
                    // artifact's content for the `ViewContent` itself on the
                    // passthrough path, so the recorded hashout must be the one
                    // that content answers with, or the parent's `hashin` would
                    // be keyed on a value nothing else agrees with.
                    let hashout = view.hashout().with_context(|| {
                        format!("hashout of support artifact from '{}'", input.source_addr)
                    })?;
                    artifacts.push(outputartifact::OutputArtifact {
                        group: String::new(),
                        name: format!("support_{i}"),
                        r#type: outputartifact::Type::SupportFile,
                        hashout,
                        content: outputartifact::Content::View(outputartifact::ContentView {
                            view,
                        }),
                    });
                }
                hplugin::driver::inputartifact::Type::Dep => {
                    let view = Arc::new(ViewContent::new(
                        Arc::clone(&input.artifact.content),
                        transform.clone(),
                        dep_package(input),
                        plan.clone(),
                    ));
                    // Resolving here surfaces a collision as a failure of *this*
                    // target, naming the group, rather than as a confusing
                    // staging error inside whichever consumer happened to
                    // materialize it first.
                    view.mapping().with_context(|| {
                        format!(
                            "applying group {} transform to outputs of '{}'",
                            req.target.addr.format(),
                            input.source_addr
                        )
                    })?;
                    let hashout = view.hashout().with_context(|| {
                        format!("hashout of view over '{}'", input.source_addr)
                    })?;
                    artifacts.push(outputartifact::OutputArtifact {
                        group: String::new(),
                        name: format!("view_{i}"),
                        r#type: outputartifact::Type::Output,
                        hashout,
                        content: outputartifact::Content::View(outputartifact::ContentView {
                            view,
                        }),
                    });
                }
            }
        }

        Ok(RunResponse {
            artifacts,
            ..Default::default()
        })
    }

    async fn run_shell<'a, 'io>(
        &self,
        _req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        anyhow::bail!("run_shell not implemented for group driver")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::parse_addr;
    use hplugin::driver::Driver as EDriver;
    use hplugin::provider::TargetSpec;
    use std::collections::HashMap;

    fn ctoken() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    fn make_parse_req(addr_str: &str, config: HashMap<String, Value>) -> ParseRequest {
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: std::sync::Arc::new(TargetSpec {
                addr: parse_addr(addr_str).unwrap(),
                driver: DRIVER_NAME.to_string(),
                config,
                ..Default::default()
            }),
        }
    }

    /// Every config key `GroupSpec::from` understands must appear in the LSP
    /// schema, so the parser and the completion list are caught drifting apart.
    #[test]
    fn test_schema_lists_every_config_key() {
        use hcore::htvalue::signature::ParamType;
        use hplugin::driver::Driver as _;
        let schema = Driver.schema();
        let by_name: HashMap<&str, &hplugin::driver::DriverField> =
            schema.fields.iter().map(|f| (f.name.as_str(), f)).collect();
        for key in [
            "deps",
            "include",
            "exclude",
            "strip_prefix",
            "prefix",
            "rename",
        ] {
            assert!(by_name.contains_key(key), "schema missing field `{key}`");
            assert!(!by_name[key].doc.is_empty(), "field `{key}` has no doc");
        }
        let str_or_list =
            ParamType::union(vec![ParamType::String, ParamType::list(ParamType::String)]);
        assert_eq!(by_name["deps"].ty, str_or_list);
        assert_eq!(by_name["include"].ty, str_or_list);
        assert_eq!(by_name["strip_prefix"].ty, ParamType::String);
    }

    #[tokio::test]
    async fn test_parse_no_deps_ok() {
        let driver = Driver;
        let res = driver
            .parse(make_parse_req("//pkg:g", HashMap::new()), &ctoken())
            .await
            .unwrap();
        assert!(res.target_def.inputs.is_empty());
        assert!(res.target_def.transparent);
        assert!(!res.target_def.cache.enabled);
    }

    #[tokio::test]
    async fn test_parse_deps_become_inputs() {
        let driver = Driver;
        let config = HashMap::from([(
            "deps".to_string(),
            Value::List(vec![
                Value::String("//pkg:a".to_string()),
                Value::String("//pkg:b".to_string()),
            ]),
        )]);
        let res = driver
            .parse(make_parse_req("//pkg:g", config), &ctoken())
            .await
            .unwrap();
        assert_eq!(res.target_def.inputs.len(), 2);
        assert_eq!(res.target_def.inputs[0].r#ref.r#ref.format(), "//pkg:a");
        assert_eq!(res.target_def.inputs[1].r#ref.r#ref.format(), "//pkg:b");
        assert!(res.target_def.transparent);
    }

    #[tokio::test]
    async fn test_parse_unknown_key_errors() {
        let driver = Driver;
        let config = HashMap::from([("foo".to_string(), Value::String("bar".to_string()))]);
        let result = driver
            .parse(make_parse_req("//pkg:g", config), &ctoken())
            .await;
        let Err(err) = result else {
            panic!("expected error, got Ok");
        };
        assert!(
            format!("{err:#}").contains("foo"),
            "error should mention unknown key: {err:#}"
        );
    }

    #[tokio::test]
    async fn test_parse_labels_preserved() {
        let driver = Driver;
        let mut req = make_parse_req("//pkg:g", HashMap::new());
        std::sync::Arc::get_mut(&mut req.target_spec)
            .expect("spec uniquely owned in test")
            .labels = vec!["my_label".to_string()];
        let res = driver.parse(req, &ctoken()).await.unwrap();
        assert_eq!(res.target_def.labels, vec!["my_label"]);
    }

    #[tokio::test]
    async fn test_parse_hash_changes_with_deps() {
        let driver = Driver;
        let empty = driver
            .parse(make_parse_req("//pkg:g", HashMap::new()), &ctoken())
            .await
            .unwrap()
            .target_def
            .hash;

        let with_dep = driver
            .parse(
                make_parse_req(
                    "//pkg:g",
                    HashMap::from([(
                        "deps".to_string(),
                        Value::List(vec![Value::String("//pkg:a".to_string())]),
                    )]),
                ),
                &ctoken(),
            )
            .await
            .unwrap()
            .target_def
            .hash;

        assert_ne!(empty, with_dep);
    }

    // ---- filter / relocate mode ----

    fn deps_value(addrs: &[&str]) -> Value {
        Value::List(
            addrs
                .iter()
                .map(|a| Value::String((*a).to_string()))
                .collect(),
        )
    }

    async fn parse_group(extra: &[(&str, Value)]) -> TargetDef {
        let mut config = HashMap::from([("deps".to_string(), deps_value(&["//pkg:a"]))]);
        for (k, v) in extra {
            config.insert((*k).to_string(), v.clone());
        }
        Driver
            .parse(make_parse_req("//pkg:g", config), &ctoken())
            .await
            .expect("parse")
            .target_def
    }

    /// The no-regression guard: a group without a transform must stay exactly
    /// what it was — transparent, no declared outputs, never executed.
    #[tokio::test]
    async fn aggregate_group_stays_transparent() {
        let def = parse_group(&[]).await;
        assert!(def.transparent, "aggregate group must stay transparent");
        assert!(def.outputs.is_empty());
        assert!(!def.cache.enabled);
    }

    /// ...and its def hash must be byte-identical to the pre-transform digest,
    /// so shipping this feature invalidates no existing cached revision.
    #[tokio::test]
    async fn aggregate_group_hash_is_unchanged_by_the_feature() {
        let def = parse_group(&[]).await;
        let mut h = Xxh3::new();
        h.update(b"//pkg:g");
        h.update(b"//pkg:a");
        let expected = format!("{:016x}", h.digest()).into_bytes();
        assert_eq!(
            def.hash, expected,
            "an aggregate group's def hash must not move when transforms exist"
        );
    }

    /// A transform makes the group a real target: it can no longer be inlined,
    /// because inlining would drop the transform out of consumers' cache keys.
    #[tokio::test]
    async fn transform_makes_the_group_non_transparent_and_uncacheable() {
        for (key, value) in [
            ("include", Value::List(vec![Value::String("**/*.so".into())])),
            ("exclude", Value::List(vec![Value::String("**/x".into())])),
            ("strip_prefix", Value::String("build/out".into())),
            ("prefix", Value::String("lib".into())),
            (
                "rename",
                Value::Map(HashMap::from([("a".to_string(), Value::String("b".into()))])),
            ),
        ] {
            let def = parse_group(&[(key, value)]).await;
            assert!(!def.transparent, "`{key}` must make the group concrete");
            assert!(
                !def.cache.enabled,
                "`{key}` group owns no bytes and must stay uncacheable"
            );
            assert_eq!(def.outputs.len(), 1, "`{key}` must declare an output group");
            assert_eq!(def.outputs[0].group, "");
        }
    }

    /// The cache-key property this whole design rests on: change a transform,
    /// and the group's def hash moves — which is what propagates into every
    /// consumer's `hashin`.
    #[tokio::test]
    async fn transform_is_folded_into_the_def_hash() {
        let base = parse_group(&[]).await.hash;
        let lib = parse_group(&[("prefix", Value::String("lib".into()))])
            .await
            .hash;
        let bin = parse_group(&[("prefix", Value::String("bin".into()))])
            .await
            .hash;
        let lib_again = parse_group(&[("prefix", Value::String("lib".into()))])
            .await
            .hash;

        assert_ne!(base, lib, "adding a transform must change the def hash");
        assert_ne!(lib, bin, "changing a transform must change the def hash");
        assert_eq!(lib, lib_again, "the same transform must hash stably");
    }

    /// `rename` is a dict, so its iteration order is not stable; the def hash
    /// must not depend on it or a target would spuriously miss cache.
    #[tokio::test]
    async fn rename_hash_is_independent_of_map_order() {
        let a = parse_group(&[(
            "rename",
            Value::Map(HashMap::from([
                ("x".to_string(), Value::String("1".into())),
                ("y".to_string(), Value::String("2".into())),
            ])),
        )])
        .await
        .hash;
        let b = parse_group(&[(
            "rename",
            Value::Map(HashMap::from([
                ("y".to_string(), Value::String("2".into())),
                ("x".to_string(), Value::String("1".into())),
            ])),
        )])
        .await
        .hash;
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn include_patterns_are_reflected_in_declared_outputs() {
        let def = parse_group(&[(
            "include",
            Value::List(vec![Value::String("bin/**".into())]),
        )])
        .await;
        let PathContent::Glob(ref g) = def.outputs[0].paths[0].content else {
            panic!("expected a glob path, got {:?}", def.outputs[0].paths[0]);
        };
        assert_eq!(g, "bin/**");
    }

    #[tokio::test]
    async fn aggregate_group_run_is_still_a_hard_error() {
        let def = parse_group(&[]).await;
        let err = run_group_err(&def, &[]).await;
        assert!(
            format!("{err:#}").contains("inlined before execution"),
            "{err:#}"
        );
    }

    // A minimal in-memory `Content` standing in for a dep's cached artifact.
    struct FakeArtifact {
        paths: Vec<String>,
    }

    impl hcore::hartifactcontent::Content for FakeArtifact {
        fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
            anyhow::bail!("not used")
        }
        fn walk(
            &self,
        ) -> anyhow::Result<
            Box<dyn Iterator<Item = anyhow::Result<hcore::hartifactcontent::WalkEntry>> + '_>,
        > {
            Ok(Box::new(self.paths.iter().map(|p| {
                Ok(hcore::hartifactcontent::WalkEntry {
                    path: std::path::PathBuf::from(p),
                    kind: hcore::hartifactcontent::WalkEntryKind::File {
                        data: Box::new(std::io::Cursor::new(Vec::new())),
                        x: false,
                    },
                })
            })))
        }
        fn hashout(&self) -> anyhow::Result<String> {
            Ok("dephash".to_string())
        }
        fn entry_paths(&self) -> anyhow::Result<Vec<std::path::PathBuf>> {
            Ok(self
                .paths
                .iter()
                .map(std::path::PathBuf::from)
                .collect())
        }
    }

    async fn run_group(def: &TargetDef, dep_paths: &[&[&str]]) -> anyhow::Result<RunResponse> {
        let inputs: Vec<hplugin::driver::RunInput> = dep_paths
            .iter()
            .enumerate()
            .map(|(i, paths)| hplugin::driver::RunInput {
                artifact: hplugin::driver::inputartifact::InputArtifact {
                    r#type: hplugin::driver::inputartifact::Type::Dep,
                    origin_id: format!("dep{i}"),
                    content: Arc::new(FakeArtifact {
                        paths: paths.iter().map(|s| (*s).to_string()).collect(),
                    }),
                },
                origin_id: format!("dep{i}"),
                source_addr: parse_addr("//pkg:a").expect("addr"),
                filters: vec![],
                annotations: Default::default(),
            })
            .collect();

        Driver
            .run(
                RunRequest {
                    request_id: &"test".to_string(),
                    target: def,
                    tree_root_path: Default::default(),
                    inputs,
                    hashin: "hashin",
                    stdin: None,
                    stdout: None,
                    stderr: None,
                    sandbox_dir: Default::default(),
                },
                &ctoken(),
            )
            .await
    }

    /// `RunResponse` has no `Debug`, so `expect_err` is unavailable.
    async fn run_group_err(def: &TargetDef, dep_paths: &[&[&str]]) -> anyhow::Error {
        match run_group(def, dep_paths).await {
            Ok(_) => panic!("expected an error, got Ok"),
            Err(e) => e,
        }
    }

    #[tokio::test]
    async fn run_produces_a_view_artifact_with_rewritten_paths() {
        let def = parse_group(&[
            ("strip_prefix", Value::String("build/out".into())),
            ("prefix", Value::String("lib".into())),
        ])
        .await;
        let resp = run_group(&def, &[&["build/out/server", "build/out/a/b.so"]])
            .await
            .expect("run");

        assert_eq!(resp.artifacts.len(), 1);
        let art = &resp.artifacts[0];
        assert!(
            matches!(art.content, outputartifact::Content::View(_)),
            "must produce a zero-copy view, not a packed artifact"
        );
        let mut paths: Vec<String> = hcore::hartifactcontent::Content::entry_paths(art)
            .expect("entry paths")
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["lib/a/b.so", "lib/server"]);
    }

    /// The artifact hashout must reflect the transform, since that is what
    /// carries the relocation into a consumer's input hash.
    #[tokio::test]
    async fn run_artifact_hashout_tracks_the_transform() {
        let lib = parse_group(&[("prefix", Value::String("lib".into()))]).await;
        let bin = parse_group(&[("prefix", Value::String("bin".into()))]).await;

        let a = run_group(&lib, &[&["x"]]).await.expect("run").artifacts[0]
            .hashout
            .clone();
        let b = run_group(&bin, &[&["x"]]).await.expect("run").artifacts[0]
            .hashout
            .clone();

        assert_ne!(a, b);
        assert_ne!(a, "dephash", "must not inherit the dep's hashout");
    }

    /// The string form is a transform like any other: it makes the group
    /// concrete so it reaches consumers' cache keys.
    #[tokio::test]
    async fn string_rename_makes_the_group_concrete() {
        let def = parse_group(&[("rename", Value::String("bin/myserver".into()))]).await;
        assert!(!def.transparent);
        assert!(!def.cache.enabled);
    }

    /// `rename` as a string fixes the destination, so a prefix alongside it
    /// would be computed and discarded — rejected rather than silently dropped.
    #[tokio::test]
    async fn string_rename_with_a_prefix_is_rejected() {
        let config = HashMap::from([
            ("deps".to_string(), deps_value(&["//pkg:a"])),
            ("rename".to_string(), Value::String("bin/s".into())),
            ("prefix".to_string(), Value::String("lib".into())),
        ]);
        // `ParseResponse` has no `Debug`, so `expect_err` is unavailable.
        let err = match Driver.parse(make_parse_req("//pkg:g", config), &ctoken()).await {
            Ok(_) => panic!("must reject dead prefix config"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("strip_prefix"), "{msg}");
    }

    /// The dict form coexists with prefixes: it places what it names and the
    /// prefixes place everything else.
    #[tokio::test]
    async fn dict_rename_with_a_prefix_is_allowed() {
        let config = HashMap::from([
            ("deps".to_string(), deps_value(&["//pkg:a"])),
            (
                "rename".to_string(),
                Value::Map(HashMap::from([(
                    "a/x".to_string(),
                    Value::String("out/x".into()),
                )])),
            ),
            ("prefix".to_string(), Value::String("lib".into())),
        ]);
        let def = Driver
            .parse(make_parse_req("//pkg:g", config), &ctoken())
            .await
            .expect("dict rename must coexist with a prefix")
            .target_def;
        assert!(!def.transparent);
    }

    /// The point of the string form: the author writes only a destination, and
    /// it applies to the dep's single output wherever that output lives.
    #[tokio::test]
    async fn string_rename_places_the_sole_output_without_naming_it() {
        let def = parse_group(&[("rename", Value::String("bin/myserver".into()))]).await;
        let resp = run_group(&def, &[&["build/out/server"]]).await.expect("run");
        let paths: Vec<String> = hcore::hartifactcontent::Content::entry_paths(&resp.artifacts[0])
            .expect("entry paths")
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, vec!["bin/myserver"]);
    }

    /// Two candidate outputs and one destination is a hard error naming both.
    #[tokio::test]
    async fn string_rename_with_two_outputs_is_rejected() {
        let def = parse_group(&[("rename", Value::String("bin/s".into()))]).await;
        let err = run_group_err(&def, &[&["a/server", "b/server"]]).await;
        let msg = format!("{err:#}");
        assert!(msg.contains("a/server"), "{msg}");
        assert!(msg.contains("b/server"), "{msg}");
        assert!(msg.contains("//pkg:g"), "error should name the group: {msg}");
    }

    /// Every produced artifact's recorded `hashout` must equal what its own
    /// content answers with. The engine hands the `ViewContent` itself to
    /// consumers on the passthrough path while the *parent's* `hashin` is
    /// computed from the recorded field, so the two disagreeing would key a
    /// cache entry on a value nothing else uses.
    #[tokio::test]
    async fn recorded_hashout_matches_the_artifacts_own_content() {
        let def = parse_group(&[("prefix", Value::String("lib".into()))]).await;
        let resp = run_group(&def, &[&["a/x", "b/y"]]).await.expect("run");
        assert!(!resp.artifacts.is_empty());
        for art in &resp.artifacts {
            let outputartifact::Content::View(v) = &art.content else {
                panic!("expected a view artifact");
            };
            assert_eq!(
                art.hashout,
                v.view.hashout().expect("view hashout"),
                "recorded hashout must match the content's own"
            );
        }
    }

    /// A `strip_prefix` covering one dep but not another is legitimate — the
    /// typo check runs over the union, not per dep.
    #[tokio::test]
    async fn strip_prefix_covering_only_one_dep_is_accepted() {
        let def = parse_group(&[("strip_prefix", Value::String("build/out".into()))]).await;
        let resp = run_group(&def, &[&["build/out/server"], &["assets/logo.png"]])
            .await
            .expect("must accept a prefix that only covers one dep");
        assert_eq!(resp.artifacts.len(), 2);
    }

    #[tokio::test]
    async fn strip_prefix_matching_no_dep_is_rejected() {
        let def = parse_group(&[("strip_prefix", Value::String("nope".into()))]).await;
        let err = run_group_err(&def, &[&["build/out/server"]]).await;
        let msg = format!("{err:#}");
        assert!(msg.contains("nope"), "{msg}");
        assert!(msg.contains("build/out/server"), "should list what is available: {msg}");
    }

    #[tokio::test]
    async fn rename_typo_is_rejected_naming_the_group() {
        let def = parse_group(&[(
            "rename",
            Value::Map(HashMap::from([(
                "sever".to_string(),
                Value::String("bin/s".into()),
            )])),
        )])
        .await;
        let err = run_group_err(&def, &[&["server"]]).await;
        let msg = format!("{err:#}");
        assert!(msg.contains("sever"), "{msg}");
        assert!(msg.contains("//pkg:g"), "error should name the group: {msg}");
    }

    /// Two files colliding must fail here, naming the group, rather than
    /// surfacing later as a staging error inside an unrelated consumer.
    #[tokio::test]
    async fn collision_fails_in_the_group_not_the_consumer() {
        let def = parse_group(&[("strip_prefix", Value::String("a".into()))]).await;
        let err = run_group_err(&def, &[&["a/x", "x"]]).await;
        let msg = format!("{err:#}");
        assert!(msg.contains("collision"), "{msg}");
        assert!(msg.contains("//pkg:g"), "error should name the group: {msg}");
    }
}
