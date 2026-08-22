//! The `devenv` driver (builds the environment artifact) and the `devenv` exec
//! runner (reads it back). One name, two halves — see `docs/EXEC_RUNNERS.md` §5.

pub mod snapshot;

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hexec_runner::{
    EnvSession, ExecRunner, ExecSession, Identity, OpenRequest, SessionCaps, SessionDescription,
};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as TPath};
use hplugin::driver::targetdef::{CacheConfig, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse,
};
use hplugin::htspec::Spec;
use snapshot::{LocalPaths, Snapshot, Variable};
use std::collections::BTreeMap;
use std::hash::{Hash as _, Hasher as _};
use std::sync::Arc;

pub const NAME: &str = "devenv";

/// The snapshot file a `devenv` target produces.
const OUT_NAME: &str = "devenv-env.json";

/// Config for a `devenv` target.
#[derive(Spec)]
struct DevenvSpec {
    /// The `devenv` binary. Defaults to `devenv` on the driver's PATH.
    bin: Option<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct DevenvDef {
    bin: String,
}

pub struct Driver {
    tree_root: std::path::PathBuf,
}

impl Driver {
    pub fn new(tree_root: std::path::PathBuf) -> Self {
        Self { tree_root }
    }
}

#[async_trait]
impl ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: NAME.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        DevenvSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let spec = DevenvSpec::from(&req.target_spec.config).with_context(|| "devenv spec")?;
        let def = DevenvDef {
            bin: spec.bin.unwrap_or_else(|| "devenv".to_string()),
        };

        let mut h = xxhash_rust::xxh3::Xxh3::new();
        snapshot::SNAPSHOT_FORMAT_VERSION.hash(&mut h);
        def.bin.hash(&mut h);
        req.target_spec.addr.format().hash(&mut h);

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: vec![],
                raw_def: Arc::new(def),
                inputs: vec![],
                outputs: vec![Output {
                    group: String::new(),
                    // Declared output paths are workspace-relative, i.e.
                    // package-prefixed — the same normalization `pluginexec`
                    // applies to `out =`. A bare name would be looked for at the
                    // workspace root and never found.
                    paths: vec![TPath {
                        content: Content::FilePath(hmodel::htpkg::join_rel_checked(
                            req.target_spec.addr.package.as_str(),
                            OUT_NAME,
                        )?),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }],
                support_files: vec![],
                // Local cache only. The snapshot's `PATH` is a list of
                // host-local `/nix/store` paths, and `plugin-nix` already
                // refuses to share the same kind of artifact for the same
                // reason ("wrappers point at host-local /nix/store; remote cache
                // must stay disabled"). A machine that pulled this from a shared
                // cache without those store paths would get an environment of
                // directories that do not exist.
                cache: CacheConfig::on(false),
                pty: false,
                hash: h.finish().to_le_bytes().to_vec(),
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
        let def = req.request.target.def_de::<DevenvDef>().clone();
        let out_path = req.sandbox_pkg_dir.join(OUT_NAME);

        // `devenv` must run against the real tree, not the sandbox: the
        // environment it describes is the workspace's, and `devenv.nix` lives
        // there. This is the one read outside the sandbox in the whole design,
        // and it is why the *inputs* (devenv.nix/yaml/lock) must be declared on
        // the target — they, not this directory, are what the cache key sees.
        let spec = hproc::proc_exec::Spec {
            program: std::path::PathBuf::from(&def.bin),
            args: vec!["print-dev-env".into(), "--json".into()],
            env: passthrough_env(),
            cwd: self.tree_root.clone(),
            stdin: hproc::proc_exec::StdioSpec::Null,
            stdout: hproc::proc_exec::StdioSpec::Piped,
            stderr: hproc::proc_exec::StdioSpec::Piped,
            setsid: true,
            ctty: false,
        };

        // `output`, not `spawn`: the JSON is collected in full after the wait,
        // which needs the unbounded drain (a dev shell's dump is well past the
        // streaming bound).
        let out = req
            .runner
            .output(spec, ctoken)
            .await
            .with_context(|| format!("running `{} print-dev-env --json`", def.bin))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!(
                "`{} print-dev-env --json` failed ({}):\n{stderr}",
                def.bin,
                out.status
            );
        }

        let snap = snapshot_from_json(&out.stdout, &self.local_paths())?;
        if snap.env.is_empty() {
            anyhow::bail!(
                "the devenv environment came out empty after filtering, which would describe \
                 nothing and make every target using it share a cache key with targets using a \
                 different runner"
            );
        }

        let json = serde_json::to_vec_pretty(&snap).context("serialize devenv snapshot")?;
        std::fs::write(&out_path, json).with_context(|| format!("write {}", out_path.display()))?;

        if !snap.dropped_path_entries.is_empty() {
            tracing::info!(
                dropped = snap.dropped_path_entries.len(),
                "devenv: dropped non-/nix/store PATH entries; targets needing those tools must \
                 declare them as `tools =` deps"
            );
        }

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

impl Driver {
    fn local_paths(&self) -> LocalPaths {
        LocalPaths {
            tree_root: self.tree_root.to_string_lossy().into_owned(),
            home: std::env::var("HOME").unwrap_or_default(),
            tmpdir: std::env::var("TMPDIR").unwrap_or_default(),
        }
    }
}

/// What `devenv` itself needs to run: it shells out to nix, which needs the
/// user's store, channels and TLS trust. Mirrors `plugin-nix`'s passthrough for
/// the same reason — the spawn is `env_clear`ed, so anything nix needs must be
/// named.
fn passthrough_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    [
        "HOME",
        "USER",
        "PATH",
        "NIX_PATH",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "NIX_SSL_CERT_FILE",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "CURL_CA_BUNDLE",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "NO_PROXY",
        "https_proxy",
        "http_proxy",
        "no_proxy",
    ]
    .iter()
    .filter_map(|n| {
        std::env::var(n)
            .ok()
            .map(|v| (std::ffi::OsString::from(n), std::ffi::OsString::from(v)))
    })
    .collect()
}

#[derive(serde::Deserialize)]
struct PrintDevEnv {
    #[serde(default)]
    variables: BTreeMap<String, Variable>,
    #[serde(default)]
    bash_functions: BTreeMap<String, serde_json::Value>,
}

fn snapshot_from_json(stdout: &[u8], local: &LocalPaths) -> anyhow::Result<Snapshot> {
    // `bashFunctions` in the wire format; serde's rename is applied here rather
    // than in the struct so the field name reads as Rust.
    let raw: serde_json::Value = serde_json::from_slice(stdout).with_context(|| {
        // Show what actually came back. "expected value at line 1 column 1"
        // alone cannot distinguish "empty output" from "devenv printed
        // progress on stdout", and those have different fixes.
        let head: String = String::from_utf8_lossy(stdout).chars().take(200).collect();
        if stdout.is_empty() {
            "`devenv print-dev-env --json` produced no output on stdout".to_string()
        } else {
            format!("parse `devenv print-dev-env --json` output; first bytes: {head:?}")
        }
    })?;
    let parsed: PrintDevEnv = serde_json::from_value(serde_json::json!({
        "variables": raw.get("variables").cloned().unwrap_or_default(),
        "bash_functions": raw.get("bashFunctions").cloned().unwrap_or_default(),
    }))
    .context("decode devenv env")?;

    Ok(snapshot::build(
        &parsed.variables,
        parsed.bash_functions.keys().cloned().collect(),
        local,
    ))
}

/// The runner half: a pure parse of the artifact the driver produced.
///
/// It reads nothing else. `open` runs after `hashin` is computed and does not
/// run at all on a fully-cached build, so anything discovered here would be
/// unhashed input that cannot be validated on the build where a stale artifact
/// is served (`docs/EXEC_RUNNERS.md` §4.7).
pub struct Runner;

#[async_trait]
impl ExecRunner for Runner {
    async fn open(
        &self,
        req: OpenRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<Arc<dyn ExecSession>> {
        let artifact = req
            .artifacts
            .iter()
            .find(|a| a.path.ends_with(OUT_NAME))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "runner {} produced no {OUT_NAME}: a `devenv` runner target must be built by \
                     the `devenv` driver",
                    req.runner_addr,
                )
            })?;

