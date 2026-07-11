use super::super::common::{tempdir, validate_dataset};
use std::fs;

#[tokio::test]
async fn test_events_tsv_missing_onset() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    )
    .unwrap();

    let func_dir = tmp.join("sub-01").join("func");
    fs::create_dir_all(&func_dir).unwrap();

    // Create events.tsv but with missing 'onset' column
    // The validator checks 'columns' in the TSV (parsed by tabular_data.rs which relies on context.columns)
    // Actually our test generator just writes the file contents.
    // Let's write a TSV with only 'duration' and 'trial_type'
    fs::write(
        func_dir.join("sub-01_task-rest_events.tsv"),
        "duration\ttrial_type\n2.0\trest\n",
    )
    .unwrap();

    let issues = validate_dataset(&tmp).await;

    let missing_onset = issues
        .issues
        .iter()
        .any(|i| i.code == "TSV_COLUMN_MISSING" && i.sub_code.as_deref() == Some("onset"));
    assert!(
        missing_onset,
        "Expected Events missing onset error. Issues: {:#?}",
        issues
    );
}

#[tokio::test]
async fn test_participants_tsv_missing_participant_id() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    )
    .unwrap();

    // Create participants.tsv with missing participant_id
    fs::write(tmp.join("participants.tsv"), "age\tsex\n25\tM\n").unwrap();

    let issues = validate_dataset(&tmp).await;

    let missing_id = issues
        .issues
        .iter()
        .any(|i| i.code == "TSV_COLUMN_MISSING" && i.sub_code.as_deref() == Some("participant_id"));
    assert!(
        missing_id,
        "Expected Participants missing participant_id error. Issues: {:#?}",
        issues
    );
}
