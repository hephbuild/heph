//! `oci_runner` — run a target's processes inside a container.
//!
//! Two halves, like every runner: a **driver** that resolves the image and
//! writes it as the target's artifact, and a **runner** that reads that artifact
//! and serves sessions from it.
//!
//! The container is started **once per environment** and every target is
//! `docker exec`'d into it. The alternative — `docker run --rm` per spawn — is
//! simpler and needs no teardown, but it pays a container create per process,
//! which is the cost a session exists to amortize.
//!
//! ## Why this is `Wrap` and not `Agent`
//!
//! `docker exec` creates the process on the far side of the daemon socket, so
//! the environment heph sets belongs to the `docker` CLI on *this* side and the
//! container process sees none of it. That is exactly the distinction
//! [`WrapEnv::Args`] exists for: each variable is rendered into the wrapper's
//! own argv as `-e K=V`. A wrapper of this kind using `Inherit` would run every
//! target with an environment it never asked for and no error to show for it.
//!
//! ## What is pinned, and what is not
//!
//! An image referenced by **digest** is content the cache key already covers, so
//! the session reports `Identity::Pinned`. A tag is a claim — it can move under
//! a build — so a tagged reference reports `Asserted` and says why. heph does
//! not refuse a tag: choosing a weakly-pinned environment is the user's call,
//! and reporting the tradeoff honestly is the job.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hexec_runner::{
    ExecRunner, ExecSession, Identity, OpenRequest, SessionCaps, SessionDescription, WrapEnv,
    WrapSession,
};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as OutPath};
use hplugin::driver::targetdef::{CacheConfig, Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::Spec;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

use super::{dep_single_file, ws_path};

pub const DRIVER_NAME: &str = "oci_runner";

/// Bump to invalidate every runner artifact when this file's meaning changes,
/// for the same reason the devenv snapshot carries one: the runner half is not
/// a target, so it has no `TargetDef.hash` of its own.
const FORMAT_VERSION: u32 = 1;

const IMAGE_ORIGIN: &str = "image";

#[derive(Spec)]
struct OciRunnerSpec {
    /// The image to run in.
    ///
    /// Either a **literal reference** (`ubuntu@sha256:…`, or a tag) or the
    /// **addr of a target** that writes one — an `oci_load` target's `.ref`
    /// output. An addr is anything starting with `//` or `:`.
    ///
    /// Prefer a digest: it is the difference between `Pinned` and `Asserted`.
    image: String,
    /// The `docker` binary. Defaults to `docker` on the driver's PATH.
    bin: Option<String>,
    /// The command that holds the container open. Defaults to
    /// `["sleep", "infinity"]`, which needs a `sleep` in the image — a
    /// distroless or scratch image must name something it does have.
    keepalive: Vec<String>,
    /// Extra `docker run` arguments, applied when the container starts:
    /// `["--network", "none"]`, `["-v", "/opt/toolchain:/opt/toolchain:ro"]`.
    ///
    /// The sandbox root is always mounted at the same path inside the container
    /// — without it a target's `$OUT` and `$SRC` name paths that do not exist
    /// there — so this is for anything *beyond* that.
    run_args: Vec<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Hash)]
struct OciRunnerDef {
    /// `None` when `image` was a literal; otherwise the ref is read from the
    /// dep's output at run time.
    literal_image: Option<String>,
    bin: String,
    keepalive: Vec<String>,
    run_args: Vec<String>,
    out: String,
}

/// The artifact. **This _is_ the description** — the runner half parses it and
/// reads nothing else, so everything the environment depends on is content the
/// consumer's cache key already covers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RunnerArtifactFile {
    pub format_version: u32,
    pub image: String,
    pub bin: String,
    pub keepalive: Vec<String>,
    pub run_args: Vec<String>,
}

/// What the `docker` CLI itself needs to run — a `PATH` to be found on, and the
/// daemon/socket selection a developer's shell already carries.
///
/// Deliberately not the target's environment: nothing here reaches the process
/// inside the container, which gets its environment through `-e` instead.
fn docker_cli_env() -> Vec<(OsString, OsString)> {
    [
        "PATH",
        "HOME",
        "DOCKER_HOST",
        "DOCKER_CONFIG",
        "DOCKER_CONTEXT",
    ]
    .into_iter()
    .filter_map(|k| std::env::var_os(k).map(|v| (OsString::from(k), v)))
    .collect()
}

fn looks_like_addr(s: &str) -> bool {
    s.starts_with("//") || s.starts_with(':')
}

/// Whether a reference names content rather than a moving label.
fn is_digest_pinned(image: &str) -> bool {
    image.contains("@sha256:") || image.contains("@sha512:")
}

