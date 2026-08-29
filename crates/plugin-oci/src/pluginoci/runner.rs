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

pub const DRIVER_NAME: &str = "oci_runner";

const FORMAT_VERSION: u32 = 1;
const OUT_FILE: &str = "runner.json";

#[derive(Spec)]
pub(crate) struct TargetSpec {
    /// Image reference to run targets in, e.g. `myimage:dev`. It must already
    /// be in the local daemon — depend on an `oci_load` target via `deps` so it
    /// is, and so loading it is part of this target's cache key.
    #[spec(required)]
    pub image: String,
    /// Targets that must build first — normally the `oci_load` that puts
    /// `image` in the daemon. Hashed, so a rebuilt image re-derives the runner.
    pub deps: Vec<String>,
    /// Extra `docker run` arguments for the held container: `--network`,
    /// additional `-v` mounts, `--user`. The workspace and heph mounts are
    /// added automatically and must not be repeated here.
    pub run_args: Vec<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct OciRunnerDef {
    pub image: String,
    pub run_args: Vec<String>,
}

#[derive(Debug)]
pub struct Driver {
    docker_bin: String,
}

impl Default for Driver {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver {
    pub fn new() -> Self {
        Self {
            docker_bin: "docker".to_string(),
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
            anyhow::bail!("oci_runner: `image` must name an image reference");
        }

        let def = OciRunnerDef {
            image: spec.image.clone(),
            run_args: spec.run_args.clone(),
        };

        let mut h = Xxh3::new();
        h.update(b"oci_runner");
        h.update(&FORMAT_VERSION.to_le_bytes());
        h.update(def.image.as_bytes());
        for a in &def.run_args {
            h.update(a.as_bytes());
            h.update(b"\x1f");
        }
        let hash = format!("{:x}", h.digest()).into_bytes();

        let inputs = spec
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
                support_files: vec![],
                // Local yes, remote never: the answer is a fact about *this*
                // daemon's image store, and publishing it would let one
                // machine's resolution key another's builds.
                cache: CacheConfig::on(false),
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
        mut req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<OciRunnerDef>().clone();

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
                def.image.clone(),
            ];
            let cwd = req.sandbox_ws_dir.clone();
            let mut io = super::docker_build::ToolIo::from_request(&mut req.request);
            super::docker_build::run_tool(argv, &cwd, "docker image inspect", &mut io, ctoken)
                .await
                .with_context(|| {
                    format!(
                        "resolving the digest of image {:?}. It must already be in the local \
                         daemon — depend on the `oci_load` target that puts it there via `deps`, \
                         so loading it is part of this runner's cache key.",
                        def.image
                    )
                })?
                .trim()
                .to_string()
        };
        if digest.is_empty() {
            anyhow::bail!(
                "docker reported no digest for image {:?}; cannot fingerprint the container",
                def.image
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
        let heph_home = heph_home_of(&req);

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

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

/// heph's home directory, derived from the sandbox path the bridge handed us.
///
/// The sandbox lives at `<home>/sandbox/<...>`, so the home is what remains
/// above the `sandbox` component. Derived rather than read from config because
/// a driver is handed what it needs and discovers nothing.
fn heph_home_of(req: &ManagedRunRequest<'_, '_>) -> String {
    let mut p = req.sandbox_dir.as_path();
    while let Some(parent) = p.parent() {
        if p.file_name().is_some_and(|n| n == "sandbox") {
            return parent.to_string_lossy().into_owned();
        }
        p = parent;
    }
    req.sandbox_dir.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn heph_home_is_the_directory_above_sandbox() {
        let p = PathBuf::from("/home/u/.heph3/sandbox/pkg/target/abc");
        assert_eq!(home_from(&p), "/home/u/.heph3");
    }

    /// A path with no `sandbox` component must degrade to something usable
    /// rather than panicking or walking to `/`.
    #[test]
    fn an_unexpected_sandbox_layout_falls_back_to_the_path_itself() {
        let p = PathBuf::from("/tmp/weird/place");
        assert_eq!(home_from(&p), "/tmp/weird/place");
    }

    /// The same walk `heph_home_of` does, over a bare path — the request type
    /// is not constructible in a unit test.
    fn home_from(sandbox: &std::path::Path) -> String {
        let mut p = sandbox;
        while let Some(parent) = p.parent() {
            if p.file_name().is_some_and(|n| n == "sandbox") {
                return parent.to_string_lossy().into_owned();
            }
            p = parent;
        }
        sandbox.to_string_lossy().into_owned()
    }
}
