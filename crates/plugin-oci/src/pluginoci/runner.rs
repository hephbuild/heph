//! `oci_runner` — a target that describes a container, so other targets can run
//! inside it.
//!
//! Same contract as every other runner: the output is a `runner.json`, and
//! consumers name this target with `runner = "//svc:runner"`.
//!
//! # Why the image digest is the point
//!
//! "Which container did this build run in" is otherwise recorded nowhere. The
//! fingerprint here is the image's content digest, resolved from the daemon at
//! runner-build time — the one runner where the fingerprint already exists and
//! needs no invention. Retagging `:latest` moves the digest, which moves this
//! target's hashout, which re-keys every consumer. Nothing else in the tree
//! makes that happen.
//!
//! # Session, not per-exec `docker run`
//!
//! One container is held open for the build and targets run inside it via the
//! agent protocol, rather than a fresh `docker run` per exec.
//!
//! That is partly the obvious performance argument, but mostly a structural
//! one: a per-exec `docker run` needs the *target's* sandbox directory and
//! working directory in its argv, and a wrap runner's prefix is static by
//! construction — it is the same argv for every target that uses it. Bending
//! the wrap config into a template language to carry per-target paths would
//! make every runner's config harder to read for one runner's benefit. The
//! session's launch argv is static (the mounts are per *workspace*), and the
//! per-target cwd travels in the agent request where it already belongs.
//!
//! # Mount policy is correctness, not preference
//!
//! Every absolute path the driver computed — tool symlinks, `$OUT`, `SRC_*` —
//! is a host path. The workspace root and heph's home are therefore bind-mounted
//! **at the same paths inside the container**. A remapped mount would leave the
//! target resolving paths that do not exist, and it would do so silently. The
//! heph binary is mounted read-only at its own path for the same reason: the
//! agent inside the container is that binary.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as OutPath};
use hplugin::driver::targetdef::{CacheConfig, Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::Spec;
use std::collections::BTreeMap;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

const FORMAT_VERSION: u32 = 1;
const OUT_FILE: &str = "runner.json";

/// Where an `image` given as a target address is read from in the sandbox.
const IMAGE_ORIGIN: &str = "image";

/// Longest ref read out of an `image` target's output. A docker reference is
/// well under this; anything longer is not a ref (an image archive named by
/// mistake), and is refused before it is read into memory.
const MAX_REF_LEN: u64 = 4096;

#[derive(Spec)]
pub(crate) struct TargetSpec {
    /// The image to run targets in: an `oci.load` target (`":load"`,
    /// `"//svc:load"`), or an image reference already in the local daemon
    /// (`"myimage:dev"`).
    ///
    /// A target is the usual form. The runner depends on it and reads the ref
    /// it loaded from its output, so the ref is written once, on the load, and
    /// whatever tag the load settled on — derived or fixed — is the one used.
    ///
    /// A reference must already be in the daemon: depend on the `oci.load` that
    /// puts it there via `deps`, so loading it happens first.
    #[spec(required)]
    pub image: String,
    /// Targets that must build first — with a reference `image`, normally the
    /// `oci.load` that puts it in the daemon. Hashed, so a rebuilt image
    /// re-derives the runner.
    pub deps: Vec<String>,
    /// Extra `docker run` arguments for the held container: `--network`,
    /// additional `-v` mounts, `--user`. The workspace and heph mounts are
    /// added automatically and must not be repeated here.
    pub run_args: Vec<String>,
    /// What the target reserves from the resource pools while it runs, e.g.
    /// `{"cpu": 2}`. Defaults to 1 cpu. Never hashed.
    pub resources: hplugin::driver::Resources,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct OciRunnerDef {
    pub image: ImageSource,
    pub run_args: Vec<String>,
}

/// Where the runner learns which image to inspect.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ImageSource {
    /// A reference written in the BUILD file, already in the daemon.
    Ref(String),
    /// The ref the `image` target wrote to its single output (an `oci.load`),
    /// read at run. The address is in the input, not here.
    Target,
}

#[derive(Debug)]
pub struct Driver {
    docker_bin: String,
    /// heph's resolved home, mounted into the container: sandboxes and the agent
    /// socket live under it. Handed in by the host (`CreateConfig.home` for the
    /// cdylib, `PluginInit.home` in-process) — never derived from a sandbox
    /// path, where a package named `sandbox` would be mistaken for the home's.
    ///
    /// `Err` holds why the host's home is unusable; the driver is still
    /// registered, and fails its runs with that reason (see [`home_from_host`]).
    home: Result<std::path::PathBuf, String>,
}

/// The home a cdylib host passed in `CreateConfig.home`, checked: the proto
/// default is `""` (a host that predates the field, or a broken one), and a
/// relative path would be resolved against whatever this process's cwd is.
pub fn home_from_host(home: &str) -> Result<std::path::PathBuf, String> {
    if home.is_empty() {
        return Err("the host passed no heph home (CreateConfig.home is empty)".to_string());
    }
    let path = std::path::PathBuf::from(home);
    if !path.is_absolute() {
        return Err(format!(
            "the host passed a relative heph home ({home:?}); it must be absolute"
        ));
    }
    Ok(path)
}

impl Driver {
    pub fn new(home: impl Into<std::path::PathBuf>) -> Self {
        Self::from_host_home(Ok(home.into()))
    }

    /// A driver over a home that may be unusable; see [`Self::home`].
    pub fn from_host_home(home: Result<std::path::PathBuf, String>) -> Self {
        Self {
            docker_bin: "docker".to_string(),
            home,
        }
    }

    /// Use `docker_bin` instead of `docker` from `PATH`.
    pub fn with_docker_bin(mut self, docker_bin: impl Into<String>) -> Self {
        self.docker_bin = docker_bin.into();
        self
    }
}

#[async_trait]
impl ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "runner".to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        TargetSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let spec = TargetSpec::from(&req.target_spec.config)?;
        let pkg = req.target_spec.addr.package.clone();
        if spec.image.is_empty() {
            anyhow::bail!(
                "oci.runner: `image` must name an `oci.load` target or an image reference"
            );
        }
        let image_target = if super::is_addr(&spec.image) {
            Some(
                TargetAddr::parse(&spec.image, &pkg)
                    .with_context(|| format!("parse `image` target {:?}", spec.image))?,
            )
        } else {
            None
        };

        let def = OciRunnerDef {
            image: match image_target {
                Some(_) => ImageSource::Target,
                None => ImageSource::Ref(spec.image.clone()),
            },
            run_args: spec.run_args.clone(),
        };

        let mut h = Xxh3::new();
        h.update(b"oci_runner");
        h.update(&FORMAT_VERSION.to_le_bytes());
        match &def.image {
            // Unchanged from before targets were accepted, so existing runners
            // keep their def hash.
            ImageSource::Ref(r) => h.update(r.as_bytes()),
            // A NUL no reference can hold keeps this apart from every `Ref`.
            // The address is hashed through the input edge, by its content.
            ImageSource::Target => h.update(b"\0target"),
        }
        for a in &def.run_args {
            h.update(a.as_bytes());
            h.update(b"\x1f");
        }
        let hash = format!("{:x}", h.digest()).into_bytes();

        let mut inputs = spec
            .deps
            .iter()
            .enumerate()
            .map(|(i, d)| {
                Ok(Input {
                    r#ref: TargetAddr::parse(d, &pkg)?,
                    mode: InputMode::Standard,
                    origin_id: format!("oci_runner|{i}"),
                    annotations: BTreeMap::new(),
                    hashed: true,
                    runtime: false,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        if let Some(r#ref) = image_target {
            // `runtime: true`: the run reads the ref out of the sandbox.
            inputs.push(Input {
                r#ref,
                mode: InputMode::Standard,
                origin_id: IMAGE_ORIGIN.to_string(),
                annotations: BTreeMap::new(),
                hashed: true,
                runtime: true,
            });
        }

        // With a reference `image`, local yes, remote never: the answer is a
        // fact about *this* daemon's image store, and publishing it would let
        // one machine's resolution key another's builds.
        //
        // With a target `image`, not cached at all. The load's output is its
        // ref, and a fixed `tag` makes that the same bytes whichever image it
        // loaded — so the load's hashout, and with it this target's key, would
        // not move when the image did, and a cached runner would keep
        // describing the old one. Re-resolving the digest is one `docker image
        // inspect` beside an `oci.load` that runs every build anyway, and the
        // consumers still key on `runner.json`'s bytes, which move exactly when
        // the digest does.
        let cache = match def.image {
            ImageSource::Ref(_) => CacheConfig::on(false),
            ImageSource::Target => CacheConfig::off(),
        };

        let out_path = hmodel::htpkg::join_rel_checked(pkg.as_str(), OUT_FILE)
            .with_context(|| format!("resolving {OUT_FILE} in package {pkg}"))?;

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![OutPath {
                        content: Content::FilePath(out_path),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }],
                cache,
                hash,
                resources: spec.resources,
                ..Default::default()
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
        mut req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        // First, before asking docker anything: without the home there is no
        // runner to describe.
        let heph_home = self.home.as_ref().map_err(|why| {
            anyhow::anyhow!(
                "oci_runner cannot mount heph's home into the container: {why}. \
                 The heph binary and the OCI plugin are likely mismatched versions."
            )
        })?;
        let def = req.request.target.def_de::<OciRunnerDef>().clone();
        let image = match def.image {
            ImageSource::Ref(r) => r,
            ImageSource::Target => {
                let path = super::dep_single_file(&req, IMAGE_ORIGIN).context(
                    "`image` must name a target with one output holding an image ref, such as \
                     an `oci.load`",
                )?;
                read_image_ref(&path)?
            }
        };

        // The digest, from the daemon. Resolved rather than trusted: a tag is a
        // moving pointer, and a fingerprint that recorded the tag would not move
        // when the image it names was rebuilt.
        let digest = {
            let argv = vec![
                self.docker_bin.clone(),
                "image".to_string(),
                "inspect".to_string(),
                "--format".to_string(),
                "{{.Id}}".to_string(),
                image.clone(),
            ];
            let cwd = req.sandbox_ws_dir.clone();
            let mut io = super::docker_build::tool_io(&mut req.request);
            // stdout is the image digest (data), never the log.
            super::docker_build::run_tool(
                argv,
                &cwd,
                "docker image inspect",
                hdriver_support::run_log::Streams::STDERR,
                &mut io,
                ctoken,
            )
            .await
            .with_context(|| {
                format!(
                    "resolving the digest of image {image:?}. It must already be in the local \
                     daemon — name the `oci.load` target that puts it there as `image`, so \
                     loading it comes first and its ref is the one inspected."
                )
            })?
            .trim()
            .to_string()
        };
        if digest.is_empty() {
            anyhow::bail!(
                "docker reported no digest for image {image:?}; cannot fingerprint the container"
            );
        }

        // No heph binary is mounted any more. The `session` form needed one —
        // it worked by running `heph __runner-agent` inside the environment —
        // and that is exactly what stopped a container check from working on a
        // macOS host, where the binary is Darwin and the image is Linux.
        // `docker exec` needs nothing of heph inside the image.
        let tree_root = req.request.tree_root_path.to_string_lossy().into_owned();
        // Sandboxes and the agent socket both live under heph's home, and the
        // container needs to see both at their own paths.
        let heph_home = heph_home.to_string_lossy().into_owned();

        // Named `oci`, not `session`: this plugin implements the runner. See
        // `pluginoci::exec_runner` for why a held `docker run` was the wrong
        // shape — it needed heph's own binary runnable inside the image, which
        // on a macOS host it is not, and a session launch argv cannot carry a
        // per-exec cwd.
        //
        // Mounts are declared here rather than in the runner because they are a
        // property of *this workspace*: every absolute path the driver computed
        // is a host path, so the workspace root and heph's home must appear
        // inside at the same paths.
        let doc = serde_json::json!({
            "version": 1,
            "fingerprint": format!("oci:{digest}"),
            "runner": crate::pluginoci::exec_runner::RUNNER_NAME,
            "config": {
                // By digest, not by tag: the container that runs the build must
                // be the one the fingerprint describes, even if the tag moves
                // mid-build.
                "image": digest,
                "mounts": [tree_root, heph_home],
                "run_args": def.run_args.clone(),
                "docker": self.docker_bin.clone(),
            },
        });

        let out = req.sandbox_pkg_dir.join(OUT_FILE);
        tokio::fs::write(&out, serde_json::to_vec_pretty(&doc)?)
            .await
            .with_context(|| format!("write {out:?}"))?;

        Ok(ManagedRunResponse::default())
    }
}

/// The image ref an `image` target wrote: one line, surrounding whitespace
/// dropped (`oci.load` writes none; a hand-written `echo` adds a newline).
///
/// Strict on everything else: a ref that came out empty, or as more than one
/// line, is a producer that did not write a ref, and inspecting a guess at one
/// would fingerprint the wrong image or fail with a message about the guess.
fn read_image_ref(path: &std::path::Path) -> anyhow::Result<String> {
    use std::io::Read as _;
    let not_a_ref = "`image` must name a target whose output is one image ref, such as an \
                     `oci.load`";
    let mut raw = Vec::new();
    std::fs::File::open(path)
        .with_context(|| format!("open the `image` target's output {path:?}"))?
        // One byte over the cap, so an over-long file is told apart from one
        // exactly at it without reading the rest of it.
        .take(MAX_REF_LEN + 1)
        .read_to_end(&mut raw)
        .with_context(|| format!("read the `image` target's output {path:?}"))?;
    anyhow::ensure!(
        u64::try_from(raw.len()).is_ok_and(|n| n <= MAX_REF_LEN),
        "the `image` target's output {path:?} is over {MAX_REF_LEN} bytes; {not_a_ref}"
    );
    let text = std::str::from_utf8(&raw)
        .with_context(|| format!("the `image` target's output {path:?} is not text; {not_a_ref}"))?
        .trim();
    anyhow::ensure!(
        !text.is_empty(),
        "the `image` target's output {path:?} is empty; {not_a_ref}"
    );
    anyhow::ensure!(
        !text.contains(char::is_whitespace),
        "the `image` target's output {path:?} holds more than one word ({text:?}); {not_a_ref}"
    );
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::parse_addr;
    use std::collections::HashMap;

    async fn parse(addr: &str, pairs: &[(&str, Value)]) -> anyhow::Result<TargetDef> {
        let config: HashMap<String, Value> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        let req = ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(hplugin::provider::TargetSpec {
                addr: parse_addr(addr)?,
                driver: "oci.runner".to_string(),
                config,
                ..Default::default()
            }),
        };
        Ok(Driver::new("/w/.heph")
            .parse(req, &StdCancellationToken::new())
            .await?
            .target_def)
    }

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    /// A target `image` is a hashed edge the run can read, in this package
    /// when relative; the runner is then uncached so a fixed-tag reload is seen.
    #[tokio::test]
    async fn a_target_image_is_a_runtime_edge_and_uncached() -> anyhow::Result<()> {
        let def = parse("//svc:runner", &[("image", s(":load"))]).await?;
        let edge = def
            .inputs
            .iter()
            .find(|i| i.origin_id == IMAGE_ORIGIN)
            .expect("an `image` edge");
        assert_eq!(edge.r#ref.r#ref.format(), "//svc:load");
        assert!(edge.hashed && edge.runtime);
        assert_eq!(def.def::<OciRunnerDef>().image, ImageSource::Target);
        assert!(!def.cache.enabled);

        let abs = parse("//svc:runner", &[("image", s("//img:load"))]).await?;
        let edge = abs.inputs.iter().find(|i| i.origin_id == IMAGE_ORIGIN);
        assert_eq!(
            edge.map(|i| i.r#ref.r#ref.format()).as_deref(),
            Some("//img:load")
        );
        Ok(())
    }

    /// A reference `image` is as before: no edge, cached locally, never remotely.
    #[tokio::test]
    async fn a_reference_image_adds_no_edge_and_stays_locally_cached() -> anyhow::Result<()> {
        let def = parse("//svc:runner", &[("image", s("app:dev"))]).await?;
        assert!(def.inputs.iter().all(|i| i.origin_id != IMAGE_ORIGIN));
        assert_eq!(
            def.def::<OciRunnerDef>().image,
            ImageSource::Ref("app:dev".to_string())
        );
        assert!(def.cache.enabled && !def.cache.remote_enabled);
        Ok(())
    }

    /// Changing how `image` is given must not move an existing runner's def
    /// hash: every consumer of every reference runner would full-miss.
    #[tokio::test]
    async fn a_reference_runner_keeps_its_def_hash() -> anyhow::Result<()> {
        let def = parse(
            "//svc:runner",
            &[
                ("image", s("app:dev")),
                ("run_args", Value::List(vec![s("--network=host")])),
            ],
        )
        .await?;
        let mut h = Xxh3::new();
        h.update(b"oci_runner");
        h.update(&FORMAT_VERSION.to_le_bytes());
        h.update(b"app:dev");
        h.update(b"--network=host");
        h.update(b"\x1f");
        assert_eq!(def.hash, format!("{:x}", h.digest()).into_bytes());
        Ok(())
    }

    /// A reference that looks like the target marker cannot collide with it.
    #[tokio::test]
    async fn a_target_and_a_reference_never_share_a_def_hash() -> anyhow::Result<()> {
        let target = parse("//svc:runner", &[("image", s(":load"))]).await?;
        let reference = parse("//svc:runner", &[("image", s("target"))]).await?;
        assert_ne!(target.hash, reference.hash);
        Ok(())
    }

    #[test]
    fn the_ref_is_read_trimmed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("load.ref");
        std::fs::write(&path, "bwan-rc-playwright:v1.57.0\n")?;
        assert_eq!(read_image_ref(&path)?, "bwan-rc-playwright:v1.57.0");
        std::fs::write(&path, "localhost:5000/app:9f2c")?;
        assert_eq!(read_image_ref(&path)?, "localhost:5000/app:9f2c");
        Ok(())
    }

    /// Each way a producer can fail to have written a ref is refused, naming
    /// what `image` expects, rather than inspecting a guess.
    #[test]
    fn what_is_not_a_ref_is_refused() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("out");
        let over = "a".repeat(usize::try_from(MAX_REF_LEN)? + 1);
        let at_cap = "a".repeat(usize::try_from(MAX_REF_LEN)?);
        let cases: [(&[u8], &str); 6] = [
            (b"", "is empty"),
            (b" \n\t\n", "is empty"),
            (b"app:dev\nother:tag\n", "more than one word"),
            (b"app dev", "more than one word"),
            (b"\xff\xfe\x00", "is not text"),
            (over.as_bytes(), "over 4096 bytes"),
        ];
        for (body, want) in cases {
            std::fs::write(&path, body)?;
            let err = format!("{:#}", read_image_ref(&path).expect_err("refused"));
            assert!(err.contains(want), "{body:?}: {err}");
            assert!(err.contains("oci.load"), "{body:?}: {err}");
        }
        std::fs::write(&path, &at_cap)?;
        assert_eq!(read_image_ref(&path)?, at_cap);
        Ok(())
    }

    #[test]
    fn an_absolute_host_home_is_accepted() {
        assert_eq!(
            home_from_host("/w/.heph").expect("home"),
            std::path::Path::new("/w/.heph")
        );
    }

    #[test]
    fn an_empty_host_home_is_refused_with_the_reason() {
        let err = home_from_host("").expect_err("empty");
        assert!(err.contains("CreateConfig.home is empty"), "{err}");
    }

    #[test]
    fn a_relative_host_home_is_refused() {
        let err = home_from_host(".heph").expect_err("relative");
        assert!(err.contains("must be absolute"), "{err}");
    }
}