pub struct Driver;

#[async_trait]
impl ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        OciRunnerSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let spec =
            OciRunnerSpec::from(&req.target_spec.config).context("parse oci_runner config")?;

        if spec.image.trim().is_empty() {
            anyhow::bail!(
                "`image` is required: an image reference, or the addr of a target that writes one"
            );
        }

        let mut inputs = Vec::new();
        let literal_image = if looks_like_addr(&spec.image) {
            let image_ref = TargetAddr::parse(&spec.image, &addr.package)
                .with_context(|| format!("parse image target {:?}", spec.image))?;
            inputs.push(Input {
                r#ref: image_ref,
                mode: InputMode::Standard,
                origin_id: IMAGE_ORIGIN.to_string(),
                annotations: BTreeMap::new(),
                hashed: true,
                runtime: true,
            });
            None
        } else {
            Some(spec.image.clone())
        };

        let keepalive = if spec.keepalive.is_empty() {
            vec!["sleep".to_string(), "infinity".to_string()]
        } else {
            spec.keepalive
        };

        let out = ws_path(addr.package.as_str(), &format!("{}.runner.json", addr.name));
        let def = OciRunnerDef {
            literal_image,
            bin: spec.bin.unwrap_or_else(|| "docker".to_string()),
            keepalive,
            run_args: spec.run_args,
            out: out.clone(),
        };

        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("oci_runner_{}", addr.format())
            });
            FORMAT_VERSION.hash(&mut h);
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![OutPath {
                        content: Content::FilePath(out),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }],
                support_files: vec![],
                // Local cache only. The reference may name an image that
                // exists solely in *this* machine's daemon — an `oci_load`
                // target's output does — so handing it to another machine over
                // a shared cache would hand it a name it cannot resolve. Same
                // reason devenv snapshots are local-only.
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
        req: ManagedRunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<OciRunnerDef>();

        let image = match &def.literal_image {
            Some(i) => i.clone(),
            None => {
                let path = dep_single_file(&req, IMAGE_ORIGIN)
                    .context("locate the image target's ref output")?;
                std::fs::read_to_string(&path)
                    .with_context(|| format!("read image ref from {path:?}"))?
                    .trim()
                    .to_string()
            }
        };
        if image.is_empty() {
            anyhow::bail!("the image target produced an empty reference");
        }

        let artifact = RunnerArtifactFile {
            format_version: FORMAT_VERSION,
            image,
            bin: def.bin.clone(),
            keepalive: def.keepalive.clone(),
            run_args: def.run_args.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&artifact).context("encode the runner artifact")?;
        let out = req.sandbox_ws_dir.join(&def.out);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&out, &bytes).with_context(|| format!("write {}", out.display()))?;

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

/// The runner half: start one container per environment, `docker exec` into it.
pub struct Runner {
    /// Mounted into every container at the same path. Targets address `$OUT` and
    /// `$SRC` by absolute path, so the path inside must equal the path outside
    /// or every one of them dangles.
    sandbox_root: std::path::PathBuf,
}

impl Runner {
    pub fn new(sandbox_root: std::path::PathBuf) -> Self {
        Self { sandbox_root }
    }
}

