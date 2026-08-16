//! Headerless continuous recordings: the `_motion.tsv` path end to end.
//!
//! A recording has no header row, so its column names come from outside the file —
//! for `motion`, from the `name` column of the `_channels.tsv` beside it, in that
//! file's line order. That correspondence is the whole contract: column N of the
//! recording is channel N of the channels file, and nothing in the recording itself
//! records which is which.
//!
//! The vendored corpus cannot cover this — every `*_motion.tsv` in `bids-examples` is
//! a 1-byte placeholder — so the tree is synthetic, as in `test_overlay.rs`.

mod common;

use common::{count, ingest};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Channel names in deliberately non-alphabetical order, so "in line order" is
/// actually being tested rather than coincidentally matching a sort.
const CHANNELS: [&str; 3] = ["t_z", "t_x", "t_y"];

fn write_motion_tree(root: &Path) {
    let motion = root.join("sub-01/motion");
    fs::create_dir_all(&motion).unwrap();
    fs::write(
        root.join("dataset_description.json"),
        r#"{"Name":"motion test","BIDSVersion":"1.11.1"}"#,
    )
    .unwrap();

    let stem = "sub-01_task-walk_tracksys-imu";
    fs::write(
        motion.join(format!("{stem}_channels.tsv")),
        format!(
            "name\tcomponent\ttype\ttracked_point\tunits\n\
             {}\tz\tPOS\tLeftFoot\tm\n\
             {}\tx\tPOS\tLeftFoot\tm\n\
             {}\ty\tPOS\tLeftFoot\tm\n",
            CHANNELS[0], CHANNELS[1], CHANNELS[2]
        ),
    )
    .unwrap();

    // Headerless: the first line is already data. Values are chosen so each cell
    // identifies its (row, column) unambiguously.
    fs::write(
        motion.join(format!("{stem}_motion.tsv")),
        "10.0\t11.0\t12.0\n20.0\t21.0\t22.0\n30.0\t31.0\t32.0\n",
    )
    .unwrap();
    fs::write(
        motion.join(format!("{stem}_motion.json")),
        r#"{"SamplingFrequency":100,"TaskName":"walk"}"#,
    )
    .unwrap();
}

/// The recording's columns are the channels file's `name` column, in its line order,
/// and row N of the recording is sample N.
#[tokio::test]
async fn a_motion_recording_is_keyed_by_its_channels_file() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_motion_tree(dir.path());
    let db = ingest(dir.path()).await?;

    assert_eq!(count(&db, "motion_channels")?, 3, "3 channels ingested");
    assert_eq!(count(&db, "motion")?, 3, "3 samples ingested");

    // `motion` is a bare table: no schema-declared columns, so every value lands in
    // `other_data` keyed by channel name.
    let rows: Vec<(i64, String)> = {
        let mut stmt = db
            .conn
            .prepare("SELECT row_idx, other_data::VARCHAR FROM motion ORDER BY row_idx")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    assert_eq!(
        rows.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "row_idx records the recording's line order"
    );

    // The key set is exactly the channel names — not a positional `column_0`, and not
    // the channels file's own header.
    let first: HashMap<String, String> = serde_json::from_str(&rows[0].1)?;
    let mut keys: Vec<&String> = first.keys().collect();
    keys.sort();
    let mut expected: Vec<&str> = CHANNELS.to_vec();
    expected.sort();
    assert_eq!(keys, expected, "columns are named by the channels file");

    // ...and in the channels file's *line* order, which is the load-bearing part:
    // `t_z` is channel 0, so it takes the first column of every sample.
    assert_eq!(first["t_z"], "10.0");
    assert_eq!(first["t_x"], "11.0");
    assert_eq!(first["t_y"], "12.0");

    let last: HashMap<String, String> = serde_json::from_str(&rows[2].1)?;
    assert_eq!(last["t_z"], "30.0", "row 2 is the third sample");

    Ok(())
}

/// The recording table carries no typed columns of its own — the property that makes
/// it "bare", and the reason its values are JSON strings rather than DOUBLEs.
#[tokio::test]
async fn a_bare_recording_table_has_only_its_structural_columns() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_motion_tree(dir.path());
    let db = ingest(dir.path()).await?;

    let cols: Vec<String> = {
        let mut stmt = db.conn.prepare(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_name = 'motion' ORDER BY ordinal_position",
        )?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?
    };
    assert_eq!(cols, vec!["file_id", "row_idx", "other_data"]);
    Ok(())
}
