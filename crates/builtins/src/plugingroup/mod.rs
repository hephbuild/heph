use anyhow::Context as _;
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hplugin::driver::TargetAddr;
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, RunRequest, RunResponse,
    targetdef::{CacheConfig, Input, InputMode, TargetDef},
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
}

#[derive(serde::Serialize)]
struct GroupDef;

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
        let deps = GroupSpec::from(req.target_spec.config.clone())
            .context("parse group config")?
            .deps;

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
        let hash = format!("{:016x}", h.digest()).into_bytes();

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(GroupDef),
                inputs,
                outputs: vec![],
                support_files: vec![],
                cache: CacheConfig::off(),
                pty: false,
                hash,
                transparent: true,
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

    async fn run<'a, 'io>(
        &self,
        _req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        anyhow::bail!(
            "group driver run() must never be called — groups are inlined before execution"
        )
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

    #[test]
    fn test_schema_lists_deps() {
        use hcore::htvalue::signature::ParamType;
        use hplugin::driver::Driver as _;
        let schema = Driver.schema();
        assert_eq!(schema.fields.len(), 1);
        let f = &schema.fields[0];
        assert_eq!(f.name, "deps");
        assert_eq!(
            f.ty,
            ParamType::union(vec![ParamType::String, ParamType::list(ParamType::String)])
        );
        assert!(!f.doc.is_empty());
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
}