#[async_trait]
impl ExecRunner for Runner {
    async fn open(
        &self,
        req: OpenRequest,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<Arc<dyn ExecSession>> {
        let artifact = req
            .artifacts
            .iter()
            .find(|a| a.path.ends_with(".runner.json"))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "runner {} produced no *.runner.json: an `oci_runner` runner target must be \
                     built by the `oci_runner` driver",
                    req.runner_addr,
                )
            })?;
        let file: RunnerArtifactFile = serde_json::from_slice(&artifact.bytes)
            .with_context(|| format!("parse the runner artifact from {}", req.runner_addr))?;

        if file.format_version != FORMAT_VERSION {
            anyhow::bail!(
                "{} was built by a different version of the oci_runner driver (artifact v{}, this \
                 heph understands v{}) — rebuild it",
                req.runner_addr,
                file.format_version,
                FORMAT_VERSION,
            );
        }

        let mount = format!(
            "{}:{}",
            self.sandbox_root.display(),
            self.sandbox_root.display()
        );
        let mut args: Vec<OsString> = vec![
            OsString::from("run"),
            OsString::from("-d"),
            // `--rm` so a container the teardown never reached (a hard abort,
            // a killed daemon connection) still goes away when it stops.
            OsString::from("--rm"),
            // Reap zombies: a target that spawns children inside the container
            // would otherwise leave them parented to a pid 1 that never waits.
            OsString::from("--init"),
            OsString::from("-v"),
            OsString::from(&mount),
        ];
        args.extend(file.run_args.iter().map(OsString::from));
        // The image's own entrypoint would run instead of the keepalive, and
        // most images have one that exits.
        //
        // `split_first` rather than an index: the driver never writes an empty
        // keepalive, but this artifact is a file on disk and a corrupt one must
        // say so rather than panic across the plugin seam, where a panic is a
        // non-unwinding abort of the whole build.
        let (entrypoint, keepalive_args) = file.keepalive.split_first().ok_or_else(|| {
            anyhow::anyhow!(
                "{}: the runner artifact has an empty `keepalive`, so nothing would hold the \
                 container open",
                req.runner_addr
            )
        })?;
        args.push(OsString::from("--entrypoint"));
        args.push(OsString::from(entrypoint));
        args.push(OsString::from(&file.image));
        args.extend(keepalive_args.iter().map(OsString::from));

        let spec = hproc::proc_exec::Spec {
            program: std::path::PathBuf::from(&file.bin),
            args,
            env: docker_cli_env(),
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
            stdin: hproc::proc_exec::StdioSpec::Null,
            stdout: hproc::proc_exec::StdioSpec::Piped,
            stderr: hproc::proc_exec::StdioSpec::Piped,
            setsid: true,
            ctty: false,
        };
        let out = hproc::proc_exec::output(spec, ctoken)
            .await
            .with_context(|| format!("start a container for {}", req.runner_addr))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!(
                "`{} run` failed for image {} ({}):\n{stderr}",
                file.bin,
                file.image,
                out.status
            );
        }
        let cid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if cid.is_empty() {
            anyhow::bail!("`{} run -d` printed no container id", file.bin);
        }

        let teardown_bin = file.bin.clone();
        let teardown_cid = cid.clone();
        let teardown: hexec_runner::TeardownJob = Box::new(move || {
            // Blocking and fire-and-forget by contract: `Drop` and the
            // hard-abort path must both be able to reach it, and the latter
            // exits without running destructors.
            let status = std::process::Command::new(&teardown_bin)
                .args(["rm", "-f", &teardown_cid])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .with_context(|| format!("`{teardown_bin} rm -f {teardown_cid}`"))?;
            if !status.success() {
                anyhow::bail!("`{teardown_bin} rm -f {teardown_cid}` exited {status}");
            }
            Ok(())
        });

        let identity = if is_digest_pinned(&file.image) {
            Identity::Pinned {
                by: file.image.clone(),
            }
        } else {
            Identity::Asserted {
                why: format!(
                    "image {} is referenced by tag, not digest — the tag can move between builds",
                    file.image
                ),
            }
        };

        Ok(Arc::new(
            WrapSession::new(
                vec![OsString::from(&file.bin), OsString::from("exec")],
                // `-e K=V`: the environment heph sets belongs to the `docker`
                // CLI on this side of the socket, and the container process
                // sees none of it.
                WrapEnv::Args(vec!["-e".to_string(), "{K}={V}".to_string()]),
                Vec::new(),
                SessionCaps {
                    // `docker exec -t` would allocate one, but the spec's stdio
                    // is already a PTY slave heph owns; asking docker for a
                    // second is how a build ends up with two line disciplines.
                    pty: false,
                    max_concurrent: None,
                    identity,
                },
                SessionDescription {
                    runner: req.runner_addr.clone(),
                    shell_functions: Vec::new(),
                    key: req.key.clone(),
                    // `chars().take` rather than a byte slice: a container id
                    // is hex today, and a summary line is not the place to
                    // learn that assumption was wrong.
                    summary: format!(
                        "oci: {} in container {}",
                        file.image,
                        cid.chars().take(12).collect::<String>()
                    ),
                },
            )?
            .with_cwd_args(vec!["-w".to_string(), "{CWD}".to_string()])
            .with_trailing_args(vec![OsString::from(&cid)])
            .with_teardown(teardown),
        ))
    }
}

#[cfg(test)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "restriction lints scoped to production code; tests are exempt"
)]
mod tests {
    use super::*;

