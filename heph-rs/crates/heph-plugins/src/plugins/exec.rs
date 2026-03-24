//! Process execution plugin

use crate::{Plugin, PluginError, PluginRequest, PluginResponse, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::process::Stdio;
use tokio::process::Command;

/// Plugin for executing shell commands
pub struct ExecPlugin {
    shell: String,
}

impl ExecPlugin {
    pub fn new() -> Self {
        Self {
            shell: "sh".to_string(),
        }
    }

    pub fn with_shell(shell: String) -> Self {
        Self { shell }
    }
}

impl Default for ExecPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Plugin for ExecPlugin {
    fn name(&self) -> &str {
        "exec"
    }

    async fn initialize(&mut self) -> Result<()> {
        // Verify shell exists
        let status = Command::new(&self.shell)
            .arg("-c")
            .arg("echo test")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|e| {
                PluginError::InitFailed(format!("Shell {} not available: {}", self.shell, e))
            })?;

        if !status.success() {
            return Err(PluginError::InitFailed(format!(
                "Shell {} failed test command",
                self.shell
            )));
        }

        Ok(())
    }

    async fn execute(&self, request: PluginRequest) -> Result<PluginResponse> {
        let cmd = request
            .inputs
            .get("cmd")
            .ok_or_else(|| PluginError::ExecutionFailed("missing cmd input".to_string()))?;

        let working_dir = request.inputs.get("cwd");
        let env_vars = request.inputs.get("env");

        let mut command = Command::new(&self.shell);
        command.arg("-c").arg(cmd);

        if let Some(cwd) = working_dir {
            command.current_dir(cwd);
        }

        if let Some(env_str) = env_vars {
            // Parse env vars in format "KEY=VALUE,KEY2=VALUE2"
            for pair in env_str.split(',') {
                if let Some((key, value)) = pair.split_once('=') {
                    command.env(key.trim(), value.trim());
                }
            }
        }

        let output = command
            .output()
            .await
            .map_err(|e| PluginError::ExecutionFailed(format!("Command execution failed: {}", e)))?;

        let mut outputs = HashMap::new();
        outputs.insert("stdout".to_string(), output.stdout);
        outputs.insert("stderr".to_string(), output.stderr);
        outputs.insert(
            "exit_code".to_string(),
            output.status.code().unwrap_or(-1).to_string().into_bytes(),
        );

        Ok(PluginResponse {
            success: output.status.success(),
            outputs,
            error: if output.status.success() {
                None
            } else {
                Some(format!("Command failed with exit code: {:?}", output.status.code()))
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_exec_success() {
        let mut plugin = ExecPlugin::new();
        plugin.initialize().await.unwrap();

        let mut inputs = HashMap::new();
        inputs.insert("cmd".to_string(), "echo hello".to_string());

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let response = plugin.execute(request).await.unwrap();
        assert!(response.success);
        assert!(response.outputs.contains_key("stdout"));

        let stdout = String::from_utf8_lossy(&response.outputs["stdout"]);
        assert!(stdout.contains("hello"));
    }

    #[tokio::test]
    async fn test_exec_failure() {
        let mut plugin = ExecPlugin::new();
        plugin.initialize().await.unwrap();

        let mut inputs = HashMap::new();
        inputs.insert("cmd".to_string(), "exit 1".to_string());

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let response = plugin.execute(request).await.unwrap();
        assert!(!response.success);
        assert!(response.error.is_some());
    }

    #[tokio::test]
    async fn test_exec_missing_cmd() {
        let mut plugin = ExecPlugin::new();
        plugin.initialize().await.unwrap();

        let request = PluginRequest {
            target: "test".to_string(),
            inputs: HashMap::new(),
        };

        let result = plugin.execute(request).await;
        assert!(result.is_err());
        assert!(matches!(result, Err(PluginError::ExecutionFailed(_))));
    }

    #[tokio::test]
    async fn test_exec_with_env() {
        let mut plugin = ExecPlugin::new();
        plugin.initialize().await.unwrap();

        let mut inputs = HashMap::new();
        inputs.insert("cmd".to_string(), "echo $TEST_VAR".to_string());
        inputs.insert("env".to_string(), "TEST_VAR=hello_world".to_string());

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let response = plugin.execute(request).await.unwrap();
        assert!(response.success);

        let stdout = String::from_utf8_lossy(&response.outputs["stdout"]);
        assert!(stdout.contains("hello_world"));
    }

    #[tokio::test]
    async fn test_exec_with_cwd() {
        let mut plugin = ExecPlugin::new();
        plugin.initialize().await.unwrap();

        let mut inputs = HashMap::new();
        inputs.insert("cmd".to_string(), "pwd".to_string());
        inputs.insert("cwd".to_string(), "/tmp".to_string());

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let response = plugin.execute(request).await.unwrap();
        assert!(response.success);

        let stdout = String::from_utf8_lossy(&response.outputs["stdout"]);
        assert!(stdout.contains("/tmp"));
    }

    #[tokio::test]
    async fn test_exec_captures_stderr() {
        let mut plugin = ExecPlugin::new();
        plugin.initialize().await.unwrap();

        let mut inputs = HashMap::new();
        inputs.insert("cmd".to_string(), "echo error >&2".to_string());

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let response = plugin.execute(request).await.unwrap();
        assert!(response.success);
        assert!(response.outputs.contains_key("stderr"));

        let stderr = String::from_utf8_lossy(&response.outputs["stderr"]);
        assert!(stderr.contains("error"));
    }

    #[tokio::test]
    async fn test_exec_exit_code() {
        let mut plugin = ExecPlugin::new();
        plugin.initialize().await.unwrap();

        let mut inputs = HashMap::new();
        inputs.insert("cmd".to_string(), "exit 42".to_string());

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let response = plugin.execute(request).await.unwrap();
        assert!(!response.success);
        assert!(response.outputs.contains_key("exit_code"));

        let exit_code = String::from_utf8_lossy(&response.outputs["exit_code"]);
        assert_eq!(exit_code, "42");
    }
}
