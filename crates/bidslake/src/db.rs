//! Thin wrapper over the DuckDB connection.
//!
//! [`BidsDb`] owns the `duckdb::Connection` and exposes the write primitives the
//! ingestion pipeline uses. Row shaping and SQL generation for these methods live
//! in [`crate::schema`]; this module just routes calls to it and shapes the rows of
//! the **static** tables itself ([`BidsDb::upsert_file_associations`],
//! [`BidsDb::upsert_bvals`], [`BidsDb::upsert_bvecs`]), whose DDL is hand-written and
//! so unknown to the schema. All four bulk paths stage through `upsert_staged`.
//!
//! Note that the tabular ingest in [`crate::bids`] (and the driver in `main`) also
//! execute their own hand-built SQL directly against the public [`BidsDb::conn`] —
//! the batched `read_csv` inserts, re-index `DELETE`s, and count-back `SELECT`s — by
//! design; this module deliberately does not gate every statement.

use crate::links::Identity;
use crate::schema::{self, Schema};
use anyhow::{Context as _, Result};
use duckdb::{Connection, params};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// A [`duckdb::Error`] as a *leaf* [`anyhow::Error`], discarding its `source()`.
///
/// `duckdb::Error`'s own `Display` is the complete diagnostic — the error class, the
/// constraint or column at fault, and DuckDB's own suggestion. Its source is the raw FFI
/// code, whose `Display` is the constant `Error code 1: Unknown error code`, so any
/// rendering that walks the chain (`{:#}`, which is what `main` prints and what the Python
/// bindings return) appends that to every message:
///
/// ```text
/// upserting 1 rows into file_registry: moving staged rows into file_registry:
/// Constraint Error: NOT NULL constraint failed: file_registry.file_id
///                                              : Error code 1: Unknown error code  <- always
/// ```
///
/// Re-wrapping the message as a fresh error truncates the chain where it stops being worth
/// reading. Mirrors `duck` in `bidslake-py`, for the same reason and with the same effect.
pub(crate) fn duck(e: duckdb::Error) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// Lowercase-hex SHA-256 of `bytes` (overlay provenance digests in `stamp_schema`).
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// What bidslake did with a file — the `status` column of `file_registry`. A closed set as a
/// type, not a `&str`, so a typo cannot silently corrupt the tabular-coverage invariant this
/// column backs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabularStatus {
    /// Its rows are in a data table, joinable on `file_id`.
    Ingested,
    /// A compressed continuous recording deliberately left on disk, contents unread.
    OnDisk,
    /// A tabular file the BIDS schema does not describe, so nothing routed it.
    Skipped,
    /// A batch `INSERT` execution failure dropped this file's rows for the run.
    Failed,
}

impl TabularStatus {
    /// The literal stored in the `status` column.
    pub fn as_str(self) -> &'static str {
        match self {
            TabularStatus::Ingested => "ingested",
            TabularStatus::OnDisk => "on_disk",
            TabularStatus::Skipped => "skipped",
            TabularStatus::Failed => "failed",
        }
    }
}

/// A cross-reference between two files derived at ingest (e.g. an fmap's
/// `IntendedFor`). Written to the `file_associations` table.
#[derive(Debug, Clone)]
pub struct FileAssociation {
    /// The `bids::file_id` of the file the edge starts from — the *data* file whose
    /// `meta.associations` were resolved, or the file whose sidecar named an `IntendedFor`
    /// target. Never optional: only files the walk saw can be sources, so the id always
    /// resolves.
    pub source_file_id: Uuid,
    /// `None` when the target names a file this dataset does not ship — a dangling
    /// `IntendedFor`. Kept rather than dropped (see the table's DDL), which is why the
    /// path below travels alongside the id.
    pub target_file_id: Option<Uuid>,
    /// The target's dataset-relative path, already normalized — an `IntendedFor` written as a
    /// BIDS URI or as a subject-relative path arrives here resolved against the declaring
    /// file, and one naming *another* dataset never arrives at all.
    ///
    /// Stored even when [`Self::target_file_id`] is `None`, where it is the only record of
    /// what was pointed at, which is why it and not the target id is the third column of the
    /// table's primary key.
    pub target_file_path: String,
    /// Which relation this edge is, stored as `association_type`: a `meta.associations` key
    /// from the BIDS schema (`events`, `bval`, `channels`, …) for a structural association,
    /// or, for an `IntendedFor`, the declaring file's datatype — `fmap` becomes `fieldmap`,
    /// any other datatype is used verbatim, and a file outside a datatype directory falls
    /// back to `intended_for`.
    ///
    /// It is also what a `describes` view filters on to pick its edges (ADR 0003), so a table
    /// policy naming an association this column never carries yields a view that is simply
    /// empty.
    pub assoc_type: String,
}

/// Owns the DuckDB connection bidslake writes to and queries.
pub struct BidsDb {
    /// The open DuckDB connection, public deliberately.
    ///
    /// The ingest in [`crate::bids`] and the CLI both run SQL this type has no method for —
    /// the batched `read_csv` inserts, the re-index `DELETE`s, the count-back `SELECT`s — and
    /// this module does not try to gate every statement, so the connection is part of the
    /// API. What a caller gets is a connection the constructors have already configured:
    /// [`BidsDb::open_with_temp_dir`] has pointed its spill directory somewhere local.
    ///
    /// One connection, never a pool. `duckdb::Connection` is `Send` but not `Sync`, so a
    /// `BidsDb` is used from one place at a time; the concurrency in an ingest is in reading
    /// files, not in writing rows.
    pub conn: Connection,
}

