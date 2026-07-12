//! Regression coverage for the batched tabular ingest (Lever 1b).
//!
//! Per-row tables are ingested by grouping same-header files into a single
//! `read_csv([...])`. Three properties that batching could silently break — and
//! that the per-file path guaranteed for free — are checked here against the raw
//! TSVs, because [`tabular_coverage`](../tabular_coverage.rs) only checks *that* a
//! file was ingested, not that its rows are right:
//!
//! 1. **No rows dropped.** Each file's DB row count equals its raw data-line count
//!    (a `filename`↔path join that failed to match would silently drop rows).
//! 2. **`row_idx` is TSV line order.** The DB's `ORDER BY row_idx` onset sequence
//!    equals the file's physical line order, or tables whose consumers reconstruct
//!    sequence via `ORDER BY row_idx` (channels, electrodes, motion) would scramble.
//! 3. **`other_data` is exact.** Grouping by header (not `union_by_name`) folds in
//!    exactly each file's non-schema columns — no NULL fillers from siblings.
//!
//! Datasets: `ds001` (many same-header events → real batches) and
//! `xeeg_hed_score`, which mixes CRLF and LF line endings across events files —
//! the case that must land in separate batches so one read dialect can't misparse
//! the others.

mod common;

use bidslake::db::BidsDb;
use common::{bids_example, ingest, walk_tabular};
use std::path::Path;

/// Parse a raw TSV: `(header, data_rows)`, each row split on tab.
fn read_tsv(path: &Path) -> (Vec<String>, Vec<Vec<String>>) {
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines();
    let header = lines
        .next()
        .unwrap()
        .split('\t')
        .map(|s| s.trim_end_matches('\r').to_string())
        .collect();
    let rows = lines
        .filter(|l| !l.is_empty())
        .map(|l| l.split('\t').map(str::to_string).collect())
        .collect();
    (header, rows)
}

fn rel_of(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .to_string()
}

/// For every `_events.tsv` in `root`, assert the batched ingest reproduced the raw
/// file exactly: same row count, and `onset` in the same physical order.
fn check_events(db: &BidsDb, root: &Path) -> anyhow::Result<usize> {
    let files: Vec<std::path::PathBuf> = walk_tabular(root)
        .into_iter()
        .filter(|p| p.ends_with("_events.tsv"))
        .map(|p| root.join(p))
        .collect();

    for file in &files {
        let rel = rel_of(root, file);
        let (header, rows) = read_tsv(file);
        let onset_col = header.iter().position(|c| c == "onset").unwrap();

        let raw: Vec<Option<f64>> = rows
            .iter()
            .map(|r| match r.get(onset_col).map(String::as_str) {
                Some("n/a") | None => None,
                Some(v) => v.parse().ok(),
            })
            .collect();

        let db_onset: Vec<Option<f64>> = db
            .conn
            .prepare("SELECT onset FROM events WHERE file_path = ? ORDER BY row_idx")?
            .query_map([&rel], |r| r.get::<_, Option<f64>>(0))?
            .collect::<Result<_, _>>()?;

        assert_eq!(
            db_onset.len(),
            rows.len(),
            "row count must match raw file for {rel} (no rows dropped)"
        );
        assert_eq!(
            db_onset, raw,
            "row_idx order must match TSV line order for {rel}"
        );

        let idxs: Vec<i64> = db
            .conn
            .prepare("SELECT row_idx FROM events WHERE file_path = ? ORDER BY row_idx")?
            .query_map([&rel], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        assert_eq!(
            idxs,
            (0..rows.len() as i64).collect::<Vec<_>>(),
            "row_idx must be contiguous 0..n for {rel}"
        );
    }
    Ok(files.len())
}

#[tokio::test]
async fn ds001_batched_events_and_other_data() -> anyhow::Result<()> {
    let root = bids_example("ds001");
    let db = ingest(&root).await?;

    let n = check_events(&db, &root)?;
    assert!(n >= 40, "expected the ds001 submodule (many events files)");

    // other_data holds exactly the non-schema header columns: `cash_demean` is not
    // a BIDS events column (→ other_data), `onset` is (→ not).
    let rel = walk_tabular(&root)
        .into_iter()
        .find(|p| p.ends_with("_events.tsv"))
        .unwrap();
    let (has_extra, has_onset): (bool, bool) = db.conn.query_row(
        "SELECT list_contains(json_keys(other_data), 'cash_demean'), \
                list_contains(json_keys(other_data), 'onset') \
         FROM events WHERE file_path = ? LIMIT 1",
        [&rel],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert!(has_extra, "non-schema column must be in other_data");
    assert!(!has_onset, "schema column must not be in other_data");
    Ok(())
}

/// `xeeg_hed_score` mixes CRLF and LF events files; batching must keep them in
/// separate reads so neither dialect misparses the other (regression: they were
/// grouped together and the whole dataset failed to ingest).
#[tokio::test]
async fn xeeg_mixed_line_endings_batch_correctly() -> anyhow::Result<()> {
    let root = bids_example("xeeg_hed_score");
    let db = ingest(&root).await?;
    let n = check_events(&db, &root)?;
    assert!(n > 0, "expected xeeg_hed_score events files");
    Ok(())
}
