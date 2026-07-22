//! The `oci_image` driver: builds a container image archive from a Dockerfile +
//! build context into a cacheable target output.
//!
//! One target = one built image, materialized as a single archive file (`oci` or
//! `docker` format) plus a `digest` output group carrying the image digest for
//! cheap downstream consumption (push/load) without unpacking the archive.
//!
//! Caching is layered:
//!  1. **heph input-hash cache** (the big win): the build context files,
//!     Dockerfile, build args, target stage and platforms are the target's
//!     inputs — an unchanged context is a cache hit and the image is *not*
//!     rebuilt, the archive is served from the local or remote cache. Image
//!     nondeterminism (timestamps) does not defeat this: heph keys on inputs and
//!     serves the identical cached archive to consumers.
//!  2. **BuildKit layer cache** when a build does run: `cache_from` / `cache_to`
//!     wire `docker buildx --cache-from/--cache-to` (registry or inline refs).
//!     These are build *optimizations*, not part of the image's identity, so
//!     they are deliberately excluded from the input hash.
//!
//! The builder is host `docker buildx` (a host capability, like `http_fetch`'s
//! network); a hermetic toolchain can replace it later without changing targets.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as OutPath};
use hplugin::driver::targetdef::{Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::{Spec, TargetSpecCache};
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

pub const DRIVER_NAME: &str = "oci_image";

pub mod load;
pub mod pull;
pub mod push;

/// Archive format an image is built/consumed as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ImageFormat {
    /// OCI image layout archive (`--output type=oci`). Portable, standard;
    /// pushed/loaded daemonlessly with `skopeo`.
    Oci,
    /// Docker-format image archive (`--output type=docker`). Loadable straight
    /// into a docker daemon with `docker load`.
    Docker,
}

impl ImageFormat {
    /// The BuildKit `--output type=` value.
    fn output_type(self) -> &'static str {
        match self {
            ImageFormat::Oci => "oci",
            ImageFormat::Docker => "docker",
        }
    }

    /// The containers/image transport name for reading this archive as a source
    /// (skopeo `<transport>:<path>`).
    pub(crate) fn transport(self) -> &'static str {
        match self {
            ImageFormat::Oci => "oci-archive",
            ImageFormat::Docker => "docker-archive",
        }
    }

    pub(crate) fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "oci" => Ok(ImageFormat::Oci),
            "docker" => Ok(ImageFormat::Docker),
            other => anyhow::bail!("`format` must be \"oci\" or \"docker\", got {other:?}"),
        }
    }
}

