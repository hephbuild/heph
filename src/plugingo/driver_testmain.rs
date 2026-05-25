use crate::debug_hash::DebugHasher;
use crate::engine::driver::targetdef::path::{CodegenMode, Content, Path};
use crate::engine::driver::targetdef::{Input, InputMode, Output, TargetDef};
use crate::engine::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use crate::engine::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use crate::hasync::Cancellable;
use crate::loosespecparser::parse_map_string_strings;
use crate::plugingo::gen_testmain::{analyze_test_main, generate_testmain};
use crate::plugingo::pkg_analysis::decode_go_package;
use anyhow::Context;
use async_trait::async_trait;
use std::hash::{Hash, Hasher};
use std::io::BufRead;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

pub struct GoTestmainDriver;

#[derive(Clone, serde::Serialize, PartialEq, Eq)]
enum TestmainMode {
    /// Internal tests only (pkg.test_go_files). testmain.go imports `_test "P"`.
    Internal,
    /// External xtests only (pkg.xtest_go_files). testmain.go imports `_xtest "P_test"`.
    Xtest,
    /// Both (legacy/combined). Kept for callers that haven't split yet.
    Both,
}

#[derive(Clone, serde::Serialize)]
struct GoTestmainDef {
    golist_origin_id: String,
    mode: TestmainMode,
}

impl Hash for GoTestmainDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.golist_origin_id.hash(state);
        match self.mode {
            TestmainMode::Internal => 0u8.hash(state),
            TestmainMode::Xtest => 1u8.hash(state),
            TestmainMode::Both => 2u8.hash(state),
        }
    }
}

