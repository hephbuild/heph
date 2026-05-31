use crate::engine::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, RunRequest, RunResponse, outputartifact,
    targetdef::{
        CacheConfig, Output, TargetDef,
        path::{CodegenMode, Content, Path},
    },
};
use crate::hasync::Cancellable;
use crate::loosespecparser::{TargetSpecValue, parse_bool, parse_string};
use anyhow::Context as _;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

pub const DRIVER_NAME: &str = "textfile";

#[derive(serde::Serialize)]
struct TextFileDef {
    text: String,
    out: String,
    executable: bool,
}

pub struct Driver;

fn take_string(
    m: &mut HashMap<&str, &TargetSpecValue>,
    key: &str,
) -> anyhow::Result<Option<String>> {
    match m.remove(key) {
        Some(v) => parse_string(v).with_context(|| format!("parse `{key}`")),
        None => Ok(None),
    }
}

fn take_bool(m: &mut HashMap<&str, &TargetSpecValue>, key: &str) -> anyhow::Result<Option<bool>> {
    match m.remove(key) {
        Some(v) => parse_bool(v)
            .with_context(|| format!("parse `{key}`"))
            .map(Some),
        None => Ok(None),
    }
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
        let mut m: HashMap<&str, &TargetSpecValue> = req
            .target_spec
            .config
            .iter()
            .map(|(k, v)| (k.as_str(), v))
            .collect();

        let text = take_string(&mut m, "text")?
            .ok_or_else(|| anyhow::anyhow!("textfile driver requires `text`"))?
            .trim()
            .to_string();
        let out_rel =
            take_string(&mut m, "out")?.unwrap_or_else(|| req.target_spec.addr.name.clone());
        let executable = take_bool(&mut m, "executable")?.unwrap_or(false);

        if !m.is_empty() {
            let unknown: Vec<&str> = m.into_keys().collect();
            anyhow::bail!(
                "textfile driver does not support config keys: {:?}",
                unknown
            );
        }

        let pkg = req.target_spec.addr.package.as_str();
        let out = if pkg.is_empty() {
            out_rel
        } else {
            format!("{pkg}/{out_rel}")
        };

        let mut h = Xxh3::new();
        h.update(req.target_spec.addr.format().as_bytes());
        h.update(out.as_bytes());
        h.update(&[executable as u8]);
        h.update(text.as_bytes());
        let hash = format!("{:016x}", h.digest()).into_bytes();

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(TextFileDef {
                    text,
                    out: out.clone(),
                    executable,
                }),
                inputs: vec![],
                outputs: vec![Output {
                    group: "".to_string(),
                    paths: vec![Path {
                        content: Content::FilePath(out),
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
        let def = req.target.def::<TextFileDef>();
        let data = def.text.as_bytes().to_vec();

        let mut h = Xxh3::new();
        h.update(&data);
        h.update(def.out.as_bytes());
        h.update(&[def.executable as u8]);
        let hashout = format!("{:x}", h.digest());

        let name = std::path::Path::new(&def.out)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| def.out.clone());

        Ok(RunResponse {
            artifacts: vec![outputartifact::OutputArtifact {
                group: "".to_string(),
                name,
                r#type: outputartifact::Type::Output,
                content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                    data,
                    path: def.out.clone(),
                    x: def.executable,
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
        anyhow::bail!("run_shell not implemented for textfile driver")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::driver::Driver as EDriver;
    use crate::engine::provider::TargetSpec;
    use crate::hasync::StdCancellationToken;
    use crate::htaddr::parse_addr;

    fn ctoken() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    fn make_parse_req(addr_str: &str, config: HashMap<String, TargetSpecValue>) -> ParseRequest {
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: parse_addr(addr_str).unwrap(),
                driver: DRIVER_NAME.to_string(),
                config,
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

    fn basic_config(text: &str, out: &str) -> HashMap<String, TargetSpecValue> {
        HashMap::from([
            (
                "text".to_string(),
                TargetSpecValue::String(text.to_string()),
            ),
            ("out".to_string(), TargetSpecValue::String(out.to_string())),
        ])
    }

    #[tokio::test]
    async fn test_parse_requires_text() {
        let driver = Driver;
        let cfg = HashMap::from([(
            "out".to_string(),
            TargetSpecValue::String("bin/x".to_string()),
        )]);
        let err = driver
            .parse(make_parse_req("//pkg:t", cfg), &ctoken())
            .await
            .err()
            .expect("must error");
        assert!(err.to_string().contains("text"), "{err}");
    }

    #[tokio::test]
    async fn test_parse_out_defaults_to_addr_name() {
        let driver = Driver;
        let cfg = HashMap::from([(
            "text".to_string(),
            TargetSpecValue::String("hello".to_string()),
        )]);
        let res = driver
            .parse(make_parse_req("//pkg:wrapper", cfg), &ctoken())
            .await
            .unwrap();
        let def = res.target_def.def::<TextFileDef>();
        assert_eq!(def.out, "pkg/wrapper");
        match &res.target_def.outputs[0].paths[0].content {
            Content::FilePath(p) => assert_eq!(p, "pkg/wrapper"),
            other => panic!("expected FilePath, got {other}"),
        }
    }

    #[tokio::test]
    async fn test_parse_unknown_key_errors() {
        let driver = Driver;
        let mut cfg = basic_config("hi", "f.txt");
        cfg.insert("bogus".to_string(), TargetSpecValue::String("x".into()));
        let err = driver
            .parse(make_parse_req("//pkg:t", cfg), &ctoken())
            .await
            .err()
            .expect("must error");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[tokio::test]
    async fn test_parse_basic_outputs() {
        let driver = Driver;
        let res = driver
            .parse(
                make_parse_req("//pkg:t", basic_config("hello", "bin/wrap")),
                &ctoken(),
            )
            .await
            .unwrap();
        assert!(res.target_def.inputs.is_empty());
        assert_eq!(res.target_def.outputs.len(), 1);
        let path = &res.target_def.outputs[0].paths[0];
        match &path.content {
            Content::FilePath(p) => assert_eq!(p, "pkg/bin/wrap"),
            other => panic!("expected FilePath, got {other}"),
        }
        let def = res.target_def.def::<TextFileDef>();
        assert_eq!(def.text, "hello");
        assert_eq!(def.out, "pkg/bin/wrap");
        assert!(!def.executable);
    }

    #[tokio::test]
    async fn test_parse_hash_changes_with_text() {
        let driver = Driver;
        let a = driver
            .parse(
                make_parse_req("//pkg:t", basic_config("aaa", "f")),
                &ctoken(),
            )
            .await
            .unwrap()
            .target_def
            .hash;
        let b = driver
            .parse(
                make_parse_req("//pkg:t", basic_config("bbb", "f")),
                &ctoken(),
            )
            .await
            .unwrap()
            .target_def
            .hash;
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn test_parse_hash_changes_with_executable() {
        let driver = Driver;
        let a = driver
            .parse(
                make_parse_req("//pkg:t", basic_config("hi", "f")),
                &ctoken(),
            )
            .await
            .unwrap()
            .target_def
            .hash;
        let mut cfg = basic_config("hi", "f");
        cfg.insert("executable".to_string(), TargetSpecValue::Bool(true));
        let b = driver
            .parse(make_parse_req("//pkg:t", cfg), &ctoken())
            .await
            .unwrap()
            .target_def
            .hash;
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn test_run_produces_raw_artifact() {
        let driver = Driver;
        let mut cfg = basic_config("#!/bin/bash\necho hi\n", "bin/wrap");
        cfg.insert("executable".to_string(), TargetSpecValue::Bool(true));
        let parsed = driver
            .parse(make_parse_req("//pkg:t", cfg), &ctoken())
            .await
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_request(&parsed.target_def, &request_id, tmp.path(), &hashin);

        let res = driver.run(req, &ctoken()).await.unwrap();
        assert_eq!(res.artifacts.len(), 1);
        let a = &res.artifacts[0];
        assert_eq!(a.name, "wrap");
        assert_eq!(a.r#type, outputartifact::Type::Output);
        match &a.content {
            outputartifact::Content::Raw(raw) => {
                assert!(raw.x);
                assert_eq!(raw.path, "pkg/bin/wrap");
                assert_eq!(raw.data, b"#!/bin/bash\necho hi");
            }
            _ => panic!("expected Raw content"),
        }
    }

    #[tokio::test]
    async fn test_run_hash_stable() {
        let driver = Driver;
        let parsed = driver
            .parse(
                make_parse_req("//pkg:t", basic_config("hello", "f")),
                &ctoken(),
            )
            .await
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let request_id = "test".to_string();
        let hashin = String::new();

        let r1 = driver
            .run(
                make_run_request(&parsed.target_def, &request_id, tmp.path(), &hashin),
                &ctoken(),
            )
            .await
            .unwrap();
        let r2 = driver
            .run(
                make_run_request(&parsed.target_def, &request_id, tmp.path(), &hashin),
                &ctoken(),
            )
            .await
            .unwrap();
        assert_eq!(r1.artifacts[0].hashout, r2.artifacts[0].hashout);
    }
}
