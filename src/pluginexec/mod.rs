mod spec;

use std::process::Stdio;
use tokio::process::Command;
use async_trait::async_trait;
use tokio::io;
use crate::engine;
use crate::engine::driver::{ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest, ParseResponse, RunRequest, RunResponse};
use std::sync::Arc;
use crate::engine::driver::targetdef::TargetDef as EngineTargetDef;
use crate::hasync::Cancellable;

pub struct Driver {
    name: String,
    wrap_run: fn(Vec<String>) -> Vec<String>,
}

struct TargetDef {
    pub run: Vec<String>,
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
    pub fn new_exec() -> Driver {
        Driver{
            name: "exec".to_string(),
            wrap_run: |run| run
        }
    }
    pub fn new_bash() -> Driver {
        Driver{
            name: "bash".to_string(),
            wrap_run: |run| {
                bash_args_public(run.join("\n").as_str(), vec![])
            }
        }
    }
}

#[async_trait]
impl engine::driver::Driver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse{
            name: self.name.clone(),
        })
    }

    async fn parse(&self, req: ParseRequest, _ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<ParseResponse> {
        let spec = spec::TargetSpec::from(req.target_spec.config)?;

        Ok(ParseResponse {
            target_def: EngineTargetDef{
                addr: req.target_spec.addr,
                labels: req.target_spec.labels,
                raw_def: Arc::new(TargetDef{
                    run: spec.run,
                }),
                inputs: vec![],
                outputs: vec![],
                support_files: vec![],
                cache: spec.cache.local,
                disable_remote_cache: !spec.cache.remote,
                pty: true,
                hash: vec![],
            }
        })
    }

    async fn apply_transitive(&self, req: ApplyTransitiveRequest, _ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<ApplyTransitiveResponse> {
        Ok(ApplyTransitiveResponse{
            target_def: req.target_def,
        })
    }

    async fn run<'a>(&self, req: RunRequest<'a>, ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<RunResponse> {
        let def = req.target.def::<TargetDef>();

        let run = (self.wrap_run)(def.run.clone());

        if run.is_empty() {
            return Ok(RunResponse {
                artifacts: vec![],
            })
        }

        let env: Vec<(&str, &str)> = vec![
            // ("OUT", "out") // TODO
        ];

        let mut child = Command::new(&run[0])
            .kill_on_drop(true)
            .args(&run[1..])
            .envs(env)
            .stdin(if req.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(if req.stdout.is_some() { Stdio::piped() } else { Stdio::null() })
            .stderr(if req.stderr.is_some() { Stdio::piped() } else { Stdio::null() })
            .spawn()?;

        let child_stdin = child.stdin.take();
        let child_stdout = child.stdout.take();
        let child_stderr = child.stderr.take();

        use tokio::io::AsyncWriteExt;

        let stdin_fut = async {
            if let (Some(mut req_stdin), Some(mut child_stdin)) = (req.stdin, child_stdin) {
                let _ = io::copy(&mut req_stdin, &mut child_stdin).await;
                let _ = child_stdin.shutdown().await;
            }
        };

        let stdout_fut = async {
            if let (Some(mut child_stdout), Some(mut req_stdout)) = (child_stdout, req.stdout) {
                let _ = io::copy(&mut child_stdout, &mut req_stdout).await;
            }
        };

        let stderr_fut = async {
            if let (Some(mut child_stderr), Some(mut req_stderr)) = (child_stderr, req.stderr) {
                let _ = io::copy(&mut child_stderr, &mut req_stderr).await;
            }
        };

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
                    anyhow::bail!("process exited with status: {}", status)
                }

                Ok(RunResponse {
                    artifacts: vec![],
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::driver::{Driver as _, RunRequest};
    use crate::hasync::StdCancellationToken;
    use crate::htaddr::Addr;

    #[tokio::test]
    async fn test_run_echo_hello() -> anyhow::Result<()> {
        let driver = Driver::new_exec();
        let ctoken = StdCancellationToken::new();

        let target_def = EngineTargetDef {
            addr: Addr::default(),
            labels: vec![],
            raw_def: Arc::new(TargetDef {
                run: vec!["echo".to_string(), "hello".to_string()],
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
            sandbox_path: "".to_string(),
            tree_root_path: "".to_string(),
            inputs: vec![],
            hashin: &"".to_string(),
            stdin: None,
            stdout: Some(&mut stdout),
            stderr: None,
        };

        let _res = driver.run(req, &ctoken).await?;

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
            sandbox_path: "".to_string(),
            tree_root_path: "".to_string(),
            inputs: vec![],
            hashin: &"".to_string(),
            stdin: Some(&mut stdin),
            stdout: Some(&mut stdout),
            stderr: None,
        };

        // Use a timeout to detect the hang
        let _res = tokio::time::timeout(std::time::Duration::from_secs(1), driver.run(req, &ctoken)).await?;

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
            sandbox_path: "".to_string(),
            tree_root_path: "".to_string(),
            inputs: vec![],
            hashin: &"".to_string(),
            stdin: Some(&mut reader),
            stdout: None,
            stderr: None,
        };

        let run_fut = driver.run(req, &ctoken);

        tokio::spawn({
            let ctoken = ctoken.clone();
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                ctoken.cancel();
            }
        });

        let res = run_fut.await;
        assert!(res.is_err());
        let err = res.err().unwrap();
        assert_eq!(err.to_string(), "cancelled");

        Ok(())
    }
}
