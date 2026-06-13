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
    use std::collections::{BTreeSet, HashMap};

    #[derive(Default, Clone, Debug, Serialize, Deserialize)]
    pub struct Sandbox {
        pub(crate) tools: Vec<Tool>,
        pub(crate) deps: Vec<Dep>,
        pub env: HashMap<String, Env>,
        // Membership indexes for O(log n) dedup. Derived from tools/deps; not
        // serialized so wire format and hashin bytes are unchanged.
        #[serde(skip)]
        tool_keys: BTreeSet<String>,
        #[serde(skip)]
        dep_keys: BTreeSet<String>,
    }

    fn tool_key(t: &Tool) -> String {
        // \x1f (unit separator) keeps fields unambiguous even if a value
        // contains characters from the other field.
        format!("{}\x1f{}", t.r#ref, t.group)
    }

    fn dep_key(d: &Dep) -> String {
        format!("{}\x1f{}\x1f{:?}", d.r#ref, d.group, d.mode)
    }

    impl Sandbox {
        /// Append a tool, deduped by `(r#ref, group)`. Returns true if inserted.
        pub fn push_tool(&mut self, t: Tool) -> bool {
            if self.tool_keys.insert(tool_key(&t)) {
                self.tools.push(t);
                true
            } else {
                false
            }
        }

        /// Append a dep, deduped by `(r#ref, group, mode)`. Returns true if inserted.
        pub fn push_dep(&mut self, d: Dep) -> bool {
            if self.dep_keys.insert(dep_key(&d)) {
                self.deps.push(d);
                true
            } else {
                false
            }
        }

        pub fn tools(&self) -> &[Tool] {
            &self.tools
        }

        pub fn deps(&self) -> &[Dep] {
            &self.deps
        }

        pub(crate) fn merge_sandbox(&mut self, inbound: Sandbox, id: String) {
            for t in inbound.tools {
                self.push_tool(Tool {
                    id: format!("{}_tool_{}", id, t.id),
                    ..t
                });
            }

            for d in inbound.deps {
                self.push_dep(Dep {
                    id: format!("{}_dep_{}", id, d.id),
                    ..d
                });
            }

            self.env.extend(inbound.env);
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

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::htpkg::PkgBuf;

        fn tool(addr: &str, group: &str, id: &str) -> Tool {
            Tool {
                r#ref: TargetAddr::parse(addr, &PkgBuf::from("")).expect("parse addr"),
                group: group.to_string(),
                hash: true,
                id: id.to_string(),
            }
        }

        fn dep(addr: &str, group: &str, mode: Mode, id: &str) -> Dep {
            Dep {
                r#ref: TargetAddr::parse(addr, &PkgBuf::from("")).expect("parse addr"),
                mode,
                group: group.to_string(),
                runtime: true,
                hash: true,
                id: id.to_string(),
            }
        }

        #[test]
        fn push_tool_dedupes_by_ref_and_group() {
            let mut sb = Sandbox::default();
            assert!(sb.push_tool(tool("//pkg:t", "", "a")));
            assert!(!sb.push_tool(tool("//pkg:t", "", "b")));
            assert_eq!(sb.tools().len(), 1);
            // The first push wins (id "a" preserved).
            assert_eq!(sb.tools()[0].id, "a");
        }

        #[test]
        fn push_tool_allows_same_ref_different_group() {
            let mut sb = Sandbox::default();
            assert!(sb.push_tool(tool("//pkg:t", "g1", "a")));
            assert!(sb.push_tool(tool("//pkg:t", "g2", "b")));
            assert_eq!(sb.tools().len(), 2);
        }

        #[test]
        fn push_dep_dedupes_by_ref_group_mode() {
            let mut sb = Sandbox::default();
            assert!(sb.push_dep(dep("//pkg:d", "", Mode::None, "a")));
            assert!(!sb.push_dep(dep("//pkg:d", "", Mode::None, "b")));
            assert_eq!(sb.deps().len(), 1);
        }

        #[test]
        fn push_dep_allows_same_ref_different_mode() {
            let mut sb = Sandbox::default();
            assert!(sb.push_dep(dep("//pkg:d", "", Mode::None, "a")));
            assert!(sb.push_dep(dep("//pkg:d", "", Mode::Link, "b")));
            assert_eq!(sb.deps().len(), 2);
        }

        // Regression: two direct deps each declaring the same transitive tool
        // produced duplicate `Input`s downstream, which then unpacked twice
        // and trip EEXIST in pluginexec's symlink loop.
        #[test]
        fn merge_sandbox_dedupes_same_tool_across_transitives() {
            let mut sb = Sandbox::default();

            let mut from_a = Sandbox::default();
            from_a.push_tool(tool("//heph:node", "", "tool||0"));

            let mut from_b = Sandbox::default();
            from_b.push_tool(tool("//heph:node", "", "tool||0"));

            sb.merge_sandbox(from_a, "_transitive_aaaa_0".to_string());
            sb.merge_sandbox(from_b, "_transitive_bbbb_1".to_string());

            assert_eq!(
                sb.tools().len(),
                1,
                "same (ref, group) must appear once across merges; got: {:?}",
                sb.tools()
            );
            // The first merger's prefixed id wins.
            assert_eq!(sb.tools()[0].id, "_transitive_aaaa_0_tool_tool||0");
        }

        #[test]
        fn merge_sandbox_dedupes_same_dep_across_transitives() {
            let mut sb = Sandbox::default();
            let mut a = Sandbox::default();
            a.push_dep(dep("//lib:l", "", Mode::None, "dep||0"));
            let mut b = Sandbox::default();
            b.push_dep(dep("//lib:l", "", Mode::None, "dep||0"));

            sb.merge_sandbox(a, "x".to_string());
            sb.merge_sandbox(b, "y".to_string());

            assert_eq!(sb.deps().len(), 1);
        }
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

    pub trait RawDef: erased_serde::Serialize + Any + Send + Sync {
        fn as_any(&self) -> &(dyn Any + Send + Sync);
    }

    impl<T: erased_serde::Serialize + Any + Send + Sync> RawDef for T {
        fn as_any(&self) -> &(dyn Any + Send + Sync) {
            self
        }
    }

    erased_serde::serialize_trait_object!(RawDef);

    fn serialize_raw_def<S: serde::Serializer>(
        d: &Arc<dyn RawDef>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        erased_serde::serialize(&**d, s)
    }

    /// Per-target cache configuration.
    ///
    /// `enabled` gates local caching; `remote_enabled` gates the remote cache
    /// (only meaningful when `enabled`). `history` is how many cache revisions
    /// (distinct input hashes) to retain for this target — GC trims older ones.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    pub struct CacheConfig {
        pub enabled: bool,
        pub remote_enabled: bool,
        pub history: u32,
    }

    impl CacheConfig {
        /// Default number of cache revisions kept per target.
        pub const DEFAULT_HISTORY: u32 = 1;

        /// Caching enabled, with the given remote-cache toggle and default history.
        pub fn on(remote_enabled: bool) -> Self {
            Self {
                enabled: true,
                remote_enabled,
                history: Self::DEFAULT_HISTORY,
            }
        }

        /// Caching fully disabled (local and remote).
        pub fn off() -> Self {
            Self {
                enabled: false,
                remote_enabled: false,
                history: 0,
            }
        }
    }

    #[derive(Clone, Serialize)]
    pub struct TargetDef {
        pub addr: Addr,
        pub labels: Vec<String>,
        #[serde(serialize_with = "serialize_raw_def")]
        pub raw_def: Arc<dyn RawDef>,
        pub inputs: Vec<Input>,
        pub outputs: Vec<Output>,
        pub support_files: Vec<path::Path>,
        pub cache: CacheConfig,
        pub pty: bool,
        #[serde(serialize_with = "serialize_hash")]
        pub hash: Vec<u8>,
        pub transparent: bool,
    }

    impl TargetDef {
        pub fn def<T: 'static>(&self) -> &T {
            self.raw_def
                .as_any()
                .downcast_ref::<T>()
                .expect("TargetDef raw_def type mismatch: wrong type T requested")
        }
        pub fn set_def<T: serde::Serialize + Send + Sync + 'static>(&mut self, def: T) {
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
        pub annotations: std::collections::BTreeMap<String, String>,
        /// Contributes its hashout to the parent's hashin. Default true.
        /// Set false for inputs that materialize at runtime but must not
        /// alter the parent's cache key (e.g. `runtime_deps`).
        pub hashed: bool,
        /// Materialized into the sandbox and surfaced through link routing
        /// (SRC_*/LIST_*/tools/etc). Default true. Set false for inputs that
        /// only contribute their hashout to the parent (e.g. `hash_deps`).
        pub runtime: bool,
    }

    impl Input {
        /// Default flags: contributes to both hash and runtime.
        pub fn standard(
            r#ref: TargetAddr,
            mode: InputMode,
            origin_id: String,
            annotations: std::collections::BTreeMap<String, String>,
        ) -> Self {
            Self {
                r#ref,
                mode,
                origin_id,
                annotations,
                hashed: true,
                runtime: true,
            }
        }
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

        /// Codegen mode parsed from the BUILD-file `codegen` attribute. A bare
        /// string (`copy` / `in_place`) or absent/null → `None`. `#[derive(SpecEnum)]`
        /// supplies the [`FromSpecValue`] impl (string → variant); `None` is
        /// `#[spec(skip)]` so it is only the absent/null default, never a string.
        ///
        /// [`FromSpecValue`]: crate::htspec::FromSpecValue
        #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, crate::htspec::SpecEnum)]
        pub enum CodegenMode {
            #[default]
            #[spec(skip)]
            None,
            Copy,
            InPlace,
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
    pub annotations: std::collections::BTreeMap<String, String>,
}

pub mod outputartifact {
    use crate::hartifactcontent::{Content as HContent, WalkEntry, WalkEntryKind};
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
                    kind: WalkEntryKind::File {
                        data: Box::new(io::Cursor::new(raw.data.clone())),
                        x: raw.x,
                    },
                }))),
                Content::File(file) => Box::new(std::iter::once(Ok(WalkEntry {
                    path: PathBuf::from(&file.out_path),
                    kind: WalkEntryKind::File {
                        data: Box::new(File::open(&file.source_path)?),
                        x: file.x,
                    },
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
#[derive(Default)]
pub struct RunResponse {
    pub artifacts: Vec<outputartifact::OutputArtifact>,
    /// Cleanup closure owned by the layer that built the sandbox. The
    /// FUSE bridge rms its upper-side dir directly (bypassing the live
    /// mount, which has an orphan-on-upper bug for `unlink` of dirs);
    /// the OS bridge rms the plain sandbox dir. Result.rs hands this
    /// to `sandbox_cleaner::enqueue` after `cache_locally` finishes,
    /// so the cleaner thread never has to branch on FUSE vs OS.
    pub sandbox_cleanup: Option<crate::engine::sandbox_cleaner::SandboxCleanupJob>,
    /// FUSE slot guards held open until the result lifecycle ends
    /// (alongside the cleanup `defer!` in result.rs). Dropping a guard
    /// deregisters the slot from the shared `LayeredFs`. Result.rs
    /// holds them across `cache_locally` and drops in the same defer
    /// that enqueues the sandbox-dir cleanup.
    pub fuse_slot_guards: Vec<crate::sandboxfuse::SlotGuard>,
}

/// One config field a driver accepts in a `target(...)` call, with its type and
/// documentation. Consumed by the BUILD-file LSP to offer completion and hover for
/// a target's driver-specific keyword arguments.
#[derive(Clone, Debug)]
pub struct DriverField {
    pub name: String,
    pub ty: crate::htvalue::signature::ParamType,
    pub doc: String,
    pub required: bool,
}

/// Declarative description of the config a driver understands. Returned by
/// [`Driver::schema`]; `None` means the driver exposes no schema (the default).
#[derive(Clone, Debug, Default)]
pub struct DriverSchema {
    pub fields: Vec<DriverField>,
}

#[async_trait]
pub trait Driver: Send + Sync {
    fn config(&self, req: ConfigRequest) -> anyhow::Result<ConfigResponse>;

    /// Optional: describe the config fields this driver accepts on a target, so
    /// tooling (the BUILD-file LSP) can complete and document them. Drivers that
    /// don't implement this return `None` and the LSP simply offers no
    /// driver-specific field hints.
    fn schema(&self) -> Option<DriverSchema> {
        None
    }

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
