use std::io;
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
    use crate::engine::driver::TargetAddr;
    use crate::htaddr::Addr;

    pub struct TargetDef {
        pub addr: Addr,
        pub raw_def: Box<dyn Any + Send + Sync>,
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

    pub struct Input {
        pub r#ref: TargetAddr,
        pub mode: InputMode,
        pub origin_id: String,
    }

    pub enum InputMode {
        Unspecified,
        Link,
        None,
    }

    pub struct Output {
        pub group: String,
        pub paths: Vec<path::Path>,
    }

    pub mod path {
        pub struct Path {
            pub content: Content,
            pub codegen_tree: CodegenMode,
            pub collect: bool,
        }

        pub enum Content {
            FilePath(String),
            DirPath(String),
            Glob(String),
        }

        pub enum CodegenMode {
            Unspecified,
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
    pub enum Type {
        Output,
        SupportFile,
    }

    pub struct InputArtifact {
        pub group: String,
        pub name: String,
        pub r#type: Type,
        pub id: String,
    }
}


pub struct RunInput {
    pub artifact: inputartifact::InputArtifact,
    pub origin_id: String,
}

pub mod outputartifact {
    pub enum Type {
        Output,
        OutputListV1,
        Log,
        SupportFile,
        SupportFileListV1,
    }

    pub struct ContentRaw {
        pub data: Vec<u8>,
        pub path: String,
        pub x: bool,
    }

    pub struct ContentFile {
        pub source_path: String,
        pub out_path: String,
        pub x: bool,
    }

    pub enum Content {
        File(ContentFile),
        Raw(ContentRaw),
        TarPath(String),
        CpioPath(String),
    }

    pub struct OutputArtifact {
        pub group: String,
        pub name: String,
        pub r#type: Type,
        pub content: Content,
    }
}

pub struct RunRequest<'a> {
    pub request_id: &'a String,
    pub target: &'a targetdef::TargetDef,
    pub sandbox_path: String,
    pub tree_root_path: String,
    pub inputs: Vec<RunInput>,
    pub hashin: &'a String,
    pub stdin: &'a (dyn io::Read + Send + Sync),
    pub stdout: &'a (dyn io::Write + Send + Sync),
    pub stderr: &'a (dyn io::Write + Send + Sync),
}
pub struct RunResponse {
    pub artifacts: Vec<outputartifact::OutputArtifact>,
}

#[async_trait]
pub trait Driver: Send + Sync {
    fn config(&self, req: ConfigRequest, ctoken: &(dyn hasync::Cancellable + Send + Sync)) -> anyhow::Result<ConfigResponse>;
    async fn parse(&self, req: ParseRequest, ctoken: &(dyn hasync::Cancellable + Send + Sync)) -> anyhow::Result<ParseResponse>;
    async fn apply_transitive(&self, req: ApplyTransitiveRequest, ctoken: &(dyn hasync::Cancellable + Send + Sync)) -> anyhow::Result<ApplyTransitiveResponse>;
    async fn run<'a>(&self, req: RunRequest<'a>, ctoken: &(dyn hasync::Cancellable + Send + Sync)) -> anyhow::Result<RunResponse>;
}
