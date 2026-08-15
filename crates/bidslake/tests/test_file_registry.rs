//! The file registry and its `all_files` view (docs/adr/0006).
//!
//! `file_registry` is one row per file the walk saw, and the thing every file-keyed table
//! points at. `all_files` is that registry widened with the generated BIDS-concept columns —
//! which live on the *view* rather than the base table, so widening the concept set is a
//! `CREATE OR REPLACE VIEW` rather than a table rebuild. "Just the data files" is
//! `WHERE kind = 'data'`, not a second view.

mod common;

use bidslake::db::BidsDb;
use bidslake::schema::Schema;

fn fresh() -> anyhow::Result<BidsDb> {
    let db = BidsDb::new(":memory:")?;
    db.create_tables(&Schema::load(None)?)?;
    Ok(db)
}

fn names(db: &BidsDb, sql: &str) -> anyhow::Result<Vec<String>> {
    let mut stmt = db.conn.prepare(sql)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The registry is a base table and `all_files` is a view over it. This is the distinction
/// the rest of the design rests on: a foreign key can reference the table (DuckDB refuses one
/// against a view), while the view can be redefined by a later, wider run.
#[test]
fn all_files_is_a_view_over_the_registry() -> anyhow::Result<()> {
    let db = fresh()?;
    assert_eq!(
        names(
            &db,
            "SELECT table_name FROM duckdb_tables() WHERE table_name = 'file_registry'"
        )?,
        ["file_registry"],
        "the registry must be a base table"
    );
    assert_eq!(
        names(
            &db,
            "SELECT view_name FROM duckdb_views() WHERE view_name = 'all_files'"
        )?,
        ["all_files"],
        "all_files must be a view"
    );
    assert!(
        names(
            &db,
            "SELECT table_name FROM duckdb_tables() WHERE table_name = 'all_files'"
        )?
        .is_empty(),
        "all_files must not also be a base table"
    );
    Ok(())
}

/// The concept columns are on the view, not the registry. That placement is what makes the
/// registry free of generated columns — a precondition for the bulk staged upsert — and what
/// makes widening the concept set free.
///
/// The registry itself holds three kinds of column and no others: **identity** (`file_id`
/// and the three parts it hashes), **classification** (`kind`, `status` — what bidslake
/// decided about the file), and **observation** (`size_bytes`, `mtime_ns` — what the
/// filesystem said, as opposed to what the name or contents say). Nothing derived from the
/// BIDS vocabulary belongs here; that is the view's job.
#[test]
fn concepts_live_on_the_view_not_the_registry() -> anyhow::Result<()> {
    let db = fresh()?;
    let registry = names(
        &db,
        "SELECT column_name FROM information_schema.columns \
         WHERE table_name = 'file_registry' ORDER BY ordinal_position",
    )?;
    assert_eq!(
        registry,
        [
            "file_id",
            "dataset_id",
            "root_uri",
            "file_path",
            "kind",
            "status",
            "size_bytes",
            "mtime_ns"
        ],
        "the registry holds identity, classification and observation only"
    );

    let view = names(
        &db,
        "SELECT column_name FROM information_schema.columns WHERE table_name = 'all_files'",
    )?;
    for concept in [
        "sub",
        "ses",
        "task",
        "run",
        "datatype",
        "suffix",
        "extension",
        "modality",
    ] {
        assert!(view.contains(&concept.to_string()), "view lacks {concept}");
    }
    assert!(
        view.contains(&"kind".to_string()),
        "`kind` is how a consumer narrows the view to data files, so it must be on it"
    );
    Ok(())
}

/// The view's concept expressions compute — including `modality`, which keys off the view's
/// own `datatype` alias rather than re-matching the path — and they compute for **every** kind,
/// not just data files.
///
/// That last part is why there is one view rather than a data-file-filtered one: the per-row
/// tables (`events`, `*_channels`, …) key on *tabular* files, so a view restricted to
/// `kind = 'data'` would leave them with no relation to join for "which subject is this row
/// from?".
#[tokio::test]
async fn the_view_computes_concepts_for_every_kind() -> anyhow::Result<()> {
    let db = fresh()?;
    let mut insert = db.conn.prepare(
        "INSERT INTO file_registry (file_id, dataset_id, root_uri, file_path, kind, status) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )?;
    for (id, path, kind) in [
        (1u64, "sub-01/func/sub-01_task-x_run-2_bold.nii.gz", "data"),
        (2, "sub-01/anat/sub-01_T1w.json", "sidecar"),
        (3, "README", "other"),
        // Ids are unsigned now (`UBIGINT`), so a large one is the interesting case rather
        // than a negative one. Under the old signed `HUGEINT` half of all ids were
        // negative; nothing downstream has to allow for that any more.
        (u64::MAX, "sub-02/dwi/sub-02_dwi.nii.gz", "data"),
        // The case the single view exists for: a per-row table keys on this, and it is not
        // a data file.
        (5, "sub-01/func/sub-01_task-x_run-2_events.tsv", "tabular"),
    ] {
        insert.execute(duckdb::params![
            id,
            "ds",
            "file:///r",
            path,
            kind,
            "cataloged"
        ])?;
    }

    assert_eq!(common::count(&db, "file_registry")?, 5);
    assert_eq!(
        common::count(&db, "all_files")?,
        5,
        "the view is every file, not a filtered subset"
    );
    let data: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files WHERE kind = 'data'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(data, 2, "narrowing to data files is a filter, not a view");

    let concepts = |file_id: i64| -> anyhow::Result<(String, String, String, String, String)> {
        Ok(db.conn.query_row(
            "SELECT sub, task, run, datatype, suffix FROM all_files WHERE file_id = ?",
            [file_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?)
    };

    let (sub, task, run, datatype, suffix) = concepts(1)?;
    assert_eq!(
        (
            sub.as_str(),
            task.as_str(),
            run.as_str(),
            datatype.as_str(),
            suffix.as_str()
        ),
        ("01", "x", "2", "func", "bold")
    );
    let modality: String = db.conn.query_row(
        "SELECT modality FROM all_files WHERE file_id = 1",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(modality, "mri", "modality keys off the view's own datatype");

    // The events TSV — a `tabular` row — answers the same questions, which is what lets a
    // per-row table join through this view.
    let (sub, task, run, datatype, suffix) = concepts(5)?;
    assert_eq!(
        (
            sub.as_str(),
            task.as_str(),
            run.as_str(),
            datatype.as_str(),
            suffix.as_str()
        ),
        ("01", "x", "2", "func", "events")
    );
    Ok(())
}

/// The property the registry exists for: **every file the walk sees has a row**, addressed by
/// its own path.
///
/// Compared against the same `read_file_tree` walker the ingest uses, so the expected set
/// applies dotfile and `.bidsignore` rules identically and cannot drift. Before this, 26% of
/// ds000117 was in no table at all — including every sidecar JSON (whose `sidecars` row is
/// keyed by the *data file* it describes, leaving the JSON itself unaddressable) and every
/// `.bval`/`.bvec` (whose values land in `bvals`/`bvecs` under the gradient file's own id,
/// and reach the images through `file_associations` — docs/adr/0007).
#[tokio::test]
async fn every_walked_file_has_a_registry_row() -> anyhow::Result<()> {
    let root = common::bids_example("ds000117");
    let db = common::ingest(&root).await?;

    let walked: std::collections::BTreeSet<String> = common::walk_all(&root).into_iter().collect();
    let mut stmt = db.conn.prepare("SELECT file_path FROM file_registry")?;
    let registered: std::collections::BTreeSet<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;

    let missing: Vec<&String> = walked.difference(&registered).collect();
    let extra: Vec<&String> = registered.difference(&walked).collect();
    assert!(
        missing.is_empty(),
        "{} walked file(s) have no registry row, e.g. {:?}",
        missing.len(),
        &missing[..missing.len().min(5)]
    );
    assert!(
        extra.is_empty(),
        "{} registry row(s) name a file the walk never saw, e.g. {:?}",
        extra.len(),
        &extra[..extra.len().min(5)]
    );
    assert!(walked.len() > 2000, "expected a substantial corpus dataset");

    // The classes that used to be unrecorded, now addressable by their own path.
    for (pattern, kind) in [
        ("%_bold.json", "sidecar"),
        ("%.bval", "gradient"),
        ("%.bvec", "gradient"),
        ("README", "other"),
        ("participants.tsv", "tabular"),
        ("dataset_description.json", "description"),
    ] {
        let n: i64 = db.conn.query_row(
            "SELECT COUNT(*) FROM file_registry WHERE file_path LIKE ? AND kind = ?",
            duckdb::params![pattern, kind],
            |r| r.get(0),
        )?;
        assert!(n > 0, "no {kind} row for {pattern}");
    }
    Ok(())
}

/// `.bidsignore` is applied by the walk, so those files never reach the registry — and
/// ds000117 is the corpus dataset that actually exercises it (two patterns, 238 files).
/// Without this the test above could pass by comparing two identically-wrong sets.
#[tokio::test]
async fn bidsignored_files_are_absent_from_the_registry() -> anyhow::Result<()> {
    let root = common::bids_example("ds000117");
    let db = common::ingest(&root).await?;

    // `.bidsignore` lists `run-*_echo-*_FLASH.json` and
    // `**/sub-*_ses-mri_run-*_echo-*_FLASH.nii.gz`.
    let ignored: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM file_registry WHERE file_path LIKE '%FLASH.json' \
         OR file_path LIKE '%FLASH.nii.gz'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(ignored, 0, "a .bidsignore'd file must not be registered");

    // …but they are on disk, so the walk genuinely excluded them rather than the dataset
    // simply not having any.
    let on_disk = std::fs::read_dir(&root)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with("FLASH.json"))
        .count();
    assert!(on_disk > 0, "fixture no longer ships the ignored files");
    Ok(())
}

/// `file_id` is a `UBIGINT`, and the write path shapes values from JSON — which represents a
/// `u64` exactly, so unlike the 128-bit version it needs no decimal-string detour. Every route
/// into the column is pinned here, because getting this wrong corrupts a primary key
/// *silently*: `UBIGINT` sits in the same `is_numeric_col` set as the float types, whose
/// fallback is `parse::<f64>()`, and an f64 holds 53 bits of mantissa.
#[test]
fn a_64_bit_id_survives_the_write_path_or_is_refused() -> anyhow::Result<()> {
    let schema = Schema::load(None)?;
    let id_at = |row: &serde_json::Value| -> Option<duckdb::types::Value> {
        let cols = schema
            .write_columns("file_registry")
            .expect("registry columns");
        let idx = cols.iter().position(|c| c == "file_id").expect("file_id");
        schema.row_values("file_registry", row).unwrap()[idx].clone()
    };

    // The route the ingest uses: a plain JSON number, preserved exactly to the top of the
    // range. This is the whole point of moving off `HUGEINT` — `u64::MAX` is representable
    // in JSON, in `serde_json::Number`, in DuckDB, and in polars, with nothing in between
    // rounding or refusing it.
    for n in [0u64, 1, i64::MAX as u64, u64::MAX] {
        let row = serde_json::json!({ "file_id": n, "dataset_id": "d" });
        assert_eq!(
            id_at(&row),
            Some(duckdb::types::Value::UBigInt(n)),
            "a JSON number must round-trip every one of its 64 bits"
        );
    }

    // A decimal string still works, for a hand-built row or a caller outside the ingest.
    let row = serde_json::json!({ "file_id": u64::MAX.to_string() });
    assert_eq!(id_at(&row), Some(duckdb::types::Value::UBigInt(u64::MAX)));

    // A negative number is not an id. It must be refused rather than wrapped around into a
    // huge positive one, which would be a different file's key.
    let row = serde_json::json!({ "file_id": -1i64 });
    assert_eq!(
        id_at(&row),
        None,
        "a negative id must be refused, not wrapped"
    );

    // The trap that survives the width change: `serde_json` parses an integer literal too
    // large for `u64` as an `f64`, so the precision is gone before the write path sees it.
    // NULL fails the primary key loudly instead of corrupting it quietly.
    let too_big = "170141183460469231731687303715884105727"; // i128::MAX, well past u64
    let rounded: serde_json::Value = serde_json::from_str(&format!(r#"{{"file_id": {too_big}}}"#))?;
    assert_eq!(
        id_at(&rounded),
        None,
        "an already-rounded literal must be refused, not stored"
    );
    // …and the same value as a string, which reaches the parse rather than the number arm.
    let row = serde_json::json!({ "file_id": too_big });
    assert_eq!(id_at(&row), None, "an out-of-range string must be refused");

    // Non-numeric text is refused too, rather than becoming 0 or a partial parse.
    let row = serde_json::json!({ "file_id": "not-a-number" });
    assert_eq!(id_at(&row), None);
    Ok(())
}

/// A plain BIDS catalog carries no `projected` column anywhere — including on the registry.
/// ADR 0002 §7's "plain BIDS pays nothing, not merely little" has to keep holding now that
/// the registry is a second place the column could have appeared.
#[test]
fn a_plain_bids_registry_has_no_projection() -> anyhow::Result<()> {
    let db = fresh()?;
    assert!(
        names(
            &db,
            "SELECT table_name FROM information_schema.columns WHERE column_name = 'projected'",
        )?
        .is_empty(),
        "no projection column without a term map"
    );
    Ok(())
}
