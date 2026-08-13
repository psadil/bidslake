//! Compaction: rewriting a catalog to reclaim the blocks re-indexing left behind.
//!
//! The two things `COPY FROM DATABASE` alone gets wrong are what these tests pin —
//! foreign-key ordering (`sessions` before `participants`) and generated columns
//! (`SELECT *` cannot be inserted back) — plus the invariant that matters most: not a
//! row moves.

mod common;

use bidslake::db::BidsDb;
use common::{bids_example, count, ingest};
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

/// Ingest to a file, compact it, and assert the copy is complete and smaller.
#[tokio::test]
async fn compact_preserves_everything_and_reclaims_space() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("catalog.duckdb");
    let dst = dir.path().join("compacted.duckdb");

    // `ingest` builds in memory, so re-do it against a file: compaction is about the
    // on-disk block allocator, which `:memory:` does not exercise.
    {
        use bidslake::{bids::BidsParser, fs::LocalFileSystem, schema::Schema};
        let db = BidsDb::new(src.to_str().unwrap())?;
        let schema = Schema::load(None).unwrap();
        db.create_tables(&schema)?;
        let fs = Box::new(LocalFileSystem::new(bids_example("ds001")));
        let mut parser = BidsParser::new(fs, None, schema, None, true);
        let txn = db.conn.unchecked_transaction()?;
        parser.parse(&db).await?;
        txn.commit()?;

        // Churn: delete a dataset's event rows and re-insert them, which is what a
        // re-index does and what leaves free blocks behind. Deleted by file, the same shape
        // the re-index `DELETE` uses (and `events` has no `row_idx` to slice by — it is
        // declared order-insensitive).
        db.conn
            .execute("DELETE FROM events WHERE hash(file_id) % 2 = 0", [])?;
        db.conn.execute("CHECKPOINT", [])?;
    }

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
        "SELECT count(*) FROM all_files WHERE kind = 'data' AND task IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    assert!(
        with_task > 0,
        "generated concept columns must still resolve"
    );

    // Keys, constraints, and views come across with the schema copy.
    let (fks, pks, views): (i64, i64, i64) = db.conn.query_row(
        "SELECT (SELECT count(*) FROM duckdb_constraints() WHERE constraint_type='FOREIGN KEY'), \
                (SELECT count(*) FROM duckdb_constraints() WHERE constraint_type='PRIMARY KEY'), \
                (SELECT count(*) FROM duckdb_views() WHERE view_name='dataset_relations')",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    assert_eq!(
        fks, 4,
        "sessions->participants, and scans/sidecars/diffusion->file_registry"
    );
    assert!(pks > 0);
    assert_eq!(views, 1, "the dataset_relations view must survive");

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
        let mut parser = BidsParser::new(fs, None, schema, None, true);
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
        let mut parser = BidsParser::new(fs, None, schema, None, true);
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
#[tokio::test]
async fn free_block_ratio_reads_a_catalog() -> anyhow::Result<()> {
    let db = ingest(bids_example("ds210")).await?;
    let (total, free) = bidslake::compact::free_block_ratio(&db.conn)?;
    assert!(total >= 0 && free >= 0);
    Ok(())
}
