#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! A plugin function that *declares* a target, reached the way a cdylib
//! plugin's function is reached: exported with `make_plugin_functions` and
//! loaded with `decode_functions`, so every call crosses the real stable-ABI
//! function handle and the declaration is encoded and decoded by the
//! `CallFunction` codec. The plugins here have functions and nothing else — no
//! provider — which is the shape functions-belong-to-plugins exists for.
//!
//! The unit tests either side of this prove their own half — `plugin-sdk` that a
//! declaration survives the seam into an `FnOutcome`, `plugin-buildfile` that an
//! `FnOutcome`'s declarations become package targets. This is the joint: a
//! declaration made behind the ABI becomes a target the engine resolves and
//! builds, which is the sentence the feature ships.
//!
//! Not `bin-e2e`: the declarations ride inside the protobuf payload over the
//! function handle, so `dlopen` has nothing new to reject and a separate
//! process would buy nothing (see `.claude/testing.md`).

use hcore::htvalue::Value;
use hcore::htvalue::signature::{FnSignature, Param, ParamType};
use heph::engine::PluginParts;
use hplugin::function::{
    DeclaredState, DeclaredTarget, FnArgs, FnCallContext, FnOutcome, FunctionCaller, PluginFn,
    PluginFnDef,
};
use std::sync::Arc;

/// `heph.codegen.rule(name = …)`: declares a `bash` target that writes the
/// generated file, plus package state, and returns the new target's address.
struct RuleFn;

#[async_trait::async_trait]
impl PluginFn for RuleFn {
    async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome> {
        let name = match args.named.get("name") {
            Some(Value::String(s)) => s.clone(),
            other => anyhow::bail!("rule: `name` must be a string, got {other:?}"),
        };
        let addr = format!("//{}:{}", ctx.pkg, name);
        let mut outcome = FnOutcome::from(Value::String(addr));
        outcome
            .declare_target(DeclaredTarget {
                name,
                driver: "bash".to_string(),
                labels: vec!["codegen".to_string()],
                config: [
                    (
                        "run".to_string(),
                        Value::String("echo generated > $OUT".to_string()),
                    ),
                    ("out".to_string(), Value::String("gen.txt".to_string())),
                ]
                .into(),
                ..Default::default()
            })
            .declare_state(DeclaredState {
                provider: "codegen".to_string(),
                args: [("toolchain".to_string(), Value::String("v1".to_string()))].into(),
            });
        Ok(outcome)
    }
}

/// `heph.wrapper.rule(name = …)`: calls `heph.codegen.rule` through the
/// registry handle its plugin was created with, and relays what it declared.
struct WrapperFn {
    functions: Arc<dyn FunctionCaller>,
}

#[async_trait::async_trait]
impl PluginFn for WrapperFn {
    async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome> {
        let mut out = FnOutcome::from(Value::Null());
        let addr = out.absorb(self.functions.call("codegen", "rule", ctx, args).await?);
        out.set_value(addr);
        Ok(out)
    }
}

fn rule_def(func: Arc<dyn PluginFn>) -> PluginFnDef {
    PluginFnDef {
        name: "rule".to_string(),
        signature: FnSignature {
            positional: vec![],
            named: vec![Param::required("name", ParamType::String)],
            variadic: None,
            returns: ParamType::String,
        },
        doc: "Declare a generated file target.".to_string(),
        func,
    }
}

/// `defs` as a cdylib exports them and the host loads them: every call encodes
/// a `CallFunctionRequest` and decodes a `CallFunctionResponse`.
fn behind_the_abi(plugin: &str, defs: Vec<PluginFnDef>) -> anyhow::Result<Vec<PluginFnDef>> {
    let (named, handle) = hplugin_sdk::stabby::make_plugin_functions(plugin, defs)?;
    hplugin_stabby::load_stable::decode_functions(plugin, named.into_iter().collect(), handle)
}

fn workspace(builder: htestkit::WorkspaceBuilder) -> anyhow::Result<htestkit::Workspace> {
    builder
        .with_provider(|init| {
            Box::new(heph::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
                std::sync::Arc::clone(&init.functions),
            ))
        })
        .with_managed_driver(Box::new(heph::pluginexec::Driver::new_bash()))
        .build()
}

