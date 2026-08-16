use super::super::common::{tempdir, validate_dataset};
use rstest::rstest;
use std::fs;

/// One incomplete piece of dataset metadata per case: write it, validate, and require the
/// sub-code naming the field that is missing.
///
/// These were six functions built from the same three statements — a `dataset_description.json`,
/// sometimes one more file to bring a rule into play, and the sub-code expected back. Three of
/// them asserted two sub-codes each, so the second assertion never ran when the first failed;
/// those are two cases now and report separately.
#[rstest]
#[case::missing_name(r#"{"DatasetType": "raw"}"#, None, "Name")]
#[case::missing_bids_version(r#"{"DatasetType": "raw"}"#, None, "BIDSVersion")]
#[case::derivative_without_generated_by(
    r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "derivative"}"#,
    None,
    "GeneratedBy"
)]
#[case::missing_authors(
    r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    None,
    "Authors"
)]
// The presence of genetic_info.json is what makes the Genetics key required.
#[case::missing_genetics(
    r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    Some(("genetic_info.json", "{}")),
    "Genetics"
)]
#[case::missing_genetic_level(
    r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw", "Genetics": {}}"#,
    Some(("genetic_info.json", "{}")),
    "GeneticLevel"
)]
#[case::missing_sample_origin(
    r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw", "Genetics": {}}"#,
    Some(("genetic_info.json", "{}")),
    "SampleOrigin"
)]
// The schema key is "AtlasName", but its field name (via objects.metadata) is "Name".
#[case::missing_atlas_name(
    r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    Some(("atlas/atlas-Test_description.json", "{}")),
    "Name"
)]
#[case::missing_atlas_license(
    r#"{"Name": "Test", "BIDSVersion": "1.8.0", "DatasetType": "raw"}"#,
    Some(("atlas/atlas-Test_description.json", "{}")),
    "License"
)]
#[tokio::test]
async fn a_missing_metadata_field_raises_its_sub_code(
    #[case] description: &str,
    #[case] extra: Option<(&str, &str)>,
    #[case] expected: &str,
) {
    let tmp = tempdir();
    fs::write(tmp.join("dataset_description.json"), description).unwrap();
    if let Some((rel, contents)) = extra {
        let path = tmp.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
    }

    let issues = validate_dataset(&tmp).await;

    assert!(
        issues
            .issues
            .iter()
            .any(|i| i.sub_code.as_deref() == Some(expected)),
        "expected sub_code {expected}, got {:?}",
        issues
            .issues
            .iter()
            .map(|i| (&i.code, &i.sub_code))
            .collect::<Vec<_>>()
    );
}

/// No `dataset_description.json` at all — the one case with no description to parameterize.
#[tokio::test]
async fn test_missing_dataset_description() {
    let tmp = tempdir();
    let sub_dir = tmp.join("sub-01").join("anat");
    fs::create_dir_all(&sub_dir).unwrap();
    fs::write(sub_dir.join("sub-01_T1w.json"), "{}").unwrap();

    let issues = validate_dataset(&tmp).await;

    assert!(
        issues.has_errors(),
        "Should report error for missing dataset_description.json"
    );
}