/// Config for an `oci_image` target.
#[derive(Spec)]
struct OciImageSpec {
    /// Dockerfile path, relative to the target's package. Default `Dockerfile`.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    dockerfile: Option<String>,
    /// Build-context dependencies, grouped by name → list of target addresses.
    /// Every file these targets produce is materialized into the sandbox at its
    /// package-relative path and becomes the `docker build` context. These are
    /// hashed inputs: an unchanged context is a cache hit (no rebuild).
    context: HashMap<String, Vec<String>>,
    /// Archive format: `oci` (default) or `docker`.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    format: Option<String>,
    /// `--build-arg` values passed to the build. Hashed (they change the image).
    build_args: HashMap<String, String>,
    /// Build a specific stage (`--target`) of a multi-stage Dockerfile.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    target: Option<String>,
    /// Target platforms (`--platform`), e.g. `["linux/amd64", "linux/arm64"]`.
    /// Empty builds for the host platform. Multi-platform builds need a
    /// container-driver builder (`docker buildx create --driver docker-container`);
    /// the default daemon builder only builds one platform.
    platforms: Vec<String>,
    /// BuildKit build secrets, as raw `--secret` specs, e.g.
    /// `["id=npmrc,src=.npmrc"]` or `["id=token,env=TOKEN"]`, consumed in the
    /// Dockerfile via `RUN --mount=type=secret`. Hashed (they can change the
    /// image). Secrets are not written into the image, but keep sensitive values
    /// out of the ref by preferring `env=` sources.
    secrets: Vec<String>,
    /// SSH forwarding, as raw `--ssh` specs, e.g. `["default"]` or
    /// `["id=github,src=/path/to/key"]`, consumed via `RUN --mount=type=ssh`.
    /// Hashed.
    ssh: Vec<String>,
    /// BuildKit `--cache-from` refs (registry or inline), e.g.
    /// `["type=registry,ref=reg/img:cache"]`. A build *optimization* — excluded
    /// from the input hash, so changing it never busts the heph cache.
    cache_from: Vec<String>,
    /// BuildKit `--cache-to` refs, e.g.
    /// `["type=registry,ref=reg/img:cache,mode=max"]` or `["type=inline"]`.
    /// A build optimization — excluded from the input hash.
    cache_to: Vec<String>,
    /// Caching for the built archive. Defaults to on for both the local and
    /// remote cache. `cache = False` disables both; the dict form
    /// `{enabled, remote, history}` toggles them independently.
    cache: TargetSpecCache,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciImageDef {
    /// Package-relative Dockerfile path.
    dockerfile: String,
    /// Workspace-relative output archive path (its basename is written into the
    /// sandbox package dir at run time).
    out: String,
    /// Workspace-relative digest output path.
    digest_out: String,
    format: ImageFormat,
    /// Sorted for a stable hash.
    build_args: BTreeMap<String, String>,
    target: Option<String>,
    platforms: Vec<String>,
    secrets: Vec<String>,
    ssh: Vec<String>,
    /// Layer-cache sources — NOT hashed (build optimization only).
    cache_from: Vec<String>,
    /// Layer-cache destinations — NOT hashed.
    cache_to: Vec<String>,
}

/// Bump to invalidate cached builds when the output layout / arg recipe changes.
const OCI_IMAGE_FORMAT_VERSION: u32 = 1;

impl Hash for OciImageDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_IMAGE_FORMAT_VERSION.hash(state);
        self.dockerfile.hash(state);
        self.out.hash(state);
        self.digest_out.hash(state);
        self.format.output_type().hash(state);
        self.build_args.hash(state);
        self.target.hash(state);
        self.platforms.hash(state);
        self.secrets.hash(state);
        self.ssh.hash(state);
        // `cache_from` / `cache_to` are deliberately excluded: they are build
        // optimizations, not part of the image's identity.
    }
}

/// Assemble the `docker buildx build` argv. Pure so it can be unit-tested
/// without a docker daemon. `argv[0]` is the docker binary.
fn build_argv(
    docker_bin: &str,
    def: &OciImageDef,
    context_dir: &Path,
    dockerfile_full: &Path,
    out_tar: &Path,
    metadata_file: &Path,
) -> Vec<String> {
    let mut argv = vec![
        docker_bin.to_string(),
        "buildx".to_string(),
        "build".to_string(),
        "--file".to_string(),
        dockerfile_full.to_string_lossy().into_owned(),
        "--output".to_string(),
        format!(
            "type={},dest={}",
            def.format.output_type(),
            out_tar.to_string_lossy()
        ),
        "--metadata-file".to_string(),
        metadata_file.to_string_lossy().into_owned(),
    ];

    if let Some(target) = &def.target {
        argv.push("--target".to_string());
        argv.push(target.clone());
    }
    if !def.platforms.is_empty() {
        argv.push("--platform".to_string());
        argv.push(def.platforms.join(","));
    }
    // BTreeMap iterates sorted → deterministic argv.
    for (k, v) in &def.build_args {
        argv.push("--build-arg".to_string());
        argv.push(format!("{k}={v}"));
    }
    for secret in &def.secrets {
        argv.push("--secret".to_string());
        argv.push(secret.clone());
    }
    for ssh in &def.ssh {
        argv.push("--ssh".to_string());
        argv.push(ssh.clone());
    }
    for from in &def.cache_from {
        argv.push("--cache-from".to_string());
        argv.push(from.clone());
    }
    for to in &def.cache_to {
        argv.push("--cache-to".to_string());
        argv.push(to.clone());
    }
    argv.push(context_dir.to_string_lossy().into_owned());
    argv
}

