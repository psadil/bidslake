use super::super::common::{tempdir, validate_dataset};
use std::fs;

#[tokio::test]
async fn test_missing_subject_directory() {
    let tmp = tempdir();
    // dataset_description.json is required to make it a raw dataset
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    )
    .unwrap();

    // No sub-* directories exist
    let issues = validate_dataset(&tmp).await;

    // The subject directory is required for raw datasets
    let missing_req_dir = issues.issues.iter().any(|i| {
        i.code == "MISSING_REQUIRED_DIRECTORY" && i.message.contains("'subject' is missing")
    });
    let subject_folders = issues
        .issues
        .iter()
        .any(|i| i.code == "SUBJECT_FOLDERS" && i.message.contains("no subject directories"));

    assert!(
        missing_req_dir,
        "Expected MISSING_REQUIRED_DIRECTORY error for subject"
    );
    assert!(subject_folders, "Expected SUBJECT_FOLDERS warning");
}

#[tokio::test]
async fn test_missing_datatype_directory() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    )
    .unwrap();

    let sub_dir = tmp.join("sub-01");
    fs::create_dir_all(&sub_dir).unwrap();
    // No datatype (anat, func, etc) directory inside sub-01
    fs::write(sub_dir.join("sub-01_task-rest_bold.json"), "{}").unwrap(); // A file, but not in datatype dir

    let issues = validate_dataset(&tmp).await;

    // For raw datasets, datatype is required inside subject or session
    let missing_req_dir = issues.issues.iter().any(|i| {
        i.code == "MISSING_REQUIRED_DIRECTORY" && i.message.contains("'datatype' is missing")
    });
    assert!(
        missing_req_dir,
        "Expected MISSING_REQUIRED_DIRECTORY error for datatype"
    );
}
