use super::super::common::{create_minimal_dataset, tempdir, validate_dataset};
use rstest::rstest;
use std::fs;

/// One bad file per case: write it into an otherwise-valid dataset, validate, and require the
/// code it should raise.
///
/// These were twelve functions whose bodies were the same four statements, differing only in
/// where the file lands, what goes in it, and the code expected back — so a rule that stopped
/// firing cost one failing test out of twelve and said nothing about the other eleven. As cases
/// they report independently. `test_malformed_bval_bvec` wrote both b-files and asserted both
/// codes at once; each file now stands alone, which is what the two codes claim to be about.
/// `B_FILE` gets one case per disjunct of its `has_double_spaces || has_non_numeric` for the
/// same reason: written together, either half could stop firing unnoticed.
#[rstest]
#[case::empty_file("sub-01/anat/sub-01_T2w.json", b"", "EMPTY_FILE")]
#[case::nifti_too_small("sub-01/anat/sub-01_T2w.nii", &[0u8; 100], "NIFTI_TOO_SMALL")]
// Large enough to hold a NIfTI-1 header, but every field in it is zero.
#[case::nifti_header_unreadable("sub-01/anat/sub-01_T2w.nii", &[0u8; 600], "NIFTI_HEADER_UNREADABLE")]
#[case::json_invalid(
    "dataset_description.json",
    br#"{"Name": "Test", "BIDSVersion": "1.8.0", }"#,
    "JSON_INVALID"
)]
#[case::gz_not_gzipped(
    "sub-01/anat/sub-01_T2w.nii.gz",
    b"Not a gzipped file!",
    "GZ_NOT_GZIPPED"
)]
// A sidecar with no image beside it.
#[case::sidecar_without_datafile(
    "sub-01/func/sub-01_task-rest_bold.json",
    b"{}",
    "SIDECAR_WITHOUT_DATAFILE"
)]
#[case::malformed_bval("sub-01/dwi/sub-01_dwi.bval", &[0xFFu8, 0xFE], "MALFORMED_BVAL")]
#[case::malformed_bvec("sub-01/dwi/sub-01_dwi.bvec", &[0xFFu8, 0xFE], "MALFORMED_BVEC")]
// Three rows, and the middle one is a column short.
#[case::bvec_row_length(
    "sub-01/dwi/sub-01_dwi.bvec",
    b"1 0 0\n0 1\n0 0 1\n",
    "BVEC_ROW_LENGTH"
)]
// The two independent halves of `B_FILE`'s `has_double_spaces || has_non_numeric`.
#[case::bfile_double_space("sub-01/dwi/sub-01_dwi.bval", b"1000  0\n", "B_FILE")]
#[case::bfile_non_numeric("sub-01/dwi/sub-01_dwi.bvec", b"1 a 0\n0 1 0\n0 0 1\n", "B_FILE")]
// sub-02's data sits under a session; the fixture's sub-01 has none.
#[case::missing_session("sub-02/ses-01/anat/sub-02_ses-01_T1w.nii", &[0u8; 348], "MISSING_SESSION")]
#[case::invalid_json_encoding(
    "sub-01/anat/sub-01_T1w.json",
    &[0xFFu8, 0xFE, 0xFD],
    "INVALID_JSON_ENCODING"
)]
// A TSV whose rows end in a bare carriage return.
#[case::wrong_new_line(
    "participants.tsv",
    b"participant_id\tage\rsub-01\t25\r",
    "WRONG_NEW_LINE"
)]
// A .vhdr with neither the .eeg nor the .vmrk it points at.
#[case::brainvision_links_broken(
    "sub-01/eeg/sub-01_task-rest_eeg.vhdr",
    b"vhdr",
    "BRAINVISION_LINKS_BROKEN"
)]
#[tokio::test]
async fn a_bad_file_raises_its_code(
    #[case] rel: &str,
    #[case] content: &[u8],
    #[case] expected: &str,
) {
    let root = tempdir();
    create_minimal_dataset(&root);
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, content).unwrap();

    let issues = validate_dataset(&root).await;

    assert!(
        issues.issues.iter().any(|i| i.code == expected),
        "expected {expected} for {rel}, got {:?}",
        issues.issues.iter().map(|i| &i.code).collect::<Vec<_>>()
    );
}

/// A subject directory with nothing in it — the one case above that writes no file at all.
#[tokio::test]
async fn an_empty_subject_directory_raises_no_valid_data_found() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);
    fs::create_dir_all(tmp.join("sub-02")).unwrap();

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
async fn test_orphaned_symlink() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);

    // Instead of relying on read_file_tree (which drops broken symlinks via `ignore` crate),
    // we can directly test the validator manually.
    let link_path = tmp.join("sub-01").join("anat").join("sub-01_T1w.nii");
    // The fixture writes a real T1w image there; this test wants a broken link in its place.
    fs::remove_file(&link_path).unwrap();
    std::os::unix::fs::symlink(tmp.join("does_not_exist.nii"), &link_path).unwrap();

    let issues = validate_dataset(&tmp).await;

    let orphaned = issues.issues.iter().any(|i| i.code == "ORPHANED_SYMLINK");
    assert!(orphaned, "Expected ORPHANED_SYMLINK");
}

#[tokio::test]
async fn test_file_read() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);

    use std::os::unix::fs::PermissionsExt;
    // Create a file without read permissions
    let unreadable_path = tmp.join("sub-01").join("anat").join("sub-01_T1w.nii");
    fs::write(&unreadable_path, vec![0u8; 348]).unwrap();
    let mut perms = fs::metadata(&unreadable_path).unwrap().permissions();
    perms.set_mode(0o000); // No read permissions
    fs::set_permissions(&unreadable_path, perms).unwrap();

    let issues = validate_dataset(&tmp).await;
    let file_read = issues.issues.iter().any(|i| i.code == "FILE_READ");

    // Restore permissions so `TempDir`'s drop glue can remove the tree.
    let mut perms = fs::metadata(&unreadable_path).unwrap().permissions();
    perms.set_mode(0o644);
    fs::set_permissions(&unreadable_path, perms).unwrap();

    assert!(file_read, "Expected FILE_READ");
}