/// Extract the image digest from a `docker buildx --metadata-file` JSON blob.
fn parse_metadata_digest(metadata: &str) -> anyhow::Result<String> {
    let v: serde_json::Value =
        serde_json::from_str(metadata).context("parse buildx --metadata-file JSON")?;
    v.get("containerimage.digest")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .context("buildx metadata missing `containerimage.digest`")
}

/// Join a package-relative path onto a (possibly empty) package prefix, yielding
/// a workspace-relative path.
fn ws_path(pkg: &str, rel: &str) -> String {
    if pkg.is_empty() {
        rel.to_string()
    } else {
        format!("{pkg}/{rel}")
    }
}

pub struct Driver {
    docker_bin: String,
}

impl Default for Driver {
    fn default() -> Self {
        Driver::new()
    }
}

impl Driver {
    pub fn new() -> Self {
        Driver {
            docker_bin: "docker".to_string(),
        }
    }

    /// Override the docker binary — used by tests to substitute a fake.
    #[cfg(test)]
    fn with_binary(bin: impl Into<String>) -> Self {
        Driver {
            docker_bin: bin.into(),
        }
    }
}

#[async_trait]
impl ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        OciImageSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let spec =
            OciImageSpec::from(req.target_spec.config.clone()).context("parse oci_image config")?;
        let pkg = addr.package.clone();
        let pkg_str = addr.package.as_str();

        let format = ImageFormat::parse(spec.format.as_deref().unwrap_or("oci"))?;
        let dockerfile = spec.dockerfile.unwrap_or_else(|| "Dockerfile".to_string());
        let out = ws_path(pkg_str, "image.tar");
        let digest_out = ws_path(pkg_str, "digest");

        // Build-context inputs: every file the context targets produce lands in
        // the sandbox at its package-relative path (the build context root).
        let inputs: Vec<Input> = spec
            .context
            .into_iter()
            .flat_map(|(group, refs)| {
                let pkg = pkg.clone();
                refs.into_iter()
                    .enumerate()
                    .map(move |(i, r)| -> anyhow::Result<Input> {
                        Ok(Input {
                            r#ref: TargetAddr::parse(&r, &pkg)?,
                            mode: InputMode::Standard,
                            origin_id: format!("context|{group}|{i}"),
                            annotations: BTreeMap::new(),
                            hashed: true,
                            runtime: true,
                        })
                    })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let def = OciImageDef {
            dockerfile,
            out: out.clone(),
            digest_out: digest_out.clone(),
            format,
            build_args: spec.build_args.into_iter().collect(),
            target: spec.target,
            platforms: spec.platforms,
            secrets: spec.secrets,
            ssh: spec.ssh,
            cache_from: spec.cache_from,
            cache_to: spec.cache_to,
        };

        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("oci_image_{}", addr.format())
            });
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                outputs: vec![
                    Output {
                        group: String::new(),
                        paths: vec![OutPath {
                            content: Content::FilePath(out),
                            codegen_tree: CodegenMode::None,
                            collect: true,
                        }],
                    },
                    Output {
                        group: "digest".to_string(),
                        paths: vec![OutPath {
                            content: Content::FilePath(digest_out),
                            codegen_tree: CodegenMode::None,
                            collect: true,
                        }],
                    },
                ],
                support_files: vec![],
                cache: spec.cache.into(),
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
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<OciImageDef>();
        let pkg_dir = &req.sandbox_pkg_dir;

        let dockerfile_full = pkg_dir.join(&def.dockerfile);
        anyhow::ensure!(
            dockerfile_full.exists(),
            "oci_image: Dockerfile {:?} not found in build context (declare it in `context`)",
            def.dockerfile
        );

        let out_name = basename(&def.out)?;
        let out_tar = pkg_dir.join(out_name);
        let digest_name = basename(&def.digest_out)?;
        let digest_path = pkg_dir.join(digest_name);
        // Metadata lands outside the workspace dir so it is not collected as an
        // output.
        let metadata_file = req.sandbox_dir.join("oci-metadata.json");

        let argv = build_argv(
            &self.docker_bin,
            def,
            pkg_dir,
            &dockerfile_full,
            &out_tar,
            &metadata_file,
        );

        run_cmd_cancellable(argv, ctoken, "docker buildx build")
            .await
            .context("oci_image build")?;

        let metadata = std::fs::read_to_string(&metadata_file)
            .with_context(|| format!("read buildx metadata {metadata_file:?}"))?;
        let digest = parse_metadata_digest(&metadata)?;
        std::fs::write(&digest_path, &digest)
            .with_context(|| format!("write digest {digest_path:?}"))?;

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

