mod spec;

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
use anyhow::Context;
use async_trait::async_trait;
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::io::{BufRead, Write};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io;
use tokio::process::Command;
use xxhash_rust::xxh3::Xxh3Default;

pub struct Driver {
    name: String,
    wrap_run: fn(&Vec<String>) -> Vec<String>,
}

#[derive(Clone)]
struct TargetDef {
    pub run: Vec<String>,
    pub dep_group_inputs: BTreeMap<String, Vec<Input>>,
    pub tool_group_inputs: BTreeMap<String, Vec<Input>>,
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
            wrap_run: |run| run.clone(),
        }
    }
    pub fn new_bash() -> Self {
        Self {
            name: "bash".to_string(),
            wrap_run: |run| bash_args_public(run.join("\n").as_str(), vec![]),
        }
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
        let spec = spec::TargetSpec::from(req.target_spec.config)?;

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
            pass_env,
            runtime_pass_env: spec.runtime_pass_env,
            runtime_env: spec.runtime_env,
        };

        let hash = {
            let mut h = Xxh3Default::new();
            def.hash(&mut h);

            format!("{:x}", h.digest()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: EngineTargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels,
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
                            .map(|path| Path {
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
                                codegen_tree: CodegenMode::None,
                                collect: true,
                            })
                            .collect(),
                    })
                    .collect(),
                support_files: vec![],
                cache: spec.cache.local,
                disable_remote_cache: !spec.cache.remote,
                pty: true,
                hash,
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
            let mut h = Xxh3Default::new();
            xdef.hash(&mut h);
            format!("{:x}", h.digest()).into_bytes()
        };

        def.set_def(xdef);

        Ok(ApplyTransitiveResponse { target_def: def })
    }

    async fn run<'a>(
        &self,
        req: ManagedRunRequest<'a>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let rreq = req.request;
        let def = rreq.target.def::<TargetDef>();

        let run = (self.wrap_run)(&def.run);

        if run.is_empty() {
            anyhow::bail!("`run` is empty")
        }

        let mut env = HashMap::<String, String>::new();
        env.insert(
            "PATH".to_string(),
            [
                "/nix/var/nix/profiles/default/bin", // TODO: figure out how to make plugins provide that
                "/usr/local/bin",
                "/usr/bin",
                "/bin",
            ]
            .join(":"),
        );

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
                        entry.push_str(p);
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
                std::fs::File::create(path)?
            };
            let entry = env.entry(src_key).or_default();

            for input in inputs {
                for m in req
                    .inputs
                    .iter()
                    .filter(|m| m.input.origin_id == input.origin_id)
                {
                    let managed_list_f =
                        std::fs::File::open(&m.list_path).with_context(|| "open dep list file")?;
                    for line in std::io::BufReader::new(managed_list_f).lines() {
                        let line = line?;
                        if line.is_empty() {
                            continue;
                        }

                        if !entry.is_empty() {
                            entry.push(' ');
                        }
                        entry.push_str(&line);

                        list_f.write_all(line.as_bytes())?;
                        list_f.write_all("\n".as_bytes())?;
                    }
                }
            }
        }

        let tool_bin_dir = if !def.tool_group_inputs.is_empty() {
            let bin_dir = req.sandbox_dir.join("bin");
            std::fs::create_dir_all(&bin_dir)?;

            for inputs in def.tool_group_inputs.values() {
                for input in inputs {
                    for m in req
                        .inputs
                        .iter()
                        .filter(|m| m.input.origin_id == input.origin_id)
                    {
                        let list_f = std::fs::File::open(&m.list_path)?;
                        let file_path = std::io::BufReader::new(list_f)
                            .lines()
                            .find(|l| l.as_ref().is_ok_and(|s| !s.is_empty()))
                            .ok_or_else(|| {
                                anyhow::anyhow!("tool '{}' produced no files", input.origin_id)
                            })??;

                        let filename =
                            std::path::Path::new(&file_path)
                                .file_name()
                                .ok_or_else(|| {
                                    anyhow::anyhow!("tool file path has no filename: {}", file_path)
                                })?;
                        let bin_path = bin_dir.join(filename);
                        #[cfg(unix)]
                        std::os::unix::fs::symlink(&file_path, &bin_path)?;
                        #[cfg(not(unix))]
                        std::fs::copy(&file_path, &bin_path)?;
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

        // run is guaranteed non-empty by the bail! above, so [0] and [1..] are safe
        #[expect(
            clippy::indexing_slicing,
            reason = "run non-empty guaranteed by bail! check above"
        )]
        let mut child = Command::new(&run[0])
            .args(&run[1..])
            .kill_on_drop(true)
            .env_clear()
            .envs(env)
            .current_dir(req.sandbox_pkg_dir)
            .stdin(if rreq.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| "spawn child process")?;

        let child_stdin = child.stdin.take();
        let child_stdout = child.stdout.take();
        let child_stderr = child.stderr.take();

        use tokio::io::AsyncWriteExt;

        let stdin_fut = async {
            if let (Some(mut req_stdin), Some(mut child_stdin)) = (rreq.stdin, child_stdin) {
                // Intentionally ignore errors: stdin copy failure is non-fatal; process exit code is authoritative
                drop(io::copy(&mut req_stdin, &mut child_stdin).await);
                drop(child_stdin.shutdown().await);
            }
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
                tokio::join!(stdin_fut, stdout_fut, stderr_fut, child.wait()).3
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

    fn make_req(request: RunRequest) -> ManagedRunRequest {
        let path = request.sandbox_dir.clone();
        ManagedRunRequest {
            request,
            sandbox_dir: path.clone(),
            sandbox_ws_dir: path.clone(),
            sandbox_pkg_dir: path,
            inputs: vec![],
        }
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
        };

        let mut stdout = Vec::new();
        let request_id = "test-request".to_string();
        let tmp = tempfile::tempdir()?;

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: &"".to_string(),
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
            hashin: &"".to_string(),
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
            hashin: &"".to_string(),
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
        };
        let mut stdout = Vec::new();
        let request_id = "test".to_string();
        let tmp = tempfile::tempdir()?;
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: &"".to_string(),
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
                    target_spec: crate::engine::provider::TargetSpec {
                        addr: Addr::default(),
                        driver: "exec".to_string(),
                        config,
                        labels: vec![],
                        transitive: Default::default(),
                    },
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
                    target_spec: crate::engine::provider::TargetSpec {
                        addr: Addr::default(),
                        driver: "exec".to_string(),
                        config,
                        labels: vec![],
                        transitive: Default::default(),
                    },
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
            },
            list_path,
        })
    }

    fn make_tool_target_def(run: Vec<String>, origin_id: &str, group: &str) -> EngineTargetDef {
        EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                run,
                dep_group_inputs: BTreeMap::new(),
                tool_group_inputs: BTreeMap::from([(
                    group.to_string(),
                    vec![Input {
                        r#ref: crate::engine::driver::TargetAddr::default(),
                        mode: InputMode::Tool,
                        origin_id: origin_id.to_string(),
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
            hashin: &"".to_string(),
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
            hashin: &"".to_string(),
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
                tool_group_inputs: BTreeMap::from([(
                    "".to_string(),
                    vec![Input {
                        r#ref: crate::engine::driver::TargetAddr::default(),
                        mode: InputMode::Tool,
                        origin_id: origin_id.to_string(),
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
        };

        let mut stdout = Vec::new();
        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: &"".to_string(),
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
        };

        let request_id = "test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string().into(),
            inputs: vec![],
            hashin: &"".to_string(),
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
