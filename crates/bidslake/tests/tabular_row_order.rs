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
//! 2. **`row_idx` order policy.** A table has the column only when its line order is
//!    load-bearing. Positional tables (`*_channels`/…) are read `parallel=false`, so
//!    their `row_idx` must reproduce TSV line order — consumers reconstruct channel
//!    order via `ORDER BY row_idx`. `events` is declared order-insensitive and read in
//!    parallel, so it has **no** `row_idx` at all: its rows must still match as a
//!    multiset, and are addressed by `onset`.
//! 3. **`other_data` is exact.** Grouping by header (not `union_by_name`) folds in
//!    exactly each file's non-schema columns — no NULL fillers from siblings.
//!
//! Datasets: `ds001` (many same-header events → real batches) and
//! `xeeg_hed_score`, which mixes CRLF and LF line endings across events files —
//! the case that must land in separate batches so one read dialect can't misparse
//! the others.

mod common;

use bidslake::db::BidsDb;
use common::{bids_example, count, ingest, walk_tabular};
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

/// For every `_events.tsv` in `root`, assert the batched ingest reproduced the raw file: same
/// row count and same `onset` multiset. Not the *sequence* — `events` is declared
/// order-insensitive, so its files are read concurrently and it carries no `row_idx` to order by.
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

        let mut db_onset: Vec<Option<f64>> = db
            .conn
            .prepare("SELECT onset FROM events JOIN all_files USING (file_id) WHERE file_path = ?")?
            .query_map([&rel], |r| r.get::<_, Option<f64>>(0))?
            .collect::<Result<_, _>>()?;

        assert_eq!(
            db_onset.len(),
            rows.len(),
            "row count must match raw file for {rel} (no rows dropped)"
        );
        // Compare the onset *multiset*: a concurrent read fixes no row order, which is why
        // this table has no `row_idx` to compare a sequence by.
        let mut raw_sorted = raw.clone();
        let cmp = |a: &Option<f64>, b: &Option<f64>| {
            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
        };
        db_onset.sort_by(cmp);
        raw_sorted.sort_by(cmp);
        assert_eq!(
            db_onset, raw_sorted,
            "onset values must match raw for {rel}"
        );
    }
    Ok(files.len())
}

/// An order-insensitive table has no `row_idx` column at all — the point of the policy. A
/// column of arbitrary labels invites `ORDER BY row_idx`, which would silently return rows in
/// an order that differs between two ingests of the same dataset.
fn assert_no_row_idx(db: &BidsDb, table: &str) -> anyhow::Result<()> {
    let n: i64 = db.conn.query_row(
        "SELECT count(*) FROM information_schema.columns \
         WHERE table_name = ? AND column_name = 'row_idx'",
        [table],
        |r| r.get(0),
    )?;
    assert_eq!(
        n, 0,
        "{table} is order-insensitive and must have no row_idx"
    );
    Ok(())
}

