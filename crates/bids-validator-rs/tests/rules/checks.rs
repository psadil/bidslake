use super::super::common::{
    create_minimal_dataset, create_nifti1_header, tempdir, validate_dataset,
};
use rstest::rstest;
use std::fs;

/// One malformed NIfTI header per case: write it into an otherwise-valid dataset, validate,
/// and require the code it should raise.
///
/// These were six functions whose bodies were the same six statements, differing only in the
/// header arguments, where the image lands, and the expected code — so a rule that stopped
/// firing used to cost one failing test out of six, and told you nothing about the other
/// five. As cases they report independently, and the full ingest runs once per case either
/// way. `sform` is 0 and `srow` absent throughout; only `qform` varies.
#[rstest]
#[case::empty_shape(
    "sub-01/anat/sub-01_T2w.nii", [0, 1, 1, 1, 1, 1, 1, 1], [1.0; 8], 2, 1, "NIFTI_DIMENSION"
)]
#[case::unknown_units(
    "sub-01/anat/sub-01_T2w.nii", [3, 2, 2, 2, 1, 1, 1, 1], [1.0; 8], 0, 1, "NIFTI_UNIT"
)]
#[case::zero_voxel_size(
    "sub-01/anat/sub-01_T2w.nii",
    [3, 2, 2, 2, 1, 1, 1, 1],
    [1.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
    2, 1, "NIFTI_PIXDIM"
)]
#[case::no_orientation(
    "sub-01/anat/sub-01_T2w.nii",
    [3, 2, 2, 2, 1, 1, 1, 1],
    [1.0; 8],
    2, 0, "SFORM_AND_QFORM_IN_IMAGE_HEADER_ARE_ZERO"
)]
#[case::anat_is_4d(
    "sub-01/anat/sub-01_T1w.nii",
    [4, 2, 2, 2, 2, 1, 1, 1],
    [1.0; 8],
    2, 1, "T1W_FILE_WITH_TOO_MANY_DIMENSIONS"
)]
#[case::bold_is_3d(
    "sub-01/func/sub-01_task-rest_bold.nii", [3, 2, 2, 2, 1, 1, 1, 1], [1.0; 8], 2, 1, "BOLD_NOT_4D"
)]
#[tokio::test]
async fn a_malformed_header_raises_its_code(
    #[case] rel: &str,
    #[case] dim: [i16; 8],
    #[case] pixdim: [f32; 8],
    #[case] xyzt_units: u8,
    #[case] qform: i16,
    #[case] expected: &str,
) {
    let root = tempdir();
    create_minimal_dataset(&root);
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        create_nifti1_header(&dim, &pixdim, xyzt_units, qform, 0, None),
    )
    .unwrap();

    let issues = validate_dataset(&root).await;

    assert!(
        issues.issues.iter().any(|i| i.code == expected),
        "expected {expected} for {rel}, got {:?}",
        issues.issues.iter().map(|i| &i.code).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn test_nifti_pe_direction_consistency() {
    let root = tempdir();
    create_minimal_dataset(&root);
    let func_dir = root.join("sub-01").join("func");
    fs::create_dir_all(&func_dir).unwrap();

    let header = create_nifti1_header(
        &[4, 2, 2, 2, 2, 1, 1, 1],
        &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
        2,
        0,
        1,
        Some((
            [0.0, 1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
        )),
    );
    fs::write(func_dir.join("sub-01_task-rest_dir-AP_bold.nii"), header).unwrap();
    fs::write(
        func_dir.join("sub-01_task-rest_dir-AP_bold.json"),
        r#"{"RepetitionTime": 2.0, "TaskName": "rest", "PhaseEncodingDirection": "i"}"#,
    )
    .unwrap();

    let issues = validate_dataset(&root).await;

    let has_pe_issue = issues
        .issues
        .iter()
        .any(|i| i.code == "NIFTI_PE_DIRECTION_CONSISTENCY");
    assert!(has_pe_issue, "Expected NIFTI_PE_DIRECTION_CONSISTENCY");
}

#[tokio::test]
async fn test_dwi_missing_bvec() {
    let root = tempdir();
    create_minimal_dataset(&root);
    let dwi_dir = root.join("sub-01").join("dwi");
    fs::create_dir_all(&dwi_dir).unwrap();

    // Create DWI NIfTI file
    let header = create_nifti1_header(
        &[4, 2, 2, 2, 2, 1, 1, 1],
        &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
        2,
        1,
        0,
        None,
    );
    fs::write(dwi_dir.join("sub-01_dwi.nii.gz"), header).unwrap();

    // Create bval but no bvec
    fs::write(dwi_dir.join("sub-01_dwi.bval"), "0 1000\n").unwrap();
    // JSON sidecar not strictly required for this specific rule, but good practice
    fs::write(dwi_dir.join("sub-01_dwi.json"), "{}").unwrap();

    let issues = validate_dataset(&root).await;

    let missing_bvec = issues.issues.iter().any(|i| i.code == "DWI_MISSING_BVEC");
    assert!(missing_bvec, "Expected DWI_MISSING_BVEC");
}

#[tokio::test]
async fn test_dwi_missing_bval() {
    let root = tempdir();
    create_minimal_dataset(&root);
    let dwi_dir = root.join("sub-01").join("dwi");
    fs::create_dir_all(&dwi_dir).unwrap();

    // Create DWI NIfTI file
    let header = create_nifti1_header(
        &[4, 2, 2, 2, 2, 1, 1, 1],
        &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
        2,
        1,
        0,
        None,
    );
    fs::write(dwi_dir.join("sub-01_dwi.nii.gz"), header).unwrap();

    // Create bvec but no bval
    fs::write(dwi_dir.join("sub-01_dwi.bvec"), "1 0\n0 1\n0 0\n").unwrap();
    fs::write(dwi_dir.join("sub-01_dwi.json"), "{}").unwrap();

    let issues = validate_dataset(&root).await;

    let missing_bval = issues.issues.iter().any(|i| i.code == "DWI_MISSING_BVAL");
    assert!(missing_bval, "Expected DWI_MISSING_BVAL");
}

#[tokio::test]
async fn test_suspiciously_long_bold_design() {
    let root = tempdir();
    create_minimal_dataset(&root);
    let func_dir = root.join("sub-01").join("func");
    fs::create_dir_all(&func_dir).unwrap();

    // Create BOLD NIfTI file with duration = dim[4] * pixdim[4] = 10 * 2.0 = 20.0
    let header = create_nifti1_header(
        &[4, 2, 2, 2, 10, 1, 1, 1],                // dim[4] = 10
        &[1.0, 1.0, 1.0, 1.0, 2.0, 1.0, 1.0, 1.0], // pixdim[4] = 2.0
        2,
        1,
        0,
        None,
    );
    fs::write(func_dir.join("sub-01_task-rest_bold.nii"), header).unwrap();
    fs::write(
        func_dir.join("sub-01_task-rest_bold.json"),
        "{\"RepetitionTime\": 2.0, \"TaskName\": \"rest\"}",
    )
    .unwrap();

    // Create events TSV with onset > 20.0
    fs::write(
        func_dir.join("sub-01_task-rest_events.tsv"),
        "onset\tduration\ttrial_type\n25.0\t1.0\ttest\n",
    )
    .unwrap();
    fs::write(func_dir.join("sub-01_task-rest_events.json"), "{}").unwrap();

    let issues = validate_dataset(&root).await;

    let suspiciously_long = issues
        .issues
        .iter()
        .any(|i| i.code == "SUSPICIOUSLY_LONG_EVENT_DESIGN");

    if !suspiciously_long {
        for issue in &issues.issues {
            println!("ISSUE: {:?}", issue.code);
        }
    }

    assert!(suspiciously_long, "Expected SUSPICIOUSLY_LONG_EVENT_DESIGN");
}
