use std::fmt::Display;
use async_trait::async_trait;
use crate::{hasync, htaddr};
use crate::htaddr::Addr;
use crate::htpkg::PkgBuf;

#[derive(Default, Clone, Hash, Debug)]
pub struct TargetAddr {
    pub r#ref: Addr,
    pub output: Option<String>,
}

impl Display for TargetAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(output) = &self.output {
            write!(f, "{}|{}", self.r#ref, output)
        } else {
            write!(f, "{}", self.r#ref)
        }
    }
}

impl TargetAddr {
    pub fn parse(v: &str, base: &PkgBuf) -> anyhow::Result<Self> {
        let parts: Vec<&str> = v.split('|').collect();
        match parts.len() {
            1 => {
                Ok(TargetAddr { r#ref: htaddr::parse_addr_with_base(v, base)?, output:  None })
            }
            2 => {
                Ok(TargetAddr { r#ref: htaddr::parse_addr_with_base(parts[0], base)?, output: Some(parts[1].parse()?) })
            }
            _ => anyhow::bail!("invalid address")
        }
    }
}

pub mod sandbox {
    use std::collections::HashMap;
    use smart_default::SmartDefault;
    use crate::engine::driver::TargetAddr;

    #[derive(Default, Clone, Debug)]
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
                self.deps.push(Dep{
                    id: format!("{}_dep_{}", id, d.id),
                    ..d.clone()
                })
            }

            self.env.extend(inbound.env.into_iter().map(|(k, v)| (k, v.clone())));
        }
    }

    impl Sandbox {
        pub fn empty(&self) -> bool {
            self.tools.is_empty() && self.deps.is_empty() && self.env.is_empty()
        }
    }

    #[derive(Default, Clone, Debug)]
    pub struct Tool {
        pub r#ref: TargetAddr,
        pub group: String,
        pub hash: bool,
        pub id: String,
    }

    #[derive(Default, Clone, Debug)]
    pub struct Dep {
        pub r#ref: TargetAddr,
        pub mode: Mode,
        pub group: String,
        pub runtime: bool,
        pub hash: bool,
        pub id: String,
    }

    #[derive(Clone, SmartDefault, Debug)]
    pub enum Mode {
        #[default]
        None,
        Link,
    }

    #[derive(Clone, Debug)]
    pub struct Env {
        pub value: EnvValue,
        pub hash: bool,
        pub append: bool,
        pub append_prefix: String,
    }

    #[derive(Clone, Debug)]
    pub enum EnvValue {
        Literal(String),
        Pass(bool),
    }
}

pub struct ConfigRequest {}
pub struct ConfigResponse {
    pub name: String,
}

pub struct ParseRequest {
    pub request_id: String,
    pub target_spec: crate::engine::provider::TargetSpec,
}

pub mod targetdef {
    use std::any::Any;
    use std::sync::Arc;
    use crate::engine::driver::TargetAddr;
    use crate::htaddr::Addr;

    #[derive(Clone)]
    pub struct TargetDef {
        pub addr: Addr,
        pub labels: Vec<String>,
        pub raw_def: Arc<dyn Any + Send + Sync>,
        pub inputs: Vec<Input>,
        pub outputs: Vec<Output>,
        pub support_files: Vec<path::Path>,
        pub cache: bool,
        pub disable_remote_cache: bool,
        pub pty: bool,
        pub hash: Vec<u8>,
    }

    impl TargetDef {
        pub fn def<T: 'static>(&self) -> &T {
            self.raw_def.downcast_ref::<T>().unwrap()
        }
        pub fn set_def<T: Send + Sync + 'static>(&mut self, def: T) {
            self.raw_def = Arc::new(def);
        }
    }

    #[derive(Clone, Hash)]
    pub struct Input {
        pub r#ref: TargetAddr,
        pub mode: InputMode,
        pub origin_id: String,
    }

    #[derive(Clone, Hash)]
    pub enum InputMode {
        Standard,
        Link,
    }

    #[derive(Clone)]
    pub struct Output {
        pub group: String,
        pub paths: Vec<path::Path>,
    }

    pub mod path {
        use std::fmt::Display;

        #[derive(Clone)]
        pub struct Path {
            pub content: Content,
            pub codegen_tree: CodegenMode,
            pub collect: bool,
        }

        #[derive(Clone)]
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

        #[derive(Clone)]
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
    use std::sync::Arc;
    use crate::hartifactcontent::Content;

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
}

pub mod outputartifact {
    use std::fs::File;
    use std::io;
    use std::io::Read;
    use std::path::PathBuf;
    use crate::hartifactcontent::{Content as HContent, WalkEntry};

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

        fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item=anyhow::Result<WalkEntry>> + '_>> {
            Ok(match &self.content {
                Content::Raw(raw) => {
                    Box::new(std::iter::once(Ok(WalkEntry {
                        path: PathBuf::from(&raw.path),
                        data: Box::new(io::Cursor::new(raw.data.clone())),
                        x: raw.x,
                    })))
                },
                Content::File(file) => {
                    Box::new(std::iter::once(Ok(WalkEntry {
                        path: PathBuf::from(&file.out_path),
                        data: Box::new(File::open(&file.source_path)?),
                        x: file.x,
                    })))
                },
                Content::TarPath(path) => Box::new(crate::hartifactcontent::tar::TarWalker::new(File::open(path)?)?),
                Content::CpioPath(_) => unimplemented!("cpio is not implemented"),
            })
        }

        fn hashout(&self) -> anyhow::Result<String> {
            Ok(self.hashout.clone())
        }
    }
}

pub struct RunRequest<'a> {
    pub request_id: &'a String,
    pub target: &'a targetdef::TargetDef,
    pub tree_root_path: String,
    pub inputs: Vec<RunInput>,
    pub hashin: &'a String,
    pub stdin: Option<&'a mut (dyn tokio::io::AsyncRead + Send + Sync + Unpin)>,
    pub stdout: Option<&'a mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
    pub stderr: Option<&'a mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
    pub sandbox_dir: std::path::PathBuf,
}
pub struct RunResponse {
    pub artifacts: Vec<outputartifact::OutputArtifact>,
}

#[async_trait]
pub trait Driver: Send + Sync {
    fn config(&self, req: ConfigRequest) -> anyhow::Result<ConfigResponse>;
    async fn parse(&self, req: ParseRequest, ctoken: &(dyn hasync::Cancellable + Send + Sync)) -> anyhow::Result<ParseResponse>;
    async fn apply_transitive(&self, req: ApplyTransitiveRequest, ctoken: &(dyn hasync::Cancellable + Send + Sync)) -> anyhow::Result<ApplyTransitiveResponse>;
    async fn run<'a>(&self, req: RunRequest<'a>, ctoken: &(dyn hasync::Cancellable + Send + Sync)) -> anyhow::Result<RunResponse>;
}