#[tokio::test]
async fn ds001_batched_events_and_other_data() -> anyhow::Result<()> {
    let root = bids_example("ds001");
    let db = ingest(&root).await?;

    let n = check_events(&db, &root)?;
    assert!(n >= 40, "expected the ds001 submodule (many events files)");
    assert_no_row_idx(&db, "events")?;

    // other_data holds exactly the non-schema header columns: `cash_demean` is not
    // a BIDS events column (→ other_data), `onset` is (→ not).
    let rel = walk_tabular(&root)
        .into_iter()
        .find(|p| p.ends_with("_events.tsv"))
        .unwrap();
    let (has_extra, has_onset): (bool, bool) = db.conn.query_row(
        "SELECT list_contains(json_keys(other_data), 'cash_demean'), \
                list_contains(json_keys(other_data), 'onset') \
         FROM events JOIN all_files USING (file_id) WHERE file_path = ? LIMIT 1",
        [&rel],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert!(has_extra, "non-schema column must be in other_data");
    assert!(!has_onset, "schema column must not be in other_data");
    Ok(())
}

/// A header with a trailing tab yields an empty-string column name. Left in the
/// `other_data` extras it emits `json_object('', raw."")`, whose zero-length
/// delimited identifier is a DuckDB parser error — and because the batched path has
/// no per-file fallback, that error drops every file in the group. Real datasets do
/// ship such headers, so both SQL builders filter empty names out.
///
/// Two files with the same trailing-tab header so this exercises the *batched* path
/// (a lone file would fall to the single-file builder).
#[tokio::test]
async fn trailing_tab_header_still_ingests() -> anyhow::Result<()> {
    let dir = tempfile::TempDir::new()?;
    let root = dir.path();
    std::fs::write(
        root.join("dataset_description.json"),
        r#"{"Name": "trailing tab", "BIDSVersion": "1.8.0"}"#,
    )?;
    let func = root.join("sub-01/func");
    std::fs::create_dir_all(&func)?;
    for run in ["01", "02"] {
        std::fs::write(
            func.join(format!("sub-01_task-t_run-{run}_bold.nii.gz")),
            b"fake",
        )?;
        // Note the trailing tab on the header line, and the extra `custom` column.
        std::fs::write(
            func.join(format!("sub-01_task-t_run-{run}_events.tsv")),
            "onset\tduration\tcustom\t\n1.0\t0.5\tx\t\n2.0\t0.5\ty\t\n",
        )?;
    }

    let db = ingest(root).await?;

    let (n, failed): (i64, i64) = db.conn.query_row(
        "SELECT count(*), count(*) FILTER (WHERE status = 'failed') FROM file_registry \
         WHERE file_path LIKE '%_events.tsv'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(n, 2, "both events files must be recorded");
    assert_eq!(failed, 0, "a trailing-tab header must not fail the batch");
    assert_eq!(
        count(&db, "events")?,
        4,
        "both files' rows must land (2 rows each)"
    );

    // The real extra column still reaches other_data; the empty name does not.
    let (has_custom, has_empty): (bool, bool) = db.conn.query_row(
        "SELECT list_contains(json_keys(other_data), 'custom'), \
                list_contains(json_keys(other_data), '') \
         FROM events LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert!(has_custom, "genuine extra column must be in other_data");
    assert!(!has_empty, "empty column name must not be in other_data");
    Ok(())
}

/// Positional tables (`*_channels`/`*_electrodes`/`*_optodes`) must keep TSV line
/// order in `row_idx` — the channel/sensor order maps to a recording's columns, so
/// these are read with `parallel=false` (unlike events). `eeg_matchingpennies` has
/// a `_channels.tsv` per subject; each `name` column, read back `ORDER BY row_idx`,
/// must equal the file's physical line order.
#[tokio::test]
async fn channels_preserve_line_order() -> anyhow::Result<()> {
    let root = bids_example("eeg_matchingpennies");
    let db = ingest(&root).await?;

    let files: Vec<std::path::PathBuf> = walk_tabular(&root)
        .into_iter()
        .filter(|p| p.ends_with("_channels.tsv"))
        .map(|p| root.join(p))
        .collect();
    assert!(
        !files.is_empty(),
        "expected the eeg_matchingpennies submodule"
    );

    for file in &files {
        let rel = rel_of(&root, file);
        let (header, rows) = read_tsv(file);
        let name_col = header.iter().position(|c| c == "name").unwrap();
        let raw: Vec<String> = rows.iter().map(|r| r[name_col].clone()).collect();

        let db_names: Vec<String> = db
            .conn
            .prepare("SELECT name FROM eeg_channels JOIN all_files USING (file_id) WHERE file_path = ? ORDER BY row_idx")?
            .query_map([&rel], |r| r.get(0))?
            .collect::<Result<_, _>>()?;

        assert_eq!(
            db_names, raw,
            "channel order (row_idx) must match TSV line order for {rel}"
        );
    }
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

    // sub-ieegModulator's electrodes.tsv has mixed line endings *within* the file
    // (166 CRLF rows + a final bare-LF row) — valid per bids-validator-rs, but
    // rejected by DuckDB's strict CSV parser. `strict_mode=false` ingests it; guard
    // that it stays ingested (all 166 rows), not silently skipped.
    let electrodes: i64 = db.conn.query_row(
        "SELECT count(*) FROM ieeg_electrodes JOIN all_files USING (file_id) \
                  WHERE file_path LIKE '%sub-ieegModulator%electrodes.tsv'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        electrodes, 166,
        "mixed-line-ending electrodes.tsv must ingest under strict_mode=false"
    );
    Ok(())
}
