use heph_plugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, RunRequest, RunResponse, outputartifact,
    targetdef::{
        CacheConfig, Output, TargetDef,
        path::{CodegenMode, Content, Path},
    },
};
use heph_plugin::provider::{
    ConfigRequest as ProviderConfigRequest, ConfigResponse as ProviderConfigResponse, GetError,
    GetRequest, GetResponse, ListPackageResponse, ListPackagesRequest, ListRequest, ListResponse,
    ProbeRequest, ProbeResponse, Provider as EProvider, TargetSpec,
};
use heph_core::hasync::Cancellable;
use heph_model::htpkg::PkgBuf;
use async_trait::async_trait;
use futures::future::BoxFuture;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

const PKG: &str = "@heph/bin";
const DRIVER_NAME: &str = "hostbin";

fn find_binary(name: &str) -> anyhow::Result<String> {
    let path = which::which(name)?;
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("binary path for '{}' is not valid UTF-8", name))
}

pub struct Provider;

impl EProvider for Provider {
    fn config(&self, _req: ProviderConfigRequest) -> anyhow::Result<ProviderConfigResponse> {
        Ok(ProviderConfigResponse {
            name: "pluginhostbin".to_string(),
        })
    }

    fn list<'a>(
        &'a self,
        _req: ListRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>
    {
        Box::pin(async move {
            Ok(Box::new(std::iter::empty())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListResponse>> + Send,
                >)
        })
    }

    fn list_packages<'a>(
        &'a self,
        _req: ListPackagesRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    > {
        Box::pin(async move {
            let items: Vec<anyhow::Result<ListPackageResponse>> = vec![Ok(ListPackageResponse {
                pkg: PkgBuf::from(PKG),
            })];
            Ok(Box::new(items.into_iter())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                >)
        })
    }

    fn get<'a>(
        &'a self,
        req: GetRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
        Box::pin(async move {
            if req.addr.package != PKG {
                return Err(GetError::NotFound);
            }

            Ok(GetResponse {
                target_spec: TargetSpec {
                    addr: req.addr,
                    driver: DRIVER_NAME.to_string(),
                    config: Default::default(),
                    labels: vec![],
                    transitive: Default::default(),
                },
            })
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

#[derive(serde::Serialize)]
struct HostBinDef {
    bin_name: String,
}

pub struct Driver;

#[async_trait]
impl heph_plugin::driver::Driver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    /// The hostbin driver takes no target config (it keys off the target name).
    fn schema(&self) -> heph_plugin::driver::DriverSchema {
        heph_plugin::driver::DriverSchema::default()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let bin_name = req.target_spec.addr.name.clone();

        let mut h = Xxh3::new();
        h.update(bin_name.as_bytes());
        let hash = format!("{:x}", h.digest()).into_bytes();

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(HostBinDef {
                    bin_name: bin_name.clone(),
                }),
                inputs: vec![],
                outputs: vec![Output {
                    group: "".to_string(),
                    paths: vec![Path {
                        content: Content::FilePath(format!("__heph/hostbin/{}", bin_name)),
                        codegen_tree: CodegenMode::None,
                        collect: false,
                    }],
                }],
                support_files: vec![],
                cache: CacheConfig::off(),
                pty: false,
                hash,
                transparent: false,
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
        req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        let def = req.target.def::<HostBinDef>();
        let bin_path = find_binary(&def.bin_name)?;

        let script = format!("#!/bin/sh\nexec {} \"$@\"\n", bin_path);
        let data = script.into_bytes();

        let mut h = Xxh3::new();
        h.update(&data);
        let hashout = format!("{:x}", h.digest());

        Ok(RunResponse {
            artifacts: vec![outputartifact::OutputArtifact {
                group: "".to_string(),
                name: def.bin_name.clone(),
                r#type: outputartifact::Type::Output,
                content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                    data,
                    path: format!("__heph/hostbin/{}", def.bin_name),
                    x: true,
                }),
                hashout,
            }],
            ..Default::default()
        })
    }

