mod common;

use anyhow::Result;
use common::{TestHarness, TestRepo};
use serde_json::json;
use std::thread;
use std::time::{Duration, Instant};

fn wait_for_repo(harness: &TestHarness) -> Result<()> {
    let repo_name = harness.repo_name();
    let deadline = Instant::now() + Duration::from_secs(30);

    loop {
        let repos = harness.call_tool_text("list_repos", json!({}))?;
        if repos.contains(&repo_name) {
            return Ok(());
        }

        if Instant::now() >= deadline {
            anyhow::bail!("Timed out waiting for repo '{}' to index", repo_name);
        }

        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn loads_onec_config_dump_fixture() -> Result<()> {
    let repo = TestRepo::from_fixture("onec_config_dump")?;

    assert!(repo.path().join("Configuration.xml").is_file());
    assert!(repo.path().join("ConfigDumpInfo.xml").is_file());
    assert!(repo
        .path()
        .join("Catalogs/Products/Ext/ManagerModule.bsl")
        .is_file());
    assert!(repo
        .path()
        .join("Catalogs/Products/Forms/ItemForm/Ext/Form/Module.bsl")
        .is_file());

    Ok(())
}

#[test]
fn loads_mixed_repo_with_nested_onec_subtree_fixture() -> Result<()> {
    let repo = TestRepo::from_fixture("mixed_repo_with_onec_subtree")?;

    assert!(repo.path().join("package.json").is_file());
    assert!(repo.path().join("src/index.ts").is_file());
    assert!(repo.path().join("erp/Configuration.xml").is_file());
    assert!(repo
        .path()
        .join("erp/Documents/SalesOrder/Ext/ObjectModule.bsl")
        .is_file());

    Ok(())
}

#[test]
fn search_tools_surface_normalized_onec_documents() -> Result<()> {
    let harness = TestHarness::with_fixture("onec_config_dump")?;
    wait_for_repo(&harness)?;

    let search_output = harness.call_tool_text(
        "search_code",
        json!({
            "repo": harness.repo_name(),
            "query": "Synthetic Document: onec://Catalogs/Products#bundle",
            "max_results": 10
        }),
    )?;
    assert!(search_output.contains("Catalogs/Products/Products.xml"));
    assert!(search_output.contains("Synthetic Document: onec://Catalogs/Products#bundle"));

    let chunk_output = harness.call_tool_text(
        "search_chunks",
        json!({
            "repo": harness.repo_name(),
            "query": "Summary Document onec Catalogs Products",
            "chunk_type": "module",
            "max_results": 10
        }),
    )?;
    assert!(chunk_output.contains("Catalogs/Products/Products.xml"));
    assert!(chunk_output.contains("Synthetic Document: onec://Catalogs/Products#bundle"));

    Ok(())
}

#[test]
fn similarity_searches_include_normalized_onec_bundles() -> Result<()> {
    let harness = TestHarness::with_fixture("onec_config_dump")?;
    wait_for_repo(&harness)?;

    let hybrid_output = harness.call_tool_text(
        "hybrid_search",
        json!({
            "repo": harness.repo_name(),
            "query": "Summary Document onec Catalogs Products Form Metadata ItemForm",
            "max_results": 10
        }),
    )?;
    assert!(hybrid_output.contains("Catalogs/Products/Products.xml"));
    assert!(hybrid_output.contains("Hybrid Search Results"));

    let similar_output = harness.call_tool_text(
        "find_similar_code",
        json!({
            "repo": harness.repo_name(),
            "query": "Synthetic Document: onec://Catalogs/Products#bundle\nForm Metadata: Catalogs/Products/Forms/ItemForm.xml",
            "max_results": 10
        }),
    )?;
    assert!(similar_output.contains("Catalogs/Products/Products.xml"));
    assert!(similar_output.contains("Synthetic Document: onec://Catalogs/Products#bundle"));

    Ok(())
}
