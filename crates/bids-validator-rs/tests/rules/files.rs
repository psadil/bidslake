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

/// ADR 0002 §7: a file a configured adapter's term map recognizes is *expected*, not "not part
/// of BIDS". Two places decide `NotIncluded` — `check_file_rules`, which looks the term map up,
/// and the `errors::system::NotIncluded` rule, which re-derives it from `filename_rules` — and
/// for as long as only the first knew about adapters, configuring one suppressed nothing: every
/// file of a `recon-all` tree was still reported.
///
/// The tree here is the smallest one that shows it: two paths the freesurfer term map claims,
/// which no BIDS file rule can match because they carry no entities at all.
#[tokio::test]
async fn adapter_recognized_files_are_not_reported_as_unincluded() {
    use bids_validator_rs::config::ValidatorConfig;

    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "FreeSurfer", "BIDSVersion": "1.8.0", "DatasetType": "derivative",
            "GeneratedBy": [{"Name": "freesurfer"}]}"#,
    )
    .unwrap();
    let subject = tmp.join("sub-01_ses-V1");
    fs::create_dir_all(subject.join("stats")).unwrap();
    fs::create_dir_all(subject.join("surf")).unwrap();
    fs::write(subject.join("stats/aseg.stats"), "# ColHeaders Index\n1\n").unwrap();
    fs::write(subject.join("surf/lh.thickness"), [0u8; 8]).unwrap();
    let config = ValidatorConfig {
        adapters: vec!["freesurfer".to_string()],
        ..Default::default()
    };

    let issues = bids_validator_rs::validator::validate(
        &tmp,
        &bids_validator_rs::schema::BidsSchema::bundled().unwrap(),
        Some(&config),
    )
    .await
    .unwrap();

    let reported: Vec<&str> = issues
        .issues
        .iter()
        .filter(|i| i.code == "NOT_INCLUDED")
        .map(|i| i.location.as_str())
        .collect();
    assert!(reported.is_empty(), "still reported: {reported:?}");
}

/// The other half of the same rule: without the adapter configured, those files *are* unknown.
/// Without this, deleting the term-map lookup entirely would leave the test above passing.
#[tokio::test]
async fn the_same_files_are_unincluded_with_no_adapter_configured() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "FreeSurfer", "BIDSVersion": "1.8.0", "DatasetType": "derivative",
            "GeneratedBy": [{"Name": "freesurfer"}]}"#,
    )
    .unwrap();
    let subject = tmp.join("sub-01_ses-V1");
    fs::create_dir_all(subject.join("stats")).unwrap();
    fs::write(subject.join("stats/aseg.stats"), "# ColHeaders Index\n1\n").unwrap();

    let issues = validate_dataset(&tmp).await;

    let reported = issues
        .issues
        .iter()
        .any(|i| i.code == "NOT_INCLUDED" && i.location == "/sub-01_ses-V1/stats/aseg.stats");
    assert!(reported, "issues: {:#?}", issues.issues);
}
