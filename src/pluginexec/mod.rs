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

}

struct TargetDef {
    pub run: Vec<String>,
}

impl Driver {
    pub fn new() -> Driver {
        Driver{}
    }
}

#[async_trait]
impl engine::driver::Driver for Driver {
    fn config(&self, _req: ConfigRequest, _ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse{
            name: "exec".parse()?,
        })
    }

    async fn parse(&self, req: ParseRequest, _ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<ParseResponse> {
        Ok(ParseResponse {
            target_def: EngineTargetDef{
                addr: req.target_spec.addr,
                raw_def: Arc::new(TargetDef{
                    run: vec!["echo", "hello"].iter().map(|s| s.to_string()).collect(),
                    // run: vec!["sh", "-c", "exit 1"].iter().map(|s| s.to_string()).collect(),
                }),
                inputs: vec![],
                outputs: vec![],
                support_files: vec![],
                cache: true,
                disable_remote_cache: false,
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

        let mut child = Command::new(&def.run[0])
            .kill_on_drop(true)
            .args(&def.run[1..])
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
        let driver = Driver::new();
        let ctoken = StdCancellationToken::new();

        let target_def = EngineTargetDef {
            addr: Addr::default(),
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
        let driver = Driver::new();
        let ctoken = StdCancellationToken::new();

        let target_def = EngineTargetDef {
            addr: Addr::default(),
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
        let res = tokio::time::timeout(std::time::Duration::from_secs(1), driver.run(req, &ctoken)).await?;

        let output = String::from_utf8(stdout)?;
        assert_eq!(output, "test data");

        Ok(())
    }

    #[tokio::test]
    async fn test_run_stdin_to_stdout_timeout() -> anyhow::Result<()> {
        let driver = Driver::new();
        let ctoken = StdCancellationToken::new();

        let target_def = EngineTargetDef {
            addr: Addr::default(),
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
