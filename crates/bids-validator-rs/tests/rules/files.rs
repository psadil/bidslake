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

/// The `extensions` array on a file rule was parsed and never read, so a rule was chosen by its
/// suffix alone and every rule sharing a suffix list scored identically. BEP-011 makes that
/// concrete: `thickness` belongs to a `.shape.gii` rule that requires `hemi` and a `.dscalar.nii`
/// rule that does not, so without this check a GIFTI file was judged against whichever rule the
/// walk reached first and the requirement never bit.
#[tokio::test]
async fn a_surface_map_missing_its_hemisphere_is_reported() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "d", "BIDSVersion": "1.11.1", "DatasetType": "derivative",
            "GeneratedBy": [{"Name": "x"}]}"#,
    )
    .unwrap();
    let anat = tmp.join("sub-01/anat");
    fs::create_dir_all(&anat).unwrap();
    fs::write(anat.join("sub-01_thickness.shape.gii"), [0u8; 8]).unwrap();

    let issues = bids_validator_rs::validator::validate(
        &tmp,
        &bids_validator_rs::schema::BidsSchema::bundled().unwrap(),
        None,
    )
    .await
    .unwrap();

    let reported: Vec<&str> = issues
        .issues
        .iter()
        .filter(|i| i.sub_code.as_deref() == Some("hemisphere"))
        .map(|i| i.location.as_str())
        .collect();

    assert_eq!(reported, ["/sub-01/anat/sub-01_thickness.shape.gii"]);
}

/// The other side: the CIFTI form of the same measure carries no hemisphere, legitimately. Note
/// what this exercises — BEP-011's rules declare no `space`, so this filename fits *no* rule on
/// entities, the second narrowing pass empties, and the guard restores the full set rather than
/// leaving the file unjudged. It then has to come out clean anyway.
#[tokio::test]
async fn the_cifti_form_of_the_same_measure_needs_no_hemisphere() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "d", "BIDSVersion": "1.11.1", "DatasetType": "derivative",
            "GeneratedBy": [{"Name": "x"}]}"#,
    )
    .unwrap();
    let anat = tmp.join("sub-01/anat");
    fs::create_dir_all(&anat).unwrap();
    fs::write(
        anat.join("sub-01_space-fsLR_thickness.dscalar.nii"),
        [0u8; 8],
    )
    .unwrap();

    let issues = bids_validator_rs::validator::validate(
        &tmp,
        &bids_validator_rs::schema::BidsSchema::bundled().unwrap(),
        None,
    )
    .await
    .unwrap();

    let reported: Vec<&str> = issues
        .issues
        .iter()
        .filter(|i| i.code == "EXTENSION_MISMATCH" || i.sub_code.as_deref() == Some("hemisphere"))
        .map(|i| i.code.as_str())
        .collect();

    assert!(reported.is_empty(), "reported: {reported:?}");
}

/// A pseudo-file's extension is declared with a trailing slash because it names a directory
/// (`.ds/`, `.mefd/`, `.ome.zarr/`) while the parsed extension of the path has none. Comparing the
/// two spellings literally reported every CTF, MEF3 and OME-Zarr recording in `bids-examples` as
/// a mismatched extension — five integration tests caught it, and this is the unit-level guard.
#[tokio::test]
async fn a_pseudo_file_directory_is_not_an_extension_mismatch() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "ctf", "BIDSVersion": "1.11.1"}"#,
    )
    .unwrap();
    let meg = tmp.join("sub-01/meg/sub-01_task-rest_meg.ds");
    fs::create_dir_all(&meg).unwrap();
    fs::write(meg.join("sub-01_task-rest_meg.res4"), [0u8; 8]).unwrap();

    let issues = bids_validator_rs::validator::validate(
        &tmp,
        &bids_validator_rs::schema::BidsSchema::bundled().unwrap(),
        None,
    )
    .await
    .unwrap();

    let reported: Vec<&str> = issues
        .issues
        .iter()
        .filter(|i| i.code == "EXTENSION_MISMATCH")
        .map(|i| i.location.as_str())
        .collect();

    assert!(reported.is_empty(), "reported: {reported:?}");
}

/// The third of the reference validator's four `ruleChecks`, and the last one this crate was
/// missing. A rule's `datatypes` array was consulted only when identifying a *stem* rule; a suffix
/// rule applied wherever its suffix appeared, so a BOLD run filed under `anat/` was accepted.
#[tokio::test]
async fn a_file_in_the_wrong_datatype_directory_is_reported() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "d", "BIDSVersion": "1.11.1"}"#,
    )
    .unwrap();
    let anat = tmp.join("sub-01/anat");
    fs::create_dir_all(&anat).unwrap();
    fs::write(anat.join("sub-01_task-rest_bold.nii.gz"), [0u8; 8]).unwrap();

    let issues = bids_validator_rs::validator::validate(
        &tmp,
        &bids_validator_rs::schema::BidsSchema::bundled().unwrap(),
        None,
    )
    .await
    .unwrap();

    let reported: Vec<&str> = issues
        .issues
        .iter()
        .filter(|i| i.code == "DATATYPE_MISMATCH")
        .map(|i| i.location.as_str())
        .collect();

    assert_eq!(reported, ["/sub-01/anat/sub-01_task-rest_bold.nii.gz"]);
}

/// Why the datatype narrowing pass runs first, and why it is discarded when it empties.
///
/// Six rules carry the `electrodes` suffix. Two are unspecialized parents rendered with
/// `datatypes: []` — matching nothing — and the specializations declare `[eeg, ieeg]`, `[meg]` and
/// `[emg]`. Narrowing by datatype leaves only the `[eeg, ieeg]` rule, so the empty parents never
/// reach the checks; without that pass they would put `DATATYPE_MISMATCH` on every electrodes file
/// in the corpus.
#[tokio::test]
async fn an_unspecialized_parent_rule_loses_to_the_one_for_this_datatype() {
    let tmp = tempdir();
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "d", "BIDSVersion": "1.11.1"}"#,
    )
    .unwrap();
    let ieeg = tmp.join("sub-01/ieeg");
    fs::create_dir_all(&ieeg).unwrap();
    fs::write(
        ieeg.join("sub-01_electrodes.tsv"),
        "name\tx\ty\tz\nA1\t1\t2\t3\n",
    )
    .unwrap();

    let issues = bids_validator_rs::validator::validate(
        &tmp,
        &bids_validator_rs::schema::BidsSchema::bundled().unwrap(),
        None,
    )
    .await
    .unwrap();

    let reported: Vec<&str> = issues
        .issues
        .iter()
        .filter(|i| i.code == "DATATYPE_MISMATCH")
        .map(|i| i.location.as_str())
        .collect();

    assert!(reported.is_empty(), "reported: {reported:?}");
}
