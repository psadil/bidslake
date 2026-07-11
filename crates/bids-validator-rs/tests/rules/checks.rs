use super::super::common::{
    create_minimal_dataset, create_nifti1_header, tempdir, validate_dataset,
};
use std::fs;

#[tokio::test]
async fn test_nifti_dimension() {
    let root = tempdir();
    create_minimal_dataset(&root);
    let anat_dir = root.join("sub-01").join("anat");

    let header = create_nifti1_header(
        &[0, 1, 1, 1, 1, 1, 1, 1],
        &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
        2,
        1,
        0,
        None,
    );
    fs::write(anat_dir.join("sub-01_T2w.nii"), header).unwrap();

    let issues = validate_dataset(&root).await;

    let has_dim = issues.issues.iter().any(|i| i.code == "NIFTI_DIMENSION");
    assert!(has_dim, "Expected NIFTI_DIMENSION for empty shape");
}

#[tokio::test]
async fn test_nifti_unit() {
    let root = tempdir();
    create_minimal_dataset(&root);
    let anat_dir = root.join("sub-01").join("anat");

    let header = create_nifti1_header(
        &[3, 2, 2, 2, 1, 1, 1, 1],
        &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
        0,
        1,
        0,
        None,
    );
    fs::write(anat_dir.join("sub-01_T2w.nii"), header).unwrap();

    let issues = validate_dataset(&root).await;

    let has_unit = issues.issues.iter().any(|i| i.code == "NIFTI_UNIT");
    assert!(has_unit, "Expected NIFTI_UNIT for unknown xyzt_units");
}

#[tokio::test]
async fn test_nifti_pixdim() {
    let root = tempdir();
    create_minimal_dataset(&root);
    let anat_dir = root.join("sub-01").join("anat");

    let header = create_nifti1_header(
        &[3, 2, 2, 2, 1, 1, 1, 1],
        &[1.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
        2,
        1,
        0,
        None,
    );
    fs::write(anat_dir.join("sub-01_T2w.nii"), header).unwrap();

    let issues = validate_dataset(&root).await;

    let has_pixdim = issues.issues.iter().any(|i| i.code == "NIFTI_PIXDIM");
    assert!(has_pixdim, "Expected NIFTI_PIXDIM for 0.0 voxel size");
}

#[tokio::test]
async fn test_sform_qform_zero() {
    let root = tempdir();
    create_minimal_dataset(&root);
    let anat_dir = root.join("sub-01").join("anat");

    let header = create_nifti1_header(
        &[3, 2, 2, 2, 1, 1, 1, 1],
        &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
        2,
        0,
        0,
        None,
    );
    fs::write(anat_dir.join("sub-01_T2w.nii"), header).unwrap();

    let issues = validate_dataset(&root).await;

    let has_sform = issues
        .issues
        .iter()
        .any(|i| i.code == "SFORM_AND_QFORM_IN_IMAGE_HEADER_ARE_ZERO");
    assert!(
        has_sform,
        "Expected SFORM_AND_QFORM_IN_IMAGE_HEADER_ARE_ZERO"
    );
}

#[tokio::test]
async fn test_anat_not_3d() {
    let root = tempdir();
    create_minimal_dataset(&root);
    let anat_dir = root.join("sub-01").join("anat");

    let header = create_nifti1_header(
        &[4, 2, 2, 2, 2, 1, 1, 1],
        &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
        2,
        1,
        0,
        None,
    );
    fs::write(anat_dir.join("sub-01_T1w.nii"), header).unwrap();

    let issues = validate_dataset(&root).await;

    let has_anat = issues
        .issues
        .iter()
        .any(|i| i.code == "T1W_FILE_WITH_TOO_MANY_DIMENSIONS");
    assert!(has_anat, "Expected T1W_FILE_WITH_TOO_MANY_DIMENSIONS");
}

#[tokio::test]
async fn test_bold_not_4d() {
    let root = tempdir();
    create_minimal_dataset(&root);
    let func_dir = root.join("sub-01").join("func");
    fs::create_dir_all(&func_dir).unwrap();

    let header = create_nifti1_header(
        &[3, 2, 2, 2, 1, 1, 1, 1],
        &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
        2,
        1,
        0,
        None,
    );
    fs::write(func_dir.join("sub-01_task-rest_bold.nii"), header).unwrap();

    let issues = validate_dataset(&root).await;

    let has_bold = issues.issues.iter().any(|i| i.code == "BOLD_NOT_4D");
    assert!(has_bold, "Expected BOLD_NOT_4D");
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
