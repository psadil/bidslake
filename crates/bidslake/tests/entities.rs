//! BIDS entities / datatype / suffix / modality are exposed as generated columns
//! on `scans`, so files can be queried by concept instead of by path regex.

mod common;

use common::{bids_example, ingest};

/// A `task-facerecognition` BOLD run in ds000117 exposes every concept column.
#[tokio::test]
async fn generated_columns_populated() -> anyhow::Result<()> {
    let db = ingest(bids_example("ds000117")).await?;

    // Assert all concept columns at once: exactly the facerecognition func-bold
    // runs must have every derived column set as expected.
    let matches: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM scans \
         WHERE task = 'facerecognition' AND datatype = 'func' AND suffix = 'bold' \
           AND ses = 'mri' AND modality = 'mri' AND extension = '.nii.gz' \
           AND sub IS NOT NULL AND run IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    assert!(
        matches > 0,
        "a ds000117 task-facerecognition func BOLD scan should populate every concept column"
    );
    Ok(())
}

/// A dataset without sessions yields `ses IS NULL` on every scan — the property
/// that lets one query span a pool of session/no-session datasets uniformly.
#[tokio::test]
async fn null_session_for_sessionless_dataset() -> anyhow::Result<()> {
    let db = ingest(bids_example("ds001")).await?;

    let with_ses: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM scans WHERE ses IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(with_ses, 0, "ds001 has no sessions, so ses must be NULL");

    // task/suffix are still populated for its bold runs.
    let bold: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM scans WHERE suffix = 'bold' AND task = 'balloonanalogrisktask'",
        [],
        |r| r.get(0),
    )?;
    assert!(
        bold > 0,
        "ds001 bold runs should be queryable by task/suffix"
    );
    Ok(())
}

/// The ingest path derives implicit participants/sessions from filename entities —
/// this now goes through the shared `bids_core::entities::read_entities` parser
/// instead of the old ad-hoc regex, so pin that the derivation is still correct on a
/// multi-session dataset (`sub-XX`, `ses-mri`/`ses-meg`).
#[tokio::test]
async fn implicit_sessions_from_filename_entities() -> anyhow::Result<()> {
    let db = ingest(bids_example("ds000117")).await?;

    // Every derived id must be well-formed (`sub-…` / `ses-…`).
    let malformed: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM sessions \
         WHERE session_id NOT LIKE 'ses-%' OR participant_id NOT LIKE 'sub-%'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        malformed, 0,
        "session/participant ids must be derived from filename entities"
    );

    // The `ses-mri` sessions are implied purely by `ses-mri` in filenames.
    let mri_sessions: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM sessions WHERE session_id = 'ses-mri'",
        [],
        |r| r.get(0),
    )?;
    assert!(
        mri_sessions > 0,
        "ds000117 should derive ses-mri sessions from filename entities"
    );
    Ok(())
}

/// Querying by concept reaches datasets with AND without sessions in one shot.
#[tokio::test]
async fn query_by_concept_across_mixed_pool() -> anyhow::Result<()> {
    // ds210 has no sessions; eyetracking_fmri does — both have task-rest bold.
    let db = ingest(bids_example("ds210")).await?;
    {
        use bidslake::{bids::BidsParser, fs::LocalFileSystem, schema::Schema};
        let schema = Schema::load(None);
        let fs = Box::new(LocalFileSystem::new(bids_example("eyetracking_fmri")));
        let mut parser = BidsParser::new(fs, Some("eyetracking_fmri".to_string()), schema);
        parser.parse(&db).await?;
    }

    let rest: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM scans WHERE task = 'rest' AND datatype = 'func' AND suffix = 'bold'",
        [],
        |r| r.get(0),
    )?;
    assert!(
        rest >= 47,
        "expected rest bold runs from both datasets, got {rest}"
    );

    // The pool genuinely mixes session and no-session datasets.
    let ses_states: i64 = db.conn.query_row(
        "SELECT COUNT(DISTINCT ses IS NULL) FROM scans WHERE task = 'rest'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        ses_states, 2,
        "pool should contain both session and no-session rest scans"
    );
    Ok(())
}
