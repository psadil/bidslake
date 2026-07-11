use super::super::common::{create_minimal_dataset, tempdir, validate_dataset};
use std::fs;

#[tokio::test]
async fn test_empty_file_detection() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);

    let empty_path = tmp.join("sub-01").join("anat").join("sub-01_T2w.json");
    fs::write(&empty_path, "").unwrap();

    let issues = validate_dataset(&tmp).await;
    let empty_issues: Vec<_> = issues
        .all()
        .iter()
        .filter(|i| i.code == "EMPTY_FILE")
        .collect();
    assert!(!empty_issues.is_empty(), "Should detect the empty file");
}

#[tokio::test]
async fn test_nifti_too_small() {
    let root = tempdir();
    create_minimal_dataset(&root);

    let anat_dir = root.join("sub-01").join("anat");
    fs::write(anat_dir.join("sub-01_T2w.nii"), vec![0u8; 100]).unwrap();

    let issues = validate_dataset(&root).await;

    let has_too_small = issues.issues.iter().any(|i| i.code == "NIFTI_TOO_SMALL");
    assert!(
        has_too_small,
        "Expected NIFTI_TOO_SMALL issue for 100 byte file"
    );
}

#[tokio::test]
async fn test_nifti_header_unreadable() {
    let root = tempdir();
    create_minimal_dataset(&root);

    let anat_dir = root.join("sub-01").join("anat");
    fs::write(anat_dir.join("sub-01_T2w.nii"), vec![0u8; 600]).unwrap();

    let issues = validate_dataset(&root).await;

    let has_unreadable = issues
        .issues
        .iter()
        .any(|i| i.code == "NIFTI_HEADER_UNREADABLE");
    assert!(
        has_unreadable,
        "Expected NIFTI_HEADER_UNREADABLE issue for file with invalid header"
    );
}

#[tokio::test]
async fn test_json_invalid() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);

    // Write malformed JSON
    fs::write(
        tmp.join("dataset_description.json"),
        r#"{"Name": "Test", "BIDSVersion": "1.8.0", }"#,
    )
    .unwrap();

    let issues = validate_dataset(&tmp).await;
    let json_invalid = issues.issues.iter().any(|i| i.code == "JSON_INVALID");
    assert!(json_invalid, "Expected JSON_INVALID for trailing comma");
}

#[tokio::test]
async fn test_gz_not_gzipped() {
    let root = tempdir();
    create_minimal_dataset(&root);

    let anat_dir = root.join("sub-01").join("anat");
    // .nii.gz file with no gzip header
    fs::write(anat_dir.join("sub-01_T2w.nii.gz"), "Not a gzipped file!").unwrap();

    let issues = validate_dataset(&root).await;

    let has_gz = issues.issues.iter().any(|i| i.code == "GZ_NOT_GZIPPED");
    assert!(has_gz, "Expected GZ_NOT_GZIPPED for plain text .gz file");
}

#[tokio::test]
async fn test_sidecar_without_datafile() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);

    let func_dir = tmp.join("sub-01").join("func");
    fs::create_dir_all(&func_dir).unwrap();
    // Create sidecar without matching nifti
    fs::write(func_dir.join("sub-01_task-rest_bold.json"), "{}").unwrap();

    let issues = validate_dataset(&tmp).await;
    println!("DEBUG: {:#?}", issues);
    let sidecar_issue = issues
        .issues
        .iter()
        .any(|i| i.code == "SIDECAR_WITHOUT_DATAFILE");
    assert!(
        sidecar_issue,
        "Expected SIDECAR_WITHOUT_DATAFILE for orphaned sidecar"
    );
}

#[tokio::test]
async fn test_malformed_bval_bvec() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);
    let dwi_dir = tmp.join("sub-01").join("dwi");
    fs::create_dir_all(&dwi_dir).unwrap();

    fs::write(dwi_dir.join("sub-01_dwi.bval"), vec![0xFF, 0xFE]).unwrap();
    fs::write(dwi_dir.join("sub-01_dwi.bvec"), vec![0xFF, 0xFE]).unwrap();

    let issues = validate_dataset(&tmp).await;
    println!("DEBUG malformed bval/bvec: {:#?}", issues);
    let malformed_bval = issues.issues.iter().any(|i| i.code == "MALFORMED_BVAL");
    let malformed_bvec = issues.issues.iter().any(|i| i.code == "MALFORMED_BVEC");

    assert!(malformed_bval, "Expected MALFORMED_BVAL for empty bval");
    assert!(malformed_bvec, "Expected MALFORMED_BVEC for empty bvec");
}

#[tokio::test]
async fn test_bvec_row_length() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);
    let dwi_dir = tmp.join("sub-01").join("dwi");
    fs::create_dir_all(&dwi_dir).unwrap();

    // 3 rows but different lengths (row 2 has 2 elements instead of 3)
    let bad_bvec = "1 0 0\n0 1\n0 0 1\n";
    fs::write(dwi_dir.join("sub-01_dwi.bvec"), bad_bvec).unwrap();

    let issues = validate_dataset(&tmp).await;
    let row_length = issues.issues.iter().any(|i| i.code == "BVEC_ROW_LENGTH");
    assert!(row_length, "Expected BVEC_ROW_LENGTH");
}

