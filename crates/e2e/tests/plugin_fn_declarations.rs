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
//! Not `bin-e2e`: the declarations ride inside the prost payload over the
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
