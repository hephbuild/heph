//! Filesystem operations plugin

use crate::{Plugin, PluginError, PluginRequest, PluginResponse, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::Path;
use tokio::fs;
use tokio::io::AsyncWriteExt;

/// Plugin for filesystem operations
pub struct FsPlugin;

impl FsPlugin {
    pub fn new() -> Self {
        Self
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        fs::read(path)
            .await
            .map_err(|e| PluginError::ExecutionFailed(format!("Failed to read file {}: {}", path, e)))
    }

    async fn write_file(&self, path: &str, data: &[u8]) -> Result<()> {
        // Create parent directories if they don't exist
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent).await.map_err(|e| {
                PluginError::ExecutionFailed(format!("Failed to create directories: {}", e))
            })?;
        }

        let mut file = fs::File::create(path).await.map_err(|e| {
            PluginError::ExecutionFailed(format!("Failed to create file {}: {}", path, e))
        })?;

        file.write_all(data).await.map_err(|e| {
            PluginError::ExecutionFailed(format!("Failed to write file {}: {}", path, e))
        })?;

        Ok(())
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<String>> {
        let mut entries = Vec::new();
        let mut dir = fs::read_dir(path).await.map_err(|e| {
            PluginError::ExecutionFailed(format!("Failed to read directory {}: {}", path, e))
        })?;

        while let Some(entry) = dir.next_entry().await.map_err(|e| {
            PluginError::ExecutionFailed(format!("Failed to read directory entry: {}", e))
        })? {
            if let Some(name) = entry.file_name().to_str() {
                entries.push(name.to_string());
            }
        }

        Ok(entries)
    }

    async fn file_exists(&self, path: &str) -> bool {
        fs::metadata(path).await.is_ok()
    }

    async fn remove_file(&self, path: &str) -> Result<()> {
        fs::remove_file(path).await.map_err(|e| {
            PluginError::ExecutionFailed(format!("Failed to remove file {}: {}", path, e))
        })
    }
}

impl Default for FsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Plugin for FsPlugin {
    fn name(&self) -> &str {
        "fs"
    }

    async fn initialize(&mut self) -> Result<()> {
        Ok(())
    }

