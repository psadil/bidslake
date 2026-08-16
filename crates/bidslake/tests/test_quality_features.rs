//! Event associations, root_uri reconstruction, and sidecar deduplication —
//! exercised against a real bids-examples dataset (ds001).

mod common;

use anyhow::Result;
use common::{bids_example, ingest};

/// Every ds001 bold resolves the `_events.tsv` sitting beside it.
///
/// `file_associations` is unconditional static DDL created before the parser opens a file, so
/// asking `information_schema` whether the table exists was decided before ds001 was read and
/// would have held for an empty directory. What the table is for is the resolved edge.
#[tokio::test]
async fn every_ds001_bold_resolves_its_events_sibling() -> Result<()> {
    let db = ingest(bids_example("ds001")).await?;

    let (bolds, resolved, mismatched): (i64, i64, i64) = db.conn.query_row(
        "SELECT (SELECT COUNT(*) FROM all_files WHERE kind='data' AND suffix='bold'), \
                (SELECT COUNT(*) FROM file_associations a \
                   JOIN all_files f ON f.file_id = a.source_file_id \
                  WHERE f.suffix='bold' AND a.association_type='events' \
                    AND a.target_file_id IS NOT NULL), \
                (SELECT COUNT(*) FROM file_associations a \
                   JOIN all_files f ON f.file_id = a.source_file_id \
                  WHERE f.suffix='bold' AND a.association_type='events' \
                    AND a.target_file_path <> replace(f.file_path, '_bold.nii.gz', '_events.tsv'))",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    assert_eq!(bolds, 48, "ds001 ships 16 subjects x 3 runs of bold");
    assert_eq!(
        resolved, bolds,
        "every bold resolves its sibling events.tsv"
    );
    assert_eq!(
        mismatched, 0,
        "and resolves it to the sibling, not some other tsv"
    );
    Ok(())
}

#[tokio::test]
async fn test_root_uri_path_reconstruction() -> Result<()> {
    let db = ingest(bids_example("ds001")).await?;

    let root_uri: Option<String> =
        db.conn
            .query_row("SELECT root_uri FROM dataset_roots", [], |r| r.get(0))?;

    let uri = root_uri.expect("root_uri should be populated");
    assert!(
        uri.starts_with("file://"),
        "local paths should use file:// URI scheme, got {uri}"
    );
    assert!(
        uri.contains("ds001"),
        "root_uri should contain dataset path: {uri}"
    );
    Ok(())
}

#[tokio::test]
async fn test_sidecar_deduplication() -> Result<()> {
    let db = ingest(bids_example("ds001")).await?;

    // Known BIDS metadata fields have dedicated columns and must not also appear
    // in the other_data overflow column.
    let rows: Vec<Option<String>> = db
        .conn
        .prepare("SELECT other_data::VARCHAR FROM sidecars")?
        .query_map([], |r| r.get::<_, Option<String>>(0))?
        .collect::<Result<_, _>>()?;

    assert!(!rows.is_empty(), "ds001 should produce sidecars");

    for other_data in rows.into_iter().flatten() {
        let data: serde_json::Value = serde_json::from_str(&other_data)?;
        let obj = data.as_object().expect("other_data should be an object");
        for dedicated in ["RepetitionTime", "EchoTime", "FlipAngle"] {
            assert!(
                !obj.contains_key(dedicated),
                "{dedicated} should be in its dedicated column, not other_data"
            );
        }
    }
    Ok(())
}

/// DuckDB folds identifier case, so `sidecars` cannot carry two columns whose
/// names differ only in case. BIDS declares exactly one such pair —
/// `MISCChannelCount` (MEG) and `MiscChannelCount` (EEG/iEEG) — and the DDL keeps
/// the first, so an EEG sidecar's `MiscChannelCount` has no column of its own
/// spelling. Matching source keys exactly would strand it: missing from its
/// column *and* treated as undeclared, i.e. demoted to `other_data`.
///
/// Benign while `other_data` preserves everything; silent data loss once a table
/// declines to store undeclared columns. So the surviving column absorbs both
/// spellings, as DuckDB does with the identifier itself.
#[tokio::test]
async fn case_variant_metadata_reaches_its_typed_column() -> Result<()> {
    let dir = tempfile::TempDir::new()?;
    let root = dir.path();
    std::fs::write(
        root.join("dataset_description.json"),
        r#"{"Name": "case variants", "BIDSVersion": "1.8.0"}"#,
    )?;
    let eeg = root.join("sub-01/eeg");
    std::fs::create_dir_all(&eeg)?;
    std::fs::write(eeg.join("sub-01_task-t_eeg.edf"), b"fake")?;
    // The EEG spelling — the one BIDS uses for auxiliary analog channels, and the
    // one the case-insensitive DDL dedup drops.
    std::fs::write(
        eeg.join("sub-01_task-t_eeg.json"),
        r#"{"MiscChannelCount": 7, "SomethingCustom": "keep me"}"#,
    )?;

    let db = ingest(root).await?;

    let (count, other): (Option<i64>, Option<String>) = db.conn.query_row(
        r#"SELECT "MISCChannelCount", other_data::VARCHAR FROM sidecars
           JOIN all_files USING (file_id) WHERE file_path LIKE '%_eeg.edf'"#,
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    assert_eq!(
        count,
        Some(7),
        "MiscChannelCount must land in the surviving MISCChannelCount column"
    );
    let obj: serde_json::Value = serde_json::from_str(&other.unwrap_or_else(|| "{}".into()))?;
    assert!(
        obj.get("MiscChannelCount").is_none(),
        "a declared field must not also sit in other_data: {obj}"
    );
    // The genuinely custom field is untouched — this is a case fix, not a filter.
    assert_eq!(
        obj.get("SomethingCustom").and_then(|v| v.as_str()),
        Some("keep me")
    );
    Ok(())
}