#[async_trait]
impl ManagedDriver for GoTestmainDriver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "go_testmain".to_string(),
        })
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let pkg = req.target_spec.addr.package.clone();
        let pkg_str = pkg.as_str();
        let config = &req.target_spec.config;

        let deps = config
            .get("deps")
            .map(|v| parse_map_string_strings(v).context("parse deps"))
            .transpose()?
            .unwrap_or_default();

        let golist_addrs = deps
            .get("golist")
            .ok_or_else(|| anyhow::anyhow!("go_testmain: missing deps.golist"))?;

        if golist_addrs.is_empty() {
            anyhow::bail!("go_testmain: deps.golist must have at least one entry");
        }

        let golist_origin_id = "dep|golist|0".to_string();

        let mut inputs: Vec<Input> = Vec::new();
        for (i, addr_str) in golist_addrs.iter().enumerate() {
            inputs.push(Input {
                r#ref: TargetAddr::parse(addr_str, &pkg)
                    .with_context(|| format!("parse golist dep addr {addr_str}"))?,
                mode: InputMode::Standard,
                origin_id: format!("dep|golist|{i}"),
                annotations: std::collections::BTreeMap::new(),
                hashed: true,
                runtime: true,
            });
        }

        // Test source files: sandboxed inputs so the driver never reaches outside
        // the sandbox. Each addr is a pluginfs file or a codegen TargetAddr resolved
        // by the provider — already present in deps.test / deps.xtest at spec build time.
        for group in ["test", "xtest"] {
            if let Some(file_addrs) = deps.get(group) {
                for (i, addr_str) in file_addrs.iter().enumerate() {
                    inputs.push(Input {
                        r#ref: TargetAddr::parse(addr_str, &pkg)
                            .with_context(|| format!("parse {group} src dep addr {addr_str}"))?,
                        mode: InputMode::Standard,
                        origin_id: format!("dep|{group}|{i}"),
                        annotations: std::collections::BTreeMap::new(),
                        hashed: true,
                        runtime: true,
                    });
                }
            }
        }

        let mode = match config.get("mode") {
            Some(crate::loosespecparser::TargetSpecValue::String(s)) => match s.as_str() {
                "internal" => TestmainMode::Internal,
                "xtest" => TestmainMode::Xtest,
                "both" => TestmainMode::Both,
                other => anyhow::bail!("go_testmain: unknown mode {:?}", other),
            },
            // No mode = back-compat with combined-bin callers.
            None => TestmainMode::Both,
            Some(other) => {
                anyhow::bail!("go_testmain: mode must be string, got {:?}", other)
            }
        };

        let def = GoTestmainDef {
            golist_origin_id,
            mode,
        };

        let out_strings = config
            .get("out")
            .map(|v| parse_map_string_strings(v).context("parse out"))
            .transpose()?
            .unwrap_or_default();

        let outputs = out_strings
            .iter()
            .map(|(group, paths)| Output {
                group: group.clone(),
                paths: paths
                    .iter()
                    .map(|p| {
                        let full_path = if pkg_str.is_empty() {
                            p.clone()
                        } else {
                            format!("{pkg_str}/{p}")
                        };
                        Path {
                            content: Content::FilePath(full_path),
                            codegen_tree: CodegenMode::None,
                            collect: true,
                        }
                    })
                    .collect(),
            })
            .collect();

        let hash = {
            let mut h = DebugHasher::new(
                Xxh3Default::new(),
                &format!("go_testmain_{}", req.target_spec.addr.format()),
            );
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                outputs,
                support_files: vec![],
                cache: true,
                disable_remote_cache: false,
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
        req: ManagedRunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def::<GoTestmainDef>();

        let managed = req
            .inputs
            .iter()
            .find(|m| m.input.origin_id == def.golist_origin_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "go_testmain: golist input {} not found",
                    def.golist_origin_id
                )
            })?;

        let list_path = managed.require_list_path()?;
        let list_f = std::fs::File::open(list_path)
            .with_context(|| format!("open golist list {:?}", list_path))?;
        let pkg_bin_path = std::io::BufReader::new(list_f)
            .lines()
            .find(|l| {
                l.as_ref()
                    .is_ok_and(|s| !s.is_empty() && s.ends_with("package.bin"))
            })
            .ok_or_else(|| {
                anyhow::anyhow!("go_testmain: package.bin not found in golist input")
            })??;

        let pkg_bin_file =
            std::fs::File::open(&pkg_bin_path).with_context(|| "open package.bin")?;
        let pkg = decode_go_package(pkg_bin_file).context("decode package.bin")?;

        // Test sources are declared inputs (deps.test / deps.xtest), staged by the
        // engine into the sandbox package dir. Pass only relative names so absolute
        // sandbox paths never leak into the cached analysis; analyzer streams files
        // line-by-line out of `sandbox_pkg_dir`.
        //
        // `mode` gates which sets are analyzed so the internal testmain doesn't
        // try to open xtest files (and vice versa). The split-bin design relies
        // on this: internal bin's testmain only sees `_test "P"`, xtest bin's
        // testmain only sees `_xtest "P_test"`.
        let mut file_refs: Vec<(&str, &str)> =
            Vec::with_capacity(pkg.test_go_files.len() + pkg.xtest_go_files.len());
        if matches!(def.mode, TestmainMode::Internal | TestmainMode::Both) {
            for f in &pkg.test_go_files {
                file_refs.push(("_test", f.as_str()));
            }
        }
        if matches!(def.mode, TestmainMode::Xtest | TestmainMode::Both) {
            for f in &pkg.xtest_go_files {
                file_refs.push(("_xtest", f.as_str()));
            }
        }

        let analysis = analyze_test_main(&pkg.import_path, &req.sandbox_pkg_dir, &file_refs)
            .context("analyze test files")?;
        let content = generate_testmain(&analysis);

        std::fs::write(req.sandbox_pkg_dir.join("testmain.go"), content)
            .context("write testmain.go")?;

        Ok(ManagedRunResponse { artifacts: vec![] })
    }

    async fn run_shell<'a, 'io>(
        &self,
        _req: ManagedRunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        anyhow::bail!("run_shell not implemented for go_testmain")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::provider::TargetSpec;
    use crate::htaddr::Addr;
    use crate::htpkg::PkgBuf;
    use crate::loosespecparser::TargetSpecValue;
    use std::collections::HashMap;

    fn driver() -> GoTestmainDriver {
        GoTestmainDriver
    }

    fn noop_ctoken() -> crate::hasync::StdCancellationToken {
        crate::hasync::StdCancellationToken::new()
    }

    fn make_parse_request(pkg: &str, golist_addr: &str) -> ParseRequest {
        let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
        config.insert(
            "deps".to_string(),
            TargetSpecValue::Map(HashMap::from([(
                "golist".to_string(),
                TargetSpecValue::List(vec![TargetSpecValue::String(golist_addr.to_string())]),
            )])),
        );
        config.insert(
            "out".to_string(),
            TargetSpecValue::Map(HashMap::from([(
                "go".to_string(),
                TargetSpecValue::List(vec![TargetSpecValue::String("testmain.go".to_string())]),
            )])),
        );

        ParseRequest {
            request_id: "test".to_string(),
            target_spec: std::sync::Arc::new(TargetSpec {
                addr: Addr::new(
                    PkgBuf::from(pkg),
                    "testmain".to_string(),
                    Default::default(),
                ),
                driver: "go_testmain".to_string(),
                config,
                labels: vec![],
                transitive: Default::default(),
            }),
        }
    }

    #[test]
    fn test_driver_name_is_go_testmain() {
        let resp = driver().config(ConfigRequest {}).unwrap();
        assert_eq!(resp.name, "go_testmain");
    }

    #[tokio::test]
    async fn test_parse_includes_golist_input() {
        let ct = noop_ctoken();
        let req = make_parse_request("mypkg", "//mypkg:_golist|pkg");
        let resp = driver().parse(req, &ct).await.unwrap();
        assert!(
            resp.target_def
                .inputs
                .iter()
                .any(|i| i.origin_id == "dep|golist|0"),
            "golist input must be present"
        );
    }

    #[tokio::test]
    async fn test_parse_includes_test_and_xtest_file_inputs() {
        let ct = noop_ctoken();
        let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
        config.insert(
            "deps".to_string(),
            TargetSpecValue::Map(HashMap::from([
                (
                    "golist".to_string(),
                    TargetSpecValue::List(vec![TargetSpecValue::String(
                        "//mypkg:_golist|pkg".to_string(),
                    )]),
                ),
                (
                    "test".to_string(),
                    TargetSpecValue::List(vec![
                        TargetSpecValue::String(
                            "//@heph/fs:.read?path=mypkg/a_test.go".to_string(),
                        ),
                        TargetSpecValue::String(
                            "//@heph/fs:.read?path=mypkg/b_test.go".to_string(),
                        ),
                    ]),
                ),
                (
                    "xtest".to_string(),
                    TargetSpecValue::List(vec![TargetSpecValue::String(
                        "//@heph/fs:.read?path=mypkg/c_test.go".to_string(),
                    )]),
                ),
            ])),
        );
        config.insert(
            "out".to_string(),
            TargetSpecValue::Map(HashMap::from([(
                "go".to_string(),
                TargetSpecValue::List(vec![TargetSpecValue::String("testmain.go".to_string())]),
            )])),
        );
        let req = ParseRequest {
            request_id: "test".to_string(),
            target_spec: std::sync::Arc::new(TargetSpec {
                addr: Addr::new(
                    PkgBuf::from("mypkg"),
                    "testmain".to_string(),
                    Default::default(),
                ),
                driver: "go_testmain".to_string(),
                config,
                labels: vec![],
                transitive: Default::default(),
            }),
        };
        let resp = driver().parse(req, &ct).await.unwrap();
        let ids: Vec<&str> = resp
            .target_def
            .inputs
            .iter()
            .map(|i| i.origin_id.as_str())
            .collect();
        assert!(ids.contains(&"dep|test|0"), "missing dep|test|0 in {ids:?}");
        assert!(ids.contains(&"dep|test|1"), "missing dep|test|1 in {ids:?}");
        assert!(
            ids.contains(&"dep|xtest|0"),
            "missing dep|xtest|0 in {ids:?}"
        );
    }

    #[tokio::test]
    async fn test_parse_missing_golist_errors() {
        let ct = noop_ctoken();
        let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
        config.insert("out".to_string(), TargetSpecValue::Map(HashMap::new()));
        let req = ParseRequest {
            request_id: "test".to_string(),
            target_spec: std::sync::Arc::new(TargetSpec {
                addr: Addr::new(
                    PkgBuf::from("mypkg"),
                    "testmain".to_string(),
                    Default::default(),
                ),
                driver: "go_testmain".to_string(),
                config,
                labels: vec![],
                transitive: Default::default(),
            }),
        };
        assert!(driver().parse(req, &ct).await.is_err());
    }

    #[tokio::test]
    async fn test_parse_out_prepends_package() {
        let ct = noop_ctoken();
        let req = make_parse_request("mypkg", "//mypkg:_golist|pkg");
        let resp = driver().parse(req, &ct).await.unwrap();
        let go_out = resp
            .target_def
            .outputs
            .iter()
            .find(|o| o.group == "go")
            .unwrap();
        assert!(go_out.paths.iter().any(|p| matches!(
            &p.content,
            Content::FilePath(s) if s == "mypkg/testmain.go"
        )));
    }
}
