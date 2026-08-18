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
use rstest::rstest;
use uuid::Uuid;

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
        (1u128, "sub-01/func/sub-01_task-x_run-2_bold.nii.gz", "data"),
        (2, "sub-01/anat/sub-01_T1w.json", "sidecar"),
        (3, "README", "other"),
        // An id with every bit set. `UUID` is `PhysicalType::INT128` internally and DuckDB
        // flips the top bit so `ORDER BY uuid` agrees with `ORDER BY uuid::varchar`, so the
        // extremes of the range are worth one row rather than none.
        (u128::MAX, "sub-02/dwi/sub-02_dwi.nii.gz", "data"),
        // The case the single view exists for: a per-row table keys on this, and it is not
        // a data file.
        (5, "sub-01/func/sub-01_task-x_run-2_events.tsv", "tabular"),
    ] {
        insert.execute(duckdb::params![
            Uuid::from_u128(id).to_string(),
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

    let concepts = |n: u128| -> anyhow::Result<(String, String, String, String, String)> {
        Ok(db.conn.query_row(
            "SELECT sub, task, run, datatype, suffix FROM all_files WHERE file_id = ?",
            [Uuid::from_u128(n).to_string()],
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
        &format!(
            "SELECT modality FROM all_files WHERE file_id = '{}'",
            Uuid::from_u128(1)
        ),
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
/// and reach the images through `file_associations` — docs/adr/0003).
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
    Ok(())
}

/// The file classes that used to be unrecorded, now addressable by their own path and
/// carrying the `kind` that says what they are.
#[rstest]
#[case("%_bold.json", "sidecar")]
#[case("%.bval", "gradient")]
#[case("%.bvec", "gradient")]
#[case("README", "other")]
#[case("participants.tsv", "tabular")]
#[case("dataset_description.json", "description")]
#[tokio::test]
async fn every_file_class_is_addressable_by_its_own_path(
    #[case] pattern: &str,
    #[case] kind: &str,
) -> anyhow::Result<()> {
    let db = common::ingest(common::bids_example("ds000117")).await?;

    let n: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM file_registry WHERE file_path LIKE ? AND kind = ?",
        duckdb::params![pattern, kind],
        |r| r.get(0),
    )?;

    assert!(n > 0, "no {kind} row for {pattern}");
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

/// The route the ingest uses: the canonical hyphenated spelling, preserved exactly across
/// the whole 128-bit range. A `UUID` is the one 128-bit type that survives the trip to
/// Python, and a string is the one representation `serde_json` has for it — so unlike the
/// `HUGEINT` attempt there is nothing in between to round or refuse it.
#[rstest]
#[case(0)]
#[case(1)]
#[case(u64::MAX as u128)]
#[case(u128::MAX)]
fn an_id_round_trips_every_one_of_its_128_bits(#[case] n: u128) -> anyhow::Result<()> {
    let schema = Schema::load(None)?;
    let id = Uuid::from_u128(n);
    let row = serde_json::json!({ "file_id": id.to_string(), "dataset_id": "d" });

    let got = registry_file_id(&schema, &row);

    assert_eq!(
        got,
        Some(duckdb::types::Value::Text(id.to_string())),
        "a canonical UUID string must round-trip every one of its 128 bits"
    );
    Ok(())
}

/// The `file_id` the write path shapes out of `row`.
fn registry_file_id(schema: &Schema, row: &serde_json::Value) -> Option<duckdb::types::Value> {
    let cols = schema
        .write_columns("file_registry")
        .expect("registry columns");
    let idx = cols.iter().position(|c| c == "file_id").expect("file_id");
    schema.row_values("file_registry", row).unwrap()[idx].clone()
}

/// The routes into the column that are **not** a canonical UUID string.
///
/// Two things have to hold, and they pull in opposite directions. A *number* must be refused
/// at the Rust boundary — it is the shape a stale 64-bit id arrives in, and the generic
/// numeric arms would happily turn it into something. Anything else that is merely
/// *malformed* is allowed through to DuckDB, whose cast throws: what must never happen is a
/// third outcome where a bad id becomes NULL or a different file's key.
#[test]
fn a_128_bit_id_survives_the_write_path_or_is_refused() -> anyhow::Result<()> {
    let schema = Schema::load(None)?;
    let id_at = |row: &serde_json::Value| schema.row_values("file_registry", row);

    // A number in an id column is a caller bug, not a value to coerce — most likely a
    // 64-bit id from before the width changed. Refused with a message, rather than the NULL
    // the primary key would reject with nothing pointing at why.
    let row = serde_json::json!({ "file_id": 4_099_505_605_929_783_485i64 });
    let err = id_at(&row).unwrap_err().to_string();
    assert!(
        err.contains("UUID column") && err.contains("file_registry.file_id"),
        "a numeric id must be refused by name, got: {err}"
    );

    // The same for a negative one, which under the old signed `HUGEINT` was half the range.
    let row = serde_json::json!({ "file_id": -1i64 });
    assert!(id_at(&row).is_err(), "a negative id is still a number");

    // A malformed *string* is passed to DuckDB rather than second-guessed here, because the
    // Appender casts it and throws — see `a_malformed_id_is_refused_by_the_engine`. What
    // matters is that it is not silently dropped on the way.
    let row = serde_json::json!({ "file_id": "not-a-uuid" });
    assert_eq!(
        registry_file_id(&schema, &row),
        Some(duckdb::types::Value::Text("not-a-uuid".to_string())),
        "a malformed id must reach the engine, not become NULL here"
    );
    Ok(())
}

/// …and the engine end of that contract: the malformed id above fails the run.
///
/// This is the property the whole width argument rests on. An id that cannot be stored must
/// stop the ingest, because the alternative — a NULL, or a wrapped value — is a row filed
/// under a key that belongs to a different file, and nothing downstream could tell.
#[tokio::test]
async fn a_malformed_id_is_refused_by_the_engine() -> anyhow::Result<()> {
    let db = fresh()?;
    let schema = Schema::load(None)?;
    let row = serde_json::json!({
        "file_id": "not-a-uuid",
        "dataset_id": "ds",
        "root_uri": "file:///r",
        "file_path": "sub-01/anat/sub-01_T1w.nii.gz",
        "kind": "data",
    });

    // `{:#}` for the whole context chain: anyhow's plain `Display` prints only the outermost
    // `.context()`, which here is "upserting 1 rows into file_registry" — true, and silent
    // about why.
    let err = format!(
        "{:#}",
        db.upsert_rows(&schema, "file_registry", &[row])
            .unwrap_err()
    );

    assert!(
        err.contains("not-a-uuid") || err.contains("Conversion Error"),
        "a malformed id must fail the run and say so, got: {err}"
    );
    assert_eq!(
        common::count(&db, "file_registry")?,
        0,
        "and nothing may be stored"
    );
    Ok(())
}

/// A `file_id` derived twice is the same id, and `bids::file_id` is the one place that says
/// so — which is why it is public API rather than an internal detail.
#[test]
fn the_derivation_is_a_well_formed_v8_uuid() {
    let id = bidslake::bids::file_id("ds", "file:///r", "sub-01/anat/sub-01_T1w.nii.gz");
    assert_eq!(id.get_version_num(), 8, "RFC 9562 vendor-specific version");
    assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
    assert_eq!(
        id,
        bidslake::bids::file_id("ds", "file:///r", "sub-01/anat/sub-01_T1w.nii.gz")
    );
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
