mod filterenv;
mod pty;
mod spec;

use crate::debug_hash::DebugHasher;
use crate::engine;
use crate::engine::driver::sandbox::EnvValue;
use crate::engine::driver::targetdef::path::{CodegenMode, Content, Path};
use crate::engine::driver::targetdef::{Input, InputMode, Output, TargetDef as EngineTargetDef};
use crate::engine::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr, outputartifact,
};
use crate::engine::driver_managed::{ManagedRunRequest, ManagedRunResponse};
use crate::hasync::Cancellable;
use crate::proc_exec;
use crate::process_supervisor;
use anyhow::Context;
use async_trait::async_trait;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io;
use xxhash_rust::xxh3::Xxh3Default;

const SHELL_INIT_SH: &str = include_str!("./init.sh");

const EXEC_DEF_FORMAT_VERSION: u32 = 1;

pub struct Driver {
    name: String,
    /// PATH the driver injects into target processes. Empty falls back to a hardcoded default.
    search_path: Vec<String>,
    wrap_run: fn(&std::path::Path, &[String]) -> anyhow::Result<Vec<String>>,
    wrap_run_shell: fn(&std::path::Path, &[String]) -> anyhow::Result<Vec<String>>,
}

#[derive(Clone, serde::Serialize)]
struct TargetDef {
    pub run: Vec<String>,
    /// Deps wired into SRC_*/LIST_* at runtime AND folded into the def hash
    /// (their structure invalidates cache when group membership changes).
    /// Built from `deps` and from transitive Deps with `hash=true, runtime=true`.
    pub dep_group_inputs: BTreeMap<String, Vec<Input>>,
    /// Deps wired into SRC_*/LIST_* at runtime but intentionally excluded
    /// from the def hash. Built from `runtime_deps` and from transitive
    /// Deps with `hash=false, runtime=true`. Changing their addresses must
    /// not invalidate the cache — that's the whole point of `runtime_deps`.
    pub runtime_dep_group_inputs: BTreeMap<String, Vec<Input>>,
    pub tool_group_inputs: BTreeMap<String, Vec<Input>>,
    pub env: BTreeMap<String, String>,
    pub pass_env: BTreeMap<String, String>,
    pub runtime_pass_env: Vec<String>,
    pub runtime_env: HashMap<String, String>,
}

impl Hash for TargetDef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        EXEC_DEF_FORMAT_VERSION.hash(state);
        self.run.hash(state);
        self.dep_group_inputs.hash(state);
        // runtime_dep_group_inputs intentionally excluded — runtime_deps
        // (and runtime-only transitives) must not affect the cache key.
        self.tool_group_inputs.hash(state);
        self.env.hash(state);
        self.pass_env.hash(state);
        // runtime_pass_env and runtime_env intentionally excluded
    }
}

fn bash_args(so: Vec<String>, lo: Vec<String>) -> Vec<String> {
    // Bash also interprets a number of multi-character options. These options must appear on the command line
    // before the single-character options to be recognized.
    let mut args = vec!["bash".to_string(), "--noprofile".to_string()];

    args.extend(lo);
    args.push("-o".to_string());
    args.push("pipefail".to_string());
    args.extend(so);

    args
}

fn render_shell_init(run: &[String]) -> anyhow::Result<String> {
    let mut ctx = serde_json::Map::new();
    if !run.is_empty() {
        ctx.insert(
            "cmds".to_string(),
            serde_json::Value::String(run.join("\n")),
        );
    }
    templi::render(SHELL_INIT_SH, &serde_json::Value::Object(ctx))
        .map_err(anyhow::Error::from)
        .context("render init.sh template")
}

fn bash_args_shell(sandbox_dir: &std::path::Path, run: &[String]) -> anyhow::Result<Vec<String>> {
    let rendered = render_shell_init(run)?;
    let init_path = sandbox_dir.join("init.sh");
    std::fs::write(&init_path, rendered).context("write init.sh")?;

    Ok(bash_args(
        vec!["-i".to_string()],
        vec![
            "--rcfile".to_string(),
            init_path.to_string_lossy().into_owned(),
        ],
    ))
}

/// Threshold above which the bash command body is spilled to a script file
/// in the sandbox instead of being passed inline via `-c`. Keeps argv small
/// for tall generated scripts.
const BASH_C_INLINE_MAX: usize = 500;

pub fn bash_args_public(
    sandbox_dir: &std::path::Path,
    cmd: &str,
    termargs: Vec<String>,
) -> anyhow::Result<Vec<String>> {
    let mut args = if cmd.len() > BASH_C_INLINE_MAX {
        let script_path = sandbox_dir.join("cmd.sh");
        std::fs::write(&script_path, cmd).context("write cmd.sh")?;
        bash_args(
            vec![
                "-u".to_string(),
                "-e".to_string(),
                script_path.to_string_lossy().into_owned(),
            ],
            vec!["--norc".to_string()],
        )
    } else {
        bash_args(
            vec![
                "-u".to_string(),
                "-e".to_string(),
                "-c".to_string(),
                cmd.to_string(),
            ],
            vec!["--norc".to_string()],
        )
    };

    if !termargs.is_empty() {
        // https://unix.stackexchange.com/a/144519
        // We push "bash" as a placeholder for $0 before appending termargs
        args.push("bash".to_string());
        args.extend(termargs);
    }
    Ok(args)
}

/// Mirrors `BASH_C_INLINE_MAX` — same inline-vs-spill cutoff so the two
/// drivers behave consistently for sandbox script materialization.
const SH_C_INLINE_MAX: usize = 500;

pub fn sh_args_public(
    sandbox_dir: &std::path::Path,
    cmd: &str,
    termargs: Vec<String>,
) -> anyhow::Result<Vec<String>> {
    // POSIX sh: no `--noprofile`/`--norc` (bash-only) and no `-o pipefail`
    // (not in POSIX, varies across sh implementations). Plugingo scripts are
    // pipe-free, so `set -u -e` is enough.
    let mut args = if cmd.len() > SH_C_INLINE_MAX {
        let script_path = sandbox_dir.join("cmd.sh");
        std::fs::write(&script_path, cmd).context("write cmd.sh")?;
        vec![
            "sh".to_string(),
            "-u".to_string(),
            "-e".to_string(),
            script_path.to_string_lossy().into_owned(),
        ]
    } else {
        vec![
            "sh".to_string(),
            "-u".to_string(),
            "-e".to_string(),
            "-c".to_string(),
            cmd.to_string(),
        ]
    };

    if !termargs.is_empty() {
        args.push("sh".to_string());
        args.extend(termargs);
    }
    Ok(args)
}

impl Driver {
    pub fn new_exec() -> Self {
        Self {
            name: "exec".to_string(),
            search_path: vec![],
            wrap_run: |_, run| Ok(run.to_vec()),
            wrap_run_shell: |sandbox_dir, run| {
                let joined: Vec<String> = if run.is_empty() {
                    Vec::new()
                } else {
                    vec![run.join(" ")]
                };
                bash_args_shell(sandbox_dir, &joined)
            },
        }
    }
    pub fn new_bash() -> Self {
        Self {
            name: "bash".to_string(),
            search_path: vec![],
            wrap_run: |sandbox_dir, run| {
                bash_args_public(sandbox_dir, run.join("\n").as_str(), vec![])
            },
            wrap_run_shell: bash_args_shell,
        }
    }

    pub fn new_sh() -> Self {
        Self {
            name: "sh".to_string(),
            search_path: vec![],
            wrap_run: |sandbox_dir, run| {
                sh_args_public(sandbox_dir, run.join("\n").as_str(), vec![])
            },
            // Interactive --shell debug UX (rcfile templates, history) is
            // bash-only; recreating it for sh isn't worth the duplication.
            wrap_run_shell: bash_args_shell,
        }
    }

    pub fn from_options_exec(opts: &crate::engine::config_file::Options) -> anyhow::Result<Self> {
        Ok(Self {
            search_path: decode_path(opts)?,
            ..Self::new_exec()
        })
    }

    pub fn from_options_bash(opts: &crate::engine::config_file::Options) -> anyhow::Result<Self> {
        Ok(Self {
            search_path: decode_path(opts)?,
            ..Self::new_bash()
        })
    }

    pub fn from_options_sh(opts: &crate::engine::config_file::Options) -> anyhow::Result<Self> {
        Ok(Self {
            search_path: decode_path(opts)?,
            ..Self::new_sh()
        })
    }
}

fn spec_path_to_target_path(raw: &str, pkg: &crate::htpkg::PkgBuf, codegen: &CodegenMode) -> Path {
    let path = if pkg.is_empty() {
        raw.to_string()
    } else {
        format!("{}/{}", pkg, raw)
    };
    let content = if ["*", "?", "["].iter().any(|&p| path.contains(p)) {
        Content::Glob(path)
    } else if path.ends_with('/') {
        Content::DirPath(path)
    } else {
        Content::FilePath(path)
    };
    Path {
        content,
        codegen_tree: codegen.clone(),
        collect: true,
    }
}

fn decode_path(opts: &crate::engine::config_file::Options) -> anyhow::Result<Vec<String>> {
    crate::engine::config_file::deny_unknown("exec/bash/sh driver", opts, &["path"])?;
    Ok(
        crate::engine::config_file::decode_opt(opts, "exec/bash/sh driver", "path")?
            .unwrap_or_default(),
    )
}

/// RAII guard that restores the parent terminal's cooked mode when dropped.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        _ = crossterm::terminal::disable_raw_mode();
    }
}

async fn tee_stream(
    source: Option<impl tokio::io::AsyncRead + Unpin>,
    log: Arc<std::sync::Mutex<std::fs::File>>,
    mut sink: Option<&mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Some(mut source) = source else { return };
    let mut buf = vec![0u8; 8192];
    loop {
        match source.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                #[expect(
                    clippy::indexing_slicing,
                    reason = "n guaranteed <= buf.len() by AsyncRead contract"
                )]
                let slice = &buf[..n];
                if let Ok(mut g) = log.lock() {
                    drop(g.write_all(slice));
                }
                if let Some(ref mut out) = sink {
                    drop(out.write_all(slice).await);
                    // Flush immediately so interactive shells see each byte
                    // appear as it's typed (tokio::io::stdout is line-buffered
                    // when wired to a tty).
                    drop(out.flush().await);
                }
            }
        }
    }
}