/// Run a subprocess to completion, failing with the captured stderr on a
/// non-zero exit. `argv[0]` is the binary. `what` names the operation for error
/// context. Pure blocking work.
fn run_cmd(argv: &[String], what: &str) -> anyhow::Result<()> {
    let (bin, args) = argv.split_first().context("empty argv (internal bug)")?;
    let output = std::process::Command::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("spawn {bin}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "{what} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Run `argv` off the async runtime, racing it against cancellation. The child
/// is not killed mid-syscall (same tradeoff as http_fetch) — cancellation stops
/// awaiting it.
pub(crate) async fn run_cmd_cancellable(
    argv: Vec<String>,
    ctoken: &(dyn Cancellable + Send + Sync),
    what: &'static str,
) -> anyhow::Result<()> {
    let work = tokio::task::spawn_blocking(move || run_cmd(&argv, what));
    let run = async {
        work.await
            .with_context(|| format!("{what} task panicked"))?
            .context(what)
    };
    tokio::select! {
        r = run => r,
        () = ctoken.cancelled() => anyhow::bail!("{what}: cancelled"),
    }
}

/// Absolute path to the single file a Dep input materialized into the sandbox.
/// Reads the input's `.list` file (one absolute path per line — see
/// `driver_managed.rs::list_path_for`). Errors unless exactly one file was
/// produced, so a caller expecting one archive fails loudly on a mis-declared
/// dep.
pub(crate) fn dep_single_file(
    req: &ManagedRunRequest<'_, '_>,
    origin_id: &str,
) -> anyhow::Result<PathBuf> {
    let m = req
        .inputs
        .iter()
        .find(|m| {
            m.input.origin_id == origin_id
                && matches!(
                    m.input.artifact.r#type,
                    hplugin::driver::inputartifact::Type::Dep
                )
        })
        .with_context(|| format!("no dep input {origin_id:?} in sandbox"))?;
    let list_path = m.require_list_path()?;
    // The `.list` file holds one absolute materialized path per line.
    let content = std::fs::read_to_string(list_path)
        .with_context(|| format!("read dep list {list_path:?}"))?;
    let mut paths = content.lines().filter(|l| !l.is_empty());
    let first = paths
        .next()
        .with_context(|| format!("dep {origin_id:?} produced no files"))?;
    anyhow::ensure!(
        paths.next().is_none(),
        "dep {origin_id:?} produced more than one file; expected exactly one archive"
    );
    Ok(PathBuf::from(first))
}

fn basename(path: &str) -> anyhow::Result<&std::ffi::OsStr> {
    Path::new(path)
        .file_name()
        .with_context(|| format!("path {path:?} has no file name"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::parse_addr;
    use hplugin::provider::TargetSpec;

    fn parse_req(addr: &str, config: HashMap<String, Value>) -> ParseRequest {
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: parse_addr(addr).expect("addr"),
                driver: DRIVER_NAME.to_string(),
                config,
                ..Default::default()
            }),
        }
    }

    async fn parse(addr: &str, config: HashMap<String, Value>) -> ParseResponse {
        Driver::new()
            .parse(parse_req(addr, config), &StdCancellationToken::new())
            .await
            .expect("parse")
    }

    #[test]
    fn format_parse_rejects_unknown() {
        assert_eq!(ImageFormat::parse("oci").unwrap(), ImageFormat::Oci);
        assert_eq!(ImageFormat::parse("docker").unwrap(), ImageFormat::Docker);
        let err = ImageFormat::parse("tar").unwrap_err();
        assert!(format!("{err:#}").contains("oci"), "got: {err:#}");
    }

    /// The build command carries the format, dockerfile, output dest, metadata
    /// file, sorted build args and cache refs, ending in the context dir.
    #[test]
    fn build_argv_assembles_expected_command() {
        let def = OciImageDef {
            dockerfile: "Dockerfile".to_string(),
            out: "app/image.tar".to_string(),
            digest_out: "app/digest".to_string(),
            format: ImageFormat::Oci,
            build_args: BTreeMap::from([
                ("B".to_string(), "2".to_string()),
                ("A".to_string(), "1".to_string()),
            ]),
            target: Some("runtime".to_string()),
            platforms: vec!["linux/amd64".to_string(), "linux/arm64".to_string()],
            secrets: vec!["id=token,env=TOKEN".to_string()],
            ssh: vec!["default".to_string()],
            cache_from: vec!["type=registry,ref=reg/app:cache".to_string()],
            cache_to: vec!["type=inline".to_string()],
        };
        let argv = build_argv(
            "docker",
            &def,
            Path::new("/sbx/app"),
            Path::new("/sbx/app/Dockerfile"),
            Path::new("/sbx/app/image.tar"),
            Path::new("/sbx/meta.json"),
        );

        assert_eq!(argv[0..3], ["docker", "buildx", "build"]);
        let joined = argv.join(" ");
        assert!(joined.contains("--output type=oci,dest=/sbx/app/image.tar"));
        assert!(joined.contains("--file /sbx/app/Dockerfile"));
        assert!(joined.contains("--metadata-file /sbx/meta.json"));
        assert!(joined.contains("--target runtime"));
        assert!(joined.contains("--platform linux/amd64,linux/arm64"));
        // Sorted: A before B.
        let a = argv.iter().position(|x| x == "A=1").unwrap();
        let b = argv.iter().position(|x| x == "B=2").unwrap();
        assert!(a < b, "build args must be sorted: {argv:?}");
        assert!(joined.contains("--secret id=token,env=TOKEN"), "{joined}");
        assert!(joined.contains("--ssh default"), "{joined}");
        assert!(joined.contains("--cache-from type=registry,ref=reg/app:cache"));
        assert!(joined.contains("--cache-to type=inline"));
        // Context dir is the last arg.
        assert_eq!(argv.last().unwrap(), "/sbx/app");
    }

    #[test]
    fn docker_format_selects_docker_output_type() {
        let def = OciImageDef {
            dockerfile: "Dockerfile".to_string(),
            out: "image.tar".to_string(),
            digest_out: "digest".to_string(),
            format: ImageFormat::Docker,
            build_args: BTreeMap::new(),
            target: None,
            platforms: vec![],
            secrets: vec![],
            ssh: vec![],
            cache_from: vec![],
            cache_to: vec![],
        };
        let argv = build_argv(
            "docker",
            &def,
            Path::new("/c"),
            Path::new("/c/Dockerfile"),
            Path::new("/c/image.tar"),
            Path::new("/m.json"),
        );
        assert!(argv.join(" ").contains("type=docker,dest=/c/image.tar"));
    }

    #[test]
    fn parse_metadata_digest_extracts_containerimage_digest() {
        let meta = r#"{"containerimage.digest":"sha256:abc","image.name":"x"}"#;
        assert_eq!(parse_metadata_digest(meta).unwrap(), "sha256:abc");
        let err = parse_metadata_digest(r#"{"image.name":"x"}"#).unwrap_err();
        assert!(
            format!("{err:#}").contains("containerimage.digest"),
            "got: {err:#}"
        );
    }

    #[tokio::test]
    async fn parse_declares_context_inputs_and_two_outputs() {
        let config = HashMap::from([(
            "context".to_string(),
            Value::List(vec![Value::String(":srcs".to_string())]),
        )]);
        let resp = parse("//app:img", config).await;

        assert_eq!(resp.target_def.inputs.len(), 1);
        assert_eq!(resp.target_def.inputs[0].r#ref.r#ref.format(), "//app:srcs");
        assert!(resp.target_def.inputs[0].hashed);

        let groups: Vec<&str> = resp
            .target_def
            .outputs
            .iter()
            .map(|o| o.group.as_str())
            .collect();
        assert_eq!(groups, ["", "digest"]);
        assert!(matches!(
            &resp.target_def.outputs[0].paths[0].content,
            Content::FilePath(p) if p == "app/image.tar"
        ));
        assert!(matches!(
            &resp.target_def.outputs[1].paths[0].content,
            Content::FilePath(p) if p == "app/digest"
        ));
        let def = resp.target_def.def::<OciImageDef>();
        assert_eq!(def.dockerfile, "Dockerfile");
        assert_eq!(def.format, ImageFormat::Oci);
    }

    /// Layer-cache refs are build optimizations: changing them must NOT change
    /// the input hash (an unchanged context stays a cache hit).
    #[tokio::test]
    async fn cache_refs_do_not_affect_hash() {
        let base = HashMap::from([(
            "context".to_string(),
            Value::List(vec![Value::String(":srcs".to_string())]),
        )]);
        let mut with_cache = base.clone();
        with_cache.insert(
            "cache_to".to_string(),
            Value::List(vec![Value::String("type=inline".to_string())]),
        );

        let a = parse("//app:img", base).await;
        let b = parse("//app:img", with_cache).await;
        assert_eq!(
            a.target_def.hash, b.target_def.hash,
            "cache_to must not affect the input hash"
        );
    }

    /// Build args, in contrast, DO change the image → different hash.
    #[tokio::test]
    async fn build_args_affect_hash() {
        let base = HashMap::from([(
            "context".to_string(),
            Value::List(vec![Value::String(":srcs".to_string())]),
        )]);
        let mut with_arg = base.clone();
        with_arg.insert(
            "build_args".to_string(),
            Value::Map(HashMap::from([(
                "VERSION".to_string(),
                Value::String("1.2".to_string()),
            )])),
        );

        let a = parse("//app:img", base).await;
        let b = parse("//app:img", with_arg).await;
        assert_ne!(a.target_def.hash, b.target_def.hash);
    }

    #[tokio::test]
    async fn parse_defaults_to_local_and_remote_cache() {
        let config = HashMap::from([(
            "context".to_string(),
            Value::List(vec![Value::String(":srcs".to_string())]),
        )]);
        let resp = parse("//app:img", config).await;
        assert!(resp.target_def.cache.enabled);
        assert!(resp.target_def.cache.remote_enabled);
    }

    #[tokio::test]
    async fn parse_rejects_unknown_format() {
        let config = HashMap::from([
            (
                "context".to_string(),
                Value::List(vec![Value::String(":srcs".to_string())]),
            ),
            ("format".to_string(), Value::String("tarball".to_string())),
        ]);
        let err = Driver::new()
            .parse(parse_req("//app:img", config), &StdCancellationToken::new())
            .await
            .err()
            .expect("bad format must fail parse");
        assert!(format!("{err:#}").contains("oci"), "got: {err:#}");
    }

    #[test]
    fn with_binary_overrides_docker_bin() {
        let d = Driver::with_binary("/fake/docker");
        assert_eq!(d.docker_bin, "/fake/docker");
    }
}
