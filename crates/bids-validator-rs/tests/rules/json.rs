use super::super::common::{tempdir, validate_dataset};
use std::fs;

#[tokio::test]
async fn test_json_missing_nirs_coordsystem() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    )
    .unwrap();

    let nirs_dir = tmp.join("sub-01").join("nirs");
    fs::create_dir_all(&nirs_dir).unwrap();

    // Create a coordsystem JSON file for nirs
    fs::write(
        nirs_dir.join("sub-01_coordsystem.json"),
        r#"{"NIRSCoordinateUnits": "mm"}"#,
    )
    .unwrap();

    let issues = validate_dataset(&tmp).await;

    let missing_sys = issues.issues.iter().any(|i| {
        i.code == "JSON_KEY_REQUIRED" && i.sub_code.as_deref() == Some("NIRSCoordinateSystem")
    });
    assert!(
        missing_sys,
        "Expected CoordinateSystem missing field error. Issues: {:#?}",
        issues
    );
}