impl BidsDb {
    /// Open (or create) the database at `path`. Use `":memory:"` for a transient
    /// in-memory database.
    pub fn new(path: &str) -> Result<Self> {
        Self::open_with_temp_dir(path, None)
    }

    /// Open the database and point DuckDB's spill directory at `temp_dir` (or the
    /// platform temp directory when `None`).
    ///
    /// Worth being explicit about: DuckDB spills to a `.tmp` directory *beside the
    /// database file* by default, and a large ingest does spill — wide `sidecars`
    /// rows and multi-file `read_csv` batches both exceed the memory limit. When the
    /// catalog lives on a network filesystem, that turns a spill into random remote
    /// I/O. The default here keeps it on local disk regardless of where `--output`
    /// points.
    pub fn open_with_temp_dir(path: &str, temp_dir: Option<&std::path::Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        let temp_dir = temp_dir
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        conn.execute(
            &format!(
                "SET temp_directory = '{}'",
                temp_dir.display().to_string().replace('\'', "''")
            ),
            [],
        )?;
        Ok(Self { conn })
    }

    /// Create every table: the schema-generated ones ([`Schema::create_tables_sql`])
    /// plus the static `bvals`/`bvecs`, `file_associations`, and cross-dataset
    /// `dataset_links`/`dataset_identity` tables, then every view.
    ///
    /// Tables first — all of them — because a view may select from a static table just as
    /// easily as from a generated one, and `diffusion` selects from two *generated* views
    /// (ADR 0003), so it comes last of all.
    ///
    /// Creates nothing and returns `Err` when an existing catalog's `file_registry` lacks a
    /// *physical* column this run would write — in practice the `projected` column a term map
    /// needs — because `IF NOT EXISTS` would otherwise drop the difference in silence. The
    /// concept columns are not covered: they are select items of the `all_files` view, which
    /// is emitted `CREATE OR REPLACE`, so a wider run simply redefines them. See
    /// `check_registry_shape`, and the narrowing caveat ADR 0006 records as open.
    pub fn create_tables(&self, schema: &Schema) -> Result<()> {
        self.check_registry_shape(schema)?;
        // Tables first — all of them — then every view, because a view may select from a
        // static table (`bvals` through `file_associations`) just as easily as from a
        // generated one (`all_files` from `file_registry`). See `create_views_sql`.
        for sql in schema.create_tables_sql() {
            self.conn.execute(&sql, [])?;
        }
        // Static tables: the gradient payloads and file associations.
        self.conn.execute(schema::CREATE_BVALS_TABLE, [])?;
        self.conn.execute(schema::CREATE_BVECS_TABLE, [])?;
        self.conn
            .execute(schema::CREATE_FILE_ASSOCIATIONS_TABLE, [])?;
        self.conn
            .execute(schema::CREATE_TABULAR_UNDECLARED_COLUMNS_TABLE, [])?;
        // The ingest roots a dataset was built from (see docs/adr/0005).
        self.conn.execute(schema::CREATE_DATASET_ROOTS_TABLE, [])?;
        // `CREATE TABLE IF NOT EXISTS` leaves an already-existing table alone, so a catalog
        // built before docs/adr/0007 keeps a two-column `dataset_roots` and every read of
        // `tenure` fails on it. Adding the column here is the same courtesy `link init`
        // extends — an old catalog gains the concept without a re-index — and the default
        // is the honest answer for a root indexed before tenure existed: `attached`.
        self.conn
            .execute(schema::ADD_DATASET_ROOTS_TENURE, [])
            .context("adding dataset_roots.tenure to an existing catalog")?;
        // Cross-dataset links (see docs/adr/0003).
        self.conn.execute(schema::CREATE_DATASET_LINKS_TABLE, [])?;
        self.conn
            .execute(schema::CREATE_DATASET_IDENTITY_TABLE, [])?;

        for sql in schema.create_views_sql() {
            self.conn.execute(&sql, [])?;
        }
        // The two query-time views over `dataset_links` — provenance and naming
        // respectively (docs/adr/0003) — and `diffusion`, which composes the two generated
        // gradient views and so must follow them (docs/adr/0003).
        self.conn
            .execute(schema::CREATE_DATASET_RELATIONS_VIEW, [])?;
        self.conn
            .execute(schema::CREATE_DATASET_LINK_TARGETS_VIEW, [])?;
        self.conn.execute(schema::CREATE_DIFFUSION_VIEW, [])?;
        self.stamp_meta(schema)?;
        self.stamp_schema(schema)?;
        Ok(())
    }

