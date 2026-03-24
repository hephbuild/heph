//! Plugin SDK for Heph

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

pub mod plugins;

#[derive(Error, Debug)]
pub enum PluginError {
    #[error("Plugin not found: {0}")]
    NotFound(String),

    #[error("Plugin initialization failed: {0}")]
    InitFailed(String),

    #[error("Plugin execution failed: {0}")]
    ExecutionFailed(String),

    #[error("RPC error: {0}")]
    Rpc(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, PluginError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRequest {
    pub target: String,
    pub inputs: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginResponse {
    pub success: bool,
    pub outputs: HashMap<String, Vec<u8>>,
    pub error: Option<String>,
}

impl PluginResponse {
    /// Create a successful response
    pub fn success(outputs: HashMap<String, Vec<u8>>) -> Self {
        Self {
            success: true,
            outputs,
            error: None,
        }
    }

    /// Create a failed response
    pub fn failure(error: String) -> Self {
        Self {
            success: false,
            outputs: HashMap::new(),
            error: Some(error),
        }
    }
}

/// Plugin trait
#[async_trait]
pub trait Plugin: Send + Sync {
    /// Plugin name
    fn name(&self) -> &str;

    /// Initialize plugin
    async fn initialize(&mut self) -> Result<()>;

    /// Execute plugin
    async fn execute(&self, request: PluginRequest) -> Result<PluginResponse>;

    /// Shutdown plugin
    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Plugin manager
pub struct PluginManager {
    plugins: HashMap<String, Box<dyn Plugin>>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self {
            plugins: HashMap::new(),
        }
    }

    /// Register a plugin
    pub fn register(&mut self, plugin: Box<dyn Plugin>) {
        let name = plugin.name().to_string();
        self.plugins.insert(name, plugin);
    }

    /// Initialize all plugins
    pub async fn initialize_all(&mut self) -> Result<()> {
        for plugin in self.plugins.values_mut() {
            plugin.initialize().await?;
        }
        Ok(())
    }

    /// Execute a plugin
    pub async fn execute(
        &self,
        plugin_name: &str,
        request: PluginRequest,
    ) -> Result<PluginResponse> {
        let plugin = self
            .plugins
            .get(plugin_name)
            .ok_or_else(|| PluginError::NotFound(plugin_name.to_string()))?;

        plugin.execute(request).await
    }

    /// Get list of registered plugin names
    pub fn plugin_names(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }

    /// Check if a plugin is registered
    pub fn has_plugin(&self, name: &str) -> bool {
        self.plugins.contains_key(name)
    }

    /// Get the number of registered plugins
    pub fn plugin_count(&self) -> usize {
        self.plugins.len()
    }
}

impl Default for PluginManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPlugin {
        initialized: bool,
    }

    impl TestPlugin {
        fn new() -> Self {
            Self { initialized: false }
        }
    }

    #[async_trait]
    impl Plugin for TestPlugin {
        fn name(&self) -> &str {
            "test_plugin"
        }

        async fn initialize(&mut self) -> Result<()> {
            self.initialized = true;
            Ok(())
        }

        async fn execute(&self, _request: PluginRequest) -> Result<PluginResponse> {
            if !self.initialized {
                return Err(PluginError::InitFailed(
                    "Plugin not initialized".to_string(),
                ));
            }

            Ok(PluginResponse {
                success: true,
                outputs: HashMap::new(),
                error: None,
            })
        }
    }

    struct FailingPlugin;

    #[async_trait]
    impl Plugin for FailingPlugin {
        fn name(&self) -> &str {
            "failing_plugin"
        }

        async fn initialize(&mut self) -> Result<()> {
            Err(PluginError::InitFailed("Intentional failure".to_string()))
        }

        async fn execute(&self, _request: PluginRequest) -> Result<PluginResponse> {
            Err(PluginError::ExecutionFailed("Always fails".to_string()))
        }
    }

    #[tokio::test]
    async fn test_plugin_registration() {
        let mut manager = PluginManager::new();
        manager.register(Box::new(TestPlugin::new()));

        assert!(manager.plugins.contains_key("test_plugin"));
        assert_eq!(manager.plugin_count(), 1);
        assert!(manager.has_plugin("test_plugin"));
    }

    #[tokio::test]
    async fn test_plugin_execution() {
        let mut manager = PluginManager::new();
        manager.register(Box::new(TestPlugin::new()));
        manager.initialize_all().await.unwrap();

        let request = PluginRequest {
            target: "test".to_string(),
            inputs: HashMap::new(),
        };

        let response = manager.execute("test_plugin", request).await.unwrap();
        assert!(response.success);
        assert!(response.error.is_none());
    }

    #[tokio::test]
    async fn test_plugin_not_found() {
        let manager = PluginManager::new();

        let request = PluginRequest {
            target: "test".to_string(),
            inputs: HashMap::new(),
        };

        let result = manager.execute("nonexistent", request).await;
        assert!(result.is_err());
        assert!(matches!(result, Err(PluginError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_plugin_initialization() {
        let mut manager = PluginManager::new();
        manager.register(Box::new(TestPlugin::new()));

        let result = manager.initialize_all().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_plugin_initialization_failure() {
        let mut manager = PluginManager::new();
        manager.register(Box::new(FailingPlugin));

        let result = manager.initialize_all().await;
        assert!(result.is_err());
        assert!(matches!(result, Err(PluginError::InitFailed(_))));
    }

    #[tokio::test]
    async fn test_multiple_plugins() {
        let mut manager = PluginManager::new();
        manager.register(Box::new(TestPlugin::new()));

        struct AnotherPlugin;

        #[async_trait]
        impl Plugin for AnotherPlugin {
            fn name(&self) -> &str {
                "another_plugin"
            }

            async fn initialize(&mut self) -> Result<()> {
                Ok(())
            }

            async fn execute(&self, _request: PluginRequest) -> Result<PluginResponse> {
                Ok(PluginResponse::success(HashMap::new()))
            }
        }

        manager.register(Box::new(AnotherPlugin));
        manager.initialize_all().await.unwrap();

        assert_eq!(manager.plugin_count(), 2);
        assert!(manager.has_plugin("test_plugin"));
        assert!(manager.has_plugin("another_plugin"));

        let names = manager.plugin_names();
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn test_plugin_response_constructors() {
        let mut outputs = HashMap::new();
        outputs.insert("key".to_string(), b"value".to_vec());

        let success = PluginResponse::success(outputs.clone());
        assert!(success.success);
        assert!(success.error.is_none());
        assert_eq!(success.outputs.len(), 1);

        let failure = PluginResponse::failure("error message".to_string());
        assert!(!failure.success);
        assert!(failure.error.is_some());
        assert_eq!(failure.outputs.len(), 0);
    }
}
