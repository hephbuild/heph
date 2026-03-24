//! Build file parsing plugin

use crate::{Plugin, PluginError, PluginRequest, PluginResponse, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Build file specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildSpec {
    pub name: String,
    pub srcs: Vec<String>,
    pub deps: Vec<String>,
    pub outs: Vec<String>,
}

/// Plugin for parsing and validating build files
pub struct BuildFilePlugin;

impl BuildFilePlugin {
    pub fn new() -> Self {
        Self
    }

    fn parse_build_spec(&self, content: &str) -> Result<BuildSpec> {
        serde_json::from_str(content)
            .map_err(|e| PluginError::ExecutionFailed(format!("Failed to parse build spec: {}", e)))
    }

    fn validate_spec(&self, spec: &BuildSpec) -> Result<()> {
        if spec.name.is_empty() {
            return Err(PluginError::ExecutionFailed("Build name cannot be empty".to_string()));
        }

        // Validate target names don't contain invalid characters
        for dep in &spec.deps {
            if !dep.starts_with("//") && !dep.starts_with(':') {
                return Err(PluginError::ExecutionFailed(format!(
                    "Invalid dependency format: {}",
                    dep
                )));
            }
        }

        Ok(())
    }
}

impl Default for BuildFilePlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Plugin for BuildFilePlugin {
    fn name(&self) -> &str {
        "buildfile"
    }

    async fn initialize(&mut self) -> Result<()> {
        Ok(())
    }

    async fn execute(&self, request: PluginRequest) -> Result<PluginResponse> {
        let content = request
            .inputs
            .get("content")
            .ok_or_else(|| PluginError::ExecutionFailed("missing content input".to_string()))?;

        let spec = self.parse_build_spec(content)?;
        self.validate_spec(&spec)?;

        let mut outputs = HashMap::new();

        // Serialize spec back
        let spec_json = serde_json::to_vec(&spec)
            .map_err(|e| PluginError::ExecutionFailed(format!("Failed to serialize spec: {}", e)))?;

        outputs.insert("spec".to_string(), spec_json);
        outputs.insert("name".to_string(), spec.name.clone().into_bytes());
        outputs.insert("srcs_count".to_string(), spec.srcs.len().to_string().into_bytes());
        outputs.insert("deps_count".to_string(), spec.deps.len().to_string().into_bytes());
        outputs.insert("outs_count".to_string(), spec.outs.len().to_string().into_bytes());

        Ok(PluginResponse::success(outputs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_buildfile_parse_success() {
        let mut plugin = BuildFilePlugin::new();
        plugin.initialize().await.unwrap();

        let spec = BuildSpec {
            name: "test_target".to_string(),
            srcs: vec!["main.rs".to_string()],
            deps: vec!["//lib:common".to_string()],
            outs: vec!["binary".to_string()],
        };

        let content = serde_json::to_string(&spec).unwrap();

        let mut inputs = HashMap::new();
        inputs.insert("content".to_string(), content);

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let response = plugin.execute(request).await.unwrap();
        assert!(response.success);
        assert!(response.outputs.contains_key("spec"));
        assert!(response.outputs.contains_key("name"));
    }

    #[tokio::test]
    async fn test_buildfile_parse_invalid_json() {
        let mut plugin = BuildFilePlugin::new();
        plugin.initialize().await.unwrap();

        let mut inputs = HashMap::new();
        inputs.insert("content".to_string(), "invalid json".to_string());

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let result = plugin.execute(request).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_buildfile_validate_empty_name() {
        let mut plugin = BuildFilePlugin::new();
        plugin.initialize().await.unwrap();

        let spec = BuildSpec {
            name: "".to_string(),
            srcs: vec![],
            deps: vec![],
            outs: vec![],
        };

        let content = serde_json::to_string(&spec).unwrap();

        let mut inputs = HashMap::new();
        inputs.insert("content".to_string(), content);

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let result = plugin.execute(request).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_buildfile_validate_invalid_dep() {
        let mut plugin = BuildFilePlugin::new();
        plugin.initialize().await.unwrap();

        let spec = BuildSpec {
            name: "test".to_string(),
            srcs: vec![],
            deps: vec!["invalid_dep".to_string()],
            outs: vec![],
        };

        let content = serde_json::to_string(&spec).unwrap();

        let mut inputs = HashMap::new();
        inputs.insert("content".to_string(), content);

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let result = plugin.execute(request).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_buildfile_counts() {
        let mut plugin = BuildFilePlugin::new();
        plugin.initialize().await.unwrap();

        let spec = BuildSpec {
            name: "test".to_string(),
            srcs: vec!["a.rs".to_string(), "b.rs".to_string()],
            deps: vec!["//lib:x".to_string()],
            outs: vec!["out1".to_string(), "out2".to_string(), "out3".to_string()],
        };

        let content = serde_json::to_string(&spec).unwrap();

        let mut inputs = HashMap::new();
        inputs.insert("content".to_string(), content);

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let response = plugin.execute(request).await.unwrap();
        assert!(response.success);

        let srcs_count = String::from_utf8_lossy(&response.outputs["srcs_count"]);
        let deps_count = String::from_utf8_lossy(&response.outputs["deps_count"]);
        let outs_count = String::from_utf8_lossy(&response.outputs["outs_count"]);

        assert_eq!(srcs_count, "2");
        assert_eq!(deps_count, "1");
        assert_eq!(outs_count, "3");
    }
}