    /// Refuse to index into a catalog whose file registry is narrower than this run
    /// needs.
    ///
    /// Tables are created `IF NOT EXISTS`, so `file_registry` keeps the shape the *first*
    /// run gave it — while datasets are meant to accumulate across runs (ADR 0002). A
    /// second run needing a physical column the registry lacks would drop it silently.
    ///
    /// Only the *physical* shape is frozen, which in practice is one column: `projected`,
    /// present only when a term map is configured. The BIDS-concept columns are select items
    /// of the `all_files` view, emitted `CREATE OR REPLACE`, so a wider run redefines them
    /// retroactively for rows already stored and needs no refusal (ADR 0006).
    ///
    /// The remedy is that the adapter set describes the **catalog**, not the dataset being
    /// added: name every adapter the catalog uses on every run, or index into a fresh one.
    fn check_registry_shape(&self, schema: &Schema) -> Result<()> {
        let exists: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM duckdb_tables() WHERE table_name = 'file_registry'",
            [],
            |r| r.get(0),
        )?;
        if exists == 0 {
            return Ok(()); // fresh catalog; the DDL below defines the shape
        }
        let mut stmt = self.conn.prepare(
            "SELECT column_name FROM information_schema.columns WHERE table_name = 'file_registry'",
        )?;
        let have: std::collections::HashSet<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        let missing: Vec<String> = schema
            .write_columns("file_registry")
            .unwrap_or_default()
            .into_iter()
            .filter(|c| !have.contains(c))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        anyhow::bail!(
            "this catalog's `file_registry` has no column for {}, so what this run would \
             record there is dropped. Tables are created only if absent, so the registry \
             keeps the shape of the run that created it. The adapter set describes the \
             catalog rather than one dataset: pass every adapter this catalog uses on \
             every index run (order then does not matter), or index into a new catalog.",
            missing.join(", ")
        )
    }

    /// Record which BIDS schema version (and bidslake build) produced this
    /// catalog, in a one-row `bidslake_meta` table. Downstream readers (notably
    /// the Python query package) compare this to what they were generated
    /// against, so a version mismatch is *detectable* rather than guessed —
    /// they then fall back to runtime column introspection. Idempotent across
    /// re-indexing: the row is inserted only if the table is empty.
    fn stamp_meta(&self, schema: &Schema) -> Result<()> {
        let raw = schema.raw();
        let schema_version = raw
            .get("schema_version")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let bids_version = raw
            .get("bids_version")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS bidslake_meta (\
             schema_version TEXT, bids_version TEXT, bidslake_version TEXT)",
            [],
        )?;
        self.conn.execute(
            "INSERT INTO bidslake_meta (schema_version, bids_version, bidslake_version) \
             SELECT ?, ?, ? WHERE NOT EXISTS (SELECT 1 FROM bidslake_meta)",
            params![schema_version, bids_version, crate::BUILD],
        )?;
        Ok(())
    }

    /// Embed the effective schema so every database is self-describing: the Python
    /// query side and the `--from-db` stubgen recover the exact schema the catalog was
    /// built from without re-passing anything. When overlays were applied, their
    /// provenance is also recorded (in `bidslake_overlays`, and as `overlay_digest`).
    /// `bidslake_meta.schema_version` still holds the base version. Idempotent.
    fn stamp_schema(&self, schema: &Schema) -> Result<()> {
        let raw = schema.raw();
        let base_schema_version = raw
            .get("schema_version")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let effective_schema = serde_json::to_string(raw).unwrap_or_default();
        let overlays = schema.overlays();
        let overlay_digest: Option<String> = (!overlays.is_empty()).then(|| {
            let contents: Vec<&Value> = overlays.iter().map(|o| &o.content).collect();
            sha256_hex(&serde_json::to_vec(&contents).unwrap_or_default())
        });

        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS bidslake_schema (\
             base_schema_version TEXT, effective_schema JSON, overlay_digest TEXT)",
            [],
        )?;
        // Replaced, not inserted-if-absent. The stamp describes *this catalog's* effective
        // schema, and the tables it describes are created `IF NOT EXISTS` — so indexing one
        // root plainly and a second with `--adapter fmriprep` leaves the catalog physically
        // holding `fmriprep_confounds` while a first-run-only stamp still reports base-only
        // and a NULL `overlay_digest`. Freezing it made the self-description a lie in exactly
        // the case it exists to answer.
        //
        // What this records is the *most recent* run's schema, which is a superset of the
        // catalog's tables only when each run's overlay set includes the last one's. A stamp
        // that unioned every run's schema would be strictly better and is not what this is;
        // `check_registry_shape` remains the check that the tables themselves agree.
        self.conn.execute("DELETE FROM bidslake_schema", [])?;
        self.conn.execute(
            "INSERT INTO bidslake_schema (base_schema_version, effective_schema, overlay_digest) \
             VALUES (?, ?, ?)",
            params![base_schema_version, effective_schema, overlay_digest],
        )?;

        // Cleared before the empty check so a run with no overlays drops the rows an earlier
        // run left, rather than leaving them to describe a schema no longer in force — but
        // only if the table is already there. A catalog indexed without overlays must not
        // grow one (`test_overlay::without_overlay_confounds_is_skipped`).
        if self.table_exists("bidslake_overlays")? {
            self.conn.execute("DELETE FROM bidslake_overlays", [])?;
        }
        if overlays.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS bidslake_overlays (\
             idx INTEGER, source TEXT, sha256 TEXT, content JSON)",
            [],
        )?;
        for (i, overlay) in overlays.iter().enumerate() {
            let content = serde_json::to_string(&overlay.content).unwrap_or_default();
            let sha = sha256_hex(content.as_bytes());
            self.conn.execute(
                "INSERT INTO bidslake_overlays (idx, source, sha256, content) \
                 VALUES (?, ?, ?, ?)",
                params![i as i32, &overlay.source, sha, content],
            )?;
        }
        Ok(())
    }

    /// Whether `table` exists in this catalog. Used by the provenance stamps, which must
    /// clear a stale row set without bringing a table into being for a catalog that has none.
    fn table_exists(&self, table: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM duckdb_tables() WHERE table_name = ?",
            params![table],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Record provenance rows (`idx`, `source`, `sha256`, `content`) in a `bidslake_<kind>`
    /// table so the catalog stays self-describing. No-op when empty; a re-index replaces the
    /// previous run's rows rather than keeping them. `table` is a fixed internal name.
    fn stamp_provenance(&self, table: &str, items: &[(String, Value)]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            &format!(
                "CREATE TABLE IF NOT EXISTS {table} \
                 (idx INTEGER, source TEXT, sha256 TEXT, content JSON)"
            ),
            [],
        )?;
        // Replaced rather than inserted-if-the-idx-is-absent, for the reason `stamp_schema`
        // gives: a stamp frozen at the first run describes a catalog that no longer exists.
        // Clearing by table (not by idx) also drops a trailing entry when a later run applies
        // fewer items than an earlier one.
        self.conn.execute(&format!("DELETE FROM {table}"), [])?;
        for (i, (source, content)) in items.iter().enumerate() {
            let content_str = serde_json::to_string(content).unwrap_or_default();
            let sha = sha256_hex(content_str.as_bytes());
            self.conn.execute(
                &format!("INSERT INTO {table} (idx, source, sha256, content) VALUES (?, ?, ?, ?)"),
                params![i as i32, source, sha, content_str],
            )?;
        }
        Ok(())
    }

    /// Record the applied term maps (`bidslake_term_maps`), for a self-describing catalog.
    pub fn stamp_term_maps(&self, term_maps: &[(String, Value)]) -> Result<()> {
        self.stamp_provenance("bidslake_term_maps", term_maps)
    }

    /// Record the applied ingestion fragments (`bidslake_ingestion`).
    pub fn stamp_ingestion(&self, ingestion: &[(String, Value)]) -> Result<()> {
        self.stamp_provenance("bidslake_ingestion", ingestion)
    }

    /// Record what bidslake did with a file, on its registry row. Backs the tabular-data
    /// invariant, which is now a question the manifest answers on its own.
    ///
    /// An `UPDATE` rather than an upsert: the row already exists — the walk registered every
    /// file it saw before any of this ran — so an insert here could only create a duplicate
    /// under a second id or mask a file the walk never registered. A status for an unknown
    /// file updates nothing, which is the honest outcome.
    ///
    /// Replaces the former `tabular_files` table, whose `table_name`/`n_rows` are dropped:
    /// which table a file's rows landed in is recoverable by joining that table on `file_id`.
    pub fn record_file_status(&self, file_id: Uuid, status: TabularStatus) -> Result<()> {
        self.record_file_statuses(std::slice::from_ref(&file_id), status)
    }

    /// [`Self::record_file_status`] for a whole batch, in one statement.
    ///
    /// One statement per *file* is what this replaces, and it was the single largest cost in
    /// a raw-BIDS ingest — 64% of a 64k-file run, measured. Two things stack up per
    /// execution, and batching removes both at once:
    ///
    /// * `prepare_cached` does not help. The catalog keeps changing inside the ingest's open
    ///   transaction, so DuckDB invalidates the cached plan and `RebindPreparedStatement`
    ///   re-runs the binder and the optimizer on every single execution.
    /// * The predicate does not use the primary key. Profiles show `file_id = ?` resolving to
    ///   a scan that filters 128-bit UUIDs column-segment by column-segment, so the cost of
    ///   one update is proportional to the size of `file_registry` — making the pass
    ///   quadratic in a dataset that is mostly tabular files.
    ///
    /// The scan does not go away here; it is amortized. One statement per batch window turns
    /// `files × registry_rows` into `windows × registry_rows`.
    ///
    /// Literals rather than bound parameters because the list is variadic — the same shape
    /// the batched `DELETE ... WHERE file_id IN (…)` in `flush_tabular` already uses, and
    /// `sql_uuid_lit` renders a `Uuid` whose spelling has nothing for escaping to do.
    pub fn record_file_statuses(&self, file_ids: &[Uuid], status: TabularStatus) -> Result<()> {
        if file_ids.is_empty() {
            return Ok(());
        }
        let id_list = file_ids
            .iter()
            .map(|id| schema::dynamic::sql_uuid_lit(*id))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "UPDATE file_registry SET status = {} WHERE file_id IN ({id_list})",
            schema::dynamic::sql_lit(status.as_str())
        );
        self.conn.execute(&sql, []).with_context(|| {
            format!(
                "recording {} file(s) as {}",
                file_ids.len(),
                status.as_str()
            )
        })?;
        Ok(())
    }

    /// Record the column names a `undeclared: catalog` table saw but does not declare
    /// (`tabular_undeclared_columns`). Idempotent, and deduped by the table's primary
    /// key, so re-indexing and repeated headers both collapse to one row per name.
    pub fn record_undeclared_columns(&self, table: &str, names: &[String]) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        let mut stmt = self.conn.prepare_cached(
            "INSERT OR IGNORE INTO tabular_undeclared_columns (table_name, name) VALUES (?, ?)",
        )?;
        for name in names {
            stmt.execute(params![table, name])?;
        }
        Ok(())
    }

    /// Every ingest root registered for `dataset_id`.
    ///
    /// Empty for a dataset not yet in the catalog, which is how
    /// [`BidsParser::resolve_root`](crate::bids::BidsParser) tells a first ingest from one
    /// adding a root to an existing dataset. Ordered so an error message listing them
    /// reads the same way twice.
    pub fn dataset_roots(&self, dataset_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT root_uri FROM dataset_roots WHERE dataset_id = ? ORDER BY root_uri",
        )?;
        // `duckdb::Result`, not the crate's `Result` (now anyhow's): the iterator yields
        // the driver's error type, and collecting it needs that spelled out.
        let rows = stmt
            .query_map(params![dataset_id], |r| r.get(0))?
            .collect::<duckdb::Result<Vec<_>>>()
            .map_err(duck)
            .with_context(|| format!("reading dataset_roots for {dataset_id}"))?;
        Ok(rows)
    }

    /// Bind one ingest root to a dataset (`dataset_roots`), at the tenure this run asserts.
    ///
    /// Re-indexing the same root is a no-op rather than a primary-key violation, and
    /// deliberately **not** a plain `INSERT OR REPLACE`: `--managed` is an assertion, and its
    /// absence on a later run is not a retraction. So asserting `Managed` upserts, while the
    /// default leaves an existing row's tenure alone — otherwise every routine re-index would
    /// silently demote a managed root and quietly withdraw the authority its rows carry
    /// (docs/adr/0007).
    pub fn register_dataset_root(
        &self,
        dataset_id: &str,
        root_uri: &str,
        tenure: schema::Tenure,
    ) -> Result<()> {
        let sql = match tenure {
            schema::Tenure::Managed => {
                "INSERT INTO dataset_roots (dataset_id, root_uri, tenure) \
                 VALUES (?, ?, 'managed') \
                 ON CONFLICT (dataset_id, root_uri) DO UPDATE SET tenure = 'managed'"
            }
            schema::Tenure::Attached => {
                "INSERT INTO dataset_roots (dataset_id, root_uri, tenure) \
                 VALUES (?, ?, 'attached') \
                 ON CONFLICT (dataset_id, root_uri) DO NOTHING"
            }
        };
        let mut stmt = self.conn.prepare_cached(sql)?;
        stmt.execute(params![dataset_id, root_uri])?;
        Ok(())
    }

    /// Does `table` carry `column` in *this* catalog?
    ///
    /// Catalogs outlive the schema that built them. A column added to an existing table only
    /// reaches an older file through [`Self::create_tables`], which only `index` runs — so a
    /// read command that assumes the current shape fails with a `Binder Error` against a
    /// catalog whose only fault is predating this build. Asking first is what lets those
    /// commands degrade instead.
    pub fn has_column(&self, table: &str, column: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT 1 FROM duckdb_columns() WHERE table_name = ? AND column_name = ? LIMIT 1",
        )?;
        let found = stmt
            .query_map(params![table, column], |_| Ok(()))?
            .next()
            .transpose()
            .map_err(duck)
            .with_context(|| format!("looking for {table}.{column}"))?;
        Ok(found.is_some())
    }

    /// The tenure recorded for one root, or `None` if it is not registered.
    pub fn dataset_root_tenure(
        &self,
        dataset_id: &str,
        root_uri: &str,
    ) -> Result<Option<schema::Tenure>> {
        // Same reason as [`Self::has_column`]'s: an older catalog has no such column, and a
        // root registered before tenure existed promised exactly what `attached` means.
        if !self.has_column("dataset_roots", "tenure")? {
            let known = self.dataset_roots(dataset_id)?;
            return Ok(known
                .iter()
                .any(|uri| uri == root_uri)
                .then_some(schema::Tenure::Attached));
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT tenure FROM dataset_roots WHERE dataset_id = ? AND root_uri = ?",
        )?;
        let mut rows = stmt
            .query_map(params![dataset_id, root_uri], |r| r.get::<_, String>(0))?
            .collect::<duckdb::Result<Vec<_>>>()
            .map_err(duck)
            .with_context(|| format!("reading tenure for {root_uri}"))?;
        Ok(match rows.pop().as_deref() {
            Some("managed") => Some(schema::Tenure::Managed),
            Some(_) => Some(schema::Tenure::Attached),
            None => None,
        })
    }

    /// Record one cross-dataset link declaration (`dataset_links`). `INSERT OR REPLACE`
    /// keeps re-indexing idempotent (see docs/adr/0003).
    pub fn record_dataset_link(
        &self,
        dataset_id: &str,
        link_type: &str,
        link_name: &str,
        declared_ref: &str,
        identity: &Identity,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT OR REPLACE INTO dataset_links \
             (dataset_id, link_type, link_name, declared_ref, identity, identity_kind, identity_base) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?;
        stmt.execute(params![
            dataset_id,
            link_type,
            link_name,
            declared_ref,
            &identity.value,
            identity.kind.as_str(),
            &identity.base
        ])?;
        Ok(())
    }

    /// Record one identity a dataset *is* (`dataset_identity`). `source` is its provenance
    /// (`self`/`DatasetDOI`/`root_uri`). `INSERT OR REPLACE` for idempotent re-indexing.
    pub fn record_dataset_identity(
        &self,
        dataset_id: &str,
        identity: &Identity,
        source: &str,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT OR REPLACE INTO dataset_identity (dataset_id, identity, identity_kind, source) \
             VALUES (?, ?, ?, ?)",
        )?;
        stmt.execute(params![
            dataset_id,
            &identity.value,
            identity.kind.as_str(),
            source
        ])?;
        Ok(())
    }

    /// Drop a dataset's ingest-derived links and all its identities before re-recording
    /// them, so a re-index reflects the current `dataset_description.json`.
    ///
    /// The cut is *where the statement came from*, not what it says: `source` and `named`
    /// are both read out of `dataset_description.json`, so they must track the file, while
    /// `declared` (`--source-dataset`, `bidslake link add`) and `alias`
    /// (`bidslake link alias`) are the user's and are never cleared here. That is one
    /// column of the 2x2 documented on `CREATE_DATASET_LINKS_TABLE`, which is why `alias`
    /// needs no rule of its own — it is simply not in the list.
    pub fn clear_derived_links(&self, dataset_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM dataset_links WHERE dataset_id = ? AND link_type IN ('source', 'named')",
            params![dataset_id],
        )?;
        self.conn.execute(
            "DELETE FROM dataset_identity WHERE dataset_id = ?",
            params![dataset_id],
        )?;
        Ok(())
    }

    /// Insert one row (`data`, a JSON object) into a schema-generated table,
    /// mapping keys to columns via [`Schema::insert`].
    pub fn insert(&self, schema: &Schema, table_name: &str, data: &Value) -> Result<()> {
        schema.insert(&self.conn, table_name, data)?;
        Ok(())
    }

    /// Insert one row, replacing any already under its primary key
    /// ([`Schema::insert_or_replace`]).
    pub fn insert_or_replace(&self, schema: &Schema, table_name: &str, data: &Value) -> Result<()> {
        schema.insert_or_replace(&self.conn, table_name, data)?;
        Ok(())
    }

    /// Drop the rows one file contributed to `table`, so re-reading that file replaces them.
    ///
    /// For the per-row tables a content reader fills during the walk. These carry **no primary
    /// key** — that is what a per-row table is — so there is nothing for an upsert to conflict
    /// on, and without this a re-index does not fail: it doubles the table, and doubles it again
    /// next run. Scoped per file because the reader is invoked once per file, and because that
    /// is how the batched tabular path already clears the same class of table (see
    /// `bids::BidsParser::flush_tabular`'s pre-`DELETE`), so all per-row tables behave alike.
    ///
    /// Note the scope: rows belonging to a file that has since been *deleted* are not reached,
    /// because this only runs for files the walk still finds. Pruning those is an integrity
    /// question, deliberately not one the write path answers.
    pub fn clear_file_rows(&self, table: &str, file_id: Uuid) -> Result<()> {
        self.clear_file_rows_many(table, std::slice::from_ref(&file_id))
    }

    /// [`Self::clear_file_rows`] for many files, in one statement.
    ///
    /// These tables carry no index on `file_id`, so each `DELETE` scans; issuing one per file
    /// therefore costs `files × rows_written_so_far`, against a table the same loop is
    /// growing. Batching turns that into one scan per window. Literals rather than bound
    /// parameters because the list is variadic, matching `flush_tabular`'s own batched
    /// `DELETE`.
    pub fn clear_file_rows_many(&self, table: &str, file_ids: &[Uuid]) -> Result<()> {
        if file_ids.is_empty() {
            return Ok(());
        }
        // `table` is always an internal literal, never user input.
        let id_list = file_ids
            .iter()
            .map(|id| schema::dynamic::sql_uuid_lit(*id))
            .collect::<Vec<_>>()
            .join(", ");
        self.conn.execute(
            &format!("DELETE FROM {table} WHERE file_id IN ({id_list})"),
            [],
        )?;
        Ok(())
    }

    /// Bulk-insert many rows into a schema-generated table via the DuckDB **Appender**,
    /// used for the two widest tables (`scans`, `sidecars`).
    ///
    /// The Appender writes physical columns directly, skipping both SQL planning and — the
    /// part that actually scales with the table — the `WHERE NOT EXISTS` primary-key probe
    /// that [`Schema::insert`] wraps every row in. That probe costs more the more rows the
    /// table already holds, so it is the row-at-a-time path's real per-row cost, and the
    /// price of dropping it is that **the caller must dedup**: the Appender enforces no
    /// insert-if-not-exists guard (see the `seen` set in `bids::BidsParser`).
    ///
    /// Each row's values are shaped exactly like [`Schema::insert`] via
    /// [`Schema::row_values`], so the result is identical to inserting them one at a time.
    pub fn append_rows(&self, schema: &Schema, table_name: &str, rows: &[Value]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut appender = self
            .conn
            .appender(table_name)
            .map_err(duck)
            .with_context(|| format!("opening an appender on {table_name}"))?;
        for (i, row) in rows.iter().enumerate() {
            // The row index, because a bulk append fails on *one* row out of thousands and
            // the caller can only say how many it handed over. Without this the message is
            // a bare "Conversion Error" for a batch, with nothing to look at.
            let values = schema.row_values(table_name, row)?;
            let refs: Vec<&dyn duckdb::ToSql> =
                values.iter().map(|v| v as &dyn duckdb::ToSql).collect();
            appender
                .append_row(refs.as_slice())
                .map_err(duck)
                .with_context(|| format!("appending row {i} of {} to {table_name}", rows.len()))?;
        }
        appender
            .flush()
            .map_err(duck)
            .with_context(|| format!("flushing {} appended rows to {table_name}", rows.len()))?;
        Ok(())
    }

    /// Bulk-**upsert** many rows into a schema-generated table: the [`Self::append_rows`] path
    /// for a table a re-index rewrites, where a row already present must be replaced rather
    /// than collided with.
    ///
    /// Staged through a temporary table (see `upsert_staged`), because the two halves of that
    /// cannot be had directly. The Appender has no conflict handling at all, so it cannot
    /// upsert; and a row-at-a-time `INSERT OR REPLACE` is far more expensive than the bulk
    /// path — measured on `sidecars` over ds000117 (1,492 imaging files, 2026-08), the
    /// append went 21 ms → 759 ms and the whole run 1.8 s → 2.6 s.
    ///
    /// Upsert rather than clear-then-append so the write is self-contained: nothing has to keep
    /// a deletion's scope in step with what the run produces, and re-indexing one dataset can
    /// never reach another's rows. It does leave behind rows whose source file has since been
    /// deleted — as `scans` already does — which is a question for an integrity pass, not for
    /// the write path.
    ///
    /// `sidecars` and `file_registry` qualify for the staging requirements in
    /// `upsert_staged`; `scans` has generated columns and uses [`Self::append_rows`] behind
    /// its own read-back guard.
    pub fn upsert_rows(&self, schema: &Schema, table_name: &str, rows: &[Value]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.upsert_staged(table_name, |appender| {
            for (i, row) in rows.iter().enumerate() {
                let values = schema.row_values(table_name, row)?;
                let refs: Vec<&dyn duckdb::ToSql> =
                    values.iter().map(|v| v as &dyn duckdb::ToSql).collect();
                appender
                    .append_row(refs.as_slice())
                    .map_err(duck)
                    .with_context(|| {
                        format!("staging row {i} of {} for {table_name}", rows.len())
                    })?;
            }
            Ok(())
        })
        .with_context(|| format!("upserting {} rows into {table_name}", rows.len()))
    }

    /// The staging machinery of [`Self::upsert_rows`], with the caller filling the stage.
    ///
    /// `fill` appends into a temporary table shaped exactly like `table_name`, and one
    /// `INSERT OR REPLACE … SELECT` then moves those rows across and resolves conflicts there.
    /// Splitting it out is what lets the **static** tables share the pattern — the ones whose
    /// DDL is hand-written in [`crate::schema`] rather than generated, so they are absent from
    /// `Schema::table_columns` and [`Schema::row_values`] cannot shape them. They append their
    /// own rows instead; everything around that is identical.
    ///
    /// Two properties every caller inherits:
    ///
    /// - **The caller must ensure the staged rows are free of duplicate primary keys, because
    ///   nothing here will.** `CREATE TABLE … AS SELECT` does not copy constraints, so the
    ///   stage has no primary key and the Appender has nothing to violate; and the
    ///   `INSERT OR REPLACE … SELECT` does not raise on a duplicate *within* its source either.
    ///   Measured on the pinned engine (DuckDB 1.5.5): two staged rows sharing a key insert as
    ///   one, keeping the **first** and dropping the rest, silently. That inverts the
    ///   row-at-a-time `INSERT OR REPLACE` this replaced, where the **last** write won. So a
    ///   caller that stops deduping gets quiet data loss rather than a failed run — the one
    ///   way this path is not behaviour-preserving, and why each caller says where its
    ///   uniqueness comes from.
    ///
    ///   Conflicts with rows *already in the table* are a different matter: those are the
    ///   point, and `OR REPLACE` resolves them as intended.
    /// - **The Appender writes physical columns**, bypassing the planner, so none of the
    ///   implicit casting an `INSERT` performs happens — which is why the gradient and
    ///   association writers name the `duckdb::types::Value` themselves rather than letting a
    ///   JSON value find its own way to the column. For `file_id` that is `Value::Text`
    ///   holding the canonical UUID spelling: duckdb-rs has no UUID variant, and DuckDB's
    ///   appender casts a mismatched value with `DefaultTryCastAs` and *throws* when it
    ///   cannot — so a malformed id is a failed run, not a corrupted key.
    ///
    /// Requires `table_name` to have a primary key (nothing to upsert on otherwise) and no
    /// generated columns (`SELECT *` would materialize them, and inserting into a generated
    /// column is an error).
    fn upsert_staged(
        &self,
        table_name: &str,
        fill: impl FnOnce(&mut duckdb::Appender<'_>) -> Result<()>,
    ) -> Result<()> {
        // `table_name` is always an internal literal, never user input.
        let stage = format!("{table_name}_upsert_stage");
        // `OR REPLACE` so a previous run that died mid-flight leaves nothing to trip over.
        self.conn
            .execute_batch(&format!(
                "CREATE OR REPLACE TEMP TABLE {stage} AS SELECT * FROM {table_name} LIMIT 0"
            ))
            .map_err(duck)
            .with_context(|| format!("creating the upsert stage for {table_name}"))?;
        {
            let mut appender = self
                .conn
                .appender(&stage)
                .map_err(duck)
                .with_context(|| format!("opening an appender on {stage}"))?;
            fill(&mut appender)?;
            appender
                .flush()
                .map_err(duck)
                .with_context(|| format!("flushing the upsert stage for {table_name}"))?;
        }
        // The one that names a real constraint: this is where a duplicate primary key or a
        // foreign key into `file_registry` is rejected, and the message otherwise says only
        // which constraint, never which table it was moving rows into.
        self.conn
            .execute_batch(&format!(
                "INSERT OR REPLACE INTO {table_name} SELECT * FROM {stage}; DROP TABLE {stage};"
            ))
            .map_err(duck)
            .with_context(|| format!("moving staged rows into {table_name}"))?;
        Ok(())
    }

    /// Bulk-upsert derived file associations into `file_associations`.
    ///
    /// `INSERT OR REPLACE`, because a re-index recomputes these rows and an identical row
    /// through a bare Appender would be a primary-key violation — which aborts the ingest
    /// transaction and takes every later write with it.
    ///
    /// `OR REPLACE` rather than `OR IGNORE`, because one column sits *outside* the primary
    /// key and is exactly the one that changes between runs: `target_file_id` is the
    /// resolution of `target_file_path` against the files the catalog holds, and a target
    /// absent when this row was first written resolves once its dataset is indexed. Under
    /// `OR IGNORE` the recomputed row loses the conflict and the NULL is frozen for good --
    /// which is the normal case for a dataset spanning several roots (docs/adr/0005), where
    /// an `IntendedFor` routinely names a file another root supplies.
    ///
    /// Both halves are had at once through `upsert_staged`. The row-at-a-time
    /// `INSERT OR REPLACE` this replaced was the dominant cost of the whole run: the schema
    /// resolves roughly two associations per data file, and at ~0.5 ms of planning per
    /// statement that was most of the ingest. Measured over ds000117 (2,209 files, 909
    /// associations and 715 rows in each of `bvals`/`bvecs`, 2026-08), the three writes
    /// together went 1,183 ms → 5 ms and the whole run 1.50 s → 0.30 s; over an existing
    /// catalog, where every row conflicts, 1,744 ms → 4 ms.
    ///
    /// The caller owns primary-key dedup, and owns it alone: staging cannot check it, and a
    /// duplicate within one batch is dropped rather than refused (see `upsert_staged`, and
    /// `tests/test_staged_upsert_duplicates.rs` for the pinned behaviour).
    pub fn upsert_file_associations(&self, assocs: &[FileAssociation]) -> Result<()> {
        if assocs.is_empty() {
            return Ok(());
        }
        self.upsert_staged("file_associations", |appender| {
            for assoc in assocs {
                // The canonical spelling, which the Appender casts to `UUID` on the way
                // in — duckdb-rs has no UUID `Value` variant, so `Text` is the only way to
                // hand one over. A value that is not a UUID fails the cast loudly rather
                // than landing under a corrupted id.
                let source = duckdb::types::Value::Text(assoc.source_file_id.to_string());
                let target = match assoc.target_file_id {
                    Some(id) => duckdb::types::Value::Text(id.to_string()),
                    None => duckdb::types::Value::Null,
                };
                appender.append_row(params![
                    source,
                    target,
                    &assoc.target_file_path,
                    &assoc.assoc_type
                ])?;
            }
            Ok(())
        })
    }

    /// Bulk-upsert every `.bval` file's values, one row per volume, keyed by **that file's** id.
    ///
    /// One staged upsert for the whole dataset rather than one per gradient file: a temp table
    /// per file would cost more than the row-at-a-time path it replaces, and the rows across
    /// files are independent, so there is nothing to keep them apart.
    ///
    /// `INSERT OR REPLACE` for the same reason as the associations above — a re-index
    /// recomputes these rows — and upserting keeps the write self-contained, with no clearing
    /// step whose scope has to be kept in step with what the run produces.
    pub fn upsert_bvals(&self, files: &[BvalFile<'_>]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        self.upsert_staged("bvals", |appender| {
            for (file_id, bvals) in files {
                let id = duckdb::types::Value::Text(file_id.to_string());
                for (i, &b) in bvals.iter().enumerate() {
                    appender.append_row(params![id, i as i64, b])?;
                }
            }
            Ok(())
        })
    }

    /// Bulk-upsert every `.bvec` file's directions, one row per volume, keyed by **that
    /// file's** id.
    ///
    /// Each file's three arrays are its three rows and are the same length (`parse_bvec`
    /// rejects a ragged one), so unlike the pre-ADR-0007 writer there is no cross-file
    /// alignment to guess at here: pairing a `.bvec` with a `.bval` is the `diffusion` view's
    /// job, and it does it through the image both are associated with.
    pub fn upsert_bvecs(&self, files: &[BvecFile<'_>]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        self.upsert_staged("bvecs", |appender| {
            for (file_id, x, y, z) in files {
                let id = duckdb::types::Value::Text(file_id.to_string());
                for i in 0..x.len() {
                    appender.append_row(params![id, i as i64, x[i], y[i], z[i]])?;
                }
            }
            Ok(())
        })
    }
}

/// One `.bval` file's row of b-values, alongside the id of the gradient file it came from.
/// Borrowed from the parse, so a batch costs no copy of the values.
pub type BvalFile<'a> = (Uuid, &'a [f64]);

/// One `.bvec` file's three direction rows — `x`, `y`, `z` — alongside the id of the gradient
/// file they came from. Borrowed from the parse, so a batch costs no copy of the values.
pub type BvecFile<'a> = (Uuid, &'a [f64], &'a [f64], &'a [f64]);