    async fn execute(&self, request: PluginRequest) -> Result<PluginResponse> {
        let operation = request
            .inputs
            .get("operation")
            .ok_or_else(|| PluginError::ExecutionFailed("missing operation input".to_string()))?;

        let path = request
            .inputs
            .get("path")
            .ok_or_else(|| PluginError::ExecutionFailed("missing path input".to_string()))?;

        let mut outputs = HashMap::new();

        match operation.as_str() {
            "read" => {
                let data = self.read_file(path).await?;
                outputs.insert("data".to_string(), data);
            }
            "write" => {
                let data = request
                    .inputs
                    .get("data")
                    .ok_or_else(|| {
                        PluginError::ExecutionFailed("missing data input for write".to_string())
                    })?
                    .as_bytes()
                    .to_vec();

                self.write_file(path, &data).await?;
                outputs.insert("written".to_string(), b"true".to_vec());
            }
            "list" => {
                let entries = self.list_dir(path).await?;
                let entries_json = serde_json::to_vec(&entries).map_err(|e| {
                    PluginError::ExecutionFailed(format!("Failed to serialize entries: {}", e))
                })?;
                outputs.insert("entries".to_string(), entries_json);
            }
            "exists" => {
                let exists = self.file_exists(path).await;
                outputs.insert("exists".to_string(), exists.to_string().into_bytes());
            }
            "remove" => {
                self.remove_file(path).await?;
                outputs.insert("removed".to_string(), b"true".to_vec());
            }
            _ => {
                return Err(PluginError::ExecutionFailed(format!(
                    "Unknown operation: {}",
                    operation
                )));
            }
        }

        Ok(PluginResponse::success(outputs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_fs_write_and_read() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        let path_str = path.to_str().unwrap().to_string();

        let mut plugin = FsPlugin::new();
        plugin.initialize().await.unwrap();

        // Write
        let mut inputs = HashMap::new();
        inputs.insert("operation".to_string(), "write".to_string());
        inputs.insert("path".to_string(), path_str.clone());
        inputs.insert("data".to_string(), "hello world".to_string());

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let response = plugin.execute(request).await.unwrap();
        assert!(response.success);

        // Read
        let mut inputs = HashMap::new();
        inputs.insert("operation".to_string(), "read".to_string());
        inputs.insert("path".to_string(), path_str);

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let response = plugin.execute(request).await.unwrap();
        assert!(response.success);
        assert!(response.outputs.contains_key("data"));

        let data = String::from_utf8_lossy(&response.outputs["data"]);
        assert_eq!(data, "hello world");
    }

    #[tokio::test]
    async fn test_fs_list_directory() {
        let dir = TempDir::new().unwrap();

        // Create some files
        tokio::fs::write(dir.path().join("file1.txt"), "test").await.unwrap();
        tokio::fs::write(dir.path().join("file2.txt"), "test").await.unwrap();

        let mut plugin = FsPlugin::new();
        plugin.initialize().await.unwrap();

        let mut inputs = HashMap::new();
        inputs.insert("operation".to_string(), "list".to_string());
        inputs.insert("path".to_string(), dir.path().to_str().unwrap().to_string());

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let response = plugin.execute(request).await.unwrap();
        assert!(response.success);
        assert!(response.outputs.contains_key("entries"));

        let entries: Vec<String> =
            serde_json::from_slice(&response.outputs["entries"]).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.contains(&"file1.txt".to_string()));
        assert!(entries.contains(&"file2.txt".to_string()));
    }

    #[tokio::test]
    async fn test_fs_exists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");

        tokio::fs::write(&path, "test").await.unwrap();

        let mut plugin = FsPlugin::new();
        plugin.initialize().await.unwrap();

        // Check existing file
        let mut inputs = HashMap::new();
        inputs.insert("operation".to_string(), "exists".to_string());
        inputs.insert("path".to_string(), path.to_str().unwrap().to_string());

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let response = plugin.execute(request).await.unwrap();
        assert!(response.success);

        let exists = String::from_utf8_lossy(&response.outputs["exists"]);
        assert_eq!(exists, "true");

        // Check non-existing file
        let mut inputs = HashMap::new();
        inputs.insert("operation".to_string(), "exists".to_string());
        inputs.insert("path".to_string(), dir.path().join("nonexistent.txt").to_str().unwrap().to_string());

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let response = plugin.execute(request).await.unwrap();
        let exists = String::from_utf8_lossy(&response.outputs["exists"]);
        assert_eq!(exists, "false");
    }

    #[tokio::test]
    async fn test_fs_remove() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");

        tokio::fs::write(&path, "test").await.unwrap();
        assert!(path.exists());

        let mut plugin = FsPlugin::new();
        plugin.initialize().await.unwrap();

        let mut inputs = HashMap::new();
        inputs.insert("operation".to_string(), "remove".to_string());
        inputs.insert("path".to_string(), path.to_str().unwrap().to_string());

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let response = plugin.execute(request).await.unwrap();
        assert!(response.success);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn test_fs_invalid_operation() {
        let mut plugin = FsPlugin::new();
        plugin.initialize().await.unwrap();

        let mut inputs = HashMap::new();
        inputs.insert("operation".to_string(), "invalid".to_string());
        inputs.insert("path".to_string(), "/tmp/test".to_string());

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let result = plugin.execute(request).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_fs_missing_operation() {
        let mut plugin = FsPlugin::new();
        plugin.initialize().await.unwrap();

        let mut inputs = HashMap::new();
        inputs.insert("path".to_string(), "/tmp/test".to_string());

        let request = PluginRequest {
            target: "test".to_string(),
            inputs,
        };

        let result = plugin.execute(request).await;
        assert!(result.is_err());
    }
}
