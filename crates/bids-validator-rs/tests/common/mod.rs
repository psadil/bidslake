use bids_validator_rs::issues::DatasetIssues;
use bids_validator_rs::schema::BidsSchema;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub fn tempdir() -> PathBuf {
    let count = TEMP_DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "bids_validator_test_{}_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        count
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

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
