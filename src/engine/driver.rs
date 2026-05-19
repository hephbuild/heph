use crate::htaddr::Addr;
use crate::htpkg::PkgBuf;
use crate::{hasync, htaddr};
use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt::Display;
use std::path::PathBuf;

#[derive(Default, Clone, Hash, Debug)]
pub struct TargetAddr {
    pub r#ref: Addr,
    pub output: Option<String>,
    /// If non-empty, only files matching these paths are exposed from the dep's outputs.
    pub filters: Vec<String>,
}

impl Serialize for TargetAddr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for TargetAddr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        TargetAddr::parse(&s, &PkgBuf::from("")).map_err(serde::de::Error::custom)
    }
}

impl Display for TargetAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(output) = &self.output {
            write!(f, "{}|{}", self.r#ref, output)?;
        } else {
            write!(f, "{}", self.r#ref)?;
        }
        if !self.filters.is_empty() {
            write!(f, "[{}]", self.filters.join(","))?;
        }
        Ok(())
    }
}

impl TargetAddr {
    pub fn parse(v: &str, base: &PkgBuf) -> anyhow::Result<Self> {
        // Strip optional trailing [filter1,filter2] suffix.
        // bracket is a byte offset from rfind; '[' is ASCII so split_at is always on a char boundary.
        let (v, filters) = if let Some(bracket) = v.rfind('[') {
            let (addr_part, bracket_part) = v.split_at(bracket);
            let rest = bracket_part
                .strip_prefix('[')
                .expect("split_at(rfind('[')) guarantees '['");
            let rest = rest
                .strip_suffix(']')
                .ok_or_else(|| anyhow::anyhow!("unclosed '[' in target addr: {v}"))?;
            let filters = if rest.is_empty() {
                vec![]
            } else {
                rest.split(',').map(|s| s.to_string()).collect()
            };
            (addr_part, filters)
        } else {
            (v, vec![])
        };

        let parts: Vec<&str> = v.split('|').collect();
        match parts[..] {
            [v] => Ok(TargetAddr {
                r#ref: htaddr::parse_addr_with_base(v, base)?,
                output: None,
                filters,
            }),
            [addr, out] => Ok(TargetAddr {
                r#ref: htaddr::parse_addr_with_base(addr, base)?,
                output: Some(out.parse()?),
                filters,
            }),
            _ => anyhow::bail!("invalid address"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htpkg::PkgBuf;

    fn base() -> PkgBuf {
        PkgBuf::from("")
    }

    #[test]
    fn parse_no_output_no_filters() {
        let t = TargetAddr::parse("//foo/bar:baz", &base()).unwrap();
        assert_eq!(t.r#ref.package.as_str(), "foo/bar");
        assert_eq!(t.r#ref.name, "baz");
        assert!(t.output.is_none());
        assert!(t.filters.is_empty());
    }

    #[test]
    fn parse_with_output_no_filters() {
        let t = TargetAddr::parse("//foo:bar|out", &base()).unwrap();
        assert_eq!(t.output.as_deref(), Some("out"));
        assert!(t.filters.is_empty());
    }

    #[test]
    fn parse_with_filters_no_output() {
        let t = TargetAddr::parse("//foo:bar[a.go,b.go]", &base()).unwrap();
        assert!(t.output.is_none());
        assert_eq!(t.filters, vec!["a.go", "b.go"]);
    }

    #[test]
    fn parse_with_output_and_filters() {
        let t = TargetAddr::parse("//foo:bar|src[pkg/a.go]", &base()).unwrap();
        assert_eq!(t.output.as_deref(), Some("src"));
        assert_eq!(t.filters, vec!["pkg/a.go"]);
    }

    #[test]
    fn parse_empty_filters() {
        let t = TargetAddr::parse("//foo:bar[]", &base()).unwrap();
        assert!(t.filters.is_empty());
    }

    #[test]
    fn parse_unclosed_bracket_errors() {
        assert!(TargetAddr::parse("//foo:bar[a.go", &base()).is_err());
    }

    #[test]
    fn display_roundtrip_with_filters() {
        let t = TargetAddr::parse("//foo:bar[a.go,b.go]", &base()).unwrap();
        assert_eq!(t.to_string(), "//foo:bar[a.go,b.go]");
    }

    #[test]
    fn display_roundtrip_output_and_filters() {
        let t = TargetAddr::parse("//foo:bar|src[pkg/a.go]", &base()).unwrap();
        assert_eq!(t.to_string(), "//foo:bar|src[pkg/a.go]");
    }
}

pub mod sandbox {
    use crate::engine::driver::TargetAddr;
    use serde::{Deserialize, Serialize};
    use smart_default::SmartDefault;
    use std::collections::HashMap;

    #[derive(Default, Clone, Debug, Serialize, Deserialize)]
    pub struct Sandbox {
        pub tools: Vec<Tool>,
        pub deps: Vec<Dep>,
        pub env: HashMap<String, Env>,
    }

    impl Sandbox {
        pub(crate) fn merge_sandbox(&mut self, inbound: Sandbox, id: String) {
            for t in inbound.tools {
                self.tools.push(Tool {
                    id: format!("{}_tool_{}", id, t.id),
                    ..t.clone()
                });
            }

            for d in inbound.deps {
                self.deps.push(Dep {
                    id: format!("{}_dep_{}", id, d.id),
                    ..d.clone()
                })
            }

            self.env
                .extend(inbound.env.into_iter().map(|(k, v)| (k, v.clone())));
        }
    }

    impl Sandbox {
        pub fn empty(&self) -> bool {
            self.tools.is_empty() && self.deps.is_empty() && self.env.is_empty()
        }
    }

    #[derive(Default, Clone, Debug, Serialize, Deserialize)]
    pub struct Tool {
        pub r#ref: TargetAddr,
        pub group: String,
        pub hash: bool,
        pub id: String,
    }

    #[derive(Default, Clone, Debug, Serialize, Deserialize)]
    pub struct Dep {
        pub r#ref: TargetAddr,
        pub mode: Mode,
        pub group: String,
        pub runtime: bool,
        pub hash: bool,
        pub id: String,
    }

    #[derive(Clone, SmartDefault, Debug, Serialize, Deserialize)]
    pub enum Mode {
        #[default]
        None,
        Link,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct Env {
        pub value: EnvValue,
        pub hash: bool,
        pub append: bool,
        pub append_prefix: String,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub enum EnvValue {
        Literal(String),
        Pass,
    }
}

pub struct ConfigRequest {}
pub struct ConfigResponse {
    pub name: String,
}

pub struct ParseRequest {
    pub request_id: String,
    pub target_spec: std::sync::Arc<crate::engine::provider::TargetSpec>,
}

pub mod targetdef {
    use crate::engine::driver::TargetAddr;
    use crate::htaddr::Addr;
    use itertools::Itertools;
    use serde::Serialize;
    use std::any::Any;
    use std::sync::Arc;

    fn serialize_hash<S: serde::Serializer>(h: &[u8], s: S) -> Result<S::Ok, S::Error> {
        use std::fmt::Write;
        let mut out = String::with_capacity(h.len() * 2);
        for b in h {
            write!(out, "{:02x}", b).map_err(serde::ser::Error::custom)?;
        }
        s.serialize_str(&out)
    }

    #[derive(Clone, Serialize)]
    pub struct TargetDef {
        pub addr: Addr,
        pub labels: Vec<String>,
        #[serde(skip)]
        pub raw_def: Arc<dyn Any + Send + Sync>,
        pub inputs: Vec<Input>,
        pub outputs: Vec<Output>,
        pub support_files: Vec<path::Path>,
        pub cache: bool,
        pub disable_remote_cache: bool,
        pub pty: bool,
        #[serde(serialize_with = "serialize_hash")]
        pub hash: Vec<u8>,
        pub transparent: bool,
    }

    impl TargetDef {
        pub fn def<T: 'static>(&self) -> &T {
            self.raw_def
                .downcast_ref::<T>()
                .expect("TargetDef raw_def type mismatch: wrong type T requested")
        }
        pub fn set_def<T: Send + Sync + 'static>(&mut self, def: T) {
            self.raw_def = Arc::new(def);
        }
        pub fn output_names(&self) -> Vec<String> {
            self.outputs
                .iter()
                .map(|o| &o.group)
                .unique()
                .cloned()
                .collect()
        }
    }

    #[derive(Clone, Hash, Serialize)]
    pub struct Input {
        pub r#ref: TargetAddr,
        pub mode: InputMode,
        pub origin_id: String,
    }

    #[derive(Clone, Hash, PartialEq, Serialize)]
    pub enum InputMode {
        Standard,
        Link,
        Tool,
    }

    #[derive(Clone, Serialize)]
    pub struct Output {
        pub group: String,
        pub paths: Vec<path::Path>,
    }

    pub mod path {
        use serde::Serialize;
        use std::fmt::Display;

        #[derive(Clone, Serialize)]
        pub struct Path {
            pub content: Content,
            pub codegen_tree: CodegenMode,
            pub collect: bool,
        }

        #[derive(Clone, Serialize)]
        pub enum Content {
            FilePath(String),
            DirPath(String),
            Glob(String),
        }

        impl Display for Content {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    Content::FilePath(path) => write!(f, "FilePath({})", path),
                    Content::DirPath(path) => write!(f, "DirPath({})", path),
                    Content::Glob(pattern) => write!(f, "Glob({})", pattern),
                }
            }
        }

        #[derive(Clone, Serialize)]
        pub enum CodegenMode {
            None,
            Copy,
            Link,
        }
    }
}

pub struct ParseResponse {
    pub target_def: targetdef::TargetDef,
}

pub struct ApplyTransitiveRequest {
    pub request_id: String,
    pub target_def: targetdef::TargetDef,
    pub sandbox: sandbox::Sandbox,
}
pub struct ApplyTransitiveResponse {
    pub target_def: targetdef::TargetDef,
}

pub mod inputartifact {
    use crate::hartifactcontent::Content;
    use std::sync::Arc;

    pub enum Type {
        Dep,
        Support,
    }

    pub struct InputArtifact {
        pub r#type: Type,
        pub origin_id: String,
        pub content: Arc<dyn Content>,
    }
}

pub struct RunInput {
    pub artifact: inputartifact::InputArtifact,
    pub origin_id: String,
    /// The target that produced this artifact, used for source map generation.
    pub source_addr: Addr,
    /// If non-empty, only these file paths are exposed from this input's unpacked artifacts.
    pub filters: Vec<String>,
}

pub mod outputartifact {
    use crate::hartifactcontent::{Content as HContent, WalkEntry};
    use std::fs::File;
    use std::io;
    use std::io::Read;
    use std::path::PathBuf;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Type {
        Output,
        Log,
        SupportFile,
    }

    #[derive(Clone)]
    pub struct ContentRaw {
        pub data: Vec<u8>,
        pub path: String,
        pub x: bool,
    }

    #[derive(Clone)]
    pub struct ContentFile {
        pub source_path: String,
        pub out_path: String,
        pub x: bool,
    }

    #[derive(Clone)]
    pub enum Content {
        File(ContentFile),
        Raw(ContentRaw),
        TarPath(String),
        CpioPath(String),
    }

    #[derive(Clone)]
    pub struct OutputArtifact {
        pub group: String,
        pub name: String,
        pub r#type: Type,
        pub content: Content,
        pub hashout: String,
    }

    impl HContent for OutputArtifact {
        fn reader(&self) -> anyhow::Result<Box<dyn Read>> {
            Ok(match &self.content {
                Content::Raw(raw) => Box::new(io::Cursor::new(raw.data.clone())),
                Content::File(file) => Box::new(File::open(&file.source_path)?),
                Content::TarPath(path) => Box::new(File::open(path)?),
                Content::CpioPath(path) => Box::new(File::open(path)?),
            })
        }

        fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
            Ok(match &self.content {
                Content::Raw(raw) => Box::new(std::iter::once(Ok(WalkEntry {
                    path: PathBuf::from(&raw.path),
                    data: Box::new(io::Cursor::new(raw.data.clone())),
                    x: raw.x,
                }))),
                Content::File(file) => Box::new(std::iter::once(Ok(WalkEntry {
                    path: PathBuf::from(&file.out_path),
                    data: Box::new(File::open(&file.source_path)?),
                    x: file.x,
                }))),
                Content::TarPath(path) => Box::new(crate::hartifactcontent::tar::TarWalker::new(
                    File::open(path)?,
                )?),
                #[expect(clippy::unimplemented, reason = "cpio format is not yet implemented")]
                Content::CpioPath(_) => unimplemented!("cpio is not implemented"),
            })
        }

        fn hashout(&self) -> anyhow::Result<String> {
            Ok(self.hashout.clone())
        }
    }
}

pub struct RunRequest<'a, 'io> {
    pub request_id: &'a String,
    pub target: &'a targetdef::TargetDef,
    pub tree_root_path: PathBuf,
    pub inputs: Vec<RunInput>,
    pub hashin: &'a str,
    pub stdin: Option<&'io mut (dyn tokio::io::AsyncRead + Send + Sync + Unpin)>,
    pub stdout: Option<&'io mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
    pub stderr: Option<&'io mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
    pub sandbox_dir: std::path::PathBuf,
}
pub struct RunResponse {
    pub artifacts: Vec<outputartifact::OutputArtifact>,
}

#[async_trait]
pub trait Driver: Send + Sync {
    fn config(&self, req: ConfigRequest) -> anyhow::Result<ConfigResponse>;
    async fn parse(
        &self,
        req: ParseRequest,
        ctoken: &(dyn hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse>;
    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        ctoken: &(dyn hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse>;
    async fn run<'a, 'io>(
        &self,
        req: RunRequest<'a, 'io>,
        ctoken: &(dyn hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse>;
    async fn run_shell<'a, 'io>(
        &self,
        req: RunRequest<'a, 'io>,
        ctoken: &(dyn hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse>;
}
