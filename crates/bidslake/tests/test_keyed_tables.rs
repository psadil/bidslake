//! The keyed tabular tables — `participants`, `sessions`, `scans` — and re-ingest.
//!
//! These three are the ones that changed shape when every tabular identity was routed through
//! the batched read: they gained a per-file `aux` value, a `__src` source column, and
//! `INSERT OR REPLACE` in place of ordering-derived precedence. Two of the bugs in that change
//! were found by diffing a catalog rather than by the suite, which is what these cover.

mod common;

use anyhow::Result;
use std::path::Path;
use tempfile::TempDir;

/// Two subjects, two sessions each, with a `sessions.tsv` per subject and imaging files so the
/// walk synthesizes its stub rows first.
fn write_dataset(root: &Path) -> Result<()> {
    std::fs::write(
        root.join("dataset_description.json"),
        r#"{"Name": "keyed", "BIDSVersion": "1.8.0"}"#,
    )?;
    std::fs::write(
        root.join("participants.tsv"),
        "participant_id\tage\tsex\thandedness\n\
         sub-01\t34\tF\tr\n\
         sub-02\t51\tM\tl\n",
    )?;
    for sub in ["01", "02"] {
        for ses in ["pre", "post"] {
            let anat = root.join(format!("sub-{sub}/ses-{ses}/anat"));
            std::fs::create_dir_all(&anat)?;
            std::fs::write(anat.join(format!("sub-{sub}_ses-{ses}_T1w.nii.gz")), b"nii")?;
        }
        // `session_id` unprefixed on purpose: the batch normalizes it to `ses-…`.
        std::fs::write(
            root.join(format!("sub-{sub}/sub-{sub}_sessions.tsv")),
            "session_id\tacq_time\tsome_custom\n\
             pre\t2026-08-01T10:00:00\tx\n\
             post\t2026-08-08T10:00:00\ty\n",
        )?;
    }
    Ok(())
}

