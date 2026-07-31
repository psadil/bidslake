//! Rewriting a catalog into a fresh file to reclaim space.
//!
//! Re-indexing a dataset `DELETE`s its rows and re-inserts them (see the batch path
//! in [`crate::bids`]). DuckDB reuses the vacated blocks for later writes but never
//! returns them to the OS, and `CHECKPOINT` does not shrink the file — only writing a
//! new one does. A profiled catalog was 28% free blocks: 478 MB of a 1.74 GB file.
//!
//! `COPY FROM DATABASE src TO dst` alone does not work here, for two reasons this
//! module works around:
//!
//! - It copies tables in catalog order rather than foreign-key dependency order, so
//!   `sessions` is inserted before `participants` and the FK rejects it. The data is
//!   fine; the order is not. So the schema is copied with `(SCHEMA)` and the rows are
//!   inserted per table, in dependency order derived from `duckdb_constraints()`.
//! - `SELECT *` includes the generated BIDS-concept columns (`task`, `sub`, …), which
//!   cannot be inserted into. The physical columns come from `pragma_storage_info`,
//!   which lists exactly what is stored — no SQL parsing, and no dependence on the
//!   `Schema` that built the catalog (which may have used overlays we do not have).

use anyhow::{Context, Result};
use duckdb::Connection;
use std::collections::{HashMap, HashSet};

use crate::schema::dynamic::quote_ident;

/// What a compaction did, for reporting.
pub struct CompactStats {
    pub tables: usize,
    pub rows: i64,
    pub blocks_before: i64,
    pub blocks_after: i64,
}

/// Free-block ratio of a catalog, for the post-index advisory.
pub fn free_block_ratio(conn: &Connection) -> Result<(i64, i64)> {
    let row = conn.query_row(
        "SELECT total_blocks, free_blocks FROM pragma_database_size()",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )?;
    Ok(row)
}

/// Order `tables` so that every table follows the ones it references. Cycles (which
/// bidslake's schema does not create) are left in input order rather than dropped.
fn fk_dependency_order(conn: &Connection, tables: &[String]) -> Result<Vec<String>> {
    let mut deps: HashMap<&str, Vec<String>> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT table_name, referenced_table FROM duckdb_constraints() \
         WHERE constraint_type = 'FOREIGN KEY' AND database_name = 'src'",
    )?;
    let edges = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (table, referenced) in &edges {
        if table != referenced {
            deps.entry(table.as_str())
                .or_default()
                .push(referenced.clone());
        }
    }

    let mut ordered: Vec<String> = Vec::with_capacity(tables.len());
    let mut placed: HashSet<String> = HashSet::new();
    // Few tables and at most a couple of edges, so repeated passes are cheaper than
    // building a graph. Each pass places every table whose references are all placed.
    for _ in 0..tables.len() {
        for table in tables {
            if placed.contains(table) {
                continue;
            }
            let ready = deps.get(table.as_str()).is_none_or(|refs| {
                refs.iter()
                    .all(|r| placed.contains(r) || !tables.contains(r))
            });
            if ready {
                placed.insert(table.clone());
                ordered.push(table.clone());
            }
        }
        if ordered.len() == tables.len() {
            break;
        }
    }
    // Anything left is in a cycle; append it so nothing is silently dropped.
    ordered.extend(tables.iter().filter(|t| !placed.contains(*t)).cloned());
    Ok(ordered)
}

/// The physically stored columns of `table`, per `pragma_storage_info` — which is the
/// definition of "stored", so generated columns are excluded without parsing DDL.
/// Empty for an empty table, which is why callers skip those.
fn physical_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let sql = format!(
        "SELECT DISTINCT column_name FROM pragma_storage_info('src.{}')",
        table.replace('\'', "''")
    );
    let mut stmt = conn.prepare(&sql)?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(cols)
}

/// Rewrite the catalog at `src_path` into a new file at `dst_path`, preserving every
/// table, row, key, constraint, and view. `dst_path` must not exist.
pub fn compact(src_path: &str, dst_path: &str) -> Result<CompactStats> {
    let conn = Connection::open_in_memory().context("opening working connection")?;
    conn.execute_batch(&format!(
        "ATTACH '{}' AS src (READ_ONLY); ATTACH '{}' AS dst;",
        src_path.replace('\'', "''"),
        dst_path.replace('\'', "''"),
    ))
    .context("attaching source and destination")?;

    let (blocks_before, _) = {
        let mut stmt =
            conn.prepare("SELECT total_blocks, free_blocks FROM pragma_database_size() WHERE database_name = 'src'")?;
        stmt.query_row([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
    };

    // DDL only — primary keys, foreign keys, views, and generated columns all come
    // across; the rows are inserted below, in an order the foreign keys accept.
    conn.execute_batch("COPY FROM DATABASE src TO dst (SCHEMA);")
        .context("copying schema")?;

    let tables: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT table_name FROM duckdb_tables() WHERE database_name = 'src' ORDER BY table_name",
        )?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let ordered = fk_dependency_order(&conn, &tables)?;

    let mut rows_copied = 0i64;
    for table in &ordered {
        let ident = quote_ident(table);
        let n: i64 = conn.query_row(&format!("SELECT count(*) FROM src.{ident}"), [], |r| {
            r.get(0)
        })?;
        if n == 0 {
            continue;
        }
        let cols = physical_columns(&conn, table)?;
        if cols.is_empty() {
            anyhow::bail!("table {table:?} has {n} rows but no stored columns");
        }
        let select = cols
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute(
            &format!("INSERT INTO dst.{ident} BY NAME SELECT {select} FROM src.{ident}"),
            [],
        )
        .with_context(|| format!("copying rows of {table}"))?;
        rows_copied += n;
    }

    // Every table must arrive with the same row count; a silent shortfall here would
    // be worse than a failed compaction.
    for table in &ordered {
        let ident = quote_ident(table);
        let (before, after): (i64, i64) = conn.query_row(
            &format!(
                "SELECT (SELECT count(*) FROM src.{ident}), (SELECT count(*) FROM dst.{ident})"
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if before != after {
            anyhow::bail!("{table}: copied {after} rows but source has {before}");
        }
    }

    conn.execute_batch("CHECKPOINT dst;")
        .context("checkpointing destination")?;
    let blocks_after: i64 = conn.query_row(
        "SELECT total_blocks FROM pragma_database_size() WHERE database_name = 'dst'",
        [],
        |r| r.get(0),
    )?;

    Ok(CompactStats {
        tables: ordered.len(),
        rows: rows_copied,
        blocks_before,
        blocks_after,
    })
}
