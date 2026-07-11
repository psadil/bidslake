use super::super::common::{tempdir, validate_dataset};
use std::fs;

#[tokio::test]
async fn test_unrecognized_file() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    )
    .unwrap();

    // Create an unrecognized file at the root
    fs::write(tmp.join("unknown_file.txt"), "some content").unwrap();

    let issues = validate_dataset(&tmp).await;

    // It should have NOT_INCLUDED
    let not_included = issues
        .issues
        .iter()
        .any(|i| i.code == "NOT_INCLUDED" && i.location == "/unknown_file.txt");
    assert!(
        not_included,
        "Expected NOT_INCLUDED error for unknown file. Issues: {:#?}",
        issues
    );
}

#[tokio::test]
async fn test_missing_required_entity() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    )
    .unwrap();

    let func_dir = tmp.join("sub-01").join("func");
    fs::create_dir_all(&func_dir).unwrap();
    // Raw bold data file (not a sidecar) is missing 'task' entity
    fs::write(func_dir.join("sub-01_bold.nii.gz"), [0u8; 10]).unwrap();

    let issues = validate_dataset(&tmp).await;

    // There should be a missing entity error for the data file
    let missing_task = issues.issues.iter().any(|i| {
        i.message
            .contains("Required entity 'task' (task) is missing")
            && i.location == "/sub-01/func/sub-01_bold.nii.gz"
    });
    assert!(
        missing_task,
        "Expected missing task entity error for raw bold data file. Issues: {:#?}",
        issues
    );
}
