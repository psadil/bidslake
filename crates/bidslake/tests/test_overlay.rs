//! Schema-augmentation (overlay) integration tests.
//!
//! Builds a tiny synthetic fMRIPrep-style *derivative* dataset and indexes it both
//! with and without the bundled `fmriprep` overlay, proving the overlay is
//! load-bearing: with it, a `desc-confounds_timeseries.tsv` becomes a first-class
//! typed table and the pipeline's non-BIDS `from`/`to`/`mode` transform entities are
//! parsed; without it, the confounds file is recorded `skipped` and no such table
//! exists.

mod common;

use bidslake::schema::{AppliedOverlay, Schema};
use common::{count, ingest_with_schema};
use std::fs;
use std::path::Path;

/// Write the synthetic derivative tree under `root`.
fn write_derivative_tree(root: &Path) {
    let func = root.join("sub-01/func");
    let anat = root.join("sub-01/anat");
    fs::create_dir_all(&func).unwrap();
    fs::create_dir_all(&anat).unwrap();

    fs::write(
        root.join("dataset_description.json"),
        r#"{"Name":"fMRIPrep test derivative","BIDSVersion":"1.11.1","DatasetType":"derivative","GeneratedBy":[{"Name":"fMRIPrep","Version":"23.2.0"}]}"#,
    )
    .unwrap();

    // A preprocessed BOLD scan + sidecar.
    fs::write(func.join("sub-01_task-rest_desc-preproc_bold.nii.gz"), b"").unwrap();
    fs::write(
        func.join("sub-01_task-rest_desc-preproc_bold.json"),
        r#"{"RepetitionTime":2.0,"SkullStripped":true}"#,
    )
    .unwrap();

    // A confounds timeseries: ordered rows (row N == volume N); first FD is n/a.
    fs::write(
        func.join("sub-01_task-rest_desc-confounds_timeseries.tsv"),
        "trans_x\ttrans_y\ttrans_z\tframewise_displacement\tglobal_signal\n\
         0.10\t0.20\t0.30\tn/a\t100.5\n\
         0.11\t0.19\t0.31\t0.05\t100.6\n\
         0.12\t0.18\t0.29\t0.04\t100.4\n",
    )
    .unwrap();

    // A spatial transform whose from/to/mode entities are not in base BIDS.
    fs::write(
        anat.join("sub-01_from-T1w_to-MNI152NLin2009cAsym_mode-image_xfm.h5"),
        b"",
    )
    .unwrap();
}

fn fmriprep_overlay() -> AppliedOverlay {
    AppliedOverlay {
        source: "fmriprep".to_string(),
        content: bids_schema::overlay::bundled_overlay("fmriprep")
            .expect("bundled fmriprep overlay"),
    }
}

#[tokio::test]
async fn overlay_makes_confounds_a_typed_ordered_table() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_derivative_tree(dir.path());

    let schema = Schema::load_with_overlays(None, &[fmriprep_overlay()])?;
    let db = ingest_with_schema(dir.path(), schema).await?;

    // The new table exists with the typed confound columns.
    assert_eq!(
        count(&db, "fmriprep_confounds")?,
        3,
        "3 confound rows ingested"
    );
    let cols: Vec<String> = {
        let mut stmt = db.conn.prepare(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_name = 'fmriprep_confounds' ORDER BY column_name",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<_, _>>()?
    };
    for expected in [
        "trans_x",
        "framewise_displacement",
        "global_signal",
        "row_idx",
    ] {
        assert!(
            cols.contains(&expected.to_string()),
            "missing column {expected}"
        );
    }

    // Rows preserve TSV line order (volume order), and the first FD is NULL (n/a).
    let ordered: Vec<(i64, f64, Option<f64>)> = {
        let mut stmt = db.conn.prepare(
            "SELECT row_idx, trans_x, framewise_displacement FROM fmriprep_confounds ORDER BY row_idx",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, Option<f64>>(2)?,
            ))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    assert_eq!(
        ordered.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(ordered[0].1, 0.10, "trans_x of first volume");
    assert!(
        ordered[0].2.is_none(),
        "first framewise_displacement is n/a -> NULL"
    );

    // The transform's non-BIDS entities are parsed into generated scans columns.
    let (from, to, mode, suffix): (String, String, String, String) = db.conn.query_row(
        r#"SELECT "from", "to", "mode", suffix FROM scans WHERE "from" IS NOT NULL"#,
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    assert_eq!(
        (from.as_str(), to.as_str(), mode.as_str(), suffix.as_str()),
        ("T1w", "MNI152NLin2009cAsym", "image", "xfm")
    );

    // The database is self-describing: overlay provenance is stamped.
    assert_eq!(count(&db, "bidslake_schema")?, 1);
    let (idx, source): (i32, String) = db.conn.query_row(
        "SELECT idx, source FROM bidslake_overlays ORDER BY idx LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!((idx, source.as_str()), (0, "fmriprep"));

    Ok(())
}

#[tokio::test]
async fn without_overlay_confounds_is_skipped() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_derivative_tree(dir.path());

    // Control: the *same* tree, indexed with the plain base schema.
    let db = ingest_with_schema(dir.path(), Schema::load(None)?).await?;

    let table_exists: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'fmriprep_confounds'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        table_exists, 0,
        "no fmriprep_confounds table without the overlay"
    );

    let status: String = db.conn.query_row(
        "SELECT status FROM tabular_files WHERE file_path LIKE '%confounds%'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        status, "skipped",
        "confounds tsv is skipped without the overlay"
    );

    // Every database embeds its effective schema, but an un-augmented one records no
    // overlay provenance (NULL digest, and no bidslake_overlays table).
    let digest_is_null: bool = db.conn.query_row(
        "SELECT overlay_digest IS NULL FROM bidslake_schema",
        [],
        |r| r.get(0),
    )?;
    assert!(digest_is_null, "un-augmented DB has no overlay digest");
    let has_overlays_tbl: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'bidslake_overlays'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        has_overlays_tbl, 0,
        "no bidslake_overlays table without overlays"
    );

    Ok(())
}

#[test]
fn conflicting_overlay_is_rejected() {
    // An overlay that tries to *change* an existing base entity (subject's short
    // name) rather than add — additive-only merge must reject it.
    let overlay = AppliedOverlay {
        source: "bad".to_string(),
        content: serde_json::json!({
            "objects": { "entities": { "subject": { "name": "SUBJECT_RENAMED" } } }
        }),
    };
    let err = Schema::load_with_overlays(None, &[overlay]).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("conflict") || msg.contains("additive"),
        "expected an additive-only conflict error, got: {msg}"
    );
}