/// Tee chunks from a [`proc_exec::ChunkReader`] into the log file and the
/// optional TUI sink. Used in non-PTY mode where the child stdout/stderr
/// pipes are drained on a dedicated `std::thread` (inside `proc_exec`) and
/// surfaced to async-land via `std::sync::mpsc` — `ChunkReader::recv`
/// internally uses `block_in_place` on a kernel condvar, bypassing tokio's
/// macOS-flaky cross-thread waker.
async fn tee_chunks(
    reader: Option<proc_exec::ChunkReader>,
    log: Arc<std::sync::Mutex<std::fs::File>>,
    mut sink: Option<&mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
) {
    use tokio::io::AsyncWriteExt;
    let Some(mut reader) = reader else { return };
    loop {
        let chunk = match reader.recv().await {
            Ok(Some(c)) => c,
            _ => break,
        };
        if let Ok(mut g) = log.lock() {
            drop(g.write_all(&chunk));
        }
        if let Some(ref mut out) = sink {
            drop(out.write_all(&chunk).await);
            drop(out.flush().await);
        }
    }
}

/// PTY-mode stdin pump: copies bytes from the parent's stdin (TtyReader or
/// similar) into the PTY master via `AsyncPty`. Uses tokio `io::copy` since
/// `AsyncPty` is a normal tokio AsyncWrite over an `AsyncFd` — `EVFILT_READ`
/// / `EVFILT_WRITE` wake reliability is fine on macOS.
async fn pump_stdin_pty(
    mut src: &mut (dyn tokio::io::AsyncRead + Send + Sync + Unpin),
    mut sink: pty::AsyncPty,
    cancel: tokio::sync::oneshot::Receiver<()>,
) {
    use tokio::io::AsyncWriteExt;
    tokio::select! {
        _ = cancel => {}
        _ = io::copy(&mut src, &mut sink) => {}
    }
    drop(sink.shutdown().await);
}

/// Pump bytes from an async source into a [`proc_exec::StdinPump`]. Mirrors
/// the existing tokio `io::copy` path but writes through the platform-aware
/// stdin pump (sync `write_all` under `block_in_place` on macOS, native
/// tokio AsyncWrite on Linux).
async fn pump_stdin(
    src: &mut (dyn tokio::io::AsyncRead + Send + Sync + Unpin),
    mut sink: proc_exec::StdinPump,
    cancel: tokio::sync::oneshot::Receiver<()>,
) {
    use tokio::io::AsyncReadExt;
    let copy = async {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = match src.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            #[expect(
                clippy::indexing_slicing,
                reason = "n <= buf.len() by AsyncRead::read contract"
            )]
            let chunk = &buf[..n];
            if sink.write_all(chunk).await.is_err() {
                return;
            }
        }
    };
    tokio::select! {
        _ = cancel => {}
        _ = copy => {}
    }
    drop(sink.shutdown().await);
}

#[async_trait]
impl engine::driver_managed::ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: self.name.clone(),
        })
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let spec = spec::TargetSpec::from(req.target_spec.config.clone())?;

        let pkg = req.target_spec.addr.package.clone();

        let build_dep_inputs = |deps: HashMap<String, Vec<String>>,
                                origin_prefix: &'static str,
                                hashed: bool,
                                runtime: bool|
         -> anyhow::Result<Vec<(String, Input)>> {
            deps.into_iter()
                .flat_map(|(k, v)| {
                    let pkg = pkg.clone();
                    v.into_iter().enumerate().map(
                        move |(i, v)| -> anyhow::Result<(String, Input)> {
                            Ok((
                                k.parse()?,
                                Input {
                                    r#ref: TargetAddr::parse(&v, &pkg)?,
                                    mode: InputMode::Standard,
                                    origin_id: format!("{}|{}|{}", origin_prefix, k, i),
                                    annotations: BTreeMap::new(),
                                    hashed,
                                    runtime,
                                },
                            ))
                        },
                    )
                })
                .collect::<anyhow::Result<Vec<_>>>()
        };

        let dep_inputs = build_dep_inputs(spec.deps, "dep", true, true)?;
        let hash_dep_inputs = build_dep_inputs(spec.hash_deps, "hash_dep", true, false)?;
        let runtime_dep_inputs = build_dep_inputs(spec.runtime_deps, "runtime_dep", false, true)?;

        let mut dep_group_inputs: BTreeMap<String, Vec<Input>> = BTreeMap::new();
        for (group, input) in &dep_inputs {
            dep_group_inputs
                .entry(group.clone())
                .or_default()
                .push(input.clone());
        }

        let mut runtime_dep_group_inputs: BTreeMap<String, Vec<Input>> = BTreeMap::new();
        for (group, input) in &runtime_dep_inputs {
            runtime_dep_group_inputs
                .entry(group.clone())
                .or_default()
                .push(input.clone());
        }

        let tool_inputs = spec
            .tools
            .into_iter()
            .flat_map(|(k, v)| {
                let pkg = pkg.clone();
                v.into_iter()
                    .enumerate()
                    .map(move |(i, v)| -> anyhow::Result<(String, Input)> {
                        Ok((
                            k.parse()?,
                            Input {
                                r#ref: TargetAddr::parse(&v, &pkg)?,
                                mode: InputMode::Tool,
                                origin_id: format!("tool|{}|{}", k, i),
                                annotations: BTreeMap::from([(
                                    "unpack_root".to_string(),
                                    "tools".to_string(),
                                )]),
                                hashed: true,
                                runtime: true,
                            },
                        ))
                    })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let mut tool_group_inputs: BTreeMap<String, Vec<Input>> = BTreeMap::new();
        for (group, input) in &tool_inputs {
            tool_group_inputs
                .entry(group.clone())
                .or_default()
                .push(input.clone());
        }

        let pass_env: BTreeMap<String, String> = spec
            .pass_env
            .into_iter()
            .filter_map(|name| std::env::var(&name).ok().map(|val| (name, val)))
            .collect();

        let def = TargetDef {
            run: spec.run,
            dep_group_inputs,
            runtime_dep_group_inputs,
            tool_group_inputs,
            env: spec.env.into_iter().collect(),
            pass_env,
            runtime_pass_env: spec.runtime_pass_env,
            runtime_env: spec.runtime_env,
        };

        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("exec_def_{}", req.target_spec.addr.format())
            });
            def.hash(&mut h);

            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: EngineTargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs: dep_inputs
                    .into_iter()
                    .map(|(_, v)| v)
                    .chain(hash_dep_inputs.into_iter().map(|(_, v)| v))
                    .chain(runtime_dep_inputs.into_iter().map(|(_, v)| v))
                    .chain(tool_inputs.into_iter().map(|(_, v)| v))
                    .collect(),
                outputs: spec
                    .outputs
                    .iter()
                    .map(|(k, v)| Output {
                        group: k.clone(),
                        paths: v
                            .iter()
                            .map(|p| spec_path_to_target_path(p, &pkg, &spec.codegen))
                            .collect(),
                    })
                    .collect(),
                support_files: spec
                    .support_files
                    .iter()
                    .map(|p| spec_path_to_target_path(p, &pkg, &CodegenMode::None))
                    .collect(),
                cache: spec.cache.local,
                disable_remote_cache: !spec.cache.remote,
                pty: true,
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
        let mut def = req.target_def.clone();
        let mut xdef = def.def::<TargetDef>().clone();
        for tool in req.sandbox.tools {
            let input = Input {
                r#ref: tool.r#ref.clone(),
                mode: InputMode::Tool,
                origin_id: tool.id.clone(),
                annotations: BTreeMap::from([("unpack_root".to_string(), "tools".to_string())]),
                hashed: tool.hash,
                runtime: true,
            };
            xdef.tool_group_inputs
                .entry(tool.group)
                .or_default()
                .push(input.clone());
            def.inputs.push(input);
        }
        for dep in req.sandbox.deps {
            let input = Input {
                r#ref: dep.r#ref.clone(),
                mode: InputMode::Standard,
                origin_id: dep.id.clone(),
                annotations: BTreeMap::new(),
                hashed: dep.hash,
                runtime: dep.runtime,
            };
            // Route into runtime wiring only when the dep is meant to be
            // available at runtime. Hash-only transitive deps still need to
            // appear in `def.inputs` so the engine resolves them and folds
            // their hashout into hashin, but they must not be wired into
            // SRC_*/LIST_*.
            if dep.runtime {
                let target_map = if dep.hash {
                    &mut xdef.dep_group_inputs
                } else {
                    &mut xdef.runtime_dep_group_inputs
                };
                target_map.entry(dep.group).or_default().push(input.clone());
            }
            def.inputs.push(input);
        }
        for (name, env) in req.sandbox.env {
            match env.value {
                EnvValue::Pass => {
                    if env.hash {
                        if let Ok(value) = std::env::var(&name) {
                            xdef.pass_env.insert(name, value);
                        }
                    } else {
                        xdef.runtime_pass_env.push(name);
                    }
                }
                EnvValue::Literal(v) => {
                    if env.hash {
                        xdef.pass_env.insert(name, v);
                    } else {
                        xdef.runtime_env.insert(name, v);
                    }
                }
            }
        }

        def.hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("exec_def_tr_{}", def.addr.format())
            });
            xdef.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        def.set_def(xdef);

        Ok(ApplyTransitiveResponse { target_def: def })
    }

    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        self.run_inner(req, ctoken, false).await
    }

    fn supports_shell(&self) -> bool {
        true
    }

    async fn run_shell<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        self.run_inner(req, ctoken, true).await
    }
}

