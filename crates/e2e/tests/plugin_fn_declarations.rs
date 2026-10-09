#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! A provider function that *declares* a target, reached the way a cdylib plugin
//! is reached: through `make_dyn_provider` + `StableRemoteProvider`, so the call
//! crosses the real stable-ABI dispatch and the declaration is encoded and
//! decoded by the `CallFunction` codec.
//!
//! The unit tests either side of this prove their own half — `plugin-sdk` that a
//! declaration survives the seam into an `FnOutcome`, `plugin-buildfile` that an
//! `FnOutcome`'s declarations become package targets. This is the joint: a
//! declaration made behind the ABI becomes a target the engine resolves and
//! builds, which is the sentence the feature ships.
//!
//! Not `bin-e2e`: the declarations ride inside the existing prost payload over
//! unchanged vtable slots, so `dlopen` has nothing new to reject and a separate
//! process would buy nothing (see `.claude/testing.md`).

use hcore::htvalue::Value;
use hcore::htvalue::signature::{FnSignature, Param, ParamType};
use heph::engine::provider::{
    ConfigRequest, ConfigResponse, FnArgs, FnCallContext, GetError, GetRequest, GetResponse,
    ListPackageResponse, ListPackagesRequest, ListRequest, ListResponse, ProbeRequest,
    ProbeResponse, Provider, ProviderFn, ProviderFunctionDef,
};
use heph::hasync::Cancellable;
use hplugin::provider::{DeclaredState, DeclaredTarget, FnOutcome};
use std::sync::Arc;

/// `heph.codegen.rule(name = …)`: declares a `bash` target that writes the
/// generated file, plus package state, and returns the new target's address.
struct RuleFn;

#[async_trait::async_trait]
impl ProviderFn for RuleFn {
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

/// A provider that exposes nothing but that one function.
struct CodegenProvider;

impl Provider for CodegenProvider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "codegen".to_string(),
        })
    }
    fn list<'a>(
        &'a self,
        _req: ListRequest,
        _ct: &'a (dyn Cancellable + Send + Sync),
    ) -> futures::future::BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
    > {
        Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<_>) })
    }
    fn list_packages<'a>(
        &'a self,
        _req: ListPackagesRequest,
        _ct: &'a (dyn Cancellable + Send + Sync),
    ) -> futures::future::BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    > {
        Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<_>) })
    }
    fn get<'a>(
        &'a self,
        _req: GetRequest,
        _ct: &'a (dyn Cancellable + Send + Sync),
    ) -> futures::future::BoxFuture<'a, Result<GetResponse, GetError>> {
        Box::pin(async { Err(GetError::NotFound) })
    }
    fn probe<'a>(
        &'a self,
        _req: ProbeRequest,
        _ct: &'a (dyn Cancellable + Send + Sync),
    ) -> futures::future::BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async { Ok(ProbeResponse { states: vec![] }) })
    }
    fn functions(&self) -> Vec<ProviderFunctionDef> {
        vec![ProviderFunctionDef {
            name: "rule".to_string(),
            signature: FnSignature {
                positional: vec![],
                named: vec![Param::required("name", ParamType::String)],
                variadic: None,
                returns: ParamType::String,
            },
            doc: "Declare a generated file target.".to_string(),
            func: Arc::new(RuleFn),
        }]
    }
}

#[tokio::test]
async fn a_declaration_from_behind_the_plugin_abi_becomes_a_package_target() -> anyhow::Result<()> {
    let ws = htestkit::WorkspaceBuilder::new()?
        .with_provider(|init| {
            Box::new(heph::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
            ))
        })
        // Reached exactly as a loaded cdylib is: every call to `rule` encodes a
        // `CallFunctionRequest` and decodes a `CallFunctionResponse`.
        .with_provider(|_| {
            Box::new(hplugin_stabby::load_stable::StableRemoteProvider::new(
                hplugin_sdk::stabby::make_dyn_provider(Arc::new(CodegenProvider)),
                "codegen",
            ))
        })
        .with_managed_driver(Box::new(heph::pluginexec::Driver::new_bash()))
        .build()?;

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