        let snap: Snapshot = serde_json::from_slice(&artifact.bytes)
            .with_context(|| format!("parse {OUT_NAME} from {}", req.runner_addr))?;

        if snap.format_version != snapshot::SNAPSHOT_FORMAT_VERSION {
            anyhow::bail!(
                "{} was built by a different version of the devenv driver (snapshot v{}, this \
                 heph understands v{}) — rebuild it",
                req.runner_addr,
                snap.format_version,
                snapshot::SNAPSHOT_FORMAT_VERSION,
            );
        }

        let base_env = snap
            .env
            .iter()
            .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v)))
            .collect();

        Ok(Arc::new(EnvSession::new(
            base_env,
            SessionCaps {
                pty: true,
                max_concurrent: None,
                // Pinned: the snapshot's PATH is store-only, so its bytes
                // describe the exact toolchain rather than asserting one.
                identity: Identity::Pinned {
                    by: format!("{} ({})", req.runner_addr, req.key),
                },
            },
            SessionDescription {
                runner: req.runner_addr.clone(),
                shell_functions: snap.shell_functions.clone(),
                key: req.key,
                summary: format!(
                    "devenv: {} vars, {} shell functions not available",
                    snap.env.len(),
                    snap.shell_functions.len()
                ),
            },
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devenvs_real_json_shape() {
        let json = br#"{
          "bashFunctions": {"fmt-all": "x", "lint": "y"},
          "variables": {
            "PATH": {"type": "exported", "value": "/nix/store/a/bin:/usr/bin"},
            "CC": {"type": "exported", "value": "clang"},
            "envHooks": {"type": "array", "value": ["a", "b"]},
            "DEVENV_ROOT": {"type": "exported", "value": "/repo"}
          }
        }"#;
        let snap = snapshot_from_json(
            json,
            &LocalPaths {
                tree_root: "/repo".to_string(),
                home: "/home/u".to_string(),
                tmpdir: String::new(),
            },
        )
        .expect("parse");

        assert_eq!(
            snap.env.get("PATH").map(String::as_str),
            Some("/nix/store/a/bin")
        );
        assert_eq!(snap.env.get("CC").map(String::as_str), Some("clang"));
        assert!(
            !snap.env.contains_key("envHooks"),
            "arrays are not environment"
        );
        assert!(!snap.env.contains_key("DEVENV_ROOT"));
        assert_eq!(snap.shell_functions, vec!["fmt-all", "lint"]);
        assert_eq!(snap.dropped_path_entries, vec!["/usr/bin"]);
    }
}
