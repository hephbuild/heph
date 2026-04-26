use async_trait::async_trait;
use crate::hasync;
use crate::htaddr::Addr;

#[derive(Default, Clone)]
pub struct TargetAddr {
    pub r#ref: Addr,
    pub output: Option<String>,
}

pub mod sandbox {
    use std::collections::HashMap;
    use smart_default::SmartDefault;
    use crate::engine::driver::TargetAddr;

    #[derive(Default, Clone)]
    pub struct Sandbox {
        pub tools: Vec<Tool>,
        pub deps: Vec<Dep>,
        pub env: HashMap<String, Env>,
    }

    #[derive(Default, Clone)]
    pub struct Tool {
        pub r#ref: TargetAddr,
        pub group: String,
        pub hash: bool,
        pub id: String,
    }

    #[derive(Default, Clone)]
    pub struct Dep {
        pub r#ref: TargetAddr,
        pub mode: Mode,
        pub group: String,
        pub runtime: bool,
        pub hash: bool,
        pub id: String,
    }

    #[derive(Clone, SmartDefault)]
    pub enum Mode {
        #[default]
        None,
        Link,
    }

    #[derive(Clone)]
    pub struct Env {
        pub value: EnvValue,
        pub hash: bool,
        pub append: bool,
        pub append_prefix: String,
    }

    #[derive(Clone)]
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
    }

    #[derive(Clone)]
    pub struct Input {
        pub r#ref: TargetAddr,
        pub mode: InputMode,
        pub origin_id: String,
    }

    #[derive(Clone)]
    pub enum InputMode {
        Unspecified,
        Link,
        None,
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
    use crate::hartifactcontent::Artifact;

    pub enum Type {
        Output,
        SupportFile,
    }

    pub struct InputArtifact {
        pub group: String,
        pub name: String,
        pub r#type: Type,
        pub id: String,
        pub content: Box<dyn Artifact>,
    }
}

pub struct RunInput {
    pub artifact: inputartifact::InputArtifact,
    pub origin_id: String,
}

pub mod outputartifact {
    use std::fs::File;
    use std::{fs, io};
    use std::io::Read;
    use std::path::PathBuf;
    use crate::hartifactcontent::{Artifact, WalkEntry};

    #[derive(Clone)]
    pub enum Type {
        Output,
        OutputListV1,
        Log,
        SupportFile,
        SupportFileListV1,
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
    }
    
    impl Artifact for OutputArtifact {
        fn reader(&self) -> anyhow::Result<Box<dyn Read>> {
            Ok(match &self.content {
                Content::Raw(raw) => Box::new(io::Cursor::new(raw.data.clone())),
                Content::File(file) => Box::new(File::open(&file.source_path)?),
                Content::TarPath(path) => Box::new(File::open(path)?),
                Content::CpioPath(path) => Box::new(File::open(path)?),
            })
        }

        fn content_type(&self) -> anyhow::Result<crate::hartifactcontent::Type> {
            Ok(match &self.content {
                Content::Raw(_) => crate::hartifactcontent::Type::Tar,
                Content::File(_) => crate::hartifactcontent::Type::Tar,
                Content::TarPath(_) => crate::hartifactcontent::Type::Tar,
                Content::CpioPath(_) => crate::hartifactcontent::Type::Cpio,
            })
        }

        fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item=anyhow::Result<WalkEntry>> + '_>> {
            Ok(match &self.content {
                Content::Raw(raw) => {
                    Box::new(std::iter::once(Ok(WalkEntry {
                        path: PathBuf::from(&raw.path),
                        data: raw.data.clone(),
                        x: raw.x,
                    })))
                },
                Content::File(file) => {
                    let data = fs::read(&file.source_path)?;
                    Box::new(std::iter::once(Ok(WalkEntry {
                        path: PathBuf::from(&file.out_path),
                        data,
                        x: file.x,
                    })))
                },
                Content::TarPath(path) => Box::new(crate::hartifactcontent::tar::TarWalker::new(File::open(path)?)?),
                Content::CpioPath(path) => unimplemented!("cpio is not implemented"),
            })
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
