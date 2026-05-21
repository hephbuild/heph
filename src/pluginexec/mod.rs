mod pty;
mod spec;

use crate::debug_hash::DebugHasher;
use crate::engine;
use crate::engine::driver::sandbox::EnvValue;
use crate::engine::driver::targetdef::path::{Content, Path};
use crate::engine::driver::targetdef::{Input, InputMode, Output, TargetDef as EngineTargetDef};
use crate::engine::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr, outputartifact,
};
use crate::engine::driver_managed::{ManagedRunRequest, ManagedRunResponse};
use crate::hasync::Cancellable;
use anyhow::Context;
use async_trait::async_trait;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, Write};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io;
use tokio::process::Command;
use xxhash_rust::xxh3::Xxh3Default;

const SHELL_INIT_SH: &str = include_str!("./init.sh");

pub struct Driver {
    name: String,
    /// PATH the driver injects into target processes. Empty falls back to a hardcoded default.
    search_path: Vec<String>,
    wrap_run: fn(&[String]) -> Vec<String>,
    wrap_run_shell: fn(&std::path::Path, &[String]) -> anyhow::Result<Vec<String>>,
}

#[derive(Clone, serde::Serialize)]
struct TargetDef {
    pub run: Vec<String>,
    pub dep_group_inputs: BTreeMap<String, Vec<Input>>,
    pub tool_group_inputs: BTreeMap<String, Vec<Input>>,
    pub env: BTreeMap<String, String>,
    pub pass_env: BTreeMap<String, String>,
    pub runtime_pass_env: Vec<String>,
    pub runtime_env: HashMap<String, String>,
}

impl Hash for TargetDef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.run.hash(state);
        self.dep_group_inputs.hash(state);
        self.tool_group_inputs.hash(state);
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

pub fn bash_args_public(cmd: &str, termargs: Vec<String>) -> Vec<String> {
    let mut args = bash_args(
        vec![
            "-u".to_string(),
            "-e".to_string(),
            "-c".to_string(),
            cmd.to_string(),
        ],
        vec!["--norc".to_string()],
    );

    if termargs.is_empty() {
        args
    } else {
        // https://unix.stackexchange.com/a/144519
        // We push "bash" as a placeholder for $0 before appending termargs
        args.push("bash".to_string());
        args.extend(termargs);
        args
    }
}

