//! Supply chain security tool handlers

use anyhow::Result;
use serde_json::Value;

use super::{ArgExtractor, ToolHandler};
use crate::engine::CodeIntelEngine;

/// Handler for generate_sbom tool
///
/// Phase C1: Added `compact` parameter for minified JSON output
pub struct GenerateSbomHandler;

#[async_trait::async_trait]
impl ToolHandler for GenerateSbomHandler {
    fn name(&self) -> &'static str {
        "generate_sbom"
    }

    async fn execute(&self, engine: &CodeIntelEngine, args: Value) -> Result<String> {
        let repo = args.get_str("repo").unwrap_or("");
        let format = args.get_str("format").unwrap_or("cyclonedx");
        let compact = args.get_bool_or("compact", false);
        engine.generate_sbom(repo, format, compact).await
    }
}

/// Handler for check_dependencies tool
pub struct CheckDependenciesHandler;

#[async_trait::async_trait]
impl ToolHandler for CheckDependenciesHandler {
    fn name(&self) -> &'static str {
        "check_dependencies"
    }

    async fn execute(&self, engine: &CodeIntelEngine, args: Value) -> Result<String> {
        let repo = args.get_str("repo").unwrap_or("");
        let severity_threshold = args.get_str("severity_threshold");
        let include_dev = args.get_bool_or("include_dev", true);
        engine
            .check_dependencies(repo, severity_threshold, include_dev)
            .await
    }
}

/// Handler for check_licenses tool
pub struct CheckLicensesHandler;

#[async_trait::async_trait]
impl ToolHandler for CheckLicensesHandler {
    fn name(&self) -> &'static str {
        "check_licenses"
    }

    async fn execute(&self, engine: &CodeIntelEngine, args: Value) -> Result<String> {
        let repo = args.get_str("repo").unwrap_or("");
        let project_license = args.get_str("project_license");
        let fail_on_copyleft = args.get_bool_or("fail_on_copyleft", false);
        engine
            .check_licenses(repo, project_license, fail_on_copyleft)
            .await
    }
}

/// Handler for find_upgrade_path tool
pub struct FindUpgradePathHandler;

#[async_trait::async_trait]
impl ToolHandler for FindUpgradePathHandler {
    fn name(&self) -> &'static str {
        "find_upgrade_path"
    }

    async fn execute(&self, engine: &CodeIntelEngine, args: Value) -> Result<String> {
        let repo = args.get_str("repo").unwrap_or("");
        let dependency = args.get_str("dependency");
        engine.find_upgrade_path(repo, dependency).await
    }
}