#[tokio::test]
async fn a_declaration_from_behind_the_plugin_abi_becomes_a_package_target() -> anyhow::Result<()> {
    // A function-only plugin: no provider, one function, behind the ABI.
    let ws = workspace(
        htestkit::WorkspaceBuilder::new()?.with_plugin("codegen", |_| {
            Ok(PluginParts::default()
                .with_functions(behind_the_abi("codegen", vec![rule_def(Arc::new(RuleFn))])?))
        }),
    )?;

    // The BUILD file writes no `target()` of its own — the plugin declares it.
    ws.write_build_file("p", r#"gen = heph.codegen.rule(name = "gen")"#);

    // It resolves as a target of the package, carrying what was declared behind
    // the seam, including the driver and the labels.
    let spec = ws.get_spec("//p:gen").await?;
    assert_eq!(spec.driver, "bash");
    assert_eq!(spec.labels, vec!["codegen".to_string()]);

    // …and it builds, which is the whole point: a plugin paved a tool without
    // shipping a provider or driver of its own.
    let result = ws.run("//p:gen").await?;
    assert_eq!(htestkit::artifact_string(&result).trim(), "generated");
    Ok(())
}

/// I5 end to end: function-only plugin `wrapper` calls function-only plugin
/// `codegen` through the registry its create entry was handed — by
/// `(plugin, fn)`, back over the seam — and `codegen`'s declared target lands
/// in the calling package because `wrapper` absorbs it.
#[tokio::test]
async fn declarations_absorbed_across_a_plugin_hop() -> anyhow::Result<()> {
    let ws = workspace(
        htestkit::WorkspaceBuilder::new()?
            .with_plugin("codegen", |_| {
                Ok(PluginParts::default()
                    .with_functions(behind_the_abi("codegen", vec![rule_def(Arc::new(RuleFn))])?))
            })
            .with_plugin("wrapper", |init| {
                // What a cdylib's create entry receives: this engine's registry,
                // as an ABI handle, resolved through the engine's slot.
                let functions = hplugin_sdk::stabby::guest_functions(
                    hplugin_stabby::host::HostFunctionRegistry::wrap(
                        Arc::clone(&init.functions),
                        init.runtime.clone(),
                    ),
                );
                Ok(PluginParts::default().with_functions(behind_the_abi(
                    "wrapper",
                    vec![rule_def(Arc::new(WrapperFn { functions }))],
                )?))
            }),
    )?;

    ws.write_build_file("p", r#"gen = heph.wrapper.rule(name = "gen")"#);

    let spec = ws.get_spec("//p:gen").await?;
    assert_eq!(
        spec.driver, "bash",
        "codegen's declaration crossed two hops"
    );
    assert_eq!(spec.labels, vec!["codegen".to_string()]);
    let result = ws.run("//p:gen").await?;
    assert_eq!(htestkit::artifact_string(&result).trim(), "generated");
    Ok(())
}

/// `heph.heavy.rule(name = …)`: declares a `bash` target whose config carries
/// `resources`, the second place a declaration is written (after `target()`).
struct HeavyFn {
    resources: Value,
}

#[async_trait::async_trait]
impl PluginFn for HeavyFn {
    async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome> {
        let Some(Value::String(name)) = args.named.get("name").cloned() else {
            anyhow::bail!("rule: `name` must be a string");
        };
        let addr = format!("//{}:{}", ctx.pkg, name);
        let mut outcome = FnOutcome::from(Value::String(addr));
        outcome.declare_target(DeclaredTarget {
            name,
            driver: "bash".to_string(),
            config: [
                (
                    "run".to_string(),
                    Value::String("echo heavy > $OUT".to_string()),
                ),
                ("out".to_string(), Value::String("heavy.txt".to_string())),
                ("resources".to_string(), self.resources.clone()),
            ]
            .into(),
            ..Default::default()
        });
        Ok(outcome)
    }
}

fn heavy_workspace(resources: Value) -> anyhow::Result<htestkit::Workspace> {
    let ws = workspace(
        htestkit::WorkspaceBuilder::new()?.with_plugin("heavy", move |_| {
            Ok(PluginParts::default().with_functions(behind_the_abi(
                "heavy",
                vec![rule_def(Arc::new(HeavyFn { resources }))],
            )?))
        }),
    )?;
    ws.write_build_file("p", r#"heavy = heph.heavy.rule(name = "heavy")"#);
    Ok(ws)
}

/// A declaration's `resources` crosses the function seam with the rest of its
/// config and lands on the def, as `target()`'s does; and the target builds.
#[tokio::test]
async fn declared_target_carries_resources() -> anyhow::Result<()> {
    let ws = heavy_workspace(Value::Map(
        [
            ("cpu".to_string(), Value::Int(2)),
            ("memory".to_string(), Value::String("1G".to_string())),
        ]
        .into(),
    ))?;
    let rs = ws.engine.new_state();
    let def = Arc::clone(&ws.engine)
        .get_def(rs, &heph::htaddr::parse_addr("//p:heavy")?)
        .await?;
    assert_eq!(def.target_def.resources.declared("cpu"), Some(2_000_000));
    assert_eq!(def.target_def.resources.declared("memory"), Some(1 << 30));
    let result = ws.run("//p:heavy").await?;
    assert_eq!(htestkit::artifact_string(&result).trim(), "heavy");
    Ok(())
}

/// A pool name the SDK helper refuses fails the declared target's def, naming
/// the target and the bad name, as it does for `target()`.
#[tokio::test]
async fn declared_target_with_a_bad_pool_name_is_refused() -> anyhow::Result<()> {
    let ws = heavy_workspace(Value::Map(
        [("Docker-Pool".to_string(), Value::Int(1))].into(),
    ))?;
    let rs = ws.engine.new_state();
    let err = Arc::clone(&ws.engine)
        .get_def(rs, &heph::htaddr::parse_addr("//p:heavy")?)
        .await
        .err()
        .expect("a bad pool name is refused");
    let err = format!("{err:#}");
    assert!(err.contains("//p:heavy"), "names the target: {err}");
    assert!(
        err.contains(r#""Docker-Pool" is not a valid pool name"#),
        "names the pool: {err}"
    );
    Ok(())
}
