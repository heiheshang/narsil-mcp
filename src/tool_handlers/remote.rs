//! Remote repository tool handlers

use anyhow::Result;
use serde_json::Value;

use super::{ArgExtractor, ToolHandler};
use crate::engine::CodeIntelEngine;

/// Handler for add_remote_repo tool
pub struct AddRemoteRepoHandler;

#[async_trait::async_trait]
impl ToolHandler for AddRemoteRepoHandler {
    fn name(&self) -> &'static str {
        "add_remote_repo"
    }

    async fn execute(&self, engine: &CodeIntelEngine, args: Value) -> Result<String> {
        let url = args.get_str("url").unwrap_or("");
        let sparse_paths: Option<Vec<String>> = args.get_array("sparse_paths").map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        });
        engine.add_remote_repo(url, sparse_paths.as_deref()).await
    }
}

/// Handler for list_remote_files tool
pub struct ListRemoteFilesHandler;

#[async_trait::async_trait]
impl ToolHandler for ListRemoteFilesHandler {
    fn name(&self) -> &'static str {
        "list_remote_files"
    }

    async fn execute(&self, engine: &CodeIntelEngine, args: Value) -> Result<String> {
        let url = args.get_str("url").unwrap_or("");
        let path = args.get_str("path");
        engine.list_remote_files(url, path).await
    }
}

/// Handler for get_remote_file tool
pub struct GetRemoteFileHandler;

#[async_trait::async_trait]
impl ToolHandler for GetRemoteFileHandler {
    fn name(&self) -> &'static str {
        "get_remote_file"
    }

    async fn execute(&self, engine: &CodeIntelEngine, args: Value) -> Result<String> {
        let url = args.get_str("url").unwrap_or("");
        let path = args.get_str("path").unwrap_or("");
        engine.get_remote_file(url, path).await
    }
}
