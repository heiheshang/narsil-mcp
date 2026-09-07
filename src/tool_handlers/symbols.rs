//! Symbol-related tool handlers

use anyhow::Result;
use serde_json::Value;

use super::{ArgExtractor, ToolHandler};
use crate::index::{CodeIntelEngine, FindSymbolsQuery, SymbolDefinitionQuery};

/// Handler for find_symbols tool
pub struct FindSymbolsHandler;

#[async_trait::async_trait]
impl ToolHandler for FindSymbolsHandler {
    fn name(&self) -> &'static str {
        "find_symbols"
    }

    async fn execute(&self, engine: &CodeIntelEngine, args: Value) -> Result<String> {
        engine
            .find_symbols(FindSymbolsQuery {
                repo: args.get_str("repo").unwrap_or(""),
                symbol_type: args.get_str("symbol_type"),
                pattern: args.get_str_any(&["pattern", "query", "name"]),
                name_regex: args.get_str("name_regex"),
                file_pattern: args.get_str("file_pattern"),
                exclude_tests: args.get_bool("exclude_tests"),
                limit: args.get_limit_any(&["limit", "max_results"], 200),
                compact: args.get_bool_or("compact", false),
            })
            .await
    }
}

/// Handler for get_symbol_definition tool
pub struct GetSymbolDefinitionHandler;

#[async_trait::async_trait]
impl ToolHandler for GetSymbolDefinitionHandler {
    fn name(&self) -> &'static str {
        "get_symbol_definition"
    }

    async fn execute(&self, engine: &CodeIntelEngine, args: Value) -> Result<String> {
        engine
            .get_symbol_definition(SymbolDefinitionQuery {
                repo: args.get_str("repo").unwrap_or(""),
                symbol: args.get_str_any(&["symbol", "function"]).unwrap_or(""),
                context_lines: args.get_u64_or("context_lines", 5) as usize,
                outline: args.get_bool_or("outline", false),
                contains: args.get_str("contains"),
                offset: args.get_u64_or("offset", 0) as usize,
                max_lines: args.get_u64_or("max_lines", 0) as usize,
                file: args.get_str("file"),
                receiver: args.get_str("receiver"),
            })
            .await
    }
}

/// Handler for find_references tool
pub struct FindReferencesHandler;

#[async_trait::async_trait]
impl ToolHandler for FindReferencesHandler {
    fn name(&self) -> &'static str {
        "find_references"
    }

    async fn execute(&self, engine: &CodeIntelEngine, args: Value) -> Result<String> {
        let repo = args.get_str("repo").unwrap_or("");
        let symbol = args.get_str_any(&["symbol", "function"]).unwrap_or("");
        let include_def = args.get_bool_or("include_definition", true);
        let exclude_tests = args.get_bool("exclude_tests");
        engine
            .find_references(repo, symbol, include_def, exclude_tests)
            .await
    }
}

/// Handler for get_dependencies tool
pub struct GetDependenciesHandler;

#[async_trait::async_trait]
impl ToolHandler for GetDependenciesHandler {
    fn name(&self) -> &'static str {
        "get_dependencies"
    }

    async fn execute(&self, engine: &CodeIntelEngine, args: Value) -> Result<String> {
        let repo = args.get_str("repo").unwrap_or("");
        let path = args.get_str("path").unwrap_or("");
        let direction = args.get_str("direction").unwrap_or("both");
        engine.get_dependencies(repo, path, direction).await
    }
}

/// Handler for find_symbol_usages tool
pub struct FindSymbolUsagesHandler;

#[async_trait::async_trait]
impl ToolHandler for FindSymbolUsagesHandler {
    fn name(&self) -> &'static str {
        "find_symbol_usages"
    }

    async fn execute(&self, engine: &CodeIntelEngine, args: Value) -> Result<String> {
        let repo = args.get_str("repo").unwrap_or("");
        let symbol = args.get_str_any(&["symbol", "function"]).unwrap_or("");
        let include_imports = args.get_bool_or("include_imports", true);
        let exclude_tests = args.get_bool("exclude_tests");
        engine
            .find_symbol_usages(repo, symbol, include_imports, exclude_tests)
            .await
    }
}

/// Handler for get_export_map tool
pub struct GetExportMapHandler;

#[async_trait::async_trait]
impl ToolHandler for GetExportMapHandler {
    fn name(&self) -> &'static str {
        "get_export_map"
    }

    async fn execute(&self, engine: &CodeIntelEngine, args: Value) -> Result<String> {
        let repo = args.get_str("repo").unwrap_or("");
        let path = args.get_str("path").unwrap_or("");
        engine.get_export_map(repo, path).await
    }
}

/// Handler for workspace_symbol_search tool
pub struct WorkspaceSymbolSearchHandler;

#[async_trait::async_trait]
impl ToolHandler for WorkspaceSymbolSearchHandler {
    fn name(&self) -> &'static str {
        "workspace_symbol_search"
    }

    async fn execute(&self, engine: &CodeIntelEngine, args: Value) -> Result<String> {
        let query = args.get_str("query").unwrap_or("");
        let kind = args.get_str("kind");
        let limit = args.get_limit_any(&["limit", "max_results"], 20);
        engine.workspace_symbol_search(query, kind, limit).await
    }
}