impl Driver {
    async fn run_inner<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
        shell: bool,
    ) -> anyhow::Result<ManagedRunResponse> {
        crate::hmemoizer::set_phase("pluginexec:sandbox_setup");
        let rreq = req.request;
        let def = rreq.target.def::<TargetDef>();

        let run = {
            if shell {
                (self.wrap_run_shell)(&rreq.sandbox_dir, &def.run)?
            } else {
                (self.wrap_run)(&rreq.sandbox_dir, &def.run)?
            }
        };

        if run.is_empty() {
            anyhow::bail!("`run` is empty")
        }

        let mut env = HashMap::<String, String>::new();
        if shell && let Ok(term) = std::env::var("TERM") {
            env.insert("TERM".to_string(), term);
        }
        let path_value = if self.search_path.is_empty() {
            [
                "/usr/local/bin",
                "/usr/bin",
                "/bin",
            ]
            .join(":")
        } else {
            self.search_path.join(":")
        };
        env.insert("PATH".to_string(), path_value);
        env.insert(
            "WORKSPACE_ROOT".to_string(),
            req.sandbox_ws_dir.to_string_lossy().to_string(),
        );

        for (k, v) in &def.env {
            env.insert(k.clone(), v.clone());
        }

        let pkg_prefix = {
            let pkg = rreq.target.addr.package.as_str();
            if pkg.is_empty() {
                String::new()
            } else {
                format!("{}/", pkg)
            }
        };
        for output in &rreq.target.outputs {
            let key = if output.group.is_empty() {
                "OUT".to_string()
            } else {
                format!("OUT_{}", output.group.to_uppercase())
            };
            let entry = env.entry(key).or_default();
            for path in &output.paths {
                match &path.content {
                    Content::Glob(_) => {}
                    Content::FilePath(p) | Content::DirPath(p) => {
                        if !entry.is_empty() {
                            entry.push(' ');
                        }
                        let rel = p.strip_prefix(&pkg_prefix).unwrap_or(p);
                        entry.push_str(rel);

                        let abs = req.sandbox_ws_dir.join(p);
                        let dir_to_create = match &path.content {
                            Content::FilePath(_) => abs.parent(),
                            Content::DirPath(_) => Some(abs.as_path()),
                            Content::Glob(_) => None,
                        };
                        if let Some(d) = dir_to_create {
                            std::fs::create_dir_all(d)
                                .with_context(|| format!("create output dir {}", d.display()))?;
                        }
                    }
                }
            }
        }

        // Merge hashed and runtime-only dep groups so each group ends up
        // with a single SRC_*/LIST_* env entry covering both sources.
        // `deps` come first to preserve their established input ordering;
        // `runtime_deps` (also `runtime=true` transitives with `hash=false`)
        // append after.
        let mut merged_dep_groups: BTreeMap<&str, Vec<&Input>> = BTreeMap::new();
        for (group, inputs) in &def.dep_group_inputs {
            merged_dep_groups
                .entry(group.as_str())
                .or_default()
                .extend(inputs.iter());
        }
        for (group, inputs) in &def.runtime_dep_group_inputs {
            merged_dep_groups
                .entry(group.as_str())
                .or_default()
                .extend(inputs.iter());
        }

        for (group, inputs) in &merged_dep_groups {
            let src_key = if group.is_empty() {
                "SRC".to_string()
            } else {
                format!("SRC_{}", group.to_uppercase())
            };

            let mut list_f = {
                let path = req
                    .sandbox_dir
                    .join("list")
                    .join(format!("dep_{}.list", group));
                env.entry(format!("LIST_{src_key}")).or_default().push_str(
                    path.as_path()
                        .to_str()
                        .expect("sandbox dir path must be valid UTF-8"),
                );
                std::fs::File::create(&path)
                    .with_context(|| format!("create dep list file {:?}", path))?
            };
            let entry = env.entry(src_key).or_default();

            for input in inputs {
                // Filter to Dep-type ManagedRunInputs only. A target may have
                // both Dep outputs and Support files sharing the same
                // origin_id; the support input has no list file and must not
                // leak into SRC_/LIST_ env routing.
                if let Some(m) = req.inputs.iter().find(|m| {
                    m.input.origin_id == input.origin_id
                        && matches!(
                            m.input.artifact.r#type,
                            crate::engine::driver::inputartifact::Type::Dep
                        )
                }) {
                    let list_path = m.require_list_path()?;
                    let managed_list_f = std::fs::File::open(list_path).with_context(|| {
                        format!(
                            "open dep list file {:?} (origin_id={})",
                            list_path, input.origin_id
                        )
                    })?;
                    for line in std::io::BufReader::new(managed_list_f).lines() {
                        let line = line
                            .with_context(|| format!("read line from dep list {:?}", list_path))?;
                        if line.is_empty() {
                            continue;
                        }

                        if !entry.is_empty() {
                            entry.push(' ');
                        }
                        entry.push_str(&line);

                        list_f
                            .write_all(line.as_bytes())
                            .with_context(|| format!("write to dep list (group={group})"))?;
                        list_f.write_all("\n".as_bytes()).with_context(|| {
                            format!("write newline to dep list (group={group})")
                        })?;
                    }
                }
            }
        }

        let tool_bin_dir = if !def.tool_group_inputs.is_empty() {
            let bin_dir = req.sandbox_dir.join("bin");
            std::fs::create_dir_all(&bin_dir)
                .with_context(|| format!("create tool bin dir {:?}", bin_dir))?;

            // Symlink each unique `bin/<filename>` exactly once. Two tool
            // inputs producing the same filename — whether from the same
            // source target referenced through multiple groups, or from
            // distinct fs:* wrappers around the same on-disk binary — are
            // treated as the same logical entry. The address-level
            // invariant (no two engine inputs share `(r#ref, group)`) is
            // enforced upstream by `Sandbox::merge_sandbox`; here we just
            // skip the redundant symlink to avoid EEXIST.
            //
            // Multi-output tool refs produce N RunInputs sharing one
            // origin_id; the bridge appends every output's file path to the
            // same list file. engine/link.rs validates 1 FilePath per output.
            let mut linked: std::collections::HashSet<std::ffi::OsString> =
                std::collections::HashSet::new();
            for (group, inputs) in &def.tool_group_inputs {
                let tool_key = if group.is_empty() {
                    "TOOL".to_string()
                } else {
                    format!("TOOL_{}", group.to_uppercase())
                };
                // Per-group dedup for $TOOL_<G> values. `linked` is process-wide
                // (avoids EEXIST on symlink); `group_seen` is per-group so each
                // TOOL_<G> reflects that group's references without duplicates
                // when one group lists the same filename via multiple inputs.
                let mut group_seen: std::collections::HashSet<std::ffi::OsString> =
                    std::collections::HashSet::new();
                for input in inputs {
                    // Filter to Dep-type — Support inputs that travel with the
                    // tool target's deps share its origin_id but must not be
                    // symlinked into bin/.
                    let Some(m) = req.inputs.iter().find(|m| {
                        m.input.origin_id == input.origin_id
                            && matches!(
                                m.input.artifact.r#type,
                                crate::engine::driver::inputartifact::Type::Dep
                            )
                    }) else {
                        continue;
                    };
                    let list_path = m.require_list_path()?;
                    let list_f = std::fs::File::open(list_path).with_context(|| {
                        format!(
                            "open tool list {:?} (group={group}, origin_id={})",
                            list_path, input.origin_id
                        )
                    })?;
                    let mut any = false;
                    for line in std::io::BufReader::new(list_f).lines() {
                        let file_path = line
                            .with_context(|| format!("read line from tool list {:?}", list_path))?;
                        if file_path.is_empty() {
                            continue;
                        }
                        any = true;
                        let filename = std::path::Path::new(&file_path)
                            .file_name()
                            .ok_or_else(|| {
                                anyhow::anyhow!("tool file path has no filename: {}", file_path)
                            })?
                            .to_os_string();
                        let bin_path = bin_dir.join(&filename);

                        if group_seen.insert(filename.clone()) {
                            let bin_path_str = bin_path.to_str().ok_or_else(|| {
                                anyhow::anyhow!("bin path is not valid UTF-8: {:?}", bin_path)
                            })?;
                            let entry = env.entry(tool_key.clone()).or_default();
                            if !entry.is_empty() {
                                entry.push(' ');
                            }
                            entry.push_str(bin_path_str);
                        }

                        if !linked.insert(filename.clone()) {
                            continue;
                        }
                        #[cfg(unix)]
                        std::os::unix::fs::symlink(&file_path, &bin_path).with_context(|| {
                            format!("symlink tool {file_path:?} -> {bin_path:?}")
                        })?;
                        #[cfg(not(unix))]
                        std::fs::copy(&file_path, &bin_path)
                            .with_context(|| format!("copy tool {file_path:?} -> {bin_path:?}"))?;
                    }
                    if !any {
                        anyhow::bail!("tool '{}' produced no files", input.origin_id);
                    }
                }
            }

            Some(bin_dir)
        } else {
            None
        };

        env.extend(def.pass_env.iter().map(|(k, v)| (k.clone(), v.clone())));

        for name in &def.runtime_pass_env {
            if let Ok(value) = std::env::var(name) {
                env.insert(name.clone(), value);
            }
        }

        env.extend(def.runtime_env.iter().map(|(k, v)| (k.clone(), v.clone())));

        if let Some(bin_dir) = tool_bin_dir {
            let bin_dir_str = bin_dir
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("bin dir path is not valid UTF-8"))?
                .to_string();
            let path_entry = env.entry("PATH".to_string()).or_default();
            if path_entry.is_empty() {
                *path_entry = bin_dir_str;
            } else {
                let old = path_entry.clone();
                *path_entry = format!("{}:{}", bin_dir_str, old);
            }
        }

        let output_log_path = req.sandbox_dir.join("log.txt");
        let output_log =
            std::fs::File::create(&output_log_path).with_context(|| "create log file")?;
        let output_log_file = Arc::new(std::sync::Mutex::new(output_log));

        // Shell mode runs the child attached to a freshly-allocated PTY so bash
        // sees a real terminal and runs interactively. The parent forwards
        // stdin/stdout via the PTY master through the same tee paths used by
        // the non-shell case (`tee_stream` on AsyncPty for PTY; `tee_chunks`
        // on ChunkReader for piped stdio).
        let pty_pair = if shell {
            Some(pty::open_pty().context("openpty")?)
        } else {
            None
        };

        // run is guaranteed non-empty by the bail! above, so [0] and [1..] are safe
        let (program, args) = {
            #[expect(
                clippy::indexing_slicing,
                reason = "run non-empty guaranteed by bail! check above"
            )]
            (run[0].clone(), run[1..].to_vec())
        };

        let args_os: Vec<OsString> = args.iter().map(OsString::from).collect();

        // execve caps total argv+envp at ARG_MAX and each entry at MAX_ARG_STRLEN.
        // Drop overlong entries and evict longest until under the limit.
        let env_vec: Vec<(String, String)> = env.into_iter().collect();
        let mut argv_for_filter: Vec<OsString> = Vec::with_capacity(1 + args_os.len());
        argv_for_filter.push(OsString::from(&program));
        argv_for_filter.extend(args_os.iter().cloned());
        let env_vec = filterenv::filter_long_env(env_vec, &argv_for_filter);
        let env_pairs: Vec<(OsString, OsString)> = env_vec
            .into_iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect();

        let spec = if let Some((master, slave)) = &pty_pair {
            // Inherit the parent's terminal size so bash can wrap and place the
            // prompt correctly. Falls back to 80x24 if the parent has no tty.
            let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            pty::set_winsize(master, rows, cols).context("set pty winsize")?;

            // Copy the parent's line discipline (ICRNL/ONLCR/etc.) onto the
            // slave so bash sees a standard cooked terminal. Must run BEFORE
            // we put the parent into raw mode below.
            pty::inherit_termios(slave).context("copy parent termios to pty slave")?;

            let stdin_fd = slave.try_clone().context("dup pty slave for stdin")?;
            let stdout_fd = slave.try_clone().context("dup pty slave for stdout")?;
            let stderr_fd = slave.try_clone().context("dup pty slave for stderr")?;
            // setsid+ctty: child becomes session leader and the inherited fd 0
            // becomes its controlling terminal. The supervisor's `killpg`
            // reaps the whole tree on hard-shutdown.
            proc_exec::Spec {
                program: PathBuf::from(program),
                args: args_os,
                env: env_pairs,
                cwd: req.sandbox_pkg_dir.clone(),
                stdin: proc_exec::StdioSpec::Fd(stdin_fd),
                stdout: proc_exec::StdioSpec::Fd(stdout_fd),
                stderr: proc_exec::StdioSpec::Fd(stderr_fd),
                setsid: true,
                ctty: true,
            }
        } else {
            // setsid: true so the child becomes its own process-group
            // leader (pid == pgid). Without this, any descendant that
            // double-forks (e.g. Go test runners spawning helper daemons)
            // gets reparented to launchd on the immediate child's exit
            // and keeps holding the stdout/stderr pipe write ends — the
            // drain threads' `read()` then never returns 0 and
            // `Handle::wait` blocks indefinitely waiting for EOF. With a
            // pgid, the supervisor sidecar's `killpg(pid, SIGKILL)` on
            // cancel/parent-death reaps the whole tree, and the bounded
            // drain-join in `Handle::wait` can kill stragglers with the
            // same call. No controlling terminal (ctty: false) since
            // these are non-interactive children.
            let stdin = if rreq.stdin.is_some() {
                proc_exec::StdioSpec::Piped
            } else {
                proc_exec::StdioSpec::Null
            };
            proc_exec::Spec {
                program: PathBuf::from(program),
                args: args_os,
                env: env_pairs,
                cwd: req.sandbox_pkg_dir.clone(),
                stdin,
                stdout: proc_exec::StdioSpec::Piped,
                stderr: proc_exec::StdioSpec::Piped,
                setsid: true,
                ctty: false,
            }
        };

        crate::hmemoizer::set_phase("pluginexec:spawn");
        let mut handle = proc_exec::spawn(spec).with_context(|| "spawn child process")?;
        let child_pid = handle.pid();

        // Drop the parent's copy of the slave so the master sees EOF when the
        // child exits.
        let pty_master = pty_pair.map(|(master, _slave)| master);

        // Put the parent terminal into raw mode so keystrokes are forwarded
        // byte-by-byte to the child PTY without local echo or line buffering.
        // The child's PTY slave owns line discipline and echo.
        let _raw_guard = if shell {
            crossterm::terminal::enable_raw_mode()
                .ok()
                .map(|()| RawModeGuard)
        } else {
            None
        };

        // Signal that cancels the stdin pump once the child has exited. Without
        // it, shell mode would deadlock waiting on a parent-stdin read that
        // nothing intends to satisfy.
        let (stdin_cancel_tx, stdin_cancel_rx) = tokio::sync::oneshot::channel::<()>();

        // Build futures for stdin pump and stdout/stderr tee. The two modes
        // differ in plumbing — PTY uses AsyncPty over the master fd (tokio
        // IO driver, EVFILT_READ — reliable on macOS), pipe mode uses the
        // off-tokio ChunkReader / StdinPump from proc_exec.
        enum IoFutures<'r> {
            Pty {
                stdin: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'r>>,
                stdout: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'r>>,
            },
            Pipes {
                stdin: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'r>>,
                stdout: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'r>>,
                stderr: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'r>>,
            },
        }

        let io_futures: IoFutures<'_> = if let Some(master) = pty_master {
            let read_fd = master.try_clone().context("dup pty master for read")?;
            let reader = pty::AsyncPty::new(read_fd).context("async pty reader")?;
            let writer = pty::AsyncPty::new(master).context("async pty writer")?;

            let log_for_out = Arc::clone(&output_log_file);
            let stdout_fut = Box::pin(tee_stream(Some(reader), log_for_out, rreq.stdout));

            let stdin_fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> =
                if let Some(src) = rreq.stdin {
                    Box::pin(pump_stdin_pty(src, writer, stdin_cancel_rx))
                } else {
                    drop(writer);
                    Box::pin(async {})
                };
            IoFutures::Pty {
                stdin: stdin_fut,
                stdout: stdout_fut,
            }
        } else {
            let stdin_pump = handle.take_stdin();
            let stdout_reader = handle.take_stdout();
            let stderr_reader = handle.take_stderr();
            let log_for_out = Arc::clone(&output_log_file);
            let log_for_err = Arc::clone(&output_log_file);
            let stdout_fut = Box::pin(tee_chunks(stdout_reader, log_for_out, rreq.stdout));
            let stderr_fut = Box::pin(tee_chunks(stderr_reader, log_for_err, rreq.stderr));
            let stdin_fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> =
                match (rreq.stdin, stdin_pump) {
                    (Some(src), Some(pump)) => Box::pin(pump_stdin(src, pump, stdin_cancel_rx)),
                    _ => Box::pin(async {}),
                };
            IoFutures::Pipes {
                stdin: stdin_fut,
                stdout: stdout_fut,
                stderr: stderr_fut,
            }
        };

        crate::hmemoizer::set_phase("pluginexec:wait_subprocess");
        // The wait happens in a spawned task. `Handle::wait_or_cancel` uses
        // `block_in_place` internally to park on a `std::sync::mpsc` from the
        // kqueue watcher — kernel condvar wake, no tokio waker on the wait
        // path itself. We `tokio::spawn` so the wait doesn't share a worker
        // with the IO pumps (which would deadlock when the child's pipe
        // buffer fills behind a parked worker).
        //
        // Awaiting the spawned task's JoinHandle is the single residual
        // exposure to the macOS waker bug (`RCA_MACOS_WAKER.md`): the wake
        // fires post-exit and tokio's work-stealing re-polls eventually, so
        // a dropped wake produces at most a small scheduling delay, not a
        // deadlock. Same shape as the previously-verified design.
        let wait_handle: tokio::task::JoinHandle<anyhow::Result<std::process::ExitStatus>> =
            tokio::spawn(async move {
                // Take ownership of `ctoken` indirectly: we observe its state
                // from this task via a freshly cloned `cancellable_token`
                // already captured by closure. ctoken itself is a trait
                // object outside, which is not Send-friendly to move here —
                // route cancellation through the outer select! below instead.
                let status = handle.wait().await.context("wait for child process")?;
                _ = stdin_cancel_tx.send(());
                Ok(status)
            });
        tokio::pin!(wait_handle);

        match io_futures {
            IoFutures::Pty { stdin, stdout } => {
                tokio::select! {
                    _ = ctoken.cancelled() => {
                        process_supervisor::kill_child(child_pid);
                        _ = (&mut wait_handle).await;
                        anyhow::bail!("cancelled")
                    },
                    res = async {
                        let io = async { tokio::join!(stdin, stdout) };
                        tokio::pin!(io);
                        tokio::select! {
                            wait_res = &mut wait_handle => {
                                crate::hmemoizer::set_phase("pluginexec:post_wait_io_drain");
                                _ = tokio::time::timeout(
                                    std::time::Duration::from_millis(50),
                                    &mut io,
                                ).await;
                                wait_res
                            }
                            _ = &mut io => {
                                crate::hmemoizer::set_phase("pluginexec:post_io_wait");
                                (&mut wait_handle).await
                            },
                        }
                    } => {
                        crate::hmemoizer::set_phase("pluginexec:post_wait_status_check");
                        let status = res
                            .context("wait task panicked")?
                            .context("wait for child process")?;
                        if !status.success() {
                            let log = std::fs::read_to_string(&output_log_path).unwrap_or_default();
                            anyhow::bail!("process exited with status: {}\n{}", status, log)
                        }
                    }
                }
            }
            IoFutures::Pipes {
                stdin,
                stdout,
                stderr,
            } => {
                tokio::select! {
                    _ = ctoken.cancelled() => {
                        process_supervisor::kill_child(child_pid);
                        _ = (&mut wait_handle).await;
                        anyhow::bail!("cancelled")
                    },
                    res = async {
                        let io = async { tokio::join!(stdin, stdout, stderr) };
                        tokio::pin!(io);
                        tokio::select! {
                            wait_res = &mut wait_handle => {
                                crate::hmemoizer::set_phase("pluginexec:post_wait_io_drain");
                                _ = tokio::time::timeout(
                                    std::time::Duration::from_millis(50),
                                    &mut io,
                                ).await;
                                wait_res
                            }
                            _ = &mut io => {
                                crate::hmemoizer::set_phase("pluginexec:post_io_wait");
                                (&mut wait_handle).await
                            },
                        }
                    } => {
                        crate::hmemoizer::set_phase("pluginexec:post_wait_status_check");
                        let status = res
                            .context("wait task panicked")?
                            .context("wait for child process")?;
                        if !status.success() {
                            let log = std::fs::read_to_string(&output_log_path).unwrap_or_default();
                            anyhow::bail!("process exited with status: {}\n{}", status, log)
                        }
                    }
                }
            }
        }
        crate::hmemoizer::set_phase("pluginexec:post_wait_done");

        Ok(ManagedRunResponse {
            artifacts: vec![outputartifact::OutputArtifact {
                group: "".to_string(),
                name: "log.txt".to_string(),
                r#type: outputartifact::Type::Log,
                content: outputartifact::Content::File(outputartifact::ContentFile {
                    source_path: output_log_path
                        .to_str()
                        .ok_or_else(|| anyhow::anyhow!("log path is not valid UTF-8"))?
                        .parse()?,
                    out_path: "log.txt".to_string(),
                    x: false,
                }),
                hashout: "".to_string(),
            }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::driver::RunRequest;
    use crate::engine::driver_managed::ManagedDriver;
    use crate::hasync::StdCancellationToken;
    use crate::htaddr::Addr;
    use enclose::enclose;

    fn make_req<'a, 'io>(request: RunRequest<'a, 'io>) -> ManagedRunRequest<'a, 'io> {
        let path = request.sandbox_dir.clone();
        ManagedRunRequest {
            request,
            sandbox_dir: path.clone(),
            sandbox_ws_dir: path.clone(),
            sandbox_pkg_dir: path,
            inputs: vec![],
        }
    }

    #[test]
    fn bash_args_public_spills_long_cmd_to_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let short = "echo hi";
        let short_args = bash_args_public(dir.path(), short, vec![]).expect("short");
        assert!(short_args.iter().any(|a| a == "-c"));
        assert!(short_args.iter().any(|a| a == short));
        assert!(!dir.path().join("cmd.sh").exists());

        let long = "x".repeat(BASH_C_INLINE_MAX + 1);
        let long_args = bash_args_public(dir.path(), &long, vec![]).expect("long");
        assert!(!long_args.iter().any(|a| a == "-c"));
        let script = dir.path().join("cmd.sh");
        assert!(script.exists());
        assert_eq!(std::fs::read_to_string(&script).expect("read"), long);
        assert!(
            long_args
                .iter()
                .any(|a| a == script.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn sh_args_public_spills_long_cmd_to_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let short = "echo hi";
        let short_args = sh_args_public(dir.path(), short, vec![]).expect("short");
        assert_eq!(short_args.first().map(String::as_str), Some("sh"));
        assert!(short_args.iter().any(|a| a == "-c"));
        assert!(short_args.iter().any(|a| a == short));
        assert!(!short_args.iter().any(|a| a == "--noprofile"));
        assert!(!short_args.iter().any(|a| a == "--norc"));
        assert!(!short_args.iter().any(|a| a == "pipefail"));
        assert!(!dir.path().join("cmd.sh").exists());

        let long = "x".repeat(SH_C_INLINE_MAX + 1);
        let long_args = sh_args_public(dir.path(), &long, vec![]).expect("long");
        assert!(!long_args.iter().any(|a| a == "-c"));
        let script = dir.path().join("cmd.sh");
        assert!(script.exists());
        assert_eq!(std::fs::read_to_string(&script).expect("read"), long);
        assert!(
            long_args
                .iter()
                .any(|a| a == script.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn from_options_sh_no_path() {
        let opts = crate::engine::config_file::Options::new();
        let d = Driver::from_options_sh(&opts).expect("from_options");
        assert_eq!(d.name, "sh");
        assert!(d.search_path.is_empty());
    }

    #[test]
    fn from_options_exec_no_path() {
        let opts = crate::engine::config_file::Options::new();
        let d = Driver::from_options_exec(&opts).expect("from_options");
        assert_eq!(d.name, "exec");
        assert!(d.search_path.is_empty());
    }

    #[test]
    fn from_options_exec_reads_path() {
        let mut opts = crate::engine::config_file::Options::new();
        opts.insert(
            "path".to_string(),
            serde_yaml::from_str("[/usr/bin, /bin]").expect("yaml"),
        );
        let d = Driver::from_options_exec(&opts).expect("from_options");
        assert_eq!(d.search_path, vec!["/usr/bin", "/bin"]);
    }

    #[test]
    fn from_options_bash_rejects_unknown_key() {
        let mut opts = crate::engine::config_file::Options::new();
        opts.insert("bogus".to_string(), serde_yaml::Value::Bool(true));
        let err = Driver::from_options_bash(&opts).err().expect("must error");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn test_render_shell_init_with_cmds() {
        let run = vec!["echo hi".to_string(), "ls -la".to_string()];
        let out = render_shell_init(&run).expect("render");
        assert!(out.contains("run()"), "missing run() definition: {out}");
        assert!(out.contains("xrun()"), "missing xrun() definition: {out}");
        assert!(out.contains("echo hi\nls -la"), "cmds not joined: {out}");
        assert!(!out.contains("{{"), "template tokens left: {out}");
    }

    #[test]
    fn test_render_shell_init_without_cmds() {
        let out = render_shell_init(&[]).expect("render");
        assert!(!out.contains("run()"), "should not have run() block: {out}");
        assert!(
            !out.contains("HEPH_EOF"),
            "should not have show() block: {out}"
        );
        assert!(!out.contains("{{"), "template tokens left: {out}");
    }

    #[tokio::test]
    async fn test_run_echo_hello() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                run: vec!["echo".to_string(), "hello".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: true,
            disable_remote_cache: false,
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let mut stdout = Vec::new();
        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: Some(&mut stdout),
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
        };

        let _res = driver.run(make_req(req), &ctoken).await?;

        let output = String::from_utf8(stdout)?;
        assert_eq!(output.trim(), "hello");

        Ok(())
    }

    #[tokio::test]
    async fn test_run_cat_hang() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                run: vec!["cat".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: true,
            disable_remote_cache: false,
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let mut stdin = std::io::Cursor::new(b"test data");
        let mut stdout = Vec::new();
        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: Some(&mut stdin),
            stdout: Some(&mut stdout),
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
        };

        // Use a timeout to detect the hang
        let _res = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            driver.run(make_req(req), &ctoken),
        )
        .await?;

        let output = String::from_utf8(stdout)?;
        assert_eq!(output, "test data");

        Ok(())
    }

    #[tokio::test]
    async fn test_run_stdin_to_stdout_timeout() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                run: vec!["cat".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: true,
            disable_remote_cache: false,
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;

        // Use a pipe that will never resolve to simulate a hang
        let (mut reader, _writer) = io::duplex(64);

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: Some(&mut reader),
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
        };

        let run_fut = driver.run(make_req(req), &ctoken);

        tokio::spawn(enclose!((ctoken) async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            ctoken.cancel();
        }));

        let res = run_fut.await;
        assert!(res.is_err());
        let err = res.err().unwrap();
        assert_eq!(err.to_string(), "cancelled");

        Ok(())
    }

    /// Regression for shell/build deadlock when the child needs concurrent
    /// IO pump progress while the spawned wait task parks a worker. The
    /// stdout drain on a dedicated `std::thread` (inside `proc_exec`) plus
    /// the wait poll on a separate spawn must keep the pipe buffer
    /// draining; without that, the child blocks on `write` and never exits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_run_large_output_does_not_deadlock_multi_thread() -> anyhow::Result<()> {
        let driver = Driver::new_bash();
        let ctoken = StdCancellationToken::new();
        // 256 KiB — bigger than macOS pipe buffers (16-64 KiB), so without
        // a draining stdout pump the child would block on write forever.
        let payload_bytes: usize = 256 * 1024;
        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                run: vec![format!("head -c {payload_bytes} /dev/urandom | base64")],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: true,
            disable_remote_cache: false,
            pty: false,
            hash: vec![],
            transparent: false,
        };

        let mut stdout = Vec::new();
        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: Some(&mut stdout),
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
        };

        let res = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            driver.run(make_req(req), &ctoken),
        )
        .await
        .context("driver.run deadlocked under multi-thread runtime")?;
        res?;

        assert!(
            stdout.len() >= payload_bytes,
            "expected >= {payload_bytes} bytes, got {}",
            stdout.len()
        );
        Ok(())
    }

    async fn run_bash_env(
        run_cmd: &str,
        pass_env: BTreeMap<String, String>,
        runtime_pass_env: Vec<String>,
        runtime_env: HashMap<String, String>,
    ) -> anyhow::Result<String> {
        let driver = Driver::new_bash();
        let ctoken = StdCancellationToken::new();
        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                run: vec![run_cmd.to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env,
                runtime_pass_env,
                runtime_env,
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: true,
            disable_remote_cache: false,
            pty: true,
            hash: vec![],
            transparent: false,
        };
        let mut stdout = Vec::new();
        let request_id = "test".to_string();
        let tmp = tempfile::tempdir()?;
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: Some(&mut stdout),
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
        };
        driver.run(make_req(req), &ctoken).await?;
        Ok(String::from_utf8(stdout)?.trim().to_string())
    }

    #[tokio::test]
    async fn test_run_pass_env_injected() -> anyhow::Result<()> {
        let out = run_bash_env(
            "echo $MY_PASS_VAR",
            BTreeMap::from([("MY_PASS_VAR".to_string(), "pass_value".to_string())]),
            vec![],
            HashMap::new(),
        )
        .await?;
        assert_eq!(out, "pass_value");
        Ok(())
    }

    #[tokio::test]
    async fn test_run_runtime_env_injected() -> anyhow::Result<()> {
        let out = run_bash_env(
            "echo $MY_RUNTIME_ENV",
            BTreeMap::new(),
            vec![],
            HashMap::from([(
                "MY_RUNTIME_ENV".to_string(),
                "runtime_env_value".to_string(),
            )]),
        )
        .await?;
        assert_eq!(out, "runtime_env_value");
        Ok(())
    }

    #[tokio::test]
    async fn test_run_runtime_pass_env_injected() -> anyhow::Result<()> {
        unsafe {
            std::env::set_var("RHEPH_TEST_RUNTIME_PASS", "runtime_pass_value");
        }
        let out = run_bash_env(
            "echo $RHEPH_TEST_RUNTIME_PASS",
            BTreeMap::new(),
            vec!["RHEPH_TEST_RUNTIME_PASS".to_string()],
            HashMap::new(),
        )
        .await?;
        assert_eq!(out, "runtime_pass_value");
        Ok(())
    }

    #[tokio::test]
    async fn test_run_env_not_leaked_from_parent() -> anyhow::Result<()> {
        unsafe {
            std::env::set_var("RHEPH_TEST_PARENT_ONLY", "should_not_see_this");
        }
        let out = run_bash_env(
            "echo ${RHEPH_TEST_PARENT_ONLY:-absent}",
            BTreeMap::new(),
            vec![],
            HashMap::new(),
        )
        .await?;
        assert_eq!(out, "absent");
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_pass_env_resolves_value() -> anyhow::Result<()> {
        unsafe {
            std::env::set_var("RHEPH_TEST_PARSE_PASS", "resolved_value");
        }
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let config = HashMap::from([
            (
                "run".to_string(),
                crate::loosespecparser::TargetSpecValue::String("echo".to_string()),
            ),
            (
                "pass_env".to_string(),
                crate::loosespecparser::TargetSpecValue::List(vec![
                    crate::loosespecparser::TargetSpecValue::String(
                        "RHEPH_TEST_PARSE_PASS".to_string(),
                    ),
                ]),
            ),
        ]);
        let res = driver
            .parse(
                crate::engine::driver::ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: std::sync::Arc::new(crate::engine::provider::TargetSpec {
                        addr: Addr::default(),
                        driver: "exec".to_string(),
                        config,
                        labels: vec![],
                        transitive: Default::default(),
                    }),
                },
                &ctoken,
            )
            .await?;
        let def = res.target_def.def::<TargetDef>();
        assert_eq!(
            def.pass_env.get("RHEPH_TEST_PARSE_PASS"),
            Some(&"resolved_value".to_string())
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_pass_env_missing_var_skipped() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let config = HashMap::from([
            (
                "run".to_string(),
                crate::loosespecparser::TargetSpecValue::String("echo".to_string()),
            ),
            (
                "pass_env".to_string(),
                crate::loosespecparser::TargetSpecValue::List(vec![
                    crate::loosespecparser::TargetSpecValue::String(
                        "RHEPH_TEST_DEFINITELY_UNSET_99999".to_string(),
                    ),
                ]),
            ),
        ]);
        let res = driver
            .parse(
                crate::engine::driver::ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: std::sync::Arc::new(crate::engine::provider::TargetSpec {
                        addr: Addr::default(),
                        driver: "exec".to_string(),
                        config,
                        labels: vec![],
                        transitive: Default::default(),
                    }),
                },
                &ctoken,
            )
            .await?;
        let def = res.target_def.def::<TargetDef>();
        assert!(def.pass_env.is_empty());
        Ok(())
    }

    /// Drives `parse()` with the given `extra` config keys merged on top of a
    /// minimal exec spec (just `run`). Returns the resulting EngineTargetDef.
    async fn parse_with(
        extra: HashMap<String, crate::loosespecparser::TargetSpecValue>,
    ) -> anyhow::Result<crate::engine::driver::targetdef::TargetDef> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let mut config = HashMap::from([(
            "run".to_string(),
            crate::loosespecparser::TargetSpecValue::String("echo".to_string()),
        )]);
        config.extend(extra);
        let res = driver
            .parse(
                crate::engine::driver::ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: std::sync::Arc::new(crate::engine::provider::TargetSpec {
                        addr: Addr::default(),
                        driver: "exec".to_string(),
                        config,
                        labels: vec![],
                        transitive: Default::default(),
                    }),
                },
                &ctoken,
            )
            .await?;
        Ok(res.target_def)
    }

    #[tokio::test]
    async fn test_parse_hash_deps_routes_inputs_with_flags() -> anyhow::Result<()> {
        let extra = HashMap::from([(
            "hash_deps".to_string(),
            crate::loosespecparser::TargetSpecValue::List(vec![
                crate::loosespecparser::TargetSpecValue::String("//some:dep".to_string()),
            ]),
        )]);
        let td = parse_with(extra).await?;
        let def = td.def::<TargetDef>();

        // hash_deps must not appear in any pluginexec runtime wiring map.
        assert!(def.dep_group_inputs.is_empty());
        assert!(def.runtime_dep_group_inputs.is_empty());

        // They DO show up as engine inputs (so engine resolves them and folds
        // their hashout into hashin) with hashed=true / runtime=false.
        let hash_dep_input = td
            .inputs
            .iter()
            .find(|i| i.origin_id.starts_with("hash_dep|"))
            .expect("hash_dep input present");
        assert!(hash_dep_input.hashed);
        assert!(!hash_dep_input.runtime);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_runtime_deps_routes_inputs_with_flags() -> anyhow::Result<()> {
        let extra = HashMap::from([(
            "runtime_deps".to_string(),
            crate::loosespecparser::TargetSpecValue::List(vec![
                crate::loosespecparser::TargetSpecValue::String("//some:dep".to_string()),
            ]),
        )]);
        let td = parse_with(extra).await?;
        let def = td.def::<TargetDef>();

        // runtime_deps wire into the runtime-only SRC_*/LIST_* map (excluded
        // from def.hash), and are NOT in the hashed dep_group_inputs map.
        assert!(def.dep_group_inputs.is_empty());
        assert_eq!(def.runtime_dep_group_inputs.len(), 1);

        let runtime_dep_input = td
            .inputs
            .iter()
            .find(|i| i.origin_id.starts_with("runtime_dep|"))
            .expect("runtime_dep input present");
        assert!(!runtime_dep_input.hashed);
        assert!(runtime_dep_input.runtime);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_runtime_deps_excluded_from_def_hash() -> anyhow::Result<()> {
        let base = parse_with(HashMap::new()).await?;
        let with_runtime = parse_with(HashMap::from([(
            "runtime_deps".to_string(),
            crate::loosespecparser::TargetSpecValue::List(vec![
                crate::loosespecparser::TargetSpecValue::String("//some:dep".to_string()),
            ]),
        )]))
        .await?;
        // Adding a `runtime_deps` entry must NOT change the per-target def
        // hash; otherwise the cache key would depend on runtime-only state.
        assert_eq!(base.hash, with_runtime.hash);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_hash_deps_excluded_from_def_hash() -> anyhow::Result<()> {
        // hash_deps are tracked via their hashout (flows into hashin via the
        // engine), not via the per-target def hash. So adding/changing a
        // hash_dep does not change `def.hash` either — invalidation happens
        // through the engine's input-result mixing.
        let base = parse_with(HashMap::new()).await?;
        let with_hash = parse_with(HashMap::from([(
            "hash_deps".to_string(),
            crate::loosespecparser::TargetSpecValue::List(vec![
                crate::loosespecparser::TargetSpecValue::String("//some:dep".to_string()),
            ]),
        )]))
        .await?;
        assert_eq!(base.hash, with_hash.hash);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_deps_change_def_hash() -> anyhow::Result<()> {
        // Plain `deps` are structural — they must invalidate the per-target
        // def hash. (This is the property `runtime_deps` deliberately lacks.)
        let base = parse_with(HashMap::new()).await?;
        let with_deps = parse_with(HashMap::from([(
            "deps".to_string(),
            crate::loosespecparser::TargetSpecValue::List(vec![
                crate::loosespecparser::TargetSpecValue::String("//some:dep".to_string()),
            ]),
        )]))
        .await?;
        assert_ne!(base.hash, with_deps.hash);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_env_value_change_def_hash() -> anyhow::Result<()> {
        // Literal `env` values must invalidate the per-target def hash —
        // changing an env value is a semantic change to the target's input.
        let with_v1 = parse_with(HashMap::from([(
            "env".to_string(),
            crate::loosespecparser::TargetSpecValue::Map(HashMap::from([(
                "FOO".to_string(),
                crate::loosespecparser::TargetSpecValue::String("v1".to_string()),
            )])),
        )]))
        .await?;
        let with_v2 = parse_with(HashMap::from([(
            "env".to_string(),
            crate::loosespecparser::TargetSpecValue::Map(HashMap::from([(
                "FOO".to_string(),
                crate::loosespecparser::TargetSpecValue::String("v2".to_string()),
            )])),
        )]))
        .await?;
        assert_ne!(with_v1.hash, with_v2.hash);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_env_key_change_def_hash() -> anyhow::Result<()> {
        // Changing an env key (not just its value) must also invalidate the
        // def hash.
        let with_foo = parse_with(HashMap::from([(
            "env".to_string(),
            crate::loosespecparser::TargetSpecValue::Map(HashMap::from([(
                "FOO".to_string(),
                crate::loosespecparser::TargetSpecValue::String("v".to_string()),
            )])),
        )]))
        .await?;
        let with_bar = parse_with(HashMap::from([(
            "env".to_string(),
            crate::loosespecparser::TargetSpecValue::Map(HashMap::from([(
                "BAR".to_string(),
                crate::loosespecparser::TargetSpecValue::String("v".to_string()),
            )])),
        )]))
        .await?;
        assert_ne!(with_foo.hash, with_bar.hash);
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_env_added_changes_def_hash() -> anyhow::Result<()> {
        // Adding any env entry where there was none must change the def hash.
        let base = parse_with(HashMap::new()).await?;
        let with_env = parse_with(HashMap::from([(
            "env".to_string(),
            crate::loosespecparser::TargetSpecValue::Map(HashMap::from([(
                "FOO".to_string(),
                crate::loosespecparser::TargetSpecValue::String("v".to_string()),
            )])),
        )]))
        .await?;
        assert_ne!(base.hash, with_env.hash);
        Ok(())
    }

    fn make_tool_binary(
        dir: &std::path::Path,
        name: &str,
        body: &str,
    ) -> anyhow::Result<std::path::PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}"))?;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms)?;
        Ok(path)
    }

    fn make_tool_managed_input(
        origin_id: &str,
        tool_path: &std::path::Path,
        list_dir: &std::path::Path,
    ) -> anyhow::Result<crate::engine::driver_managed::ManagedRunInput> {
        make_tool_managed_input_full(
            origin_id,
            tool_path,
            list_dir,
            crate::htaddr::Addr::default(),
            "",
        )
    }

    fn make_tool_managed_input_with_source(
        origin_id: &str,
        tool_path: &std::path::Path,
        list_dir: &std::path::Path,
        source_addr: crate::htaddr::Addr,
    ) -> anyhow::Result<crate::engine::driver_managed::ManagedRunInput> {
        make_tool_managed_input_full(origin_id, tool_path, list_dir, source_addr, "")
    }

    fn make_tool_managed_input_full(
        origin_id: &str,
        tool_path: &std::path::Path,
        list_dir: &std::path::Path,
        source_addr: crate::htaddr::Addr,
        hashout: &str,
    ) -> anyhow::Result<crate::engine::driver_managed::ManagedRunInput> {
        use crate::engine::driver::{RunInput, inputartifact, outputartifact};
        let list_path = list_dir.join(format!("input_{origin_id}.list"));
        std::fs::write(&list_path, format!("{}\n", tool_path.display()))?;
        Ok(crate::engine::driver_managed::ManagedRunInput {
            input: RunInput {
                artifact: inputartifact::InputArtifact {
                    r#type: inputartifact::Type::Dep,
                    origin_id: origin_id.to_string(),
                    content: Arc::new(outputartifact::OutputArtifact {
                        group: "".to_string(),
                        name: "".to_string(),
                        r#type: outputartifact::Type::Output,
                        content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                            data: vec![],
                            path: "".to_string(),
                            x: false,
                        }),
                        hashout: hashout.to_string(),
                    }),
                },
                origin_id: origin_id.to_string(),
                source_addr,
                filters: vec![],
                annotations: BTreeMap::new(),
            },
            list_path: Some(list_path.clone()),
            unpack_root: list_dir.to_path_buf(),
        })
    }

    fn make_tool_target_def(run: Vec<String>, origin_id: &str, group: &str) -> EngineTargetDef {
        EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                run,
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::from([(
                    group.to_string(),
                    vec![Input {
                        r#ref: crate::engine::driver::TargetAddr::default(),
                        mode: InputMode::Tool,
                        origin_id: origin_id.to_string(),
                        annotations: BTreeMap::from([(
                            "unpack_root".to_string(),
                            "tools".to_string(),
                        )]),
                        hashed: true,
                        runtime: true,
                    }],
                )]),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: true,
            disable_remote_cache: false,
            pty: true,
            hash: vec![],
            transparent: false,
        }
    }

    #[tokio::test]
    async fn test_tool_binary_symlinked_in_bin() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        let tool_path = make_tool_binary(tmp.path(), "mytool", "echo mytool_output")?;
        let origin_id = "tool||0";
        let managed_input = make_tool_managed_input(origin_id, &tool_path, tmp.path())?;
        // exec driver: "mytool" is resolved via the child PATH which will be set to bin_dir
        let target_def = make_tool_target_def(vec!["mytool".to_string()], origin_id, "");

        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
        };
        driver
            .run(
                ManagedRunRequest {
                    sandbox_dir: tmp.path().to_path_buf(),
                    sandbox_ws_dir: tmp.path().to_path_buf(),
                    sandbox_pkg_dir: tmp.path().to_path_buf(),
                    request: req,
                    inputs: vec![managed_input],
                },
                &ctoken,
            )
            .await?;

        let bin_tool = tmp.path().join("bin").join("mytool");
        assert!(bin_tool.exists(), "bin/mytool should exist");
        assert!(
            bin_tool.symlink_metadata()?.file_type().is_symlink(),
            "bin/mytool should be a symlink"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_tool_callable_by_name() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        let tool_path = make_tool_binary(tmp.path(), "mytool", "echo tool_was_called")?;
        let origin_id = "tool||0";
        let managed_input = make_tool_managed_input(origin_id, &tool_path, tmp.path())?;
        let target_def = make_tool_target_def(vec!["mytool".to_string()], origin_id, "");

        let mut stdout = Vec::new();
        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: Some(&mut stdout),
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
        };
        driver
            .run(
                ManagedRunRequest {
                    sandbox_dir: tmp.path().to_path_buf(),
                    sandbox_ws_dir: tmp.path().to_path_buf(),
                    sandbox_pkg_dir: tmp.path().to_path_buf(),
                    request: req,
                    inputs: vec![managed_input],
                },
                &ctoken,
            )
            .await?;

        assert_eq!(String::from_utf8(stdout)?.trim(), "tool_was_called");
        Ok(())
    }

    #[tokio::test]
    async fn test_tool_bin_prepended_to_existing_path() -> anyhow::Result<()> {
        let driver = Driver::new_bash();
        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        let tool_path = make_tool_binary(tmp.path(), "mytool", "echo ok")?;
        let origin_id = "tool||0";
        let managed_input = make_tool_managed_input(origin_id, &tool_path, tmp.path())?;

        let existing_path = "/usr/bin:/bin";
        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                run: vec!["echo $PATH".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::from([(
                    "".to_string(),
                    vec![Input {
                        r#ref: crate::engine::driver::TargetAddr::default(),
                        mode: InputMode::Tool,
                        origin_id: origin_id.to_string(),
                        annotations: BTreeMap::from([(
                            "unpack_root".to_string(),
                            "tools".to_string(),
                        )]),
                        hashed: true,
                        runtime: true,
                    }],
                )]),
                pass_env: BTreeMap::from([("PATH".to_string(), existing_path.to_string())]),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: true,
            disable_remote_cache: false,
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let mut stdout = Vec::new();
        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: Some(&mut stdout),
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
        };
        driver
            .run(
                ManagedRunRequest {
                    sandbox_dir: tmp.path().to_path_buf(),
                    sandbox_ws_dir: tmp.path().to_path_buf(),
                    sandbox_pkg_dir: tmp.path().to_path_buf(),
                    request: req,
                    inputs: vec![managed_input],
                },
                &ctoken,
            )
            .await?;

        let path_out = String::from_utf8(stdout)?;
        let path_out = path_out.trim();
        let bin_dir = tmp.path().join("bin").to_string_lossy().into_owned();
        assert!(
            path_out.starts_with(&bin_dir),
            "PATH should start with bin dir; got: {path_out}"
        );
        assert!(
            path_out.contains(existing_path),
            "PATH should retain existing entries; got: {path_out}"
        );
        Ok(())
    }

    /// End-to-end regression for multi-output tool refs.
    ///
    /// A multi-output tool target (one Output group per program, each with
    /// 1 FilePath) resolves to N `RunInput`s that share one `origin_id`.
    /// `inputs_result_exec` (engine/execute.rs) creates one `RunInput` per
    /// output artifact; the managed bridge then unpacks each one and APPENDS
    /// the produced paths to the same `input_<origin_id>.list`. The tool
    /// symlinker must end up with one symlink per binary in bin/.
    ///
    /// This goes through `ManagedDriverBridge::run` (the real path) with N
    /// real tar artifacts, not pre-built list files, so it exercises the
    /// unpack-then-symlink flow that production hits.
    #[tokio::test]
    async fn test_multi_output_tool_via_bridge() -> anyhow::Result<()> {
        use crate::engine::driver::{Driver as _, RunInput, inputartifact, outputartifact};
        use crate::engine::driver_managed::ManagedDriverBridge;

        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        // Pretend nix produced 4 wrapper scripts; each output group ships one.
        let store_dir = tmp.path().join("store");
        std::fs::create_dir_all(&store_dir)?;
        let names = ["node", "npm", "npx", "yarn"];

        // Build 4 tar artifacts, each containing one file at `pkg/bin/<name>`.
        // Mirrors what ManagedDriverBridge::run_inner packs at the end of run.
        let tar_dir = tmp.path().join("tars");
        std::fs::create_dir_all(&tar_dir)?;
        let origin_id = "tool||0";
        let mut artifacts: Vec<RunInput> = Vec::new();
        for name in names {
            let src = make_tool_binary(&store_dir, name, "echo ok")?;
            let mut tp = crate::hartifactcontent::tar::TarPacker::new();
            tp.create_file(
                src.to_string_lossy().into_owned(),
                format!("pkg/bin/{name}"),
            );
            let tar_path = tar_dir.join(format!("{name}.tar"));
            let f = std::fs::File::create(&tar_path)?;
            tp.pack(f)?;

            artifacts.push(RunInput {
                artifact: inputartifact::InputArtifact {
                    r#type: inputartifact::Type::Dep,
                    origin_id: origin_id.to_string(),
                    content: Arc::new(outputartifact::OutputArtifact {
                        group: name.to_string(),
                        name: format!("{name}.tar"),
                        r#type: outputartifact::Type::Output,
                        content: outputartifact::Content::TarPath(
                            tar_path.to_string_lossy().into_owned(),
                        ),
                        hashout: format!("h_{name}"),
                    }),
                },
                origin_id: origin_id.to_string(),
                source_addr: crate::htaddr::Addr::default(),
                filters: vec![],
                annotations: BTreeMap::from([("unpack_root".to_string(), "tools".to_string())]),
            });
        }

        // Exec target with one declared tool input matching the shared origin_id.
        let target_def = EngineTargetDef {
            addr: Addr::new(
                crate::htpkg::PkgBuf::from("pkg"),
                "consumer".to_string(),
                BTreeMap::new(),
            ),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                run: vec!["true".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::from([(
                    "".to_string(),
                    vec![Input {
                        r#ref: crate::engine::driver::TargetAddr::default(),
                        mode: InputMode::Tool,
                        origin_id: origin_id.to_string(),
                        annotations: BTreeMap::from([(
                            "unpack_root".to_string(),
                            "tools".to_string(),
                        )]),
                        hashed: true,
                        runtime: true,
                    }],
                )]),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: true,
            disable_remote_cache: false,
            pty: false,
            hash: vec![],
            transparent: false,
        };

        let sandbox = tmp.path().join("sandbox");
        std::fs::create_dir_all(&sandbox)?;

        let bridge = ManagedDriverBridge::new_os_for_test(Box::new(Driver::new_bash()));

        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: tmp.path().to_path_buf(),
            inputs: artifacts,
            hashin: "h",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox.clone(),
        };

        bridge.run(req, &ctoken).await?;

        let bin_dir = sandbox.join("bin");
        let listed: Vec<_> = std::fs::read_dir(&bin_dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        for name in names {
            assert!(
                bin_dir.join(name).exists(),
                "bin/{name} missing; bin/ contents: {:?}",
                listed
            );
        }
        Ok(())
    }

    /// Same shape but at the driver level, bypassing the bridge (the bridge
    /// merges N inputs into one list file; this asserts the symlink loop is
    /// correct given that already-merged state).
    #[tokio::test]
    async fn test_multi_output_tool_symlinks_all_binaries() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        // Four real binary files on disk.
        let tool_dir = tmp.path().join("store");
        std::fs::create_dir_all(&tool_dir)?;
        let names = ["node", "npm", "npx", "yarn"];
        let tool_paths: Vec<std::path::PathBuf> = names
            .iter()
            .map(|n| make_tool_binary(&tool_dir, n, "echo ok").expect("make tool"))
            .collect();

        // One shared list file with all 4 paths (mirrors the bridge's append
        // behavior when N RunInputs share an origin_id).
        let origin_id = "tool||0";
        let list_dir = tmp.path().join("ws");
        std::fs::create_dir_all(&list_dir)?;
        let list_path = list_dir.join(format!("input_{origin_id}.list"));
        let mut contents = String::new();
        for p in &tool_paths {
            contents.push_str(&format!("{}\n", p.display()));
        }
        std::fs::write(&list_path, contents)?;

        // N ManagedRunInput, all sharing the same origin_id and list_path —
        // exactly what `inputs_result_exec` + the managed bridge produce for
        // an N-output tool ref.
        use crate::engine::driver::{RunInput, inputartifact, outputartifact};
        let make_managed = || crate::engine::driver_managed::ManagedRunInput {
            input: RunInput {
                artifact: inputartifact::InputArtifact {
                    r#type: inputartifact::Type::Dep,
                    origin_id: origin_id.to_string(),
                    content: Arc::new(outputartifact::OutputArtifact {
                        group: "".to_string(),
                        name: "".to_string(),
                        r#type: outputartifact::Type::Output,
                        content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                            data: vec![],
                            path: "".to_string(),
                            x: false,
                        }),
                        hashout: "".to_string(),
                    }),
                },
                origin_id: origin_id.to_string(),
                source_addr: crate::htaddr::Addr::default(),
                filters: vec![],
                annotations: BTreeMap::from([("unpack_root".to_string(), "tools".to_string())]),
            },
            list_path: Some(list_path.clone()),
            unpack_root: list_dir.clone(),
        };
        let managed_inputs: Vec<_> = (0..names.len()).map(|_| make_managed()).collect();

        let target_def = make_tool_target_def(vec!["true".to_string()], origin_id, "");

        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
        };
        driver
            .run(
                ManagedRunRequest {
                    sandbox_dir: tmp.path().to_path_buf(),
                    sandbox_ws_dir: tmp.path().to_path_buf(),
                    sandbox_pkg_dir: tmp.path().to_path_buf(),
                    request: req,
                    inputs: managed_inputs,
                },
                &ctoken,
            )
            .await?;

        let bin_dir = tmp.path().join("bin");
        for name in names {
            let bin = bin_dir.join(name);
            assert!(
                bin.exists(),
                "bin/{name} must exist (multi-output tool); got dir: {:?}",
                std::fs::read_dir(&bin_dir)
                    .map(|rd| rd.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
                    .unwrap_or_default()
            );
            assert!(
                bin.symlink_metadata()?.file_type().is_symlink(),
                "bin/{name} must be a symlink"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_no_bin_dir_without_tools() -> anyhow::Result<()> {
        let driver = Driver::new_bash();
        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                run: vec!["true".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: true,
            disable_remote_cache: false,
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
        };
        driver.run(make_req(req), &ctoken).await?;

        assert!(
            !tmp.path().join("bin").exists(),
            "bin/ should not be created when no tools"
        );
        Ok(())
    }

    /// Two tool inputs producing the same `bin/<filename>` are silently
    /// deduped at symlink time — first wins, second is skipped. Address-level
    /// uniqueness (no two engine inputs share `(r#ref, group)`) is enforced
    /// upstream by `Sandbox::merge_sandbox`; here we just avoid EEXIST.
    #[tokio::test]
    async fn overlapping_tool_filenames_dedupe_silently() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        std::fs::create_dir_all(&dir_a)?;
        std::fs::create_dir_all(&dir_b)?;
        let tool_a = make_tool_binary(&dir_a, "node", "echo a")?;
        let tool_b = make_tool_binary(&dir_b, "node", "echo b")?;

        let mi_a = make_tool_managed_input("tool|a|0", &tool_a, tmp.path())?;
        let mi_b = make_tool_managed_input("tool|b|0", &tool_b, tmp.path())?;

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                run: vec!["true".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::from([(
                    "".to_string(),
                    vec![
                        Input {
                            r#ref: crate::engine::driver::TargetAddr::default(),
                            mode: InputMode::Tool,
                            origin_id: "tool|a|0".to_string(),
                            annotations: BTreeMap::new(),
                            hashed: true,
                            runtime: true,
                        },
                        Input {
                            r#ref: crate::engine::driver::TargetAddr::default(),
                            mode: InputMode::Tool,
                            origin_id: "tool|b|0".to_string(),
                            annotations: BTreeMap::new(),
                            hashed: true,
                            runtime: true,
                        },
                    ],
                )]),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: true,
            disable_remote_cache: false,
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
        };
        driver
            .run(
                ManagedRunRequest {
                    sandbox_dir: tmp.path().to_path_buf(),
                    sandbox_ws_dir: tmp.path().to_path_buf(),
                    sandbox_pkg_dir: tmp.path().to_path_buf(),
                    request: req,
                    inputs: vec![mi_a, mi_b],
                },
                &ctoken,
            )
            .await?;

        assert!(
            tmp.path().join("bin").join("node").exists(),
            "bin/node must exist after dedup"
        );
        Ok(())
    }

    /// Same source target referenced via two tool groups (e.g. `tools = [t]`
    /// in two different groups) must not surface an overlap — symlink the
    /// destination once and continue.
    #[tokio::test]
    async fn same_source_tool_filename_dedupes_silently() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;

        let tool_path = make_tool_binary(tmp.path(), "node", "echo ok")?;
        let src = Addr::new(
            crate::htpkg::PkgBuf::from("pkg"),
            "node_tool".to_string(),
            BTreeMap::new(),
        );

        // Two ManagedRunInputs from the same source target, distinct
        // origin_ids (different tool groups).
        let mi_a =
            make_tool_managed_input_with_source("tool|g1|0", &tool_path, tmp.path(), src.clone())?;
        let mi_b = make_tool_managed_input_with_source("tool|g2|0", &tool_path, tmp.path(), src)?;

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                run: vec!["true".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::from([
                    (
                        "g1".to_string(),
                        vec![Input {
                            r#ref: crate::engine::driver::TargetAddr::default(),
                            mode: InputMode::Tool,
                            origin_id: "tool|g1|0".to_string(),
                            annotations: BTreeMap::new(),
                            hashed: true,
                            runtime: true,
                        }],
                    ),
                    (
                        "g2".to_string(),
                        vec![Input {
                            r#ref: crate::engine::driver::TargetAddr::default(),
                            mode: InputMode::Tool,
                            origin_id: "tool|g2|0".to_string(),
                            annotations: BTreeMap::new(),
                            hashed: true,
                            runtime: true,
                        }],
                    ),
                ]),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
            }),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: true,
            disable_remote_cache: false,
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
        };
        driver
            .run(
                ManagedRunRequest {
                    sandbox_dir: tmp.path().to_path_buf(),
                    sandbox_ws_dir: tmp.path().to_path_buf(),
                    sandbox_pkg_dir: tmp.path().to_path_buf(),
                    request: req,
                    inputs: vec![mi_a, mi_b],
                },
                &ctoken,
            )
            .await?;

        assert!(
            tmp.path().join("bin").join("node").exists(),
            "bin/node must exist after dedup",
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_run_creates_output_dirs() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                run: vec!["true".to_string()],
                dep_group_inputs: BTreeMap::new(),
                runtime_dep_group_inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                tool_group_inputs: BTreeMap::new(),
                pass_env: BTreeMap::new(),
                runtime_pass_env: vec![],
                runtime_env: HashMap::new(),
            }),
            inputs: vec![],
            outputs: vec![
                Output {
                    group: "file".to_string(),
                    paths: vec![Path {
                        content: Content::FilePath("nested/dir/out.txt".to_string()),
                        codegen_tree: CodegenMode::None,
                        collect: false,
                    }],
                },
                Output {
                    group: "dir".to_string(),
                    paths: vec![Path {
                        content: Content::DirPath("a/b/c".to_string()),
                        codegen_tree: CodegenMode::None,
                        collect: false,
                    }],
                },
                Output {
                    group: "glob".to_string(),
                    paths: vec![Path {
                        content: Content::Glob("never/created/**/*.txt".to_string()),
                        codegen_tree: CodegenMode::None,
                        collect: false,
                    }],
                },
            ],
            support_files: vec![],
            cache: true,
            disable_remote_cache: false,
            pty: true,
            hash: vec![],
            transparent: false,
        };

        let request_id = "test".to_string();
        let tmp = tempfile::tempdir()?;

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: tmp.path().to_path_buf(),
        };

        driver.run(make_req(req), &ctoken).await?;

        assert!(
            tmp.path().join("nested/dir").is_dir(),
            "FilePath parent must be created",
        );
        assert!(
            !tmp.path().join("nested/dir/out.txt").exists(),
            "FilePath itself must not be created",
        );
        assert!(tmp.path().join("a/b/c").is_dir(), "DirPath must be created",);
        assert!(
            !tmp.path().join("never").exists(),
            "Glob must not trigger dir creation",
        );
        Ok(())
    }
}
