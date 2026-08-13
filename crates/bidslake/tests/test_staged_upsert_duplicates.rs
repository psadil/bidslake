//! What the staged upsert does with a duplicate primary key, in both directions.
//!
//! `BidsDb::upsert_staged` writes through a temp table cloned with `CREATE TABLE … AS SELECT`,
//! which does **not** copy constraints — so the stage has no primary key, the Appender has
//! nothing to violate, and the closing `INSERT OR REPLACE … SELECT` does not raise on a
//! duplicate within its own source either. Two staged rows sharing a key insert as one,
//! keeping the *first*.
//!
//! That is the opposite of the row-at-a-time `INSERT OR REPLACE` this replaced, where the
//! *last* write won, and it is silent either way. Every caller therefore dedups before it
//! stages, and these tests exist so that contract is executable rather than a comment: if a
//! DuckDB upgrade ever starts raising here (or starts letting both rows through), the write
//! path's correctness argument has changed and this fails.

use bidslake::db::{BidsDb, FileAssociation};
use bidslake::schema::Schema;

/// `file_associations` because it carries no foreign keys — the duplicate behaviour is a
/// property of the staging pattern, and this keeps the setup to one table.
fn db() -> anyhow::Result<BidsDb> {
    let db = BidsDb::new(":memory:")?;
    db.create_tables(&Schema::load(None)?)?;
    Ok(db)
}

fn assoc(target_id: &str) -> FileAssociation {
    FileAssociation {
        source_file_id: "1".to_string(),
        target_file_id: Some(target_id.to_string()),
        target_file_path: "sub-01/func/sub-01_task-rest_bold.nii.gz".to_string(),
        assoc_type: "events".to_string(),
    }
}

/// The property the callers must not rely on being caught.
#[test]
fn a_duplicate_within_one_batch_is_dropped_silently_keeping_the_first() -> anyhow::Result<()> {
    let db = db()?;

    // Same primary key, different `target_file_id` — the column that sits outside the key and
    // is the one a re-index legitimately changes.
    db.upsert_file_associations(&[assoc("100"), assoc("200")])?;

    let (rows, target): (i64, i64) = db.conn.query_row(
        "SELECT COUNT(*), MAX(target_file_id) FROM file_associations",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(rows, 1, "the duplicate collapsed rather than erroring");
    assert_eq!(
        target, 100,
        "the *first* staged row won; the row-at-a-time path this replaced kept the last"
    );
    Ok(())
}

/// The other direction, which is the whole point of `OR REPLACE`: a row already in the table
/// is refreshed, not skipped and not collided with. `target_file_id` starting NULL and
/// resolving on a later run is the case docs/adr/0005 turns on.
#[test]
fn a_conflict_with_an_existing_row_replaces_it() -> anyhow::Result<()> {
    let db = db()?;

    let mut dangling = assoc("0");
    dangling.target_file_id = None;
    db.upsert_file_associations(&[dangling])?;
    let unresolved: Option<i64> =
        db.conn
            .query_row("SELECT target_file_id FROM file_associations", [], |r| {
                r.get(0)
            })?;
    assert_eq!(unresolved, None, "the target was not in the catalog yet");

    // A second run, with the target now resolvable.
    db.upsert_file_associations(&[assoc("42")])?;

    let (rows, target): (i64, Option<i64>) = db.conn.query_row(
        "SELECT COUNT(*), MAX(target_file_id) FROM file_associations",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(rows, 1, "the re-index refreshed rather than duplicating");
    assert_eq!(target, Some(42), "and the NULL was not frozen in");
    Ok(())
}

/// An empty batch must not create, and then fail to drop, a staging table.
#[test]
fn an_empty_batch_writes_nothing() -> anyhow::Result<()> {
    let db = db()?;
    db.upsert_file_associations(&[])?;
    db.upsert_bvals(&[])?;
    db.upsert_bvecs(&[])?;
    let rows: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM file_associations", [], |r| r.get(0))?;
    assert_eq!(rows, 0);
    Ok(())
}
