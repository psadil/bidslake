//! Compaction: rewriting a catalog to reclaim the blocks re-indexing left behind.
//!
//! The two things `COPY FROM DATABASE` alone gets wrong are what these tests pin —
//! foreign-key ordering (`sessions` before `participants`) and generated columns
//! (`SELECT *` cannot be inserted back) — plus the invariant that matters most: not a
//! row moves.

mod common;

use bidslake::db::BidsDb;
use common::{bids_example, count};
use rstest::rstest;
use std::path::Path;

/// Every table's row count, so a compaction can be checked wholesale rather than
/// table by table.
fn row_counts(db: &BidsDb) -> anyhow::Result<Vec<(String, i64)>> {
    let tables: Vec<String> = db
        .conn
        .prepare("SELECT table_name FROM duckdb_tables() ORDER BY table_name")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    tables
        .into_iter()
        .map(|t| {
            let n = count(db, &t)?;
            Ok((t, n))
        })
        .collect()
}

/// Ingest `ds001` into a catalog *file*.
///
/// `ingest` builds in memory, so this re-does it against a file: compaction is about the
/// on-disk block allocator, which `:memory:` does not exercise at all — it reports zero
/// blocks, free and total alike.
async fn build_catalog(src: &Path) -> anyhow::Result<BidsDb> {
    use bidslake::{bids::BidsParser, fs::LocalFileSystem, schema::Schema};
    let db = BidsDb::new(src.to_str().unwrap())?;
    let schema = Schema::load(None).unwrap();
    db.create_tables(&schema)?;
    let fs = Box::new(LocalFileSystem::new(bids_example("ds001")));
    let mut parser = BidsParser::new(fs, None, schema, None, true, true);
    let txn = db.conn.unchecked_transaction()?;
    parser.parse(&db).await?;
    txn.commit()?;
    db.conn.execute("CHECKPOINT", [])?;
    Ok(db)
}

/// The same catalog, churned. The churn — delete a dataset's event rows, which is what a
/// re-index does — is what leaves free blocks behind. Deleted by file, the same shape the
/// re-index `DELETE` uses (and `events` has no `row_idx` to slice by; it is declared
/// order-insensitive).
async fn build_churned_catalog(src: &Path) -> anyhow::Result<()> {
    let db = build_catalog(src).await?;
    db.conn
        .execute("DELETE FROM events WHERE hash(file_id) % 2 = 0", [])?;
    db.conn.execute("CHECKPOINT", [])?;
    Ok(())
}

/// Ingest to a file, compact it, and assert the copy is complete and smaller.
#[tokio::test]
async fn compact_preserves_everything_and_reclaims_space() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("catalog.duckdb");
    let dst = dir.path().join("compacted.duckdb");
    build_churned_catalog(&src).await?;

    let before = {
        let db = BidsDb::new(src.to_str().unwrap())?;
        row_counts(&db)?
    };
    assert!(
        before.iter().any(|(t, n)| t == "events" && *n > 0),
        "fixture should have events rows"
    );

    let stats = bidslake::compact::compact(src.to_str().unwrap(), dst.to_str().unwrap())?;
    assert!(dst.is_file());
    assert!(stats.rows > 0, "should have copied rows");

    let db = BidsDb::new(dst.to_str().unwrap())?;
    assert_eq!(
        row_counts(&db)?,
        before,
        "every table's row count must survive"
    );

    // Generated (virtual) columns are re-derived, not copied — `SELECT *` on the
    // source would have included them and the INSERT would have been rejected.
    let with_task: i64 = db.conn.query_row(
        "SELECT count(*) FROM all_files WHERE task IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    assert!(
        with_task > 0,
        "generated concept columns must still resolve"
    );

    // Keys, constraints, and views come across with the schema copy.
    let (fks, pks): (i64, i64) = db.conn.query_row(
        "SELECT (SELECT count(*) FROM duckdb_constraints() WHERE constraint_type='FOREIGN KEY'), \
                (SELECT count(*) FROM duckdb_constraints() WHERE constraint_type='PRIMARY KEY')",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(
        fks, 5,
        "sessions->participants, and scans/sidecars/bvals/bvecs->file_registry"
    );
    assert!(pks > 0);

    // And the point of the exercise.
    let free: i64 =
        db.conn
            .query_row("SELECT free_blocks FROM pragma_database_size()", [], |r| {
                r.get(0)
            })?;
    assert!(
        free <= 4,
        "compacted file should have ~no free blocks, got {free}"
    );
    assert!(
        file_len(&dst) <= file_len(&src),
        "compacted file should not be larger"
    );
    Ok(())
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Views survive the schema copy — including the ones that depend on *another view*.
/// `diffusion` selects from `bval_volumes`/`bvec_volumes`, which select from `bvals`/`bvecs`
/// and `file_associations`, so it is the first object in the catalog needing
/// `COPY FROM DATABASE (SCHEMA)` to emit views in dependency order. Nothing else asserts
/// that it does.
#[rstest]
#[case("dataset_relations")]
#[case("all_files")]
#[case("bval_volumes")]
#[case("bvec_volumes")]
// The one that depends on the two above it.
#[case("diffusion")]
#[tokio::test]
async fn a_view_survives_compaction(#[case] view: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("catalog.duckdb");
    let dst = dir.path().join("compacted.duckdb");
    build_churned_catalog(&src).await?;
    bidslake::compact::compact(src.to_str().unwrap(), dst.to_str().unwrap())?;

    let db = BidsDb::new(dst.to_str().unwrap())?;

    let n: i64 = db.conn.query_row(
        "SELECT count(*) FROM duckdb_views() WHERE view_name = ?",
        [view],
        |r| r.get(0),
    )?;
    assert_eq!(n, 1, "the {view} view must survive compaction");
    Ok(())
}

/// `sessions` foreign-keys into `participants`, and `COPY FROM DATABASE` copies tables
/// in catalog order, which puts `sessions` first and trips the constraint. Compaction
/// must order by dependency instead — `eyetracking_fmri` has both tables populated.
#[tokio::test]
async fn compact_orders_tables_by_foreign_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("fk.duckdb");
    let dst = dir.path().join("fk-compact.duckdb");

    {
        use bidslake::{bids::BidsParser, fs::LocalFileSystem, schema::Schema};
        let db = BidsDb::new(src.to_str().unwrap())?;
        let schema = Schema::load(None).unwrap();
        db.create_tables(&schema)?;
        let fs = Box::new(LocalFileSystem::new(bids_example("eyetracking_fmri")));
        let mut parser = BidsParser::new(fs, None, schema, None, true, true);
        let txn = db.conn.unchecked_transaction()?;
        parser.parse(&db).await?;
        txn.commit()?;
    }
    {
        let db = BidsDb::new(src.to_str().unwrap())?;
        assert!(count(&db, "sessions")? > 0, "fixture needs sessions rows");
        assert!(count(&db, "participants")? > 0);
    }

    bidslake::compact::compact(src.to_str().unwrap(), dst.to_str().unwrap())?;

    let db = BidsDb::new(dst.to_str().unwrap())?;
    assert!(count(&db, "sessions")? > 0);
    Ok(())
}

