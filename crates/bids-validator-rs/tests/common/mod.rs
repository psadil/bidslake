use bids_validator_rs::issues::DatasetIssues;
use bids_validator_rs::schema::BidsSchema;
use std::fs;
use std::path::Path;

/// A temporary directory that removes itself.
///
/// Derefs to `Path`, so it is used exactly like the `PathBuf` this replaced — `root.join(..)`
/// and `&root` both still work — while the drop glue does what the old hand-rolled version
/// never did. That version built a path from a timestamp and a counter, `create_dir_all`'d
/// it, and returned it by value with no owner, so each of the ~45 tests calling it left its
/// tree (NIfTI fixtures included) in the system temp directory on every run.
///
/// `tempfile` was already a declared dev-dependency of this crate, and unused.
pub struct TestDir(tempfile::TempDir);

impl std::ops::Deref for TestDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.0.path()
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        self.0.path()
    }
}

pub fn tempdir() -> TestDir {
    TestDir(tempfile::tempdir().expect("temp dir should be creatable"))
}

/// A dataset the validator accepts: the errors a test asserts should be the ones it wrote.
///
/// The T1w *image* matters. Writing only the sidecar left every dataset built from here
/// carrying a `SIDECAR_WITHOUT_DATAFILE` error — invisible to the ~27 tests that ask whether
/// one specific code is present, and the reason `test_minimal_valid_dataset` could assert
/// nothing for so long without anyone noticing. A test that wants a *broken* T1w overwrites
/// this file, which is what the `rules/checks.rs` family already does.
pub fn create_minimal_dataset(root: &Path) {
    fs::write(
        root.join("dataset_description.json"),
        r#"{"Name": "Test Dataset", "BIDSVersion": "1.10.1", "DatasetType": "raw"}"#,
    )
    .unwrap();

    fs::write(
        root.join("participants.tsv"),
        "participant_id\tage\tsex\nsub-01\t25\tM\n",
    )
    .unwrap();

    let anat_dir = root.join("sub-01").join("anat");
    fs::create_dir_all(&anat_dir).unwrap();

    fs::write(
        anat_dir.join("sub-01_T1w.json"),
        r#"{"RepetitionTime": 2.0, "MagneticFieldStrength": 3}"#,
    )
    .unwrap();

    // 3D, unit millimetres, qform set — the combination the `rules/checks.rs` cases
    // deliberately perturb one field at a time to trigger their codes.
    fs::write(
        anat_dir.join("sub-01_T1w.nii"),
        create_nifti1_header(
            &[3, 2, 2, 2, 1, 1, 1, 1],
            &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
            2,
            1,
            0,
            None,
        ),
    )
    .unwrap();
}

pub async fn validate_dataset(root: &Path) -> DatasetIssues {
    let schema = BidsSchema::bundled().unwrap();
    bids_validator_rs::validator::validate(root, &schema, None)
        .await
        .unwrap()
}

pub fn create_nifti1_header(
    dim: &[i16; 8],
    pixdim: &[f32; 8],
    xyzt_units: u8,
    qform_code: i16,
    sform_code: i16,
    srow: Option<([f32; 4], [f32; 4], [f32; 4])>,
) -> Vec<u8> {
    let mut buf = vec![0u8; 348];
    buf[0..4].copy_from_slice(&348i32.to_le_bytes());
    for i in 0..8 {
        buf[40 + i * 2..42 + i * 2].copy_from_slice(&dim[i].to_le_bytes());
    }
    for i in 0..8 {
        buf[76 + i * 4..80 + i * 4].copy_from_slice(&pixdim[i].to_le_bytes());
    }
    buf[123] = xyzt_units;
    buf[252..254].copy_from_slice(&qform_code.to_le_bytes());
    buf[254..256].copy_from_slice(&sform_code.to_le_bytes());

    if let Some((sx, sy, sz)) = srow {
        for i in 0..4 {
            buf[280 + i * 4..284 + i * 4].copy_from_slice(&sx[i].to_le_bytes());
            buf[296 + i * 4..300 + i * 4].copy_from_slice(&sy[i].to_le_bytes());
            buf[312 + i * 4..316 + i * 4].copy_from_slice(&sz[i].to_le_bytes());
        }
    }
    buf
}
