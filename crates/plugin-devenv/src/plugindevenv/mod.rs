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

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

/// A `HashMap` iterates in an arbitrary order; the def hash must not.
fn sorted_pairs(m: std::collections::HashMap<String, String>) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = m.into_iter().collect();
    v.sort();
    v
}

/// The snapshot file a `devenv` target produces.
const OUT_NAME: &str = "devenv-env.json";

/// Config for a `devenv` target.
#[derive(Spec)]
struct DevenvSpec {
    /// The `devenv` binary. Defaults to `devenv` on the driver's PATH.
    bin: Option<String>,
    /// Variables this runner adds on top of what devenv reported, as literals.
    /// Captured with the environment, so changing one re-keys every target
    /// built in it.
    env: std::collections::HashMap<String, String>,
    /// Host variables whose **values** are captured with the environment. Use
    /// this for something that should re-key when it changes.
    pass_env: Vec<String>,
    /// Literal variables applied at spawn instead of being captured.
    runtime_env: std::collections::HashMap<String, String>,
    /// Host variables read at spawn. Only the *name* is ever hashed, so use
    /// this for anything that legitimately differs per machine or per login —
    /// `SSH_AUTH_SOCK`, `DOCKER_HOST`, a personal token. `"*"` passes the whole
    /// host environment.
    runtime_pass_env: Vec<String>,
    /// `"snapshot"` (default), `"session"` or `"wrap"`.
    ///
    /// `snapshot` captures the environment once and applies it to every target
    /// — no live process, cacheable, and the environment's own bytes are its
    /// cache identity. `session` additionally holds a `devenv shell` open and
    /// forks every target's process from inside it, which is what makes
    /// `enterShell` side effects and shell functions available. It costs one
    /// shell per heph process, forever, and cannot amortize across machines.
    ///
    /// `wrap` prefixes every spawn with `devenv shell --`. It is a
    /// **demonstration** of the generic `Wrap` lane, not a recommendation:
    /// measured at 4.5 s per spawn on this repo's own shell, it re-runs
    /// `enterShell` once per target rather than once per build, and it does
    /// **not** make shell functions callable — `devenv shell -- prog` execs
    /// `prog` directly, with no functions defined. Use it only for a runner a
    /// handful of targets name.
    mode: Option<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct DevenvDef {
    bin: String,
    mode: snapshot::Mode,
    declared: hexec_runner::SessionEnv,
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
        let mode = match spec.mode.as_deref() {
            None | Some("snapshot") => snapshot::Mode::Snapshot,
            Some("session") => snapshot::Mode::Session,
            Some("wrap") => snapshot::Mode::Wrap,
            Some(other) => anyhow::bail!(
                "devenv `mode` must be \"snapshot\", \"session\" or \"wrap\", got {other:?}"
            ),
        };
        // Sorted on the way in so the def hash does not depend on map order —
        // the same reason `NixDef` sorts its package list.
        let declared = hexec_runner::SessionEnv {
            env: sorted_pairs(spec.env),
            pass_env: sorted(spec.pass_env),
            runtime_env: sorted_pairs(spec.runtime_env),
            runtime_pass_env: sorted(spec.runtime_pass_env),
        };
        let def = DevenvDef {
            bin: spec.bin.unwrap_or_else(|| "devenv".to_string()),
            mode,
            declared,
        };

        let mut h = xxhash_rust::xxh3::Xxh3::new();
        snapshot::SNAPSHOT_FORMAT_VERSION.hash(&mut h);
        def.bin.hash(&mut h);
        // The mode changes what the artifact contains (a session snapshot also
        // carries the shell prelude) and how it is consumed, so it must change
        // the key.
        def.mode.hash(&mut h);
        // `env` and `pass_env` are folded into the captured environment, so they
        // must move the key. `runtime_*` are hashed as *declarations* too — the
        // artifact is the environment's identity, and heph will not claim two
        // differently-declared environments are the same one. What stays out of
        // the key is the ambient *value* a `runtime_pass_env` name pulls in,
        // which is read at spawn and never written down.
        def.declared.env.hash(&mut h);
        def.declared.pass_env.hash(&mut h);
        def.declared.runtime_env.hash(&mut h);
        def.declared.runtime_pass_env.hash(&mut h);
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

        let snap = snapshot_from_json(
            &out.stdout,
            &self.local_paths(),
            def.mode,
            def.bin.clone(),
            def.declared.clone(),
        )?;
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

fn snapshot_from_json(
    stdout: &[u8],
    local: &LocalPaths,
    mode: snapshot::Mode,
    bin: String,
    declared: hexec_runner::SessionEnv,
) -> anyhow::Result<Snapshot> {
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

    // Only in session mode: the definitions are large, and a snapshot runner
    // cannot use them — carrying them anyway would bloat every consumer's cache
    // key for something nothing reads.
    let prelude = if mode == snapshot::Mode::Session {
        render_prelude(&parsed.bash_functions)
    } else {
        String::new()
    };

    Ok(snapshot::build_with_prelude(
        &parsed.variables,
        parsed.bash_functions.keys().cloned().collect(),
        local,
        mode,
        bin,
        prelude,
        declared,
    ))
}

/// Render devenv's function bodies as a bash snippet the agent sources before
/// each command.
///
/// `print-dev-env` gives each body without the `name () {…}` wrapper, so it is
/// rebuilt here — and every function is then **exported**.
///
/// The export is the part that makes this work at all. The agent sources this
/// into one bash, but the command it runs is itself `bash -c …` (that is how
/// `pluginexec` invokes a target), and a function defined in a parent shell is
/// not visible to a child shell unless exported. Without `export -f`, the
/// prelude is defined in a process the target never runs in — which is exactly
/// how this failed the first time.
fn render_prelude(functions: &BTreeMap<String, serde_json::Value>) -> String {
    let mut out = String::new();
    for (name, body) in functions {
        if !is_exportable_function_name(name) {
            continue;
        }
        if let Some(b) = body.as_str() {
            out.push_str(name);
            out.push_str(" () {");
            out.push_str(b);
            out.push_str("}\n");
            out.push_str("export -f ");
            out.push_str(name);
            out.push('\n');
        }
    }
    out
}

/// A name bash can both declare and `export -f`.
///
/// Stricter than "a name devenv used": bash accepts some punctuation in a
/// function name but cannot export it, and a failed `export -f` in the prelude
/// takes down every command in the session — so anything not a plain identifier
/// is skipped rather than risked.
fn is_exportable_function_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The runner half: a pure parse of the artifact the driver produced.
///
/// It reads nothing else. `open` runs after `hashin` is computed and does not
/// run at all on a fully-cached build, so anything discovered here would be
/// unhashed input that cannot be validated on the build where a stale artifact
/// is served (`docs/EXEC_RUNNERS.md` §4.7).
///
/// In `session` mode it additionally *starts* something — a `devenv shell`
/// holding an agent — but strictly from the artifact's contents. What it starts
/// is decided by bytes the cache key already covers; only the socket path and
/// the pid are new, and neither describes the environment.
pub struct Runner {
    /// Where agent sockets live, and the heph binary that serves as both agent
    /// and client. Absent in a host with no session support, which makes
    /// `mode = "session"` an error rather than a silent downgrade.
    session: Option<SessionSupport>,
}

#[derive(Clone, Debug)]
pub struct SessionSupport {
    pub heph_bin: std::path::PathBuf,
    pub socket_dir: std::path::PathBuf,
    /// Where `devenv.nix` lives. `devenv shell` must run there — it was the
    /// socket directory once, which has no `devenv.nix`, and the only symptom
    /// was a five-minute wait for a socket that was never going to appear.
    pub tree_root: std::path::PathBuf,
}

impl Runner {
    pub fn new(session: Option<SessionSupport>) -> Self {
        Self { session }
    }
}

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

        let base_env: Vec<(std::ffi::OsString, std::ffi::OsString)> = snap
            .env
            .iter()
            .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v)))
            .collect();

        match snap.mode {
            snapshot::Mode::Session => return self.open_session(&req, &snap, base_env).await,
            snapshot::Mode::Wrap => return Self::open_wrap(&req, &snap, base_env),
            snapshot::Mode::Snapshot => {}
        }

        Ok(Arc::new(EnvSession::with_declared(
            base_env,
            snap.declared.clone(),
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

impl Runner {
    /// `mode = "wrap"`: prefix every spawn with `devenv shell --`.
    ///
    /// This is the tree's demonstration that the generic [`WrapSession`] lane
    /// works — it is the shape a `docker exec` or `chroot` runner has — and it
    /// is deliberately *not* the recommended way to use devenv. Three measured
    /// reasons, in the order they will bite:
    ///
    /// 1. **Cost.** `devenv shell -- true` is ~4.5 s warm on this repo. Snapshot
    ///    mode pays `devenv print-dev-env --json` **once, as a cached target**;
    ///    this pays a full shell entry per spawn. A Go build is thousands of
    ///    processes.
    /// 2. **`enterShell` runs per target**, not per build. A shell that starts a
    ///    service or writes to the tree does it once per spawn, which is exactly
    ///    the side-effect-freedom the target model asks for.
    /// 3. **It does not buy shell functions.** `devenv shell -- prog` execs
    ///    `prog` directly with no functions defined (`declare -F` reports none),
    ///    so the one thing `session` mode exists for is still missing here.
    ///
    /// Identity is therefore `Asserted`, not `Pinned`: the snapshot's bytes no
    /// longer describe what the target runs in — the live shell does, per spawn,
    /// outside any key.
    ///
    /// [`WrapEnv::Inherit`], because `devenv shell` execs the inner program and
    /// the spec's environment carries through — unlike `docker exec`, which is
    /// what the `Args` half of that enum is for.
    fn open_wrap(
        req: &OpenRequest,
        snap: &Snapshot,
        base_env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    ) -> anyhow::Result<Arc<dyn ExecSession>> {
        // `snap.bin` rather than a bare "devenv": the driver captured this
        // snapshot with a specific binary and the wrapper must be the same one.
        // It is in the def hash, so the key covers it.
        let bin = if snap.bin.is_empty() {
            "devenv"
        } else {
            snap.bin.as_str()
        };
        let prefix = vec![
            std::ffi::OsString::from(bin),
            std::ffi::OsString::from("shell"),
            std::ffi::OsString::from("--"),
        ];

        Ok(Arc::new(hexec_runner::WrapSession::new(
            prefix,
            hexec_runner::WrapEnv::Inherit,
            base_env,
            SessionCaps {
                pty: true,
                max_concurrent: None,
                identity: Identity::Asserted {
                    why: "a `devenv shell` entered per spawn; what it produces is outside the \
                          cache key, and `enterShell` runs once per target"
                        .to_string(),
                },
            },
            SessionDescription {
                runner: req.runner_addr.clone(),
                // Named, but still not callable: `devenv shell -- prog` execs
                // `prog` rather than sourcing anything, so the diagnostic that
                // says "that is a shell function" stays useful here.
                shell_functions: snap.shell_functions.clone(),
                key: req.key.clone(),
                summary: format!(
                    "devenv wrap: `{bin} shell --` per spawn, {} vars",
                    snap.env.len()
                ),
            },
        )?))
    }

    /// `mode = "session"`: hold a `devenv shell` open with an agent inside it,
    /// and fork every target's process from there.
    async fn open_session(
        &self,
        req: &OpenRequest,
        snap: &Snapshot,
        base_env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    ) -> anyhow::Result<Arc<dyn ExecSession>> {
        let support = self.session.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "{} asks for `mode = \"session\"`, but this heph was built without session \
                 support",
                req.runner_addr,
            )
        })?;

        let socket = hexec_runner::agent::socket_path(&support.socket_dir, &req.key);
        std::fs::create_dir_all(&support.socket_dir)?;

        // The prelude goes to a file rather than the command line: a dev shell's
        // function bodies run to tens of kilobytes, well past what argv can
        // carry, and `execve` would fail with E2BIG rather than anything that
        // explains itself.
        let prelude_path = socket.with_extension("prelude.sh");
        std::fs::write(&prelude_path, &snap.shell_prelude)?;

        // The agent runs INSIDE the shell — that is the whole difference from
        // snapshot mode. `devenv shell -- <cmd>` is the supported way in.
        // The binary the driver captured with, not whatever `devenv` a PATH
        // resolves to — a configured `bin` was previously ignored here.
        let mut cmd = std::process::Command::new(if snap.bin.is_empty() {
            "devenv"
        } else {
            snap.bin.as_str()
        });
        cmd.arg("shell")
            .arg("--")
            .arg(&support.heph_bin)
            .arg("__runner-agent")
            .arg("--socket")
            .arg(&socket)
            .arg("--prelude")
            .arg(&prelude_path)
            .current_dir(&support.tree_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());

        // Its stderr goes to a file, not to /dev/null. A session that fails to
        // start is otherwise a five-minute timeout with no reason attached, and
        // the reason is always in what `devenv` said on the way down.
        let log_path = socket.with_extension("log");
        match std::fs::File::create(&log_path) {
            Ok(f) => {
                cmd.stderr(std::process::Stdio::from(f));
            }
            Err(_) => {
                cmd.stderr(std::process::Stdio::null());
            }
        }
        let mut child = cmd
            .spawn()
            .with_context(|| "starting `devenv shell` for a session runner")?;
        let pid = child.id();

        // Wait for the socket rather than assume it: a cold `devenv shell` is
        // tens of seconds, and connecting too early would fail every target of
        // the build for a reason that reads like the environment is broken.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
        while !socket.exists() {
            if std::time::Instant::now() > deadline {
                let tail = std::fs::read_to_string(&log_path)
                    .map(|s| s.lines().rev().take(20).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_default();
                anyhow::bail!(
                    "`devenv shell` did not open its exec agent at {} within 5 minutes.\n\
                     Its last output:\n{tail}",
                    socket.display(),
                );
            }
            // A shell that has already exited is never going to open it, and
            // waiting out the deadline only delays the same failure.
            if let Ok(Some(status)) = child.try_wait() {
                let tail = std::fs::read_to_string(&log_path)
                    .map(|s| s.lines().rev().take(20).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_default();
                anyhow::bail!(
                    "`devenv shell` exited ({status}) before opening its exec agent.\n\
                     Its last output:\n{tail}"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let teardown_socket = socket.clone();
        let teardown: hexec_runner::TeardownJob = Box::new(move || {
            // Killing the shell closes the agent with it. Sync and
            // fire-and-forget by contract — `Drop` and process exit must both
            // be able to reach it.
            //
            // SAFETY: `kill` with a pid this process spawned; the worst case is
            // ESRCH if it is already gone, which is not an error here.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
            // Best-effort: the agent removes it on a clean exit, so it is
            // usually already gone.
            drop(std::fs::remove_file(&teardown_socket));
            Ok(())
        });

        Ok(Arc::new(hexec_runner::AgentSession::new(
            support.heph_bin.clone(),
            socket,
            base_env,
            SessionCaps {
                pty: true,
                max_concurrent: None,
                // Asserted, not Pinned: the environment is now whatever that
                // live shell has. The lockfiles pin what it was *asked* to be,
                // which is a claim rather than an observation — and
                // `enterShell` side effects and running services are outside
                // any key by construction.
                identity: Identity::Asserted {
                    why: "a live `devenv shell`; enterShell effects and services are outside the \
                          cache key"
                        .to_string(),
                },
            },
            SessionDescription {
                runner: req.runner_addr.clone(),
                shell_functions: snap.shell_functions.clone(),
                key: req.key.clone(),
                summary: format!(
                    "devenv session: {} vars, {} shell functions",
                    snap.env.len(),
                    snap.shell_functions.len()
                ),
            },
            Some(teardown),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap_for(mode: snapshot::Mode) -> Snapshot {
        let json = br#"{"bashFunctions":{"fmt":"  echo hi\n"},"variables":{"CC":{"type":"exported","value":"clang"},"PATH":{"type":"exported","value":"/nix/store/a/bin"}}}"#;
        snapshot_from_json(
            json,
            &LocalPaths {
                tree_root: "/r".to_string(),
                home: "/h".to_string(),
                tmpdir: String::new(),
            },
            mode,
            "/nix/store/d/bin/devenv".into(),
            Default::default(),
        )
        .expect("snapshot")
    }

    fn open_req() -> OpenRequest {
        OpenRequest {
            key: "k".to_string(),
            runner_addr: "//:devenv".to_string(),
            artifacts: vec![],
        }
    }

    fn a_spec() -> hproc::proc_exec::Spec {
        hproc::proc_exec::Spec {
            program: std::path::PathBuf::from("go"),
            args: vec![std::ffi::OsString::from("build")],
            env: vec![(
                std::ffi::OsString::from("CC"),
                std::ffi::OsString::from("mine"),
            )],
            cwd: std::path::PathBuf::from("/ws"),
            stdin: hproc::proc_exec::StdioSpec::Null,
            stdout: hproc::proc_exec::StdioSpec::Piped,
            stderr: hproc::proc_exec::StdioSpec::Piped,
            setsid: false,
            ctty: false,
        }
    }

    /// `wrap` defers each spawn to `devenv shell --`, using the binary the
    /// driver captured with rather than whatever a PATH resolves to.
    #[test]
    fn wrap_mode_prefixes_every_spawn_with_devenv_shell() {
        let snap = snap_for(snapshot::Mode::Wrap);
        let session = Runner::open_wrap(&open_req(), &snap, vec![]).expect("wrap session");

        let out = session.prepare(a_spec()).expect("prepare");
        assert_eq!(
            out.program,
            std::path::PathBuf::from("/nix/store/d/bin/devenv")
        );
        let args: Vec<String> = out
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["shell", "--", "go", "build"]);
    }

    /// The target still wins on a collision, exactly as under `snapshot`.
    ///
    /// Only up to the point where `devenv shell` runs: it re-derives the
    /// variables it owns and those win over what is handed in, which is one of
    /// the reasons this mode is a demonstration rather than a recommendation.
    #[test]
    fn wrap_mode_puts_the_environment_under_the_target() {
        let snap = snap_for(snapshot::Mode::Wrap);
        let base = vec![
            (
                std::ffi::OsString::from("CC"),
                std::ffi::OsString::from("clang"),
            ),
            (
                std::ffi::OsString::from("EXTRA"),
                std::ffi::OsString::from("1"),
            ),
        ];
        let session = Runner::open_wrap(&open_req(), &snap, base).expect("wrap session");

        let out = session.prepare(a_spec()).expect("prepare");
        let cc = out
            .env
            .iter()
            .find(|(k, _)| k == "CC")
            .map(|(_, v)| v.to_string_lossy().into_owned());
        assert_eq!(cc.as_deref(), Some("mine"), "the target's own value wins");
        assert!(out.env.iter().any(|(k, _)| k == "EXTRA"));
    }

    /// A live shell entered per spawn cannot claim what a captured snapshot can.
    #[test]
    fn wrap_mode_is_asserted_not_pinned() {
        let snap = snap_for(snapshot::Mode::Wrap);
        let session = Runner::open_wrap(&open_req(), &snap, vec![]).expect("wrap session");
        assert!(
            !session.caps().identity.is_pinned(),
            "wrap re-enters the shell per spawn, outside any cache key"
        );
    }

    /// `wrap` has no prelude — and that is not a bug, it is the reason the mode
    /// buys nothing over `snapshot` for shell functions: `devenv shell -- prog`
    /// execs `prog` with no functions defined.
    #[test]
    fn wrap_mode_carries_no_prelude_and_still_names_the_functions() {
        let snap = snap_for(snapshot::Mode::Wrap);
        assert!(snap.shell_prelude.is_empty());
        assert_eq!(snap.shell_functions, vec!["fmt"]);
    }

    /// The mode travels in the artifact, because `open` never sees a def.
    #[test]
    fn the_mode_round_trips_through_the_artifact() {
        for mode in [
            snapshot::Mode::Snapshot,
            snapshot::Mode::Session,
            snapshot::Mode::Wrap,
        ] {
            let snap = snap_for(mode);
            let bytes = serde_json::to_vec(&snap).expect("encode");
            let back: Snapshot = serde_json::from_slice(&bytes).expect("decode");
            assert_eq!(back.mode, mode);
            assert_eq!(back.bin, "/nix/store/d/bin/devenv");
        }
    }

    /// Session mode carries function *definitions*, snapshot mode only their
    /// names — the definitions are large and a snapshot runner cannot use them.
    #[test]
    fn only_session_mode_carries_the_prelude() {
        let json = br#"{"bashFunctions":{"fmt":"  echo hi\n"},"variables":{"CC":{"type":"exported","value":"clang"}}}"#;
        let local = LocalPaths {
            tree_root: "/r".to_string(),
            home: "/h".to_string(),
            tmpdir: String::new(),
        };
        let snap = snapshot_from_json(
            json,
            &local,
            snapshot::Mode::Snapshot,
            "devenv".into(),
            Default::default(),
        )
        .expect("snapshot");
        assert!(snap.shell_prelude.is_empty());
        assert_eq!(snap.shell_functions, vec!["fmt"]);

        let sess = snapshot_from_json(
            json,
            &local,
            snapshot::Mode::Session,
            "devenv".into(),
            Default::default(),
        )
        .expect("session");
        assert!(
            sess.shell_prelude.contains("fmt () {"),
            "{:?}",
            sess.shell_prelude
        );
    }

    /// A name that is not a shell identifier cannot be declared; pasting it
    /// would produce a snippet that fails to parse and take every command in
    /// the session down with it.
    #[test]
    fn a_non_identifier_function_name_is_skipped() {
        let mut fns = BTreeMap::new();
        fns.insert("ok_name".to_string(), serde_json::json!("  :\n"));
        fns.insert("bad name; rm -rf /".to_string(), serde_json::json!("  :\n"));
        // bash declares this one but cannot `export -f` it, and a failed export
        // in the prelude takes down every command in the session.
        fns.insert("has-dash".to_string(), serde_json::json!("  :\n"));
        let out = render_prelude(&fns);
        assert!(out.contains("ok_name () {"));
        assert!(!out.contains("rm -rf"), "{out}");
        assert!(!out.contains("has-dash"), "{out}");
    }

    /// Every function is exported, or it is defined in a shell the target never
    /// runs in — `pluginexec` invokes the target through its own `bash -c`.
    #[test]
    fn functions_are_exported_to_child_shells() {
        let mut fns = BTreeMap::new();
        fns.insert("fmt_all".to_string(), serde_json::json!("  echo hi\n"));
        let out = render_prelude(&fns);
        assert!(out.contains("fmt_all () {"), "{out}");
        assert!(out.contains("export -f fmt_all"), "{out}");
    }

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
            snapshot::Mode::Snapshot,
            "devenv".into(),
            Default::default(),
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
