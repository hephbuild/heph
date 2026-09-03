//! The `oci` exec runner: hold one container open per environment and run each
//! target in it with `docker exec`.
//!
//! # Why this is a runner and not a `session` config
//!
//! This plugin used to emit `"runner": "session"` with a `docker run` launch,
//! and let the builtin hold that container open. That works, and for devenv it
//! remains the right answer — but it costs two things a container should not
//! have to pay.
//!
//! - **The heph binary had to run inside the image.** `session` works by
//!   launching `heph __runner-agent` *in* the environment, so the container
//!   needed heph's own binary bind-mounted in and executable there. On macOS it
//!   simply is not: the host binary is Darwin and the image is Linux, which is
//!   why `example/execrunner` carries a note saying its container check only
//!   works on Linux. `docker exec` needs nothing of heph inside the image.
//! - **A `session` launch argv is static.** It is fixed when the runner target
//!   is built, so it cannot carry anything per-exec — and a target's cwd is
//!   per-exec by definition. The agent papered over this by being handed the
//!   spec through a socket. Running `docker exec -w <cwd>` says it directly.
//!
//! # What it does not fix
//!
//! Terminal fidelity. `docker exec` connects the target's stdio through the
//! daemon rather than handing it heph's PTY slave, so `--shell` into a
//! container gets docker's line discipline, not ours. That was true of the
//! `session` form too — the agent could pass descriptors, but only to a process
//! the *container's* `docker run` had already started, not to one the daemon
//! attaches later. It stays a known gap in `docs/EXEC_RUNNERS.md`.
//!
//! # Paths
//!
//! Every absolute path in the rewritten spec is a host path the driver already
//! computed — `$OUT`, `SRC_*`, tool symlinks. The workspace root and heph's home
//! are therefore bind-mounted **at the same paths inside**, and a remapped mount
//! would leave targets resolving paths that do not exist. That constraint is
//! unchanged from the `session` form and is why the mounts are not configurable.

use hexecrunner::SpecRewrite;
use hexecrunner::registry::{ExecRunner, RunnerCtx};
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::Mutex;

/// The name a `runner.json` selects this by.
pub const RUNNER_NAME: &str = "oci";

/// The runner half of this plugin's `runner.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OciConfig {
    /// The image, **by digest**: the container that runs the build must be the
    /// one the fingerprint describes even if the tag moves mid-build.
    pub image: String,
    /// Host paths bind-mounted at the same path inside. See the module note.
    #[serde(default)]
    pub mounts: Vec<String>,
    /// Extra `docker run` arguments from the runner target (`--network`, extra
    /// `-v`, `--user`).
    #[serde(default)]
    pub run_args: Vec<String>,
    /// The `docker` binary, so a target can point at a compatible CLI.
    #[serde(default = "default_docker")]
    pub docker: String,
}

fn default_docker() -> String {
    "docker".to_string()
}

/// Host environment the `docker` **client** needs to reach the daemon.
///
/// Deliberately an allowlist, and deliberately separate from the target's
/// environment: the target's goes inside via `-e`, and this is only what the
/// client process itself reads. Passing everything would put the developer's
/// ambient environment into a spawn whose cache key claims the container's.
///
/// `DOCKER_HOST`/`DOCKER_CONTEXT`/`DOCKER_CONFIG` are how a non-default daemon
/// is found (OrbStack, Colima, a remote socket); `HOME` is where the client
/// reads `~/.docker/config.json`; `PATH` is for the client's own helpers
/// (credential stores, buildx).
const CLIENT_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_CONFIG",
    "DOCKER_CERT_PATH",
    "DOCKER_TLS_VERIFY",
    "XDG_RUNTIME_DIR",
];

fn client_env() -> Vec<(OsString, OsString)> {
    CLIENT_ENV
        .iter()
        .filter_map(|k| std::env::var_os(k).map(|v| (OsString::from(k), v)))
        .collect()
}