/// A `sessions.tsv` carries no subject of its own — the subject is in its *filename*, and the
/// batch stamps it from the per-file `aux` value. Nothing covered that: no test writes a
/// `sessions.tsv`, and the corpus datasets that have one are named by no test, so losing the
/// stamp would NULL every session's `participant_id` with the suite green.
#[tokio::test]
async fn sessions_tsv_stamps_the_subject_from_its_filename() -> Result<()> {
    let dir = TempDir::new()?;
    let root = dir.path().join("ds");
    std::fs::create_dir_all(&root)?;
    write_dataset(&root)?;
    let db = common::ingest(&root).await?;

    // Every session row is attributed to the subject whose file it came from.
    let rows: Vec<(String, String)> = db
        .conn
        .prepare("SELECT participant_id, session_id FROM sessions ORDER BY 1, 2")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    assert_eq!(
        rows,
        vec![
            ("sub-01".to_string(), "ses-post".to_string()),
            ("sub-01".to_string(), "ses-pre".to_string()),
            ("sub-02".to_string(), "ses-post".to_string()),
            ("sub-02".to_string(), "ses-pre".to_string()),
        ],
        "sessions keyed by (subject from filename, normalized session)"
    );

    let orphans: i64 = db.conn.query_row(
        "SELECT count(*) FROM sessions WHERE participant_id IS NULL",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(orphans, 0, "no session may lose its subject");

    // The TSV's own columns won over the walk's bare stub, and the undeclared one overflowed.
    // `acq_time` is schema-typed TIMESTAMP here, so assert it parsed and landed rather than
    // reading it back as text.
    let (has_acq, custom): (bool, Option<String>) = db.conn.query_row(
        "SELECT acq_time IS NOT NULL, json_extract_string(other_data, '$.some_custom') \
         FROM sessions WHERE participant_id = 'sub-01' AND session_id = 'ses-pre'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert!(has_acq, "acq_time from sessions.tsv must be stored");
    assert_eq!(custom.as_deref(), Some("x"), "undeclared column overflowed");
    Ok(())
}

/// `participants.tsv` also arrives after the walk's stubs, so its values only survive because
/// the keyed insert replaces rather than ignores.
#[tokio::test]
async fn participants_tsv_wins_over_the_walk_stub() -> Result<()> {
    let dir = TempDir::new()?;
    let root = dir.path().join("ds");
    std::fs::create_dir_all(&root)?;
    write_dataset(&root)?;
    let db = common::ingest(&root).await?;

    let rows: Vec<(String, Option<f64>, Option<String>)> = db
        .conn
        .prepare("SELECT participant_id, age, sex FROM participants ORDER BY 1")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    assert_eq!(rows.len(), 2, "one row per subject, not one per source");
    assert_eq!(rows[0].0, "sub-01");
    assert_eq!(rows[0].1, Some(34.0), "age from participants.tsv");
    assert_eq!(rows[0].2.as_deref(), Some("F"));
    Ok(())
}

/// `tabular_files.n_rows` is a user-facing provenance column that nothing asserted — the
/// tabular-coverage test checks *classification* only, and this field once read 0 for every
/// keyed file. The count is identity-dependent: a per-row table stores the source path, while
/// a keyed table's rows are found by the key they carry.
#[tokio::test]
async fn tabular_files_records_the_rows_each_file_contributed() -> Result<()> {
    let dir = TempDir::new()?;
    let root = dir.path().join("ds");
    std::fs::create_dir_all(root.join("sub-01/func"))?;
    write_dataset(&root)?;
    // A per-row table too, so both branches are exercised in one catalog.
    std::fs::write(
        root.join("sub-01/func/sub-01_task-rest_bold.nii.gz"),
        b"nii",
    )?;
    std::fs::write(
        root.join("sub-01/func/sub-01_task-rest_events.tsv"),
        "onset\tduration\ttrial_type\n0.0\t1.0\ta\n2.0\t1.0\tb\n4.0\t1.0\tc\n",
    )?;
    let db = common::ingest(&root).await?;

    // Per-row: exactly the lines of the file.
    let events: i64 = db.conn.query_row(
        "SELECT n_rows FROM tabular_files \
         WHERE file_path = 'sub-01/func/sub-01_task-rest_events.tsv'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(events, 3, "events n_rows == its data lines");

    // Keyed: the rows bearing the key, which includes stubs the walk made for sessions the
    // file also lists — so assert it is attributed at all rather than pinning an exact count.
    for f in [
        "participants.tsv",
        "sub-01/sub-01_sessions.tsv",
        "sub-02/sub-02_sessions.tsv",
    ] {
        let n: i64 = db.conn.query_row(
            "SELECT n_rows FROM tabular_files WHERE file_path = ?",
            [f],
            |r| r.get(0),
        )?;
        assert!(n > 0, "{f} must not report 0 rows");
    }

    // Nothing recorded as ingested may claim a table it did not land in.
    let bad: i64 = db.conn.query_row(
        "SELECT count(*) FROM tabular_files WHERE status = 'ingested' AND table_name IS NULL",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(bad, 0);
    Ok(())
}

/// Re-ingesting the same dataset into the same catalog must be a no-op.
///
/// This is the guard for the dedup the bulk `Appender` pushed onto its callers: it runs no
/// primary-key check, so `scans`/`sidecars` rely on the parser's own `seen` set. No test
/// ingested twice and compared — one only re-ran the DDL, another re-ingested but asserted
/// nothing beyond "did not error".
#[tokio::test]
async fn reingesting_the_same_dataset_changes_nothing() -> Result<()> {
    let dir = TempDir::new()?;
    let root = dir.path().join("ds");
    std::fs::create_dir_all(root.join("sub-01/func"))?;
    write_dataset(&root)?;
    std::fs::write(
        root.join("sub-01/func/sub-01_task-rest_bold.nii.gz"),
        b"nii",
    )?;
    std::fs::write(
        root.join("sub-01/func/sub-01_task-rest_events.tsv"),
        "onset\tduration\n0.0\t1.0\n2.0\t1.0\n",
    )?;
    std::fs::write(
        root.join("sub-01/func/sub-01_task-rest_bold.json"),
        r#"{"RepetitionTime": 2.0, "EchoTime": 0.03}"#,
    )?;

    let tables = [
        "scans",
        "sidecars",
        "events",
        "participants",
        "sessions",
        "tabular_files",
        "dataset_description",
    ];
    let counts = |db: &bidslake::db::BidsDb| -> Result<Vec<(String, i64)>> {
        tables
            .iter()
            .map(|t| {
                let n: i64 = db
                    .conn
                    .query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))?;
                Ok((t.to_string(), n))
            })
            .collect()
    };

    let db = common::ingest_as(&root, "study").await?;
    let first = counts(&db)?;
    assert!(
        first.iter().all(|(_, n)| *n > 0),
        "every table should have rows to compare: {first:?}"
    );

    common::ingest_into(&db, &root, "study").await?;
    let second = counts(&db)?;
    assert_eq!(first, second, "a second identical ingest must add no rows");

    // And the values are unchanged, not merely the counts.
    let (rt, age): (Option<f64>, Option<f64>) = db.conn.query_row(
        "SELECT (SELECT RepetitionTime FROM sidecars LIMIT 1), \
                (SELECT age FROM participants WHERE participant_id = 'sub-01')",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(rt, Some(2.0));
    assert_eq!(age, Some(34.0));
    Ok(())
}
