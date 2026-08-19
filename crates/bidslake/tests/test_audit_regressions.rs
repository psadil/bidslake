//! Regressions for four defects found by the 2026-08 ingest performance audit.
//!
//! Grouped in one file because they were found together and share the same shape: each is a
//! thing the catalog *claimed* that was not so, and none was caught by an existing test.

mod common;

use common::{bids_example, ingest};

/// A file is never its own association.
///
/// `find_associated_file` searches the source file's own directory first, and the schema's
/// `events` and `physio` entries select on `extension != '.json'` alone — so every
/// `*_events.tsv` matched itself on suffix, extension and entities and was written as an
/// `events` association pointing at itself. On a raw 4,000-subject tree that was 16,000 of
/// 32,000 rows: half the table. ADR 0003's `describes` views join through it, so a file
/// described itself.
#[tokio::test]
async fn no_file_is_its_own_association() -> anyhow::Result<()> {
    let db = ingest(bids_example("ds001")).await?;

    let self_edges: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM file_associations WHERE source_file_id = target_file_id",
        [],
        |r| r.get(0),
    )?;

    assert_eq!(self_edges, 0, "a file was recorded as its own association");
    Ok(())
}

/// Removing the self-edges did not remove the real ones: `ds001` still resolves its
/// `bold` → `events` associations. Paired with the test above, which alone would pass if
/// association resolution stopped working entirely.
#[tokio::test]
async fn real_event_associations_survive() -> anyhow::Result<()> {
    let db = ingest(bids_example("ds001")).await?;

    let events: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM file_associations a \
         JOIN all_files src ON src.file_id = a.source_file_id \
         WHERE a.association_type = 'events' AND src.file_path LIKE '%_bold.nii.gz'",
        [],
        |r| r.get(0),
    )?;

    assert!(
        events > 0,
        "BOLD runs should still resolve their events.tsv; got {events}"
    );
    Ok(())
}

/// The provenance stamp describes the catalog as it *is*, not as the first run left it.
///
/// `stamp_schema` inserted only when `bidslake_schema` was empty, so the stamp froze at the
/// first `index`. Tables are created `IF NOT EXISTS`, so indexing one root plainly and a
/// second with `--adapter fmriprep` left the catalog physically holding `fmriprep_confounds`
/// while `effective_schema` still reported base-only and `overlay_digest` was NULL. ADR 0002
/// makes the adapter set a statement about the catalog, and `check_registry_shape` does not
/// catch this.
#[tokio::test]
async fn the_schema_stamp_reflects_the_latest_run_not_the_first() -> anyhow::Result<()> {
    let db = bidslake::db::BidsDb::new(":memory:")?;
    common::ingest_into(&db, bids_example("ds001"), "plain").await?;
    common::ingest_with_adapters_into(
        &db,
        bids_example("ds001"),
        &["fmriprep"],
        Some("with-adapter"),
    )
    .await?;

    let has_digest: bool = db.conn.query_row(
        "SELECT overlay_digest IS NOT NULL FROM bidslake_schema",
        [],
        |r| r.get(0),
    )?;

    assert!(
        has_digest,
        "a run that applied an overlay left the stamp claiming none was applied"
    );
    Ok(())
}

/// The stamp is one row, not one per run — a re-index replaces it rather than appending.
#[tokio::test]
async fn the_schema_stamp_stays_a_single_row_across_runs() -> anyhow::Result<()> {
    let db = bidslake::db::BidsDb::new(":memory:")?;
    common::ingest_into(&db, bids_example("ds001"), "first").await?;
    common::ingest_into(&db, bids_example("ds002"), "second").await?;

    let rows: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM bidslake_schema", [], |r| r.get(0))?;

    assert_eq!(
        rows, 1,
        "each index run appended a stamp instead of replacing it"
    );
    Ok(())
}

/// `read_head` returns a *complete* first line, however many reads that takes.
///
/// It issued a single `file.read(&mut buf)` and truncated to whatever came back. `read` may
/// legally return fewer bytes than asked for and does so routinely over NFS, so the header
/// line came back truncated — and the header line is not incidental: the batched tabular
/// ingest groups files by its exact bytes and takes the column names from it, so a short read
/// produced a wrong catalog rather than an error.
///
/// A wide header is the realistic case: an fMRIPrep `desc-confounds_timeseries.tsv` at the
/// measured 1,841 columns runs past 25 KB, well beyond any single chunk.
#[tokio::test]
async fn read_head_returns_a_whole_header_line() -> anyhow::Result<()> {
    use bidslake::fs::{BidsFileSystem, LocalFileSystem};

    let dir = tempfile::tempdir()?;
    let columns: Vec<String> = (0..1841).map(|i| format!("a_comp_cor_{i:05}")).collect();
    let header = columns.join("\t");
    std::fs::write(
        dir.path().join("wide.tsv"),
        format!("{header}\n0\t1\t2\n").as_bytes(),
    )?;

    let fs = LocalFileSystem::new(dir.path().to_path_buf());
    let head = fs
        .read_head(std::path::Path::new("wide.tsv"), 64 * 1024)
        .await?;

    assert_eq!(
        head.lines().next().unwrap_or_default(),
        header,
        "the first line came back truncated"
    );
    Ok(())
}