/// Resolve the `docker` binary to an absolute path.
///
/// The rewritten spec is spawned with a cleared environment, so a bare name has
/// no `PATH` to be found on — and the sandbox `PATH` a driver would supply is
/// the *target's*, which has nothing to do with where this host keeps docker.
/// Resolved here, in the process that actually knows.
fn resolve_docker(bin: &str) -> anyhow::Result<std::path::PathBuf> {
    if bin.contains('/') {
        return Ok(std::path::PathBuf::from(bin));
    }
    let path = std::env::var_os("PATH")
        .ok_or_else(|| anyhow::anyhow!("oci runner: PATH is not set, cannot locate {bin:?}"))?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow::anyhow!("oci runner: {bin:?} not found on this host's PATH"))
}

/// A container held open for one `(runner address, fingerprint)`.
struct Live {
    id: String,
    docker: String,
    /// The image's own `PATH`, read once from the container's config.
    ///
    /// A target's `PATH` reaches the container as an explicit `-e PATH=…`,
    /// which *replaces* the image's rather than extending it. Without the
    /// image's own value behind it, entering a container would strip `/usr/bin`
    /// from every target that declares a tool.
    path: Option<OsString>,
}

pub struct OciRunner {
    /// Keyed on `(addr, fingerprint)` — the same key the builtin session pool
    /// uses, and for the same reason: a rebuilt image moves the fingerprint, so
    /// the next exec gets a new container rather than the stale one.
    live: Mutex<HashMap<(String, String), Live>>,
}

impl Default for OciRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl OciRunner {
    #[must_use]
    pub fn new() -> Self {
        Self {
            live: Mutex::new(HashMap::new()),
        }
    }

    /// The container for this environment, starting one if needed.
    ///
    /// Started detached with a command that does nothing and never exits: the
    /// container is a *place to exec into*, so its own entrypoint is irrelevant
    /// and must not be whatever the image happens to declare — an image whose
    /// entrypoint exits immediately would otherwise leave nothing to exec into.
    fn container(
        &self,
        ctx: &RunnerCtx<'_>,
        cfg: &OciConfig,
    ) -> anyhow::Result<(String, Option<OsString>)> {
        let key = (ctx.addr.to_string(), ctx.fingerprint.to_string());
        let mut live = self
            .live
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("oci runner container table poisoned"))?;
        if let Some(c) = live.get(&key) {
            return Ok((c.id.clone(), c.path.clone()));
        }

        let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--rm".into()];
        for m in &cfg.mounts {
            args.push("-v".into());
            args.push(format!("{m}:{m}"));
        }
        args.extend(cfg.run_args.iter().cloned());
        args.push(cfg.image.clone());
        // `sh -c 'while :; do sleep 3600; done'` rather than `sleep infinity`:
        // busybox and coreutils disagree about `infinity`, and a `FROM scratch`
        // image has neither — but an image with no shell cannot host a build
        // anyway, so a shell is the floor this runner already requires.
        args.push("sh".into());
        args.push("-c".into());
        args.push("while :; do sleep 3600; done".into());

        let out = std::process::Command::new(&cfg.docker)
            .args(&args)
            .output()
            .map_err(|e| anyhow::anyhow!("oci runner {}: run `{}`: {e}", ctx.addr, cfg.docker))?;
        if !out.status.success() {
            anyhow::bail!(
                "oci runner {}: `{} run` failed for image {}: {}",
                ctx.addr,
                cfg.docker,
                cfg.image,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if id.is_empty() {
            anyhow::bail!(
                "oci runner {}: `{} run -d` printed no container id",
                ctx.addr,
                cfg.docker
            );
        }
        let path = image_path(&cfg.docker, &id);
        live.insert(
            key,
            Live {
                id: id.clone(),
                docker: cfg.docker.clone(),
                path: path.clone(),
            },
        );
        Ok((id, path))
    }
}

/// The `docker exec` argv for one target.
///
/// Separate from [`OciRunner::prepare`] so the argv can be asserted without a
/// daemon: everything above it needs a running container, and this is the part
/// that decides what the target actually sees.
fn exec_args(id: &str, image_path: Option<&OsStr>, rewrite: SpecRewrite) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["exec".into()];
    args.push("-w".into());
    args.push(rewrite.cwd.into_os_string());
    for (k, v) in &rewrite.env {
        args.push("-e".into());
        // `-e KEY=VALUE` as one argument, so a value containing `=` or
        // whitespace survives — docker splits on the first `=` only.
        let mut kv = k.clone();
        kv.push("=");
        if k == "PATH" {
            // `-e PATH` replaces the image's rather than extending it, so the
            // image's own value has to be restored *behind* what the target
            // carries: its declared tools and heph's builtins lead, the image's
            // directories follow. Without this, entering a container would
            // strip `/usr/bin` from every target that declares a tool.
            kv.push(join_path_values(v, image_path));
        } else {
            kv.push(v);
        }
        args.push(kv);
    }
    args.push(OsString::from(id));
    args.push(rewrite.program.into_os_string());
    args.extend(rewrite.args);
    args
}

