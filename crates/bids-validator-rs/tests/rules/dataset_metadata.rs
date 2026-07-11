use super::super::common::{tempdir, validate_dataset};
use std::fs;

#[tokio::test]
async fn test_missing_dataset_description() {
    let tmp = tempdir();
    let sub_dir = tmp.join("sub-01").join("anat");
    fs::create_dir_all(&sub_dir).unwrap();
    fs::write(sub_dir.join("sub-01_T1w.json"), "{}").unwrap();

    let issues = validate_dataset(&tmp).await;
    // Missing required fields (Name, BIDSVersion) in dataset_description.json
    // Wait, the file itself is missing, so it should report an error?
    // The previous test checked `has_errors()`. Let's just keep that or check the exact issue.
    assert!(
        issues.has_errors(),
        "Should report error for missing dataset_description.json"
    );
}

#[tokio::test]
async fn test_dataset_description_missing_fields() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"DatasetType": "raw"}"#, // missing Name and BIDSVersion
    )
    .unwrap();

    let issues = validate_dataset(&tmp).await;
    let missing_name = issues
        .issues
        .iter()
        .any(|i| i.sub_code.as_deref() == Some("Name"));
    let missing_bids_version = issues
        .issues
        .iter()
        .any(|i| i.sub_code.as_deref() == Some("BIDSVersion"));
    assert!(missing_name, "Expected Name missing error");
    assert!(missing_bids_version, "Expected BIDSVersion missing error");
}

#[tokio::test]
async fn test_derivative_description() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "derivative"}"#, // missing GeneratedBy
    )
    .unwrap();

    let issues = validate_dataset(&tmp).await;
    let missing_gen_by = issues
        .issues
        .iter()
        .any(|i| i.sub_code.as_deref() == Some("GeneratedBy"));
    assert!(
        missing_gen_by,
        "Expected GeneratedBy missing error for derivative dataset"
    );
}

#[tokio::test]
async fn test_dataset_authors() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#, // missing Authors
    )
    .unwrap();

    let issues = validate_dataset(&tmp).await;
    let missing_authors = issues
        .issues
        .iter()
        .any(|i| i.sub_code.as_deref() == Some("Authors"));
    assert!(
        missing_authors,
        "Expected Authors missing warning (dataset_authors)"
    );
}

#[tokio::test]
async fn test_dataset_description_with_genetics() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#, // missing Genetics
    )
    .unwrap();
    fs::write(tmp.join("genetic_info.json"), r#"{}"#).unwrap(); // triggers rule

    let issues = validate_dataset(&tmp).await;
    let missing_genetics = issues
        .issues
        .iter()
        .any(|i| i.sub_code.as_deref() == Some("Genetics"));
    assert!(missing_genetics, "Expected Genetics missing error");
}

#[tokio::test]
async fn test_genetic_info() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw", "Genetics": {}}"#,
    )
    .unwrap();
    // genetic_info.json missing GeneticLevel and SampleOrigin
    fs::write(tmp.join("genetic_info.json"), r#"{}"#).unwrap();

    let issues = validate_dataset(&tmp).await;
    let missing_level = issues
        .issues
        .iter()
        .any(|i| i.sub_code.as_deref() == Some("GeneticLevel"));
    let missing_origin = issues
        .issues
        .iter()
        .any(|i| i.sub_code.as_deref() == Some("SampleOrigin"));
    assert!(missing_level, "Expected GeneticLevel missing error");
    assert!(missing_origin, "Expected SampleOrigin missing error");
}

#[tokio::test]
async fn test_atlas_description() {
    let tmp = tempdir();
    // minimal valid dataset
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    )
    .unwrap();

    // Create atlas file
    let atlas_dir = tmp.join("atlas");
    fs::create_dir_all(&atlas_dir).unwrap();
    fs::write(atlas_dir.join("atlas-Test_description.json"), r#"{}"#).unwrap(); // missing AtlasName, License

    let issues = validate_dataset(&tmp).await;
    // The schema key is "AtlasName" but its actual field name (via objects.metadata) is "Name"
    let missing_atlas_name = issues
        .issues
        .iter()
        .any(|i| i.sub_code.as_deref() == Some("Name"));
    let missing_license = issues
        .issues
        .iter()
        .any(|i| i.sub_code.as_deref() == Some("License"));
    assert!(
        missing_atlas_name,
        "Expected Name (AtlasName) missing error"
    );
    assert!(missing_license, "Expected License missing error");
}
