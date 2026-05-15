use crate::engine::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, RunRequest, RunResponse,
    outputartifact::{Content, ContentFile, OutputArtifact, Type},
    targetdef::{
        Output, TargetDef,
        path::{CodegenMode, Content as PathContent, Path},
    },
};
use crate::engine::provider::{
    ConfigRequest as ProviderConfigRequest, ConfigResponse as ProviderConfigResponse, GetError,
    GetRequest, GetResponse, ListPackageResponse, ListPackagesRequest, ListRequest, ListResponse,
    ProbeRequest, ProbeResponse, Provider as EProvider, TargetSpec,
};
use crate::hasync::Cancellable;
use crate::htaddr::Addr;
use crate::htpkg::PkgBuf;
use crate::loosespecparser::TargetSpecValue;
use anyhow::Context;
use async_trait::async_trait;
use futures::future::BoxFuture;
use std::collections::BTreeMap;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

const PKG: &str = "@heph/fs";
const DRIVER_NAME: &str = "fs";

/// Returns the `Addr` for a single-file fs target.
/// Consumers use this to reference a file without knowing the internal address format.
pub fn file_addr(path: &str) -> Addr {
    Addr {
        package: PkgBuf::from(PKG),
        name: "file".to_string(),
        args: BTreeMap::from([("f".to_string(), path.to_string())]),
    }
}

/// Returns the `Addr` for a glob fs target.
/// `exclude` is a list of glob patterns to skip; pass `&[]` for none.
pub fn glob_addr(pattern: &str, exclude: &[&str]) -> Addr {
    let mut args = BTreeMap::from([("p".to_string(), pattern.to_string())]);
    if !exclude.is_empty() {
        args.insert("e".to_string(), exclude.join(","));
    }
    Addr {
        package: PkgBuf::from(PKG),
        name: "glob".to_string(),
        args,
    }
}

// ─── Provider ────────────────────────────────────────────────────────────────

pub struct Provider;

impl EProvider for Provider {
    fn config(&self, _req: ProviderConfigRequest) -> anyhow::Result<ProviderConfigResponse> {
        Ok(ProviderConfigResponse {
            name: "fs".to_string(),
        })
    }

    fn list<'a>(
        &'a self,
        _req: ListRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>>> {
        Box::pin(async move {
            Ok(Box::new(std::iter::empty())
                as Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>)
        })
    }