    async fn run_shell<'a, 'io>(
        &self,
        _req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        anyhow::bail!("run_shell not implemented for hostbin")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use heph_plugin::driver::{Driver as EDriver, RunRequest};
    use heph_plugin::provider::Provider as EProvider;
    use heph_core::hasync::StdCancellationToken;
    use heph_model::htaddr::parse_addr;

    fn make_ctoken() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    // --- provider ---

    #[tokio::test]
    async fn test_provider_get_wrong_package_returns_not_found() {
        let provider = Provider;
        let ctoken = make_ctoken();
        let addr = parse_addr("//other/pkg:sh").unwrap();
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor: std::sync::Arc::new(NoopExecutor),
                },
                &ctoken,
            )
            .await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    #[tokio::test]
    async fn test_provider_get_any_name_returns_spec() {
        let provider = Provider;
        let ctoken = make_ctoken();
        let addr = parse_addr("//@heph/bin:definitely_not_installed_xyz").unwrap();
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor: std::sync::Arc::new(NoopExecutor),
                },
                &ctoken,
            )
            .await
            .unwrap();
        assert_eq!(result.target_spec.driver, DRIVER_NAME);
        assert_eq!(result.target_spec.addr.name, "definitely_not_installed_xyz");
    }

    #[tokio::test]
    async fn test_provider_list_packages_returns_heph_bin() {
        let provider = Provider;
        let ctoken = make_ctoken();
        let result = provider
            .list_packages(
                ListPackagesRequest {
                    prefix: PkgBuf::from(""),
                },
                &ctoken,
            )
            .await
            .unwrap();
        let pkgs: Vec<_> = result.map(|r| r.unwrap().pkg).collect();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0], PKG);
    }

    #[tokio::test]
    async fn test_provider_list_returns_empty() {
        let provider = Provider;
        let ctoken = make_ctoken();
        let result = provider
            .list(
                ListRequest {
                    request_id: "test".to_string(),
                    package: PkgBuf::from(PKG),
                    states: vec![],
                },
                &ctoken,
            )
            .await
            .unwrap();
        assert_eq!(result.count(), 0);
    }

    // --- driver ---

    fn make_parse_request(bin_name: &str) -> ParseRequest {
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: std::sync::Arc::new(TargetSpec {
                addr: parse_addr(&format!("//@heph/bin:{}", bin_name)).unwrap(),
                driver: DRIVER_NAME.to_string(),
                config: Default::default(),
                labels: vec![],
                transitive: Default::default(),
            }),
        }
    }

    fn make_run_request<'a>(
        target: &'a TargetDef,
        request_id: &'a String,
        sandbox_dir: &std::path::Path,
        hashin: &'a String,
    ) -> RunRequest<'a, 'static> {
        RunRequest {
            request_id,
            target,
            tree_root_path: sandbox_dir.to_path_buf(),
            inputs: vec![],
            hashin,
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox_dir.to_path_buf(),
        }
    }

    #[tokio::test]
    async fn test_driver_parse_stores_bin_name() {
        let driver = Driver;
        let ctoken = make_ctoken();
        let res = driver
            .parse(make_parse_request("sh"), &ctoken)
            .await
            .unwrap();
        let def = res.target_def.def::<HostBinDef>();
        assert_eq!(def.bin_name, "sh");
    }

    #[tokio::test]
    async fn test_driver_run_missing_binary_errors() {
        let driver = Driver;
        let ctoken = make_ctoken();
        let parse_res = driver
            .parse(
                make_parse_request("__definitely_not_a_real_binary_xyz__"),
                &ctoken,
            )
            .await
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_request(&parse_res.target_def, &request_id, tmp.path(), &hashin);
        assert!(driver.run(req, &ctoken).await.is_err());
    }

    #[tokio::test]
    async fn test_driver_run_produces_exec_script() {
        let driver = Driver;
        let ctoken = make_ctoken();
        let parse_res = driver
            .parse(make_parse_request("sh"), &ctoken)
            .await
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_request(&parse_res.target_def, &request_id, tmp.path(), &hashin);

        let res = driver.run(req, &ctoken).await.unwrap();
        assert_eq!(res.artifacts.len(), 1);

        let artifact = &res.artifacts[0];
        assert_eq!(artifact.name, "sh");
        assert_eq!(artifact.r#type, outputartifact::Type::Output);

        let script = match &artifact.content {
            outputartifact::Content::Raw(raw) => {
                assert!(raw.x, "script must be executable");
                assert_eq!(raw.path, "__heph/hostbin/sh");
                String::from_utf8(raw.data.clone()).unwrap()
            }
            _ => panic!("expected Raw content"),
        };

        assert!(script.starts_with("#!/bin/sh\n"), "missing shebang");
        let sh_path = find_binary("sh").unwrap();
        assert!(
            script.contains(&format!("exec {} \"$@\"", sh_path)),
            "exec line missing or wrong path"
        );
    }

    #[tokio::test]
    async fn test_driver_run_hash_is_stable() {
        let driver = Driver;
        let ctoken = make_ctoken();
        let parse_res = driver
            .parse(make_parse_request("sh"), &ctoken)
            .await
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let request_id = "test".to_string();
        let hashin = String::new();

        let req1 = make_run_request(&parse_res.target_def, &request_id, tmp.path(), &hashin);
        let req2 = make_run_request(&parse_res.target_def, &request_id, tmp.path(), &hashin);

        let res1 = driver.run(req1, &ctoken).await.unwrap();
        let res2 = driver.run(req2, &ctoken).await.unwrap();
        assert_eq!(res1.artifacts[0].hashout, res2.artifacts[0].hashout);
    }

    struct NoopExecutor;
    impl heph_plugin::provider::ProviderExecutor for NoopExecutor {
        fn result<'a>(
            &'a self,
            _addr: &'a heph_model::htaddr::Addr,
        ) -> futures::future::BoxFuture<'a, anyhow::Result<std::sync::Arc<heph_plugin::eresult::EResult>>>
        {
            Box::pin(async { anyhow::bail!("noop") })
        }

        fn query<'a>(
            &'a self,
            _m: &'a heph_model::htmatcher::Matcher,
            _extra_skip: &'a [String],
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<heph_model::htaddr::Addr>>> {
            Box::pin(async { anyhow::bail!("noop") })
        }
    }
}
