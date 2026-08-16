use super::super::common::{tempdir, validate_dataset};
use std::fs;

// `test_entity_parsing_in_context` lived here, calling `bids_validator_rs::entities` — a bare
// re-export of `bids_core::entities` — on `sub-01_ses-pre_task-rest_run-02_bold.nii.gz`. The
// same filename is case `multiple_entities` of
// `bids_core::entities::tests::read_entities_splits_a_bids_filename`, which asserts the whole
// (suffix, extension, entities) triple at once and so also pins that nothing *extra* parsed.

#[tokio::test]
async fn test_invalid_entity_order() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    )
    .unwrap();

    let func_dir = tmp.join("sub-01").join("func");
    fs::create_dir_all(&func_dir).unwrap();
    // Valid order: sub, ses, task, run.
    // Invalid order: sub, task, ses, run.
    fs::write(
        func_dir.join("sub-01_task-rest_ses-01_run-01_bold.nii.gz"),
        "mock",
    )
    .unwrap();

    let issues = validate_dataset(&tmp).await;

    let bad_order = issues
        .issues
        .iter()
        .any(|i| i.code == "ENTITY_ORDER_INCORRECT" && i.message.contains("appears out of order"));
    assert!(
        bad_order,
        "Expected ENTITY_ORDER_INCORRECT. Issues: {:#?}",
        issues
    );
}

#[tokio::test]
async fn test_invalid_entity_value() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    )
    .unwrap();

    let func_dir = tmp.join("sub-01").join("func");
    fs::create_dir_all(&func_dir).unwrap();
    // 'run' expects index format, which is digits only. We give it '01a'.
    fs::write(
        func_dir.join("sub-01_task-rest_run-01a_bold.nii.gz"),
        "mock",
    )
    .unwrap();

    let issues = validate_dataset(&tmp).await;

    let invalid_val = issues
        .issues
        .iter()
        .any(|i| i.code == "INVALID_ENTITY_VALUE" && i.message.contains("invalid value '01a'"));
    assert!(
        invalid_val,
        "Expected INVALID_ENTITY_VALUE. Issues: {:#?}",
        issues
    );
}
