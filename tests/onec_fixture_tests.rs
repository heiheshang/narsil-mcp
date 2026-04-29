mod common;

use anyhow::Result;
use common::TestRepo;

#[test]
fn loads_onec_config_dump_fixture() -> Result<()> {
    let repo = TestRepo::from_fixture("onec_config_dump")?;

    assert!(repo.path().join("Configuration.xml").is_file());
    assert!(repo.path().join("ConfigDumpInfo.xml").is_file());
    assert!(
        repo.path()
            .join("Catalogs/Products/Ext/ManagerModule.bsl")
            .is_file()
    );
    assert!(
        repo.path()
            .join("Catalogs/Products/Forms/ItemForm/Ext/Form/Module.bsl")
            .is_file()
    );

    Ok(())
}

#[test]
fn loads_mixed_repo_with_nested_onec_subtree_fixture() -> Result<()> {
    let repo = TestRepo::from_fixture("mixed_repo_with_onec_subtree")?;

    assert!(repo.path().join("package.json").is_file());
    assert!(repo.path().join("src/index.ts").is_file());
    assert!(repo.path().join("erp/Configuration.xml").is_file());
    assert!(
        repo.path()
            .join("erp/Documents/SalesOrder/Ext/ObjectModule.bsl")
            .is_file()
    );

    Ok(())
}
