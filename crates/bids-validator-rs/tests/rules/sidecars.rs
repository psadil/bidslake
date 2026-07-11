use super::super::common::{tempdir, validate_dataset};
use std::fs;

#[tokio::test]
async fn test_sidecar_missing_required_field() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    )
    .unwrap();

    let perf_dir = tmp.join("sub-01").join("perf");
    fs::create_dir_all(&perf_dir).unwrap();

    // Create an ASL file
    fs::write(perf_dir.join("sub-01_asl.nii.gz"), "mock").unwrap();

    // Create a sidecar missing RepetitionTimePreparation
    fs::write(perf_dir.join("sub-01_asl.json"), r#"{"EchoTime": 0.01}"#).unwrap();

    let issues = validate_dataset(&tmp).await;

    let missing_rtp = issues.issues.iter().any(|i| {
        i.code == "SIDECAR_KEY_REQUIRED"
            && i.sub_code.as_deref() == Some("RepetitionTimePreparation")
    });
    assert!(
        missing_rtp,
        "Expected SIDECAR_KEY_REQUIRED for missing RepetitionTimePreparation. Issues: {:#?}",
        issues
    );
}

#[tokio::test]
async fn test_task_metadata_recommended_field() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    )
    .unwrap();

    let anat_dir = tmp.join("sub-01").join("anat");
    fs::create_dir_all(&anat_dir).unwrap();

    // Create an anat file with task entity
    fs::write(anat_dir.join("sub-01_task-memory_T1w.nii.gz"), "mock").unwrap();

    // Create a sidecar missing TaskName
    fs::write(anat_dir.join("sub-01_task-memory_T1w.json"), r#"{}"#).unwrap();

    let issues = validate_dataset(&tmp).await;

    let missing_task_name = issues
        .issues
        .iter()
        .any(|i| i.code == "SIDECAR_KEY_RECOMMENDED" && i.sub_code.as_deref() == Some("TaskName"));
    assert!(
        missing_task_name,
        "Expected TaskMetadata warning for missing TaskName. Issues: {:#?}",
        issues
    );
}