    fn list_packages<'a>(
        &'a self,
        _req: ListPackagesRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>>>>>
    {
        Box::pin(async move {
            let items: Vec<anyhow::Result<ListPackageResponse>> = vec![Ok(ListPackageResponse {
                pkg: PkgBuf::from(PKG),
            })];
            Ok(Box::new(items.into_iter())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListPackageResponse>>,
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

            match req.addr.name.as_str() {
                "file" | "glob" => {}
                _ => return Err(GetError::NotFound),
            }

            // Forward addr args as config values for the driver.
            let config = req
                .addr
                .args
                .iter()
                .map(|(k, v)| (k.clone(), TargetSpecValue::String(v.clone())))
                .collect();

            Ok(GetResponse {
                target_spec: TargetSpec {
                    addr: req.addr,
                    driver: DRIVER_NAME.to_string(),
                    config,
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

// ─── Driver ──────────────────────────────────────────────────────────────────

enum FsDef {
    File {
        path: String,
    },
    Glob {
        pattern: String,
        exclude: Vec<String>,
    },
}

pub struct Driver;

#[cfg(unix)]
fn is_exec(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_exec(_meta: &std::fs::Metadata) -> bool {
    false
}

fn file_hashout(meta: &std::fs::Metadata) -> String {
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut h = Xxh3::new();
    h.update(&size.to_le_bytes());
    h.update(&mtime.to_le_bytes());
    format!("{:x}", h.digest())
}

#[async_trait]
impl crate::engine::driver::Driver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let get_str = |key: &str| -> anyhow::Result<Option<String>> {
            match req.target_spec.config.get(key) {
                None => Ok(None),
                Some(TargetSpecValue::String(s)) => Ok(Some(s.clone())),
                Some(v) => anyhow::bail!("fs driver: '{}' must be a string, got {:?}", key, v),
            }
        };

        let file_path = get_str("f")?;
        let glob_pattern = get_str("p")?;
        let exclude_raw = get_str("e")?.unwrap_or_default();

        // "e" arg is comma-separated glob patterns.
        let exclude: Vec<String> = if exclude_raw.is_empty() {
            vec![]
        } else {
            exclude_raw.split(',').map(str::to_owned).collect()
        };

        let (def, outputs, hash) = match (file_path, glob_pattern) {
            (Some(path), _) => {
                let mut h = Xxh3::new();
                h.update(b"fs_file");
                h.update(path.as_bytes());
                let hash = format!("{:x}", h.digest()).into_bytes();

                let def = FsDef::File { path: path.clone() };
                let outputs = vec![Output {
                    group: "".to_string(),
                    paths: vec![Path {
                        content: PathContent::FilePath(path),
                        codegen_tree: CodegenMode::None,
                        collect: false,
                    }],
                }];
                (def, outputs, hash)
            }
            (None, Some(pattern)) => {
                let mut h = Xxh3::new();
                h.update(b"fs_glob");
                h.update(pattern.as_bytes());
                for e in &exclude {
                    h.update(e.as_bytes());
                }
                let hash = format!("{:x}", h.digest()).into_bytes();

                let def = FsDef::Glob {
                    pattern: pattern.clone(),
                    exclude,
                };
                let outputs = vec![Output {
                    group: "".to_string(),
                    paths: vec![Path {
                        content: PathContent::Glob(pattern),
                        codegen_tree: CodegenMode::None,
                        collect: false,
                    }],
                }];
                (def, outputs, hash)
            }
            (None, None) => {
                anyhow::bail!("fs driver requires 'f' (file path) or 'p' (glob pattern) in config")
            }
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr,
                labels: req.target_spec.labels,
                raw_def: Arc::new(def),
                inputs: vec![],
                outputs,
                support_files: vec![],
                cache: false,
                disable_remote_cache: true,
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

    async fn run<'a>(
        &self,
        req: RunRequest<'a>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        let def = req.target.def::<FsDef>();
        let root = &req.tree_root_path;

        match def {
            FsDef::File { path } => {
                let abs = root.join(path);
                let meta = std::fs::metadata(&abs)
                    .with_context(|| format!("stat file '{}'", abs.display()))?;
                let x = is_exec(&meta);
                let hashout = file_hashout(&meta);

                let source_path = abs
                    .to_str()
                    .ok_or_else(|| {
                        anyhow::anyhow!("file path is not valid UTF-8: {}", abs.display())
                    })?
                    .to_string();

                let name = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path.as_str())
                    .to_string();

                Ok(RunResponse {
                    artifacts: vec![OutputArtifact {
                        group: "".to_string(),
                        name,
                        r#type: Type::Output,
                        content: Content::File(ContentFile {
                            source_path,
                            out_path: path.clone(),
                            x,
                        }),
                        hashout,
                    }],
                })
            }

            FsDef::Glob { pattern, exclude } => {
                let built_in_excludes = [".git/**/*", ".heph3/**/*", ".heph/**/*"];

                let exclude_patterns = built_in_excludes
                    .iter()
                    .map(|s| s.to_string())
                    .chain(exclude.iter().cloned())
                    .map(|s| {
                        glob::Pattern::new(&s)
                            .with_context(|| format!("invalid exclude pattern: {}", s))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;

                let abs_pattern = root.join(pattern);
                let abs_pattern_str = abs_pattern
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("glob pattern path is not valid UTF-8"))?;

                let mut artifacts = vec![];

                for entry in glob::glob(abs_pattern_str).with_context(|| "expand glob pattern")? {
                    let entry_path = entry.with_context(|| "error reading glob entry")?;

                    if entry_path.is_dir() {
                        continue;
                    }

                    let rel = entry_path
                        .strip_prefix(root)
                        .with_context(|| "strip root prefix from glob entry")?;

                    let rel_str = rel.to_str().ok_or_else(|| {
                        anyhow::anyhow!("glob entry path is not valid UTF-8: {}", rel.display())
                    })?;

                    if exclude_patterns.iter().any(|p| p.matches_path(rel)) {
                        continue;
                    }

                    let meta = std::fs::metadata(&entry_path)
                        .with_context(|| format!("stat glob entry '{}'", entry_path.display()))?;

                    let x = is_exec(&meta);
                    let hashout = file_hashout(&meta);

                    let source_path = entry_path
                        .to_str()
                        .ok_or_else(|| anyhow::anyhow!("glob entry path is not valid UTF-8"))?
                        .to_string();

                    let name = rel_str.replace('/', "_");

                    artifacts.push(OutputArtifact {
                        group: "".to_string(),
                        name,
                        r#type: Type::Output,
                        content: Content::File(ContentFile {
                            source_path,
                            out_path: rel_str.to_string(),
                            x,
                        }),
                        hashout,
                    });
                }

                Ok(RunResponse { artifacts })
            }
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::driver::{Driver as EDriver, RunRequest};
    use crate::engine::provider::Provider as EProvider;
    use crate::hasync::StdCancellationToken;
    use crate::htaddr::parse_addr;
    use std::fs;
    use tempfile::tempdir;

    fn ctoken() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    struct NoopExecutor;
    impl crate::engine::provider::ProviderExecutor for NoopExecutor {
        fn result<'a>(
            &'a self,
            _addr: &'a crate::htaddr::Addr,
        ) -> futures::future::BoxFuture<'a, anyhow::Result<crate::engine::EResult>> {
            Box::pin(async { anyhow::bail!("noop") })
        }
    }

    fn make_get_req(addr_str: &str) -> GetRequest {
        GetRequest {
            request_id: "test".to_string(),
            addr: parse_addr(addr_str).unwrap(),
            states: vec![],
            executor: Arc::new(NoopExecutor),
        }
    }

    // ─── Provider tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_provider_wrong_package_not_found() {
        let p = Provider;
        let result = p
            .get(make_get_req("//other/pkg:file@f=foo.txt"), &ctoken())
            .await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    #[tokio::test]
    async fn test_provider_wrong_name_not_found() {
        let p = Provider;
        let result = p
            .get(
                make_get_req(&format!("//{PKG}:unknown@f=foo.txt")),
                &ctoken(),
            )
            .await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    #[tokio::test]
    async fn test_provider_file_returns_spec() {
        let p = Provider;
        let result = p
            .get(make_get_req(&format!("//{PKG}:file@f=foo.txt")), &ctoken())
            .await
            .unwrap();
        assert_eq!(result.target_spec.driver, DRIVER_NAME);
        assert_eq!(result.target_spec.addr.name, "file");
        assert_eq!(
            result.target_spec.config.get("f"),
            Some(&TargetSpecValue::String("foo.txt".to_string()))
        );
    }

    #[tokio::test]
    async fn test_provider_glob_returns_spec() {
        let p = Provider;
        let result = p
            .get(make_get_req(&format!("//{PKG}:glob@p=src/*.rs")), &ctoken())
            .await
            .unwrap();
        assert_eq!(result.target_spec.driver, DRIVER_NAME);
        assert_eq!(result.target_spec.addr.name, "glob");
        assert_eq!(
            result.target_spec.config.get("p"),
            Some(&TargetSpecValue::String("src/*.rs".to_string()))
        );
    }

    // ─── Driver tests ──────────────────────────────────────────────────────

    fn make_parse_req(config: std::collections::HashMap<String, TargetSpecValue>) -> ParseRequest {
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: TargetSpec {
                addr: parse_addr(&format!("//{PKG}:file")).unwrap(),
                driver: DRIVER_NAME.to_string(),
                config,
                labels: vec![],
                transitive: Default::default(),
            },
        }
    }

    fn make_run_req<'a>(
        target: &'a TargetDef,
        request_id: &'a String,
        root: std::path::PathBuf,
        hashin: &'a String,
    ) -> RunRequest<'a> {
        RunRequest {
            request_id,
            target,
            tree_root_path: root.clone(),
            inputs: vec![],
            hashin,
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: root,
        }
    }

    #[tokio::test]
    async fn test_driver_parse_missing_both_errors() {
        let driver = Driver;
        let result = driver
            .parse(make_parse_req(Default::default()), &ctoken())
            .await;
        let Err(err) = result else {
            panic!("expected error")
        };
        let msg = err.to_string();
        assert!(
            msg.contains("requires 'f' (file path) or 'p'"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn test_driver_parse_file_config() {
        let driver = Driver;
        let config = std::collections::HashMap::from([(
            "f".to_string(),
            TargetSpecValue::String("src/main.rs".to_string()),
        )]);
        let res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();
        let def = res.target_def.def::<FsDef>();
        assert!(matches!(def, FsDef::File { path } if path == "src/main.rs"));
        assert!(!res.target_def.cache);
        assert!(res.target_def.disable_remote_cache);
    }

    #[tokio::test]
    async fn test_driver_parse_glob_config() {
        let driver = Driver;
        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("src/*.rs".to_string()),
        )]);
        let res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();
        let def = res.target_def.def::<FsDef>();
        assert!(matches!(def, FsDef::Glob { pattern, .. } if pattern == "src/*.rs"));
    }

    #[tokio::test]
    async fn test_driver_parse_glob_with_exclude() {
        let driver = Driver;
        let config = std::collections::HashMap::from([
            (
                "p".to_string(),
                TargetSpecValue::String("**/*.rs".to_string()),
            ),
            (
                "e".to_string(),
                TargetSpecValue::String("vendor/**,generated/**".to_string()),
            ),
        ]);
        let res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();
        let def = res.target_def.def::<FsDef>();
        match def {
            FsDef::Glob { exclude, .. } => {
                assert_eq!(exclude, &["vendor/**", "generated/**"]);
            }
            FsDef::File { .. } => panic!("expected glob"),
        }
    }

    #[tokio::test]
    async fn test_driver_run_file_exists() {
        let driver = Driver;
        let tmp = tempdir().unwrap();
        let file_path = tmp.path().join("hello.txt");
        fs::write(&file_path, b"hello world").unwrap();

        let config = std::collections::HashMap::from([(
            "f".to_string(),
            TargetSpecValue::String("hello.txt".to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        let res = driver.run(req, &ctoken()).await.unwrap();

        assert_eq!(res.artifacts.len(), 1);
        let artifact = &res.artifacts[0];
        assert_eq!(artifact.name, "hello.txt");
        assert_eq!(artifact.r#type, Type::Output);
        match &artifact.content {
            Content::File(f) => {
                assert_eq!(f.out_path, "hello.txt");
                assert!(f.source_path.ends_with("hello.txt"));
            }
            _ => panic!("expected File content"),
        }
    }

    #[tokio::test]
    async fn test_driver_run_file_missing_errors() {
        let driver = Driver;
        let tmp = tempdir().unwrap();

        let config = std::collections::HashMap::from([(
            "f".to_string(),
            TargetSpecValue::String("nonexistent.txt".to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        assert!(driver.run(req, &ctoken()).await.is_err());
    }

    #[tokio::test]
    async fn test_driver_run_glob_matches_files() {
        let driver = Driver;
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("a.rs"), b"").unwrap();
        fs::write(tmp.path().join("b.rs"), b"").unwrap();
        fs::write(tmp.path().join("c.txt"), b"").unwrap();

        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("*.rs".to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        let res = driver.run(req, &ctoken()).await.unwrap();

        assert_eq!(res.artifacts.len(), 2);
        let names: Vec<_> = res.artifacts.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"a.rs"));
        assert!(names.contains(&"b.rs"));
    }

    #[tokio::test]
    async fn test_driver_run_glob_excludes_git() {
        let driver = Driver;
        let tmp = tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("config"), b"").unwrap();
        fs::write(tmp.path().join("main.rs"), b"").unwrap();

        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("**/*".to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        let res = driver.run(req, &ctoken()).await.unwrap();

        let names: Vec<_> = res.artifacts.iter().map(|a| &a.name).collect();
        assert!(
            names.iter().all(|n| !n.contains(".git")),
            "should exclude .git: {:?}",
            names
        );
        assert_eq!(res.artifacts.len(), 1);
        assert_eq!(res.artifacts[0].name, "main.rs");
    }

    #[tokio::test]
    async fn test_driver_run_glob_user_exclude() {
        let driver = Driver;
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("keep.rs"), b"").unwrap();
        fs::write(tmp.path().join("skip.rs"), b"").unwrap();

        let config = std::collections::HashMap::from([
            ("p".to_string(), TargetSpecValue::String("*.rs".to_string())),
            (
                "e".to_string(),
                TargetSpecValue::String("skip.rs".to_string()),
            ),
        ]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        let res = driver.run(req, &ctoken()).await.unwrap();

        assert_eq!(res.artifacts.len(), 1);
        assert_eq!(res.artifacts[0].name, "keep.rs");
    }

    #[tokio::test]
    async fn test_driver_run_glob_out_path_relative() {
        let driver = Driver;
        let tmp = tempdir().unwrap();
        let sub = tmp.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("lib.rs"), b"").unwrap();

        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("**/*.rs".to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        let res = driver.run(req, &ctoken()).await.unwrap();

        assert_eq!(res.artifacts.len(), 1);
        let artifact = &res.artifacts[0];
        assert_eq!(artifact.name, "sub_lib.rs");
        match &artifact.content {
            Content::File(f) => {
                assert_eq!(f.out_path, "sub/lib.rs");
            }
            _ => panic!("expected File content"),
        }
    }

    // ─── Addr utility tests ────────────────────────────────────────────────

    #[test]
    fn test_file_addr_package_and_name() {
        let addr = file_addr("src/main.rs");
        assert_eq!(addr.package.as_str(), PKG);
        assert_eq!(addr.name, "file");
        assert_eq!(addr.args.get("f").map(String::as_str), Some("src/main.rs"));
    }

    #[test]
    fn test_file_addr_roundtrips_through_format_and_parse() {
        let addr = file_addr("some/path/foo.txt");
        let formatted = addr.format();
        let parsed = crate::htaddr::parse_addr(&formatted).unwrap();
        assert_eq!(parsed.package, addr.package);
        assert_eq!(parsed.name, addr.name);
        assert_eq!(parsed.args.get("f"), addr.args.get("f"));
    }

    #[test]
    fn test_glob_addr_no_excludes() {
        let addr = glob_addr("src/**/*.rs", &[]);
        assert_eq!(addr.package.as_str(), PKG);
        assert_eq!(addr.name, "glob");
        assert_eq!(addr.args.get("p").map(String::as_str), Some("src/**/*.rs"));
        assert!(!addr.args.contains_key("e"));
    }

    #[test]
    fn test_glob_addr_with_excludes() {
        let addr = glob_addr("**/*.go", &["vendor/**", "gen/**"]);
        assert_eq!(
            addr.args.get("e").map(String::as_str),
            Some("vendor/**,gen/**")
        );
    }

    #[test]
    fn test_glob_addr_roundtrips_through_format_and_parse() {
        let addr = glob_addr("src/*.rs", &["skip.rs"]);
        let formatted = addr.format();
        let parsed = crate::htaddr::parse_addr(&formatted).unwrap();
        assert_eq!(parsed.name, "glob");
        assert_eq!(parsed.args.get("p"), addr.args.get("p"));
        assert_eq!(parsed.args.get("e"), addr.args.get("e"));
    }

    #[tokio::test]
    async fn test_provider_accepts_file_addr() {
        let p = Provider;
        let addr = file_addr("README.md");
        let result = p
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor: Arc::new(NoopExecutor),
                },
                &ctoken(),
            )
            .await
            .unwrap();
        assert_eq!(result.target_spec.driver, DRIVER_NAME);
    }

    #[tokio::test]
    async fn test_provider_accepts_glob_addr() {
        let p = Provider;
        let addr = glob_addr("**/*.rs", &[]);
        let result = p
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor: Arc::new(NoopExecutor),
                },
                &ctoken(),
            )
            .await
            .unwrap();
        assert_eq!(result.target_spec.driver, DRIVER_NAME);
    }
}