#[tokio::test]
async fn test_bfile_invalid() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);
    let dwi_dir = tmp.join("sub-01").join("dwi");
    fs::create_dir_all(&dwi_dir).unwrap();

    // Double space in bval
    fs::write(dwi_dir.join("sub-01_dwi.bval"), "1000  0\n").unwrap();
    // Non-numeric in bvec
    fs::write(dwi_dir.join("sub-01_dwi.bvec"), "1 a 0\n0 1 0\n0 0 1\n").unwrap();

    let issues = validate_dataset(&tmp).await;
    let bfile_issues: Vec<_> = issues
        .issues
        .iter()
        .filter(|i| i.code == "B_FILE")
        .collect();

    assert!(
        !bfile_issues.is_empty(),
        "Expected B_FILE for double spaces or non-numeric"
    );
}

#[tokio::test]
async fn test_missing_session() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);

    // Add sub-02 with a session, but sub-01 has no session
    let sub02_ses_dir = tmp.join("sub-02").join("ses-01").join("anat");
    fs::create_dir_all(&sub02_ses_dir).unwrap();
    fs::write(sub02_ses_dir.join("sub-02_ses-01_T1w.nii"), vec![0u8; 348]).unwrap();
    fs::write(sub02_ses_dir.join("sub-02_ses-01_T1w.json"), "{}").unwrap();

    let issues = validate_dataset(&tmp).await;
    let missing_session = issues.issues.iter().any(|i| i.code == "MISSING_SESSION");
    assert!(
        missing_session,
        "Expected MISSING_SESSION when subjects have inconsistent sessions"
    );
}

#[tokio::test]
async fn test_no_valid_data_found() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);

    // Add sub-02 but no data inside it
    let sub02_dir = tmp.join("sub-02");
    fs::create_dir_all(&sub02_dir).unwrap();

    let issues = validate_dataset(&tmp).await;
    let no_data = issues
        .issues
        .iter()
        .any(|i| i.code == "NO_VALID_DATA_FOUND_FOR_SUBJECT");
    assert!(
        no_data,
        "Expected NO_VALID_DATA_FOUND_FOR_SUBJECT for empty subject directory"
    );
}

#[tokio::test]
async fn test_invalid_json_encoding() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);

    // Write invalid UTF-8 to a JSON file
    let bad_json_path = tmp.join("sub-01").join("anat").join("sub-01_T1w.json");
    fs::write(&bad_json_path, vec![0xFF, 0xFE, 0xFD]).unwrap();

    let issues = validate_dataset(&tmp).await;
    println!("DEBUG invalid json encoding: {:#?}", issues);
    let invalid_encoding = issues
        .issues
        .iter()
        .any(|i| i.code == "INVALID_JSON_ENCODING");
    assert!(invalid_encoding, "Expected INVALID_JSON_ENCODING");
}

#[tokio::test]
async fn test_wrong_new_line() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);

    // Write TSV with \r instead of \n
    let tsv_path = tmp.join("participants.tsv");
    fs::write(&tsv_path, "participant_id\tage\rsub-01\t25\r").unwrap();

    let issues = validate_dataset(&tmp).await;
    println!("DEBUG wrong new line: {:#?}", issues);
    let wrong_newline = issues.issues.iter().any(|i| i.code == "WRONG_NEW_LINE");
    assert!(wrong_newline, "Expected WRONG_NEW_LINE");
}

#[tokio::test]
async fn test_orphaned_symlink() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);

    #[cfg(unix)]
    {
        // Instead of relying on read_file_tree (which drops broken symlinks via `ignore` crate),
        // we can directly test the validator manually.
        let link_path = tmp.join("sub-01").join("anat").join("sub-01_T1w.nii");
        std::os::unix::fs::symlink(tmp.join("does_not_exist.nii"), &link_path).unwrap();

        let issues = validate_dataset(&tmp).await;

        let orphaned = issues.issues.iter().any(|i| i.code == "ORPHANED_SYMLINK");
        assert!(orphaned, "Expected ORPHANED_SYMLINK");
    }
}

#[tokio::test]
async fn test_file_read() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Create a file without read permissions
        let unreadable_path = tmp.join("sub-01").join("anat").join("sub-01_T1w.nii");
        fs::write(&unreadable_path, vec![0u8; 348]).unwrap();
        let mut perms = fs::metadata(&unreadable_path).unwrap().permissions();
        perms.set_mode(0o000); // No read permissions
        fs::set_permissions(&unreadable_path, perms).unwrap();

        let issues = validate_dataset(&tmp).await;
        println!("DEBUG file read: {:#?}", issues);
        let file_read = issues.issues.iter().any(|i| i.code == "FILE_READ");

        // Restore permissions so cleanup doesn't fail
        let mut perms = fs::metadata(&unreadable_path).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&unreadable_path, perms).unwrap();

        assert!(file_read, "Expected FILE_READ");
    }
}

#[tokio::test]
async fn test_brainvision_links_broken() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);
    let eeg_dir = tmp.join("sub-01").join("eeg");
    fs::create_dir_all(&eeg_dir).unwrap();

    // Add just the .vhdr without .eeg or .vmrk
    fs::write(eeg_dir.join("sub-01_task-rest_eeg.vhdr"), "vhdr").unwrap();

    let issues = validate_dataset(&tmp).await;
    let broken_links = issues
        .issues
        .iter()
        .any(|i| i.code == "BRAINVISION_LINKS_BROKEN");
    assert!(broken_links, "Expected BRAINVISION_LINKS_BROKEN");
}