/// `carried`, then whatever of `image` is not already in it.
///
/// Deduplicated because the two genuinely overlap — a target whose tools live
/// under a mounted host path and an image that ships the same directory — and a
/// `PATH` that grows a duplicate per exec is a real cost on every `execvp` the
/// target makes.
fn join_path_values(carried: &OsStr, image: Option<&OsStr>) -> OsString {
    let Some(image) = image else {
        return carried.to_os_string();
    };
    let mut seen: Vec<PathBuf> = std::env::split_paths(carried).collect();
    let mut out = carried.to_os_string();
    for entry in std::env::split_paths(image) {
        if entry.as_os_str().is_empty() || seen.contains(&entry) {
            continue;
        }
        if !out.is_empty() {
            out.push(":");
        }
        out.push(&entry);
        seen.push(entry);
    }
    out
}

/// The image's `PATH`, read from the running container's config.
///
/// Asked of the daemon rather than of the image, because `docker run` arguments
/// (`--env`) can override it. Best-effort: an unreadable value costs the image's
/// own directories on the target's `PATH`, which is a degraded environment but
/// not a wrong one, and failing the build over it would be worse.
fn image_path(docker: &str, id: &str) -> Option<OsString> {
    let out = std::process::Command::new(docker)
        .args([
            "inspect",
            "--format",
            "{{range .Config.Env}}{{println .}}{{end}}",
            id,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        tracing::debug!(
            container = id,
            "could not inspect the container for its PATH; the image's own \
             directories will not be on the target's PATH"
        );
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("PATH="))
        .filter(|v| !v.is_empty())
        .map(OsString::from)
}

#[async_trait::async_trait]
impl ExecRunner for OciRunner {
    fn name(&self) -> &str {
        RUNNER_NAME
    }

    /// The container's environment is not visible from this process, so the
    /// driver's fallback `PATH` must not be reinstated over it.
    fn supplies_environment(&self) -> bool {
        true
    }

    async fn prepare(
        &self,
        ctx: &RunnerCtx<'_>,
        rewrite: SpecRewrite,
    ) -> anyhow::Result<SpecRewrite> {
        let cfg: OciConfig = serde_json::from_value(ctx.config.clone())
            .map_err(|e| anyhow::anyhow!("oci runner {}: parse config: {e}", ctx.addr))?;
        let (id, image_path) = self.container(ctx, &cfg)?;

        // `docker exec` rather than a fresh `docker run`: the cwd and the
        // environment are per-exec, and a container that is already up costs
        // nothing to enter.
        let args = exec_args(&id, image_path.as_deref(), rewrite);

        Ok(SpecRewrite {
            program: resolve_docker(&cfg.docker)?,
            args,
            // The client's own environment, not the target's — the target's is
            // already inside the `-e` arguments above. Leaving the target's here
            // would set its variables on the docker client, where they do
            // nothing, and drop the ones the client needs to find the daemon.
            env: client_env(),
            // The client runs here; only the target's cwd is inside.
            cwd: std::env::current_dir().unwrap_or_else(|_e| std::path::PathBuf::from("/")),
        })
    }

    fn shutdown(&self) {
        let Ok(mut live) = self.live.lock() else {
            return;
        };
        for (_key, c) in live.drain() {
            // Best-effort: teardown has nowhere to report to, and `--rm` means a
            // container we fail to remove still goes when the daemon reaps it.
            drop(
                std::process::Command::new(&c.docker)
                    .args(["rm", "-f", &c.id])
                    .output(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_config(json: serde_json::Value) -> OciConfig {
        serde_json::from_value(json).expect("parse")
    }

    /// The end of the whole chain: what the target inside the container is
    /// actually handed. Its declared tools and heph's builtins lead, and the
    /// image's own directories are still there behind them.
    #[test]
    fn the_exec_argv_carries_the_targets_path_ahead_of_the_images() {
        let args = exec_args(
            "deadbeef",
            Some(OsStr::new("/usr/bin:/bin")),
            SpecRewrite {
                program: PathBuf::from("bash"),
                args: vec![OsString::from("-c"), OsString::from("make")],
                env: vec![(
                    OsString::from("PATH"),
                    OsString::from("/sandbox/bin:/heph/coreutils/bin"),
                )],
                cwd: PathBuf::from("/ws/pkg"),
            },
        );
        let path = args
            .iter()
            .find_map(|a| a.to_str()?.strip_prefix("PATH="))
            .expect("the target's PATH must reach the container");
        assert_eq!(path, "/sandbox/bin:/heph/coreutils/bin:/usr/bin:/bin");
        // The container id must still separate the docker flags from the
        // target's own command.
        let id_at = args.iter().position(|a| a == "deadbeef").expect("id");
        assert_eq!(args[id_at + 1], OsString::from("bash"));
    }

    /// `-e PATH` replaces the image's, so the image's own directories have to be
    /// restored behind what the target carries — otherwise entering a container
    /// strips `/usr/bin` from every target that declares a tool.
    #[test]
    fn the_image_path_follows_what_the_target_carries() {
        let joined = join_path_values(
            OsStr::new("/sandbox/bin:/heph/coreutils/bin"),
            Some(OsStr::new("/usr/local/bin:/usr/bin:/bin")),
        );
        assert_eq!(
            joined,
            OsString::from("/sandbox/bin:/heph/coreutils/bin:/usr/local/bin:/usr/bin:/bin")
        );
    }

    /// The two overlap in practice — the workspace is mounted at the same path
    /// inside, so a tool directory can appear in both — and a duplicate costs a
    /// wasted `stat` on every `execvp` the target makes.
    #[test]
    fn a_directory_in_both_is_not_repeated() {
        let joined = join_path_values(
            OsStr::new("/sandbox/bin:/usr/bin"),
            Some(OsStr::new("/usr/bin:/bin")),
        );
        assert_eq!(joined, OsString::from("/sandbox/bin:/usr/bin:/bin"));
    }

    /// An image whose `PATH` could not be read leaves the target with exactly
    /// what it carried, rather than failing the build.
    #[test]
    fn an_unreadable_image_path_leaves_the_carried_one_alone() {
        let joined = join_path_values(OsStr::new("/sandbox/bin"), None);
        assert_eq!(joined, OsString::from("/sandbox/bin"));
    }

    #[test]
    fn a_config_needs_only_an_image() {
        let cfg = ctx_config(serde_json::json!({"image": "x@sha256:abc"}));
        assert_eq!(cfg.image, "x@sha256:abc");
        assert_eq!(cfg.docker, "docker");
        assert!(cfg.mounts.is_empty());
    }

    /// A typo in a runner config must fail at parse, not silently do nothing —
    /// the same reason every other config in this tree denies unknown fields.
    #[test]
    fn an_unknown_field_is_refused() {
        let err = serde_json::from_value::<OciConfig>(
            serde_json::json!({"image": "x", "runargs": ["--net"]}),
        )
        .expect_err("must reject");
        assert!(format!("{err}").contains("runargs"), "{err}");
    }
}
