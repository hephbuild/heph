mod spec;

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::io::BufRead;
use std::process::Stdio;
use tokio::process::Command;
use async_trait::async_trait;
use tokio::io;
use crate::engine;
use crate::engine::driver::{ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest, ParseResponse, TargetAddr};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;
use crate::engine::driver::targetdef::{Input, InputMode, Output, TargetDef as EngineTargetDef};
use crate::engine::driver::targetdef::path::{CodegenMode, Content, Path};
use crate::engine::driver_managed::{ManagedRunRequest, ManagedRunResponse};
use crate::hasync::Cancellable;

pub struct Driver {
    name: String,
    wrap_run: fn(&Vec<String>) -> Vec<String>,
}

#[derive(Hash)]
struct TargetDef {
    pub run: Vec<String>,
    pub dep_group_inputs: BTreeMap<String, Vec<Input>>,
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
        vec!["--norc".to_string()]
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
        Self{
            name: "exec".to_string(),
            wrap_run: |run| run.clone()
        }
    }
    pub fn new_bash() -> Self {
        Self{
            name: "bash".to_string(),
            wrap_run: |run| {
                bash_args_public(run.join("\n").as_str(), vec![])
            }
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
                let _ = log.lock().await.write_all(&buf[..n]).await;
                if let Some(ref mut out) = sink {
                    let _ = out.write_all(&buf[..n]).await;
                }
            }
        }
    }
}

#[async_trait]
impl engine::driver_managed::ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse{
            name: self.name.clone(),
        })
    }

    async fn parse(&self, req: ParseRequest, _ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<ParseResponse> {
        let spec = spec::TargetSpec::from(req.target_spec.config)?;

        let pkg = req.target_spec.addr.package.clone();
        let dep_inputs = spec.deps.into_iter().flat_map(|(k, v)| {
            let pkg = pkg.clone();
            v.into_iter().enumerate().map(move |(i, v)| -> anyhow::Result<(String, Input)> {
                Ok((k.parse()?, Input{
                    r#ref: TargetAddr::parse(&v, &pkg)?,
                    mode: InputMode::Standard,
                    origin_id: format!("dep|{}|{}", k, i),
                }))
            })
        }).collect::<anyhow::Result<Vec<_>>>()?;

        let mut dep_group_inputs: BTreeMap<String, Vec<Input>> = BTreeMap::new();
        for (group, input) in &dep_inputs {
            dep_group_inputs.entry(group.clone()).or_default().push(input.clone());
        }

        let def = TargetDef{
            run: spec.run,
            dep_group_inputs,
        };

        let hash = {
            let mut h = Xxh3Default::new();
            def.hash(&mut h);

            format!("{:x}", h.digest()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: EngineTargetDef{
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels,
                raw_def: Arc::new(def),
                inputs: dep_inputs.into_iter().map(|(_, v)| v).collect(),
                outputs: spec.outputs.iter().map(|(k, v)| Output{
                    group: k.clone(),
                    paths: v.iter().map(|path| Path{
                        content: {
                            if ["*", "?", "["].iter().any(|&p| path.contains(p)) { // TODO: this sucks, but its easy for now
                                Content::Glob(path.clone())
                            } else if path.ends_with("/") {
                                Content::DirPath(path.clone())
                            } else {
                                Content::FilePath(path.clone())
                            }
                        },
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }).collect(),
                }).collect(),
                support_files: vec![],
                cache: spec.cache.local,
                disable_remote_cache: !spec.cache.remote,
                pty: true,
                hash,
            }
        })
    }

    async fn apply_transitive(&self, req: ApplyTransitiveRequest, _ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<ApplyTransitiveResponse> {
        Ok(ApplyTransitiveResponse{
            target_def: req.target_def,
        })
    }

    async fn run<'a>(&self, req: ManagedRunRequest<'a>, ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<ManagedRunResponse> {
        let rreq = req.request;
        let def = rreq.target.def::<TargetDef>();

        let run = (self.wrap_run)(&def.run);

        if run.is_empty() {
            anyhow::bail!("`run` is empty")
        }

        let mut env = HashMap::<String, String>::new();

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
                        if !entry.is_empty() { entry.push(' '); }
                        entry.push_str(p);
                    }
                }
            }
        }

        for (group, inputs) in &def.dep_group_inputs {
            let key = if group.is_empty() {
                "SRC".to_string()
            } else {
                format!("SRC_{}", group.to_uppercase())
            };
            let entry = env.entry(key).or_default();
            for input in inputs {
                let Some(managed) = req.inputs.iter().find(|m| m.input.origin_id == input.origin_id) else { continue };
                let file = std::fs::File::open(&managed.list_path)?;
                for line in std::io::BufReader::new(file).lines() {
                    let line = line?;
                    if line.is_empty() { continue; }
                    if !entry.is_empty() { entry.push(' '); }
                    entry.push_str(&line);
                }
            }
        }

        env.retain(|_, v| !v.is_empty());

        let output_log = tempfile::NamedTempFile::new()?;
        let output_log_path = output_log.path().to_path_buf();
        let output_log_file = Arc::new(tokio::sync::Mutex::new(
            tokio::fs::File::from_std(output_log.reopen()?)
        ));

        let mut child = Command::new(&run[0])
            .kill_on_drop(true)
            .args(&run[1..])
            .env_clear()
            .envs(env)
            .current_dir(req.sandbox_pkg_dir)
            .stdin(if rreq.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let child_stdin = child.stdin.take();
        let child_stdout = child.stdout.take();
        let child_stderr = child.stderr.take();

        use tokio::io::AsyncWriteExt;

        let stdin_fut = async {
            if let (Some(mut req_stdin), Some(mut child_stdin)) = (rreq.stdin, child_stdin) {
                let _ = io::copy(&mut req_stdin, &mut child_stdin).await;
                let _ = child_stdin.shutdown().await;
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
                let status = res?;

                if !status.success() {
                    let log = std::fs::read_to_string(&output_log_path).unwrap_or_default();
                    anyhow::bail!("process exited with status: {}\n{}", status, log)
                }
            }
        }

        Ok(ManagedRunResponse{
            artifacts: vec![],
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
        let cwd = std::env::current_dir().unwrap();
        ManagedRunRequest {
            request,
            sandbox_dir: cwd.clone(),
            sandbox_ws_dir: cwd.clone(),
            sandbox_pkg_dir: cwd,
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

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string(),
            inputs: vec![],
            hashin: &"".to_string(),
            stdin: None,
            stdout: Some(&mut stdout),
            stderr: None,
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

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string(),
            inputs: vec![],
            hashin: &"".to_string(),
            stdin: Some(&mut stdin),
            stdout: Some(&mut stdout),
            stderr: None,
        };

        // Use a timeout to detect the hang
        let _res = tokio::time::timeout(std::time::Duration::from_secs(1), driver.run(make_req(req), &ctoken)).await?;

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

        // Use a pipe that will never resolve to simulate a hang
        let (mut reader, _writer) = io::duplex(64);

        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: "".to_string(),
            inputs: vec![],
            hashin: &"".to_string(),
            stdin: Some(&mut reader),
            stdout: None,
            stderr: None,
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
}