impl Driver {
    pub fn new_exec() -> Self {
        Self {
            name: "exec".to_string(),
            search_path: vec![],
            wrap_run: |run| run.to_vec(),
            wrap_run_shell: |sandbox_dir, run| bash_args_shell(sandbox_dir, &[run.join(" ")]),
        }
    }
    pub fn new_bash() -> Self {
        Self {
            name: "bash".to_string(),
            search_path: vec![],
            wrap_run: |run| bash_args_public(run.join("\n").as_str(), vec![]),
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
}

fn decode_path(opts: &crate::engine::config_file::Options) -> anyhow::Result<Vec<String>> {
    crate::engine::config_file::deny_unknown("exec/bash driver", opts, &["path"])?;
    Ok(
        crate::engine::config_file::decode_opt(opts, "exec/bash driver", "path")?
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
    log: Arc<tokio::sync::Mutex<tokio::fs::File>>,
    mut sink: Option<&mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Some(mut source) = source else { return };
    let mut buf = vec![0u8; 8192];
    loop {
        match source.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                // n is guaranteed <= buf.len() by AsyncRead::read contract
                #[expect(
                    clippy::indexing_slicing,
                    reason = "n guaranteed <= buf.len() by AsyncRead contract"
                )]
                let slice = &buf[..n];
                // Intentionally ignore write errors in the tee path; the process exit code is the authority
                drop(log.lock().await.write_all(slice).await);
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
        let dep_inputs = spec
            .deps
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
                                mode: InputMode::Standard,
                                origin_id: format!("dep|{}|{}", k, i),
                                annotations: BTreeMap::new(),
                            },
                        ))
                    })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let mut dep_group_inputs: BTreeMap<String, Vec<Input>> = BTreeMap::new();
        for (group, input) in &dep_inputs {
            dep_group_inputs
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
            tool_group_inputs,
            env: spec.env.into_iter().collect(),
            pass_env,
            runtime_pass_env: spec.runtime_pass_env,
            runtime_env: spec.runtime_env,
        };

        let hash = {
            let mut h = DebugHasher::new(
                Xxh3Default::new(),
                &format!("exec_def_{}", req.target_spec.addr.format()),
            );
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
                    .chain(tool_inputs.into_iter().map(|(_, v)| v))
                    .collect(),
                outputs: spec
                    .outputs
                    .iter()
                    .map(|(k, v)| Output {
                        group: k.clone(),
                        paths: v
                            .iter()
                            .map(|path| {
                                let path = if pkg.is_empty() {
                                    path.clone()
                                } else {
                                    format!("{}/{}", pkg, path)
                                };
                                Path {
                                    content: {
                                        if ["*", "?", "["].iter().any(|&p| path.contains(p)) {
                                            // TODO: this sucks, but its easy for now
                                            Content::Glob(path.clone())
                                        } else if path.ends_with("/") {
                                            Content::DirPath(path.clone())
                                        } else {
                                            Content::FilePath(path.clone())
                                        }
                                    },
                                    codegen_tree: spec.codegen.clone(),
                                    collect: true,
                                }
                            })
                            .collect(),
                    })
                    .collect(),
                support_files: vec![],
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
            };
            xdef.dep_group_inputs
                .entry(dep.group)
                .or_default()
                .push(input.clone());
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
            let mut h = DebugHasher::new(
                Xxh3Default::new(),
                &format!("exec_def_tr_{}", def.addr.format()),
            );
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
        let rreq = req.request;
        let def = rreq.target.def::<TargetDef>();

        let run = {
            if shell {
                (self.wrap_run_shell)(&rreq.sandbox_dir, &def.run)?
            } else {
                (self.wrap_run)(&def.run)
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
                "/nix/var/nix/profiles/default/bin", // TODO: figure out how to make plugins provide that
                "/usr/local/bin",
                "/usr/bin",
                "/bin",
            ]
            .join(":")
        } else {
            self.search_path.join(":")
        };
        env.insert("PATH".to_string(), path_value);

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
                    }
                }
            }
        }

        for (group, inputs) in &def.dep_group_inputs {
            let src_key = if group.is_empty() {
                "SRC".to_string()
            } else {
                format!("SRC_{}", group.to_uppercase())
            };

            let mut list_f = {
                let path = req.sandbox_dir.join(format!("dep_{}.list", group));
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
                if let Some(m) = req
                    .inputs
                    .iter()
                    .find(|m| m.input.origin_id == input.origin_id)
                {
                    let managed_list_f = std::fs::File::open(&m.list_path).with_context(|| {
                        format!(
                            "open dep list file {:?} (origin_id={})",
                            m.list_path, input.origin_id
                        )
                    })?;
                    for line in std::io::BufReader::new(managed_list_f).lines() {
                        let line = line.with_context(|| {
                            format!("read line from dep list {:?}", m.list_path)
                        })?;
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

            for (group, inputs) in &def.tool_group_inputs {
                for input in inputs {
                    // Multi-output tool refs produce N RunInputs sharing one
                    // origin_id; the bridge appends every output's file path
                    // to the same `input_<origin_id>.list`. Read that list
                    // exactly once and symlink each entry into bin/. Validated
                    // by engine/link.rs to be 1 FilePath per output, so all
                    // entries here are distinct binaries.
                    let Some(m) = req
                        .inputs
                        .iter()
                        .find(|m| m.input.origin_id == input.origin_id)
                    else {
                        continue;
                    };
                    let list_f = std::fs::File::open(&m.list_path).with_context(|| {
                        format!(
                            "open tool list {:?} (group={group}, origin_id={})",
                            m.list_path, input.origin_id
                        )
                    })?;
                    let mut any = false;
                    for line in std::io::BufReader::new(list_f).lines() {
                        let file_path = line.with_context(|| {
                            format!("read line from tool list {:?}", m.list_path)
                        })?;
                        if file_path.is_empty() {
                            continue;
                        }
                        any = true;
                        let filename =
                            std::path::Path::new(&file_path)
                                .file_name()
                                .ok_or_else(|| {
                                    anyhow::anyhow!("tool file path has no filename: {}", file_path)
                                })?;
                        let bin_path = bin_dir.join(filename);
                        #[cfg(unix)]
                        std::os::unix::fs::symlink(&file_path, &bin_path).with_context(|| {
                            let existing = std::fs::read_link(&bin_path)
                                .map(|p| format!("symlink->{:?}", p))
                                .or_else(|_| {
                                    std::fs::symlink_metadata(&bin_path)
                                        .map(|m| format!("{:?}", m.file_type()))
                                })
                                .unwrap_or_else(|_| "<unreadable>".to_string());
                            format!(
                                "symlink tool {file_path:?} -> {bin_path:?} (group={group}, \
                                 origin_id={}, existing_at_dest={existing})",
                                input.origin_id
                            )
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
        let output_log_file = Arc::new(tokio::sync::Mutex::new(tokio::fs::File::from_std(
            output_log,
        )));

        // Shell mode runs the child attached to a freshly-allocated PTY so bash
        // sees a real terminal and runs interactively. The parent forwards
        // stdin/stdout via the PTY master through the same tee_stream paths
        // used by the non-shell path.
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
            (&run[0], &run[1..])
        };
        let mut cmd = Command::new(program);
        cmd.args(args)
            .kill_on_drop(true)
            .env_clear()
            .envs(env)
            .current_dir(req.sandbox_pkg_dir);

        if let Some((master, slave)) = &pty_pair {
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
            cmd.stdin(Stdio::from(stdin_fd))
                .stdout(Stdio::from(stdout_fd))
                .stderr(Stdio::from(stderr_fd));
            #[expect(
                clippy::multiple_unsafe_ops_per_block,
                reason = "pre_exec + setsid + ioctl must all run inside the same unsafe context"
            )]
            // SAFETY: pre_exec runs between fork and exec; the closure body
            // only invokes async-signal-safe syscalls (setsid + ioctl) — both
            // documented as safe to call in that window.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        } else {
            cmd.stdin(if rreq.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        }

        let mut child = cmd.spawn().with_context(|| "spawn child process")?;

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

        type BoxedReader = Box<dyn tokio::io::AsyncRead + Unpin + Send>;
        type BoxedWriter = Box<dyn tokio::io::AsyncWrite + Unpin + Send>;
        let (child_stdin, child_stdout, child_stderr): (
            Option<BoxedWriter>,
            Option<BoxedReader>,
            Option<BoxedReader>,
        ) = if let Some(master) = pty_master {
            let read_fd = master.try_clone().context("dup pty master for read")?;
            let reader = pty::AsyncPty::new(read_fd).context("async pty reader")?;
            let writer = pty::AsyncPty::new(master).context("async pty writer")?;
            (Some(Box::new(writer)), Some(Box::new(reader)), None)
        } else {
            (
                child.stdin.take().map(|x| Box::new(x) as BoxedWriter),
                child.stdout.take().map(|x| Box::new(x) as BoxedReader),
                child.stderr.take().map(|x| Box::new(x) as BoxedReader),
            )
        };

        use tokio::io::AsyncWriteExt;

        // Signal that cancels the stdin pump once the child has exited. Without
        // it, shell mode would deadlock waiting on a parent-stdin read that
        // nothing intends to satisfy. The client is expected to provide a
        // cancellable stdin (e.g. a TtyReader), but we cancel here too in case
        // the source is `tokio::io::stdin()`-like and the runtime would
        // otherwise wait on a parked blocking thread.
        let (stdin_cancel_tx, stdin_cancel_rx) = tokio::sync::oneshot::channel::<()>();

        let stdin_fut = async move {
            let Some(mut child_stdin) = child_stdin else {
                return;
            };
            let Some(mut req_stdin) = rreq.stdin else {
                return;
            };
            tokio::select! {
                _ = stdin_cancel_rx => {}
                // Intentionally ignore errors: stdin copy failure is non-fatal; process exit code is authoritative
                _ = io::copy(&mut req_stdin, &mut child_stdin) => {}
            }
            drop(child_stdin.shutdown().await);
        };

        let stdout_fut = tee_stream(child_stdout, Arc::clone(&output_log_file), rreq.stdout);
        let stderr_fut = tee_stream(child_stderr, Arc::clone(&output_log_file), rreq.stderr);

        tokio::select! {
            _ = ctoken.cancelled() => {
                child.start_kill()?;
                child.wait().await?;
                anyhow::bail!("cancelled")
            },
            res = async {
                let io = async { tokio::join!(stdin_fut, stdout_fut, stderr_fut) };
                // Poll the child status on a tick instead of relying on
                // `child.wait()`'s SIGCHLD waker, which doesn't always fire
                // when the child has called setsid + TIOCSCTTY (PTY session
                // leader). 25ms keeps shell-exit latency invisible.
                let wait_fut = async {
                    let status = loop {
                        match child.try_wait() {
                            Ok(Some(s)) => break Ok(s),
                            Ok(None) => {
                                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                            }
                            Err(e) => break Err(e),
                        }
                    };
                    // Cancel the stdin pump as soon as the child is reaped so
                    // the io join can finish in the io-wins arm below.
                    _ = stdin_cancel_tx.send(());
                    status
                };
                tokio::pin!(io, wait_fut);
                tokio::select! {
                    s = &mut wait_fut => {
                        // Race the io pumps to drain whatever's still buffered.
                        // PTY master EOF detection through kqueue is unreliable
                        // on macOS, so cap the wait — anything left will be lost,
                        // but in shell mode the user has already seen the output
                        // streamed live.
                        _ = tokio::time::timeout(
                            std::time::Duration::from_millis(50),
                            &mut io,
                        ).await;
                        s
                    }
                    _ = &mut io => (&mut wait_fut).await,
                }
            } => {
                let status = res.with_context(|| "wait for child process")?;

                if !status.success() {
                    let log = std::fs::read_to_string(&output_log_path).unwrap_or_default();
                    anyhow::bail!("process exited with status: {}\n{}", status, log)
                }
            }
        }

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
                        hashout: "".to_string(),
                    }),
                },
                origin_id: origin_id.to_string(),
                source_addr: crate::htaddr::Addr::default(),
                filters: vec![],
                annotations: BTreeMap::new(),
            },
            list_path: list_path.clone(),
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

        let bridge = ManagedDriverBridge {
            home: tmp.path().to_path_buf(),
            driver: Box::new(Driver::new_bash()),
        };

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
            list_path: list_path.clone(),
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
}