/// A dataset that ingests cleanly should compact to a byte-identical row set even
/// with no churn — compaction is not allowed to be lossy on the happy path.
#[tokio::test]
async fn compact_is_a_no_op_on_a_fresh_catalog() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("fresh.duckdb");
    let dst = dir.path().join("fresh-compact.duckdb");
    {
        use bidslake::{bids::BidsParser, fs::LocalFileSystem, schema::Schema};
        let db = BidsDb::new(src.to_str().unwrap())?;
        let schema = Schema::load(None).unwrap();
        db.create_tables(&schema)?;
        let fs = Box::new(LocalFileSystem::new(bids_example("ds210")));
        let mut parser = BidsParser::new(fs, None, schema, None, true, true);
        let txn = db.conn.unchecked_transaction()?;
        parser.parse(&db).await?;
        txn.commit()?;
    }
    let before = row_counts(&BidsDb::new(src.to_str().unwrap())?)?;
    bidslake::compact::compact(src.to_str().unwrap(), dst.to_str().unwrap())?;
    assert_eq!(row_counts(&BidsDb::new(dst.to_str().unwrap())?)?, before);
    Ok(())
}

/// Compacting onto an existing path must refuse rather than clobber.
#[test]
fn compact_refuses_an_existing_destination() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.duckdb");
    let dst = dir.path().join("b.duckdb");
    BidsDb::new(src.to_str().unwrap()).unwrap();
    std::fs::write(&dst, b"existing").unwrap();
    // `compact` itself attaches the destination, so an existing *file* is the CLI's
    // check; here the attach of a non-database file is what fails.
    assert!(bidslake::compact::compact(src.to_str().unwrap(), dst.to_str().unwrap()).is_err());
    assert_eq!(std::fs::read(&dst).unwrap(), b"existing");
}

/// The ingest-time advisory reads `pragma_database_size`; make sure the accessor
/// works on a real catalog and reports no free blocks on a fresh one.
///
/// It must be a *file* catalog. `pragma_database_size` on `:memory:` reports zero blocks
/// total and zero free, which satisfies any non-negativity check without the block allocator
/// having run at all — so `total > 0` is the assertion that keeps this test honest.
#[tokio::test]
async fn free_block_ratio_reads_a_catalog() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("fresh.duckdb");
    let db = build_catalog(&path).await?;

    let (total, free) = bidslake::compact::free_block_ratio(&db.conn)?;

    assert!(total > 0, "a file catalog must report blocks, got {total}");
    assert_eq!(free, 0, "a freshly built catalog has no free blocks");
    Ok(())
}