    fn session(image: &str) -> anyhow::Result<Arc<dyn ExecSession>> {
        // The session is built from the artifact and a container id; the
        // `docker run` that produces that id is the only part needing a daemon,
        // so it is the one part these tests do not exercise.
        let file = RunnerArtifactFile {
            format_version: FORMAT_VERSION,
            image: image.to_string(),
            bin: "docker".to_string(),
            keepalive: vec!["sleep".to_string(), "infinity".to_string()],
            run_args: vec![],
        };
        let identity = if is_digest_pinned(&file.image) {
            Identity::Pinned {
                by: file.image.clone(),
            }
        } else {
            Identity::Asserted {
                why: format!("image {} is referenced by tag", file.image),
            }
        };
        Ok(Arc::new(
            WrapSession::new(
                vec![OsString::from(&file.bin), OsString::from("exec")],
                WrapEnv::Args(vec!["-e".to_string(), "{K}={V}".to_string()]),
                Vec::new(),
                SessionCaps {
                    pty: false,
                    max_concurrent: None,
                    identity,
                },
                SessionDescription {
                    runner: "//:ctr".to_string(),
                    shell_functions: Vec::new(),
                    key: "k".to_string(),
                    summary: "test".to_string(),
                },
            )?
            .with_cwd_args(vec!["-w".to_string(), "{CWD}".to_string()])
            .with_trailing_args(vec![OsString::from("cid123")]),
        ))
    }

    fn a_spec() -> hproc::proc_exec::Spec {
        hproc::proc_exec::Spec {
            program: std::path::PathBuf::from("cc"),
            args: vec![OsString::from("-c"), OsString::from("a.c")],
            env: vec![(OsString::from("OUT"), OsString::from("/sbx/out"))],
            cwd: std::path::PathBuf::from("/sbx/ws"),
            stdin: hproc::proc_exec::StdioSpec::Null,
            stdout: hproc::proc_exec::StdioSpec::Piped,
            stderr: hproc::proc_exec::StdioSpec::Piped,
            setsid: false,
            ctty: false,
        }
    }

    async fn argv_of(image: &str) -> anyhow::Result<Vec<String>> {
        let out = session(image)?
            .prepare(a_spec())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut v = vec![out.program.to_string_lossy().into_owned()];
        v.extend(out.args.iter().map(|a| a.to_string_lossy().into_owned()));
        Ok(v)
    }

    /// `docker exec [OPTIONS] CONTAINER COMMAND [ARG…]` — in that order.
    ///
    /// The grammar is the test: an option after the container id is read as
    /// part of the command, so a wrong order does not fail loudly, it runs the
    /// wrong thing inside the container.
    #[tokio::test]
    async fn the_spawn_becomes_a_docker_exec_in_the_right_order() -> anyhow::Result<()> {
        let argv = argv_of("ubuntu@sha256:abc").await?;
        assert_eq!(
            argv,
            vec![
                "docker",
                "exec", // options first
                "-w",
                "/sbx/ws",
                "-e",
                "OUT=/sbx/out",
                // then the operand
                "cid123", // then the command
                "cc",
                "-c",
                "a.c",
            ]
        );
        Ok(())
    }

    /// The target's working directory must reach the container.
    ///
    /// Without `-w` every target runs in the image's `WORKDIR` — usually `/` —
    /// and a build reading a relative path fails a long way from the cause.
    #[tokio::test]
    async fn the_targets_cwd_reaches_the_container() -> anyhow::Result<()> {
        let argv = argv_of("ubuntu@sha256:abc").await?;
        let w = argv.iter().position(|a| a == "-w").expect("-w is present");
        assert_eq!(argv[w + 1], "/sbx/ws");
        assert!(w < argv.iter().position(|a| a == "cid123").expect("cid"));
        Ok(())
    }

    /// The environment must ride argv, not the spec.
    ///
    /// `docker exec` creates the process on the far side of the daemon socket,
    /// so anything set on the `docker` CLI's own environment is invisible to it.
    #[tokio::test]
    async fn the_environment_rides_argv_because_the_daemon_is_in_the_way() -> anyhow::Result<()> {
        let argv = argv_of("ubuntu@sha256:abc").await?;
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-e" && w[1] == "OUT=/sbx/out"),
            "{argv:?}"
        );
        Ok(())
    }

    /// A digest is content the cache key covers; a tag is a claim.
    #[test]
    fn only_a_digest_pinned_image_reports_pinned() -> anyhow::Result<()> {
        assert!(session("ubuntu@sha256:abc")?.caps().identity.is_pinned());

        let tagged = session("ubuntu:24.04")?;
        assert!(!tagged.caps().identity.is_pinned());
        match &tagged.caps().identity {
            Identity::Asserted { why } => assert!(why.contains("tag"), "{why}"),
            Identity::Pinned { .. } => panic!("a tag is not pinned"),
        }
        Ok(())
    }

    #[test]
    fn an_addr_is_told_apart_from_a_literal_reference() {
        assert!(looks_like_addr("//app:load"));
        assert!(looks_like_addr(":load"));
        assert!(!looks_like_addr("ubuntu@sha256:abc"));
        assert!(!looks_like_addr("ghcr.io/x/y:1.2"));
    }
}
