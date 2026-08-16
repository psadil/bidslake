//! Generating the DuckDB schema from the BIDS schema.
//!
//! bidslake does not hardcode its tables — it derives them from the vendored
//! BIDS `schema.json` (embedded via `SCHEMA_JSON`). That file is an *instance*
//! of the BIDS **metaschema**, which defines the structure of concepts like
//! entities, datatypes, suffixes, and modalities. Reading those `objects.*` and
//! `rules.*` sections is how [`Schema`] stays faithful to whatever BIDS version
//! is vendored.
//!
//! ## The column model
//!
//! For each generated table, [`Schema`] stores three things:
//! - `table_definitions`: the `CREATE TABLE` text.
//! - `table_columns`: a `Vec<(col_name, col_type, json_key)>` — the ordered
//!   columns that ingestion writes to. `json_key` is the key looked up in the
//!   input JSON row for that column (see [`Schema::insert`]).
//! - `primary_keys`: the PK columns, used to build the idempotency guard.
//!
//! ### Insert-by-`json_key`, with `other_data` overflow
//!
//! [`Schema::insert`] takes a `serde_json` object (one row) and, for each column,
//! pulls the value at `json_key` (falling back to `col_name`, then to a
//! case-insensitive match — see `table_keys_lower`). Any input key that is *not*
//! claimed by a column lands in the table's `other_data JSON` column, so nothing is
//! dropped — unless the ingestion policy declares the table's undeclared columns
//! `catalog`, in which case there is no `other_data` column and the file on disk is
//! the record of them. The INSERT SQL is built once per table
//! (`build_insert_statements`) and executed via `prepare_cached`, so
//! DuckDB reuses one compiled plan across every row of that table.
//!
//! ## Generated (virtual) BIDS-concept columns
//!
//! On top of the written columns, the `scans` table carries generated columns —
//! `task`, `sub`, `ses`, `run`, …, plus `datatype`, `suffix`, `extension`, and
//! `modality` — produced by `generated_bids_columns`. These are
//! `GENERATED ALWAYS AS (…) VIRTUAL`, computed on read, so they let callers query
//! by BIDS concept (`WHERE task = 'rest'`) instead of by path regex, and are *not*
//! part of `table_columns` (the write path never touches them). They too are
//! generated from `objects.entities` / `objects.datatypes` / `rules.modalities`.
//!
//! They read from `file_path` — except that when a term map is configured, the
//! concepts *it* can project instead read `COALESCE(<the projection>, <the regex>)`
//! over a physical `projected JSON` column. A term-mapped path
//! (`sub-01/mri/wmparc.mgz`) carries almost none of its concepts in its name, so
//! without the fallback the projection would be computed and discarded. See ADR
//! 0002 §7; with no term map the wrapping is absent entirely and the DDL is
//! unchanged.
//!
//! The static tables `bvals`/`bvecs` and `file_associations` live in the parent
//! [`crate::schema`] module, not here.

use super::ingestion::{Ingestion, Undeclared};
use super::recording::Recordings;
use super::tabular::{RowIdentity, TableSpec, Tabular};
use crate::db::duck;
use anyhow::{Context as _, Result, anyhow};
use duckdb::Connection;
use serde_json::Value;
use std::collections::HashMap;

/// The vendored BIDS schema, embedded at compile time. An instance of the BIDS
/// metaschema; its `objects.*` and `rules.*` sections drive table generation.
const SCHEMA_JSON: &str = bids_schema::SCHEMA_JSON;

/// One overlay applied to the base schema. Its provenance is stamped into
/// `bidslake_overlays` (see [`crate::db::BidsDb`]).
#[derive(Clone, Debug)]
pub struct AppliedOverlay {
    /// How the overlay was named: a bundled pipeline name (`fmriprep`) or a file path.
    pub source: String,
    /// The overlay's parsed JSON content, kept verbatim for reproducibility.
    pub content: Value,
}

#[derive(Clone, Debug)]
pub struct Schema {
    schema: Value,
    /// The schema-driven tabular model (which tables exist, their columns). Drives
    /// generation of every table that comes from `rules.tabular_data`.
    tabular: Tabular,
    /// The headerless continuous recordings, derived from `rules.sidecars` and
    /// `meta.associations` (see [`super::recording`]).
    recordings: Recordings,
    table_definitions: HashMap<String, String>,
    /// Views over the generated tables, emitted after every table so their bases exist.
    /// Keyed by view name; `CREATE OR REPLACE`, so a later, wider run redefines rather than
    /// being refused (docs/adr/0006).
    view_definitions: HashMap<String, String>,
    table_columns: HashMap<String, Vec<(String, String, String)>>, // table -> [(col_name, col_type, json_key)]
    /// Lowercased `json_key`s of [`Self::table_columns`]: which source fields a table
    /// has a dedicated column for.
    ///
    /// Lowercased because DuckDB folds identifier case, so a table cannot hold two
    /// columns whose names differ only in case. Where BIDS declares such a pair the
    /// DDL keeps one and drops the other (`MISCChannelCount` beats
    /// `MiscChannelCount`), and matching exactly would strand the dropped spelling —
    /// missing from its own column *and* counted as undeclared. The surviving column
    /// absorbs both, as DuckDB does with the identifier itself.
    ///
    /// Derived, never set directly — see [`Schema::set_table_columns`]. Precomputed because
    /// the alternative is rebuilding the whole set — one entry per column, and `sidecars` has
    /// a column per BIDS metadata field — for every row on the bulk-insert path.
    table_keys_lower: HashMap<String, std::collections::HashSet<String>>,
    /// The same lowercased `json_key`s, kept in column order rather than as a set, so
    /// the case-insensitive column lookup in [`Schema::row_values`] can fold the
    /// table's side once at load instead of once per row.
    ///
    /// Derived alongside [`Self::table_keys_lower`], from the same single writer.
    table_json_keys_lower: HashMap<String, Vec<String>>,
    primary_keys: HashMap<String, Vec<String>>, // table -> [pk_col_names]
    insert_sql: HashMap<String, String>,        // table -> prebuilt INSERT statement
    // table -> the same statement as `INSERT OR REPLACE`, for the writers that must
    // refresh a row rather than defer to whatever wrote it first.
    replace_sql: HashMap<String, String>,
    /// The overlays merged into this schema, in application order (empty if none).
    overlays: Vec<AppliedOverlay>,
    /// Ingestion policy (read/catalog/ignore + per-table materialized concepts). Empty for a
    /// plain BIDS ingest, in which case every table uses the virtual regex concept columns.
    ingestion: Ingestion,
    /// BIDS concepts some configured term map is capable of projecting, unioned across
    /// them (see [`bids_schema::term_map::TermMap::projectable_concepts`]). Empty for a
    /// plain BIDS ingest.
    ///
    /// Derived from the term maps rather than declared: a term map already states what
    /// it projects, so a second declaration would be a second source of truth. It is
    /// needed at *generation* time — it decides which concept columns consult the
    /// stored projection — which is why it is threaded in alongside the ingestion
    /// policy rather than set later.
    projected_concepts: std::collections::BTreeSet<String>,
}

impl Schema {
    /// Load the schema and generate the table model. `Some(path)` reads a
    /// user-supplied schema.json — fallible, so a missing or malformed file is an
    /// `Err` rather than a panic. `None` uses the embedded schema, a build
    /// invariant that is `.expect()`ed.
    pub fn load(schema_path: Option<&str>) -> anyhow::Result<Self> {
        Self::load_with_overlays(schema_path, &[])
    }

    /// Load the schema, deep-merging any additive `overlays` (see
    /// [`bids_schema::overlay`]) onto the base before generating the table model, then
    /// validating the merged schema against the BIDS metaschema. A conflicting or
    /// non-conformant overlay is an `Err`. Merged fragments need no further handling
    /// here: the generators below all read the schema `Value`.
    pub fn load_with_overlays(
        schema_path: Option<&str>,
        overlays: &[AppliedOverlay],
    ) -> anyhow::Result<Self> {
        Self::load_full(schema_path, overlays, Ingestion::base(), &[])
    }

    /// Like [`Self::load_with_overlays`], but also attaches an [`Ingestion`] policy and the
    /// `term_maps` that will be used for the ingest. Tables whose ingestion policy declares
    /// materialized `concepts` get physical concept columns (adapter data tables); all other
    /// tables keep the virtual regex columns. Both must be set before table generation, so
    /// they are threaded in here.
    ///
    /// `term_maps` are consulted only for
    /// [`projectable_concepts`](bids_schema::term_map::TermMap::projectable_concepts) — which
    /// generated columns on the file registry must consult a stored projection. Pass `&[]`
    /// for a plain BIDS ingest, which then produces byte-identical DDL to before this
    /// existed.
    pub fn load_full(
        schema_path: Option<&str>,
        overlays: &[AppliedOverlay],
        ingestion: Ingestion,
        term_maps: &[bids_schema::term_map::TermMap],
    ) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        let mut schema: Value = match schema_path {
            Some(path) => {
                let content = std::fs::read_to_string(path)
                    .with_context(|| format!("reading schema file {path}"))?;
                serde_json::from_str(&content)
                    .with_context(|| format!("parsing schema file {path}"))?
            }
            None => serde_json::from_str(SCHEMA_JSON).expect("embedded schema.json must parse"),
        };

        if !overlays.is_empty() {
            // Keep the pre-overlay schema to validate only what the overlays *add*
            // (the base schema itself has known, tolerated metaschema deviations).
            let base = schema.clone();
            for overlay in overlays {
                bids_schema::overlay::merge_into(&mut schema, &overlay.content)
                    .with_context(|| format!("merging schema overlay {:?}", overlay.source))?;
            }
            bids_schema::overlay::validate_effective(&base, &schema)
                .context("validating augmented schema against the BIDS metaschema")?;
        }

        let tabular = Tabular::load(&schema);
        let recordings = Recordings::load(&schema, &tabular);
        let mut instance = Schema {
            schema,
            tabular,
            recordings,
            table_definitions: HashMap::new(),
            table_columns: HashMap::new(),
            table_keys_lower: HashMap::new(),
            table_json_keys_lower: HashMap::new(),
            primary_keys: HashMap::new(),
            view_definitions: HashMap::new(),
            insert_sql: HashMap::new(),
            replace_sql: HashMap::new(),
            overlays: overlays.to_vec(),
            ingestion,
            projected_concepts: term_maps
                .iter()
                .flat_map(|tm| tm.projectable_concepts())
                .collect(),
        };
        instance.generate_definitions();
        instance.build_insert_statements();
        Ok(instance)
    }

    /// The overlays merged into this schema, in application order (empty if none).
    pub fn overlays(&self) -> &[AppliedOverlay] {
        &self.overlays
    }

    /// The ingestion policy attached to this schema (empty for a plain BIDS ingest).
    pub fn ingestion(&self) -> &Ingestion {
        &self.ingestion
    }

    /// The BIDS concepts this schema's term maps can project, sorted. Empty for a
    /// plain BIDS ingest, in which case the registry stores no projection at all.
    pub fn projected_concepts(&self) -> Vec<String> {
        self.projected_concepts.iter().cloned().collect()
    }

    /// Every column this schema would put on the file registry: the generated
    /// concept columns (entity names, `datatype`, `suffix`, `extension`, `modality`,
    /// `pseudofile`) plus `projected` when a term map supplies one.
    ///
    /// Exists so an *existing* catalog can be checked against what this run expects.
    /// Tables are created `IF NOT EXISTS`, so a catalog's registry keeps the shape
    /// the first run gave it while datasets accumulate across runs (ADR 0002 §3) —
    /// and a later run whose overlays or term maps are wider has nowhere to put the
    /// difference. Derived from the same call that emits the DDL, so the two cannot
    /// drift.
    pub fn registry_concept_columns(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .generated_bids_columns(true)
            .into_iter()
            .map(|c| c.name)
            .collect();
        if !self.projected_concepts.is_empty() {
            names.push("projected".to_string());
        }
        names
    }

    /// The schema-driven tabular model — which tables exist and their columns,
    /// plus selector-based routing. Used by the ingest pipeline.
    pub fn tabular(&self) -> &Tabular {
        &self.tabular
    }

    /// The headerless continuous recordings this schema declares, derived from
    /// `rules.sidecars` and `meta.associations` (see [`super::recording`]). The
    /// ingest consults this to defer a recording to the recordings flush, where its
    /// column names are resolved from the sidecar or its channels file.
    pub fn recordings(&self) -> &Recordings {
        &self.recordings
    }

    /// The BIDS datatype directory names (`func`, `anat`, `eeg`, `phenotype`, …)
    /// from `objects.datatypes`. Ingestion uses these to derive a file's datatype
    /// from its path for selector evaluation. Delegates to the shared owner in
    /// `bids-schema` (single source of truth).
    pub fn datatypes(&self) -> Vec<String> {
        bids_schema::datatypes::datatypes(&self.schema)
    }

    /// The raw parsed BIDS schema JSON — for the shared `bids_schema` helpers
    /// (`SchemaIndex`, `FileContext`, `find_datatype`, association resolution).
    pub fn raw(&self) -> &Value {
        &self.schema
    }

    /// The schema's `meta.associations` object (the schema-driven association definitions).
    pub fn associations(&self) -> &Value {
        &self.schema["meta"]["associations"]
    }

    /// Precompute the INSERT statement for every table once, so ingestion never
    /// rebuilds the (identical, per-table) SQL string per row. Combined with
    /// `prepare_cached` at execution time, this reuses the compiled plan across
    /// all rows of a table.
    fn build_insert_statements(&mut self) {
        let tables: Vec<String> = self.table_columns.keys().cloned().collect();
        for table in tables {
            if let Some(sql) = self.build_insert_sql(&table, true) {
                self.insert_sql.insert(table.clone(), sql);
            }
            if let Some(sql) = self.build_insert_sql(&table, false) {
                self.replace_sql.insert(table, sql);
            }
        }
    }

    /// Record a table's write columns, keeping the derived lowercase key index in
    /// step. The only way `table_columns` is populated, so the two cannot drift.
    fn set_table_columns(&mut self, table: &str, fields: Vec<(String, String, String)>) {
        let lower: Vec<String> = fields
            .iter()
            .map(|(_, _, json_key)| json_key.to_lowercase())
            .collect();
        self.table_keys_lower
            .insert(table.to_string(), lower.iter().cloned().collect());
        self.table_json_keys_lower.insert(table.to_string(), lower);
        self.table_columns.insert(table.to_string(), fields);
    }

    /// Whether `table` has a dedicated column for the source field `key`, matched
    /// case-insensitively (see `table_keys_lower`).
    /// The write-path column names of `table`, in the order [`Self::row_values`] returns
    /// them. Positional, so a caller can line a shaped row up with the columns it fills.
    pub fn write_columns(&self, table: &str) -> Option<Vec<String>> {
        self.table_columns
            .get(table)
            .map(|fields| fields.iter().map(|(name, _, _)| name.clone()).collect())
    }

    pub fn declares(&self, table: &str, key: &str) -> bool {
        self.table_keys_lower
            .get(table)
            .is_some_and(|keys| keys.contains(&key.to_lowercase()))
    }

    /// Build the row-at-a-time write statement for a table from its column and
    /// primary-key metadata.
    ///
    /// `guard` picks which of the two a table needs. With it, `INSERT … WHERE NOT EXISTS
    /// (<pk>)` — first writer wins, which is what makes a walk's stub rows safe to issue
    /// repeatedly. Without it, `INSERT OR REPLACE` — last writer wins, for a row a
    /// re-index is meant to *refresh* from its source file rather than leave stale.
    fn build_insert_sql(&self, table_name: &str, guard: bool) -> Option<String> {
        let fields = self.table_columns.get(table_name)?;
        // Quote every column identifier: tabular headers include DuckDB reserved
        // words (`type`, `index`, `group`, `time`) and case-sensitive names (`HED`).
        let col_names: Vec<String> = fields.iter().map(|(n, _, _)| quote_ident(n)).collect();
        // Temporal columns (`acq_time` and any datetime sidecar field) receive raw
        // TSV strings, which are cast on insert. BIDS uses `n/a` for missing, which
        // is not a valid timestamp — `TRY_CAST` turns it (and any unparseable value)
        // into NULL rather than aborting the whole row/ingest. Numeric coercion is
        // done in Rust in `insert`; here we only guard the temporal cast.
        let selects: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, (_, col_type, _))| {
                let ph = format!("${}", i + 1);
                if is_temporal_type(col_type) {
                    format!("TRY_CAST({} AS {})", ph, col_type)
                } else {
                    ph
                }
            })
            .collect();

        let verb = if guard { "INSERT" } else { "INSERT OR REPLACE" };
        let mut sql = format!(
            "{verb} INTO {} ({}) SELECT {}",
            table_name,
            col_names.join(", "),
            selects.join(", ")
        );

        // Idempotency guard: skip the row if a matching primary key already
        // exists (safe re-indexing). Kept for correctness; made cheap by caching.
        if guard
            && let Some(pks) = self.primary_keys.get(table_name)
            && !pks.is_empty()
        {
            let where_clauses: Vec<String> = pks
                .iter()
                .map(|pk| {
                    let idx = fields
                        .iter()
                        .position(|(name, _, _)| name == pk)
                        .expect("Primary key column not found in fields");
                    format!("{} = ${}", quote_ident(pk), idx + 1)
                })
                .collect();

            sql.push_str(&format!(
                " WHERE NOT EXISTS (SELECT 1 FROM {} WHERE {})",
                table_name,
                where_clauses.join(" AND ")
            ));
        }

        Some(sql)
    }

    fn generate_definitions(&mut self) {
        // Every table that comes from `rules.tabular_data` is generated by one
        // generic path from the schema-driven [`Tabular`] model. This is what makes
        // `participants`, `sessions`, `scans`, `events`, and the per-modality tables
        // correct: their columns (and the true TSV header for each, e.g. `acq_time`
        // not the schema key `acq_time__scans`) are resolved from the schema rather
        // than hand-listed.
        let specs: Vec<TableSpec> = self.tabular.tables().to_vec();
        for spec in &specs {
            self.generate_tabular_table(spec);
        }

        // A headerless continuous recording the schema gives no `tabular_data` column
        // rule (`motion`, `stim`) has data-defined columns — from the sidecar `Columns`
        // array, or the associated `_channels.tsv` — so it becomes a bare row table:
        // everything lands in `other_data`, untyped. (The concept columns are not here;
        // they moved to the `all_files` view, docs/adr/0006.)
        //
        // Which recordings those are is *derived*, never listed. A recording whose
        // columns the schema does declare (`physio`, `physio_events`) was generated by
        // the loop above, and generating it again here would silently replace that
        // definition with a bare one — `table_definitions` is keyed by table name and
        // the last writer wins. That is reachable: an overlay may declare columns for a
        // recording, and BIDS may grow a `motionColumns` rule beside `motionChannels`.
        let recordings: Vec<_> = self.recordings.iter().cloned().collect();
        for rec in &recordings {
            if specs.iter().any(|s| s.table == rec.table) {
                continue;
            }
            self.generate_tabular_table(&TableSpec {
                table: rec.table.clone(),
                columns: Vec::new(),
                identity: RowIdentity::PerRow,
                file_based: true,
                rule_ids: Vec::new(),
            });
        }

        // `sidecars` is not a tabular file — it is the merged JSON-sidecar
        // metadata, generated from `objects.metadata`.
        self.generate_sidecars_table();

        // Dataset Description (JSON, not tabular).
        self.generate_dataset_description_def();

        // The file registry and its data-file view (docs/adr/0006). Generated rather than
        // static DDL because both depend on the schema: the view carries the BIDS-concept
        // columns, and the `projected` column exists only when a term map is configured.
        self.generate_file_registry_def();

        // Views that re-key a table's rows onto the data files they describe (docs/adr/0007).
        self.generate_described_views();
    }

    /// Emit one view per table whose ingestion policy declares [`describes`] with a `view`.
    ///
    /// The view answers "these rows, but keyed by the file they are *about*": a confounds
    /// table's rows keyed by the BOLD image, a gradient file's rows keyed by the diffusion
    /// image. The many-to-many lives in `file_associations` and is resolved here at query
    /// time, rather than being flattened at ingest by storing the rows once per described
    /// file — one inherited `dwi.bval` covers 20 images in `ds114`, and duplicating is both
    /// wasteful and a second copy to keep true.
    ///
    /// One template covers every case, because a `describes` block names exactly one
    /// association: the join is one-to-one, so there is nothing to aggregate. `t.* EXCLUDE`
    /// carries the payload columns through without this generator needing to know them,
    /// which is what lets it serve static tables (`bvals`) and schema-generated ones
    /// (`fmriprep_confounds`) with the same code.
    ///
    /// [`describes`]: super::ingestion::Describes
    fn generate_described_views(&mut self) {
        let declarations: Vec<(String, String, Option<String>, String)> = self
            .ingestion
            .described_views()
            .map(|(table, d)| {
                (
                    table.to_string(),
                    d.association.clone(),
                    d.axis.clone(),
                    d.view.clone().expect("described_views filters to Some"),
                )
            })
            .collect();

        for (table, association, axis, view) in declarations {
            // An adapter can declare a policy for a table its overlay does not create — a
            // fragment applied without its overlay, or a rule the schema no longer carries.
            // Emitting a view over a missing table would fail `create_tables` for everyone.
            // Static tables are absent from `table_definitions` but always exist, hence the
            // second arm.
            if !self.table_definitions.contains_key(&table)
                && !super::STATIC_TABLES.contains(&table.as_str())
            {
                continue;
            }
            // `row_idx` is absent on an unordered table; `validate_describes` already refuses
            // an `axis` there, so this is the ordinal-less form rather than an error.
            let ordinal = axis
                .as_deref()
                .map(|a| {
                    format!(
                        "       t.row_idx AS {},\n",
                        quote_ident(&format!("{a}_idx"))
                    )
                })
                .unwrap_or_default();
            let excluded = if axis.is_some() {
                "EXCLUDE (file_id, row_idx)"
            } else {
                "EXCLUDE (file_id)"
            };
            let sql = format!(
                "CREATE OR REPLACE VIEW {view} AS\n\
                 SELECT fa.source_file_id AS file_id,\n\
                 {ordinal}\
                 \x20      t.* {excluded},\n\
                 \x20      t.file_id AS source_file_id\n\
                 FROM {table} t\n\
                 JOIN file_associations fa ON fa.target_file_id = t.file_id\n\
                 WHERE fa.association_type = {association};",
                view = quote_ident(&view),
                table = quote_ident(&table),
                association = sql_lit(&association),
            );
            self.view_definitions.insert(view, sql);
        }
    }

    /// Generate `file_registry` — one row per file the walk saw — and the `all_files` view
    /// over it (docs/adr/0006).
    ///
    /// The registry holds identity and classification only. The BIDS-concept columns live on
    /// the **view**, not here, which is what makes widening the concept set a
    /// `CREATE OR REPLACE VIEW` rather than a table rebuild — and it leaves the base table free
    /// of generated columns, so the bulk staged upsert can write it.
    fn generate_file_registry_def(&mut self) {
        let table = "file_registry";
        // `file_id` is a 128-bit hash of the three identity columns (see `bids::file_id`),
        // so it is stable across runs and machines and needs no second UNIQUE constraint —
        // which DuckDB would anyway refuse to upsert against.
        let mut columns = vec![
            "file_id UBIGINT PRIMARY KEY".to_string(),
            "dataset_id TEXT".to_string(),
            "root_uri TEXT".to_string(),
            "file_path TEXT".to_string(),
            "kind TEXT".to_string(),
            // The fate of a file bidslake tried to *read*, absorbed from the former
            // `tabular_files` table: `ingested` (its rows are in a data table), `on_disk` (a
            // compressed recording deliberately left as a file), `skipped` (a tabular file
            // the schema does not describe) or `failed` (a batch insert errored, so this run
            // stored none of its rows). NULL for a file there was never any reading to
            // report on — `kind` is what says an image is an image.
            "status TEXT".to_string(),
            // What the walk observed about the file itself, as opposed to what its name or
            // contents say. Both NULL when the run was given `--no-stat`, and on a backend
            // that could not stat a particular file — a walk and a stat are two moments,
            // and a dataset can change between them.
            //
            // Deliberately not a checksum: hashing means *reading* every file, which is a
            // different order of cost from stat-ing it. Size and mtime answer "has this
            // changed since I looked", which is what a content-addressed workflow engine
            // and `verify` both actually ask.
            "size_bytes UBIGINT".to_string(),
            // Nanoseconds since the epoch, signed because a pre-1970 mtime is legal.
            "mtime_ns BIGINT".to_string(),
        ];
        let mut fields: Vec<(String, String, String)> = [
            ("file_id", "UBIGINT"),
            ("dataset_id", "TEXT"),
            ("root_uri", "TEXT"),
            ("file_path", "TEXT"),
            ("kind", "TEXT"),
            ("status", "TEXT"),
            ("size_bytes", "UBIGINT"),
            ("mtime_ns", "BIGINT"),
        ]
        .iter()
        .map(|(n, t)| (n.to_string(), t.to_string(), n.to_string()))
        .collect();

        // Same condition as the per-file tables': the stored projection exists only when
        // some term map can supply a concept, so a plain BIDS catalog keeps a registry with
        // no `projected` column at all.
        let project = !self.projected_concepts.is_empty();
        if project {
            columns.push("projected JSON".to_string());
            fields.push((
                "projected".to_string(),
                "JSON".to_string(),
                "projected".to_string(),
            ));
        }

        self.table_definitions.insert(
            table.to_string(),
            format!(
                "CREATE TABLE IF NOT EXISTS {table} (\n    {}\n);",
                columns.join(",\n    ")
            ),
        );
        self.set_table_columns(table, fields);
        self.primary_keys
            .insert(table.to_string(), vec!["file_id".to_string()]);

        // `all_files`: the whole registry, widened with the BIDS-concept columns.
        //
        // Every kind, not just data files, because the concepts are computed from `file_path`
        // and are just as meaningful for a `*_events.tsv` or a sidecar as for the image they
        // sit beside — and because the per-row data tables key on *tabular* files, so this is
        // the relation they join through to answer "which subject/task is this row from?".
        // Filtering to data files here would leave that question unanswerable for them.
        //
        let concepts: Vec<String> = self
            .generated_bids_columns(project)
            .iter()
            .map(ConceptColumn::as_select_item)
            .collect();
        self.view_definitions.insert(
            "all_files".to_string(),
            format!(
                "CREATE OR REPLACE VIEW all_files AS\n  SELECT *,\n         {}\n  FROM {table};",
                concepts.join(",\n         ")
            ),
        );
        // The one view. "Just the data files" is `WHERE kind = 'data'`, which is short
        // enough not to earn a second view that could drift from this one's concept
        // expressions.
    }

    /// Generate one table from a schema-derived [`TableSpec`]: DDL, the write-path
    /// column list, and the primary key. This single path replaces the former
    /// hand-written `participants`/`sessions`/`scans` generators.
    ///
    /// Structural (base) columns depend on the row identity: `PerFile` (scans) and
    /// `PerRow` carry `file_path` and get the generated virtual BIDS-concept
    /// columns; `PerEntity` (participants/sessions) are keyed by their ids. The
    /// rule's own columns follow, each written under its true TSV header, with any
    /// header already represented by a base column skipped.
    fn generate_tabular_table(&mut self, spec: &TableSpec) {
        let mut columns: Vec<String> = Vec::new();
        let mut fields: Vec<(String, String, String)> = Vec::new();
        let mut pk: Vec<String> = Vec::new();
        let mut trailing: Vec<String> = Vec::new();
        // Rule-column headers already represented by a structural base column.
        let mut skip: std::collections::HashSet<&str> = std::collections::HashSet::new();

        let base =
            |columns: &mut Vec<String>, fields: &mut Vec<(String, String, String)>, name: &str| {
                columns.push(format!("{} TEXT", name));
                fields.push((name.to_string(), "TEXT".to_string(), name.to_string()));
            };
        // The surrogate key every file-keyed table points at (docs/adr/0006). `UBIGINT`
        // rather than `TEXT`, matching `file_registry.file_id`.
        let file_key = |columns: &mut Vec<String>, fields: &mut Vec<(String, String, String)>| {
            columns.push("file_id UBIGINT".to_string());
            fields.push((
                "file_id".to_string(),
                "UBIGINT".to_string(),
                "file_id".to_string(),
            ));
        };

        match spec.identity {
            // `scans` — the BIDS `scans.tsv` table. One row per data file, keyed by the file
            // rather than by `(dataset_id, file_path)`: identity lives in `file_registry`, and
            // this table holds only what the TSV says *about* that file (`acq_time`, plus
            // whatever the schema and overlays declare).
            RowIdentity::PerFile => {
                file_key(&mut columns, &mut fields);
                pk = vec!["file_id".to_string()];
                trailing
                    .push("FOREIGN KEY (file_id) REFERENCES file_registry(file_id)".to_string());
                skip.insert("filename"); // the `filename` column IS the registry's file_path
            }
            RowIdentity::PerEntity if spec.table == "participants" => {
                base(&mut columns, &mut fields, "dataset_id");
                base(&mut columns, &mut fields, "participant_id");
                pk = vec!["dataset_id".to_string(), "participant_id".to_string()];
                skip.insert("participant_id");
            }
            RowIdentity::PerEntity => {
                // sessions
                base(&mut columns, &mut fields, "dataset_id");
                base(&mut columns, &mut fields, "participant_id");
                base(&mut columns, &mut fields, "session_id");
                pk = vec![
                    "dataset_id".to_string(),
                    "participant_id".to_string(),
                    "session_id".to_string(),
                ];
                skip.insert("session_id");
                skip.insert("participant_id");
                trailing.push(
                    "FOREIGN KEY (dataset_id, participant_id) REFERENCES participants(dataset_id, participant_id)"
                        .to_string(),
                );
            }
            RowIdentity::PerRow => {
                // The source file, by key. These tables key on *tabular* files (an
                // `*_events.tsv`, a `*_channels.tsv`), which are registry rows like any
                // other — so "which subject/task is this row from?" is answered by joining
                // `all_files`, not by concept columns of their own.
                file_key(&mut columns, &mut fields);
                // `row_idx` exists only where it means something. It carries the one thing a
                // SQL table cannot hold implicitly — the source file's line order — so a table
                // whose order is declared not load-bearing has nothing to put in it, and a
                // column of arbitrary labels would only invite `ORDER BY row_idx`.
                if self.ingestion.ordered(&spec.table) {
                    columns.push("row_idx BIGINT".to_string());
                    fields.push((
                        "row_idx".to_string(),
                        "BIGINT".to_string(),
                        "row_idx".to_string(),
                    ));
                }
            }
        }

        // Rule columns, written under their true TSV header. Identifiers are
        // quoted because BIDS headers include DuckDB reserved words (`type`,
        // `index`, `group`, `time`, …) and case-sensitive names (`HED`).
        for c in &spec.columns {
            if skip.contains(c.name.as_str()) {
                continue;
            }
            columns.push(format!("{} {}", quote_ident(&c.name), c.sql_type));
            fields.push((c.name.clone(), c.sql_type.clone(), c.name.clone()));
        }

        // Overflow for any header without a dedicated column — unless the ingestion
        // policy declares this table's undeclared columns cataloged, in which case the
        // column is omitted outright. Omitting it (rather than leaving it NULL) is what
        // makes `row_values` drop those keys for free: it iterates `table_columns`, so
        // with no `other_data` field there is no branch to populate.
        if self.ingestion.undeclared(&spec.table) == Undeclared::Store {
            columns.push("other_data JSON".to_string());
            fields.push((
                "other_data".to_string(),
                "JSON".to_string(),
                "other_data".to_string(),
            ));
        }

        // Where a table's BIDS-concept columns come from — now two cases, not ADR 0002 §7's
        // three (see docs/adr/0006):
        //
        // - MATERIALIZED (physical `TEXT`, written by a reader) for adapter *data* tables,
        //   whose paths encode no BIDS entities. Marked by the ingestion policy's per-table
        //   `concepts` list. These tables hold reader output only, so there is nothing to
        //   regex out of a path even if they still carried one.
        // - NOWHERE, for every other table here. The virtual-regex columns moved to the
        //   `all_files` view over `file_registry`, because a table keyed by `file_id` has no
        //   `file_path` to compute them from — and because computing them once, on the one
        //   relation that holds every file, is what stops 24 tables each carrying their own
        //   copy of the same 40 expressions. "Which subject is this row from?" is now a join.
        //
        // The projection fallback moved with them: `projected` is a column of the registry,
        // so `ingest_projected` writes it in exactly one place.
        let concepts: Vec<String> = self.ingestion.materialized_concepts(&spec.table).to_vec();
        if !concepts.is_empty() {
            let existing: std::collections::HashSet<String> =
                fields.iter().map(|(n, _, _)| n.clone()).collect();
            for c in concepts {
                if existing.contains(&c) {
                    continue; // already a base/rule column (e.g. row_idx, a data column)
                }
                columns.push(format!("{} TEXT", quote_ident(&c)));
                fields.push((c.clone(), "TEXT".to_string(), c));
            }
        }

        if !pk.is_empty() {
            columns.push(format!("PRIMARY KEY ({})", pk.join(", ")));
        }
        columns.extend(trailing);

        let sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (\n    {}\n);",
            spec.table,
            columns.join(",\n    ")
        );
        self.table_definitions.insert(spec.table.clone(), sql);
        let table = spec.table.clone();
        self.set_table_columns(&table, fields);
        if !pk.is_empty() {
            self.primary_keys.insert(spec.table.clone(), pk);
        }
    }

    /// Extract all metadata field definitions (`field name -> SQL type`) from
    /// schema.json.
    fn extract_metadata_fields(&self) -> HashMap<String, String> {
        let mut fields = HashMap::new();

        if let Some(metadata) = self.schema["objects"]["metadata"].as_object() {
            for (field_name, field_def) in metadata {
                fields.insert(field_name.clone(), json_type_to_sql(field_def));
            }
        }

        fields
    }

    // Type mapping lives in the module-level free functions `json_type_to_sql`
    // and `map_simple_type` (below), so `schema::tabular` can share them.

    /// Generate scans and sidecars tables
    /// Build the `GENERATED ALWAYS AS (…) VIRTUAL` column definitions that expose
    /// BIDS concepts on the `scans` table, derived from `file_path`.
    ///
    /// The column set is generated from the vendored BIDS schema (whose structure
    /// the BIDS *metaschema* defines), so it tracks the spec rather than being
    /// hardcoded:
    ///
    /// - **one column per entity** in `objects.entities`, named by the entity's
    ///   `name` (`task`, `sub`, `ses`, `run`, …). The value pattern follows the
    ///   entity's `format`: `index` entities match `[0-9]+`, `label` entities
    ///   `[0-9A-Za-z]+`. The match is boundary-anchored (`(?:^|[_/])name-(value)`)
    ///   so e.g. `rec-` never matches inside `recording-…`, and `NULLIF(…, '')`
    ///   yields SQL `NULL` when the entity is absent (e.g. `ses` for a dataset
    ///   without sessions — which is what lets one query span a mixed pool).
    /// - **`datatype`** — the BIDS datatype directory (`func`, `anat`, …) from
    ///   `objects.datatypes`.
    /// - **`suffix`** — the trailing `_<suffix>` before the extension.
    /// - **`extension`** — the file extension (`.nii.gz`, `.tsv`).
    /// - **`modality`** — the broad modality (`mri`, `eeg`, …) mapped from the
    ///   datatype directory via `rules.modalities`, emitted as a `CASE`.
    ///
    /// Example emitted column (for the `task` entity):
    /// ```sql
    /// "task" VARCHAR GENERATED ALWAYS AS (
    ///     NULLIF(regexp_extract(file_path, '(?:^|[_/])task-([0-9A-Za-z]+)', 1), '')
    /// ) VIRTUAL
    /// ```
    ///
    /// These are intentionally left out of `table_columns`, so [`Schema::insert`]
    /// neither lists nor binds them; DuckDB computes each from `file_path` at
    /// query time. Adding or changing entities is therefore a schema-file change
    /// with no impact on the write path.
    /// Wrap a generated concept expression so a stored term-map projection wins over
    /// the path regex, for the concepts some active term map can actually project.
    ///
    /// A BIDS filename *contains* its entities, so the regex is right for it and the
    /// projection is NULL. A term-mapped path (`sub-01/mri/wmparc.mgz`) contains
    /// almost none of them, so without this the projection a term map computed is
    /// discarded and the row reads as `seg`/`datatype`/`suffix` NULL.
    ///
    /// Only the projectable set is wrapped, because every wrapped column pays its COALESCE on
    /// every read — so the set is kept to the concepts some configured term map can actually
    /// supply rather than every concept column. With no term maps configured the set is empty
    /// and the DDL is byte-identical to a plain BIDS catalog's, so BIDS-only ingests pay
    /// nothing at all.
    fn project_expr(&self, name: &str, expr: &str, project: bool) -> String {
        if !project || !self.projected_concepts.contains(name) {
            return expr.to_string();
        }
        format!("COALESCE(json_extract_string(projected, '$.{name}'), {expr})")
    }

    /// The BIDS-concept columns derived from `file_path`, as name/type/expression triples.
    ///
    /// Returned as expressions rather than finished DDL because they are emitted two ways: as
    /// `GENERATED ALWAYS AS (…) VIRTUAL` columns on a base table that holds `file_path`, and as
    /// plain `… AS name` select items in the `all_files` view over `file_registry`. One producer,
    /// so the two renderings cannot drift.
    fn generated_bids_columns(&self, project: bool) -> Vec<ConceptColumn> {
        let mut cols: Vec<ConceptColumn> = Vec::new();

        // One column per BIDS entity, keyed by its short `name`. Entity set comes
        // from the shared `bids-schema` owner (already sorted for deterministic
        // DDL), so it can't diverge from the Python-generated `Entity` Literal.
        for e in bids_schema::datatypes::entities(&self.schema) {
            let valpat = if e.format.as_deref() == Some("index") {
                "[0-9]+"
            } else {
                "[0-9A-Za-z]+"
            };
            let name = &e.name;
            let expr = self.project_expr(
                name,
                &format!("NULLIF(regexp_extract(file_path, '(?:^|[_/]){name}-({valpat})', 1), '')"),
                project,
            );
            cols.push(ConceptColumn::varchar(name, expr));
        }

        // datatype directory (func/anat/dwi/...), from the shared owner.
        let datatypes = bids_schema::datatypes::datatypes(&self.schema);
        if !datatypes.is_empty() {
            let alt = datatypes.join("|");
            let expr = self.project_expr(
                "datatype",
                &format!("NULLIF(regexp_extract(file_path, '/({alt})/', 1), '')"),
                project,
            );
            cols.push(ConceptColumn::varchar("datatype", expr));
        }

        // suffix (trailing _<suffix> before the extension) and extension. `extension`
        // is never projected: the filename is authoritative for it either way.
        let suffix_expr = self.project_expr(
            "suffix",
            "NULLIF(regexp_extract(file_path, '_([A-Za-z0-9]+)\\.[^/]+$', 1), '')",
            project,
        );
        cols.push(ConceptColumn::varchar("suffix", suffix_expr));
        cols.push(ConceptColumn::varchar(
            "extension",
            "NULLIF(regexp_extract(file_path, '(\\.[^/]+)$', 1), '')".to_string(),
        ));

        // pseudofile: TRUE when the file is a BIDS "pseudo-file" — an opaque directory (`.ds`,
        // `.mefd`, `.ome.zarr`, …) that tools treat as a single file. Derived from the schema's
        // pseudo-file extensions. (Components inside such a directory are never indexed: the walk
        // emits the directory as one file and does not descend into it.)
        let pseudo_alt: Vec<String> = bids_schema::pseudo_file_extensions(&self.schema)
            .iter()
            .filter_map(|e| e.strip_suffix('/'))
            .filter(|e| !e.is_empty())
            .map(|e| e.replace('.', "\\."))
            .collect();
        if !pseudo_alt.is_empty() {
            cols.push(ConceptColumn::varchar(
                "pseudofile",
                format!("regexp_matches(file_path, '({})$')", pseudo_alt.join("|")),
            ));
        }

        // modality (mri/eeg/...) mapped from the datatype dir via rules.modalities.
        // Modality set (sorted) comes from the shared owner; the per-modality
        // datatype list is still read from the schema for the CASE arms.
        //
        // When `datatype` is projectable this keys off the `datatype` column rather
        // than re-matching the path: a term-mapped file has no `/anat/` component, so
        // matching the path directly would leave `modality` NULL on exactly the rows
        // whose datatype the projection just supplied. DuckDB permits a generated
        // column to reference another, so this composes with the COALESCE above.
        let by_datatype = project && self.projected_concepts.contains("datatype");
        let mut whens = Vec::new();
        for m in bids_schema::datatypes::modalities(&self.schema) {
            if let Some(dts) = self.schema["rules"]["modalities"][&m]["datatypes"].as_array() {
                let alt: Vec<&str> = dts.iter().filter_map(|d| d.as_str()).collect();
                if !alt.is_empty() {
                    whens.push(if by_datatype {
                        let list = alt
                            .iter()
                            .map(|d| format!("'{d}'"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("WHEN datatype IN ({list}) THEN '{m}'")
                    } else {
                        format!(
                            "WHEN regexp_matches(file_path, '/({})/') THEN '{}'",
                            alt.join("|"),
                            m
                        )
                    });
                }
            }
        }
        if !whens.is_empty() {
            cols.push(ConceptColumn::varchar(
                "modality",
                format!("CASE {} ELSE NULL END", whens.join(" ")),
            ));
        }

        cols
    }

    /// Generate the `sidecars` table — the merged JSON-sidecar metadata for each
    /// imaging file. This is *not* a tabular file: its (very wide) column set comes
    /// from `objects.metadata`, and it is keyed to `scans` by `(dataset_id,
    /// file_path)`. The `scans` table itself is generated by
    /// [`Schema::generate_tabular_table`].
    fn generate_sidecars_table(&mut self) {
        // Generate 'sidecars' table (wide, all metadata)
        let mut sidecar_columns = Vec::new();
        let mut sidecar_fields = Vec::new();
        let mut seen_lowercase_names = std::collections::HashSet::new();

        // Keyed by the file its metadata describes. Note *which* file: a sidecar row is the
        // merged metadata for a **data** file, not for the `.json` it was read from — the
        // JSON has its own registry row, under its own path (docs/adr/0006).
        sidecar_columns.push("file_id UBIGINT".to_string());
        sidecar_fields.push((
            "file_id".to_string(),
            "UBIGINT".to_string(),
            "file_id".to_string(),
        ));
        seen_lowercase_names.insert("file_id".to_string());

        // Extract ALL metadata fields from objects.metadata
        let metadata_fields = self.extract_metadata_fields();
        let mut sorted_fields: Vec<_> = metadata_fields.into_iter().collect();
        sorted_fields.sort_by(|a, b| a.0.cmp(&b.0));

        for (field_name, sql_type) in sorted_fields {
            // Keep the verbatim BIDS field name as the column (only bidslake-internal
            // columns are snake_case), so the DB column, the `metadata` dict key, and
            // the BIDS spec all agree. Dedup case-insensitively because DuckDB folds
            // identifier case — e.g. `MISCChannelCount`/`MiscChannelCount` are distinct
            // strings but a duplicate-column error unless one is dropped.
            let key = field_name.to_lowercase();
            if seen_lowercase_names.contains(&key) {
                continue;
            }

            sidecar_columns.push(format!("{} {}", quote_ident(&field_name), sql_type));
            sidecar_fields.push((field_name.clone(), sql_type, field_name));
            seen_lowercase_names.insert(key);
        }

        // Add other_data for custom fields in sidecars
        sidecar_columns.push("other_data JSON".to_string());
        sidecar_fields.push((
            "other_data".to_string(),
            "JSON".to_string(),
            "other_data".to_string(),
        ));

        // Constraints for sidecars. The foreign key targets the registry rather than `scans`
        // because DuckDB refuses one against a view, and because the registry is the one
        // relation that holds every file.
        sidecar_columns.push("PRIMARY KEY (file_id)".to_string());
        sidecar_columns.push("FOREIGN KEY (file_id) REFERENCES file_registry(file_id)".to_string());

        let sidecar_sql = format!(
            "CREATE TABLE IF NOT EXISTS sidecars (\n    {}\n);",
            sidecar_columns.join(",\n    ")
        );
        self.table_definitions
            .insert("sidecars".to_string(), sidecar_sql);
        self.set_table_columns("sidecars", sidecar_fields);
        self.primary_keys
            .insert("sidecars".to_string(), vec!["file_id".to_string()]);
    }

    fn generate_dataset_description_def(&mut self) {
        let table_name = "dataset_description";
        // No `root_uri` here: a dataset may have been built from several ingest roots, and
        // one column cannot hold them. `dataset_roots` is the registry, and it is what
        // resolution joins through (docs/adr/0005).
        let mut columns = vec!["dataset_id TEXT PRIMARY KEY".to_string()];
        let mut fields = vec![(
            "dataset_id".to_string(),
            "TEXT".to_string(),
            "dataset_id".to_string(),
        )];
        let mut seen_lowercase_names = std::collections::HashSet::new();
        seen_lowercase_names.insert("dataset_id".to_lowercase());

        // List of known dataset_description.json fields from BIDS spec
        // These are the fields that typically appear in dataset_description.json
        let dataset_description_field_names = vec![
            "Name",
            "BIDSVersion",
            "HEDVersion",
            "DatasetType",
            "License",
            "Authors",
            "Acknowledgements",
            "HowToAcknowledge",
            "Funding",
            "EthicsApprovals",
            "ReferencesAndLinks",
            "DatasetDOI",
            "GeneratedBy",
            "SourceDatasets",
            "DatasetLinks",
        ];

        // Extract these fields from the metadata object. Keep the verbatim BIDS
        // field name as the column (`Name`, `BIDSVersion`, …); only the
        // bidslake-internal columns (`dataset_id`) are snake_case.
        // Dedup case-insensitively (DuckDB folds identifier case).
        if let Some(metadata) = self.schema["objects"]["metadata"].as_object() {
            for field_name in dataset_description_field_names {
                if let Some(field_def) = metadata.get(field_name) {
                    let sql_type = json_type_to_sql(field_def);
                    let lowercase_name = field_name.to_lowercase();

                    if !seen_lowercase_names.contains(&lowercase_name) {
                        columns.push(format!("{} {}", quote_ident(field_name), sql_type));
                        fields.push((field_name.to_string(), sql_type, field_name.to_string()));
                        seen_lowercase_names.insert(lowercase_name);
                    }
                }
            }
        }

        // Add other_data for custom fields
        columns.push("other_data JSON".to_string());
        fields.push((
            "other_data".to_string(),
            "JSON".to_string(),
            "other_data".to_string(),
        ));

        let create_sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (\n    {}\n);",
            table_name,
            columns.join(",\n    ")
        );
        self.table_definitions
            .insert(table_name.to_string(), create_sql);
        self.set_table_columns(table_name, fields);
        self.primary_keys
            .insert(table_name.to_string(), vec!["dataset_id".to_string()]);
    }

    /// The `CREATE TABLE` statements only, in foreign-key-safe order.
    ///
    /// Split from [`create_views_sql`](Self::create_views_sql) because the views now sit on
    /// both sides of the static DDL: `all_files` selects from the generated `file_registry`,
    /// while a `describes` view selects from `file_associations` and (for `bvals`/`bvecs`)
    /// from tables `schema.rs` creates. So `BidsDb::create_tables` runs generated tables →
    /// static tables → every view, which one combined list could not express.
    pub fn create_tables_sql(&self) -> Vec<String> {
        // Foreign keys constrain the order: `sessions` references `participants`, and both
        // `scans` and `sidecars` reference `file_registry`. Everything else is order-free, so
        // emit the FK-constrained tables first, then the rest deterministically.
        let mut order: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for t in [
            "participants",
            "sessions",
            "file_registry",
            "scans",
            "sidecars",
        ] {
            if self.table_definitions.contains_key(t) && seen.insert(t.to_string()) {
                order.push(t.to_string());
            }
        }
        let mut rest: Vec<String> = self
            .table_definitions
            .keys()
            .filter(|k| !seen.contains(k.as_str()) && k.as_str() != "dataset_description")
            .cloned()
            .collect();
        rest.sort();
        order.extend(rest);
        if self.table_definitions.contains_key("dataset_description") {
            order.push("dataset_description".to_string());
        }

        order
            .iter()
            .filter_map(|t| self.table_definitions.get(t).cloned())
            .collect()
    }

    /// The `CREATE OR REPLACE VIEW` statements, sorted so the emitted DDL is deterministic.
    ///
    /// Run after **all** tables, generated and static alike: every view selects from at least
    /// one of them.
    pub fn create_views_sql(&self) -> Vec<String> {
        let mut views: Vec<&String> = self.view_definitions.keys().collect();
        views.sort();
        views
            .into_iter()
            .filter_map(|v| self.view_definitions.get(v).cloned())
            .collect()
    }

    #[allow(dead_code)]
    pub fn get_create_sql(&self, table_name: &str) -> Option<&str> {
        self.table_definitions.get(table_name).map(String::as_str)
    }

    pub fn insert(&self, conn: &Connection, table_name: &str, data: &Value) -> Result<()> {
        self.write_row(conn, table_name, data, &self.insert_sql)
    }

    /// Like [`Self::insert`], but the row replaces one already under its primary key
    /// instead of deferring to it.
    ///
    /// For a row whose source is a file the ingest re-reads every run, where first-writer-
    /// wins means a re-index silently keeps a stale copy: `dataset_description`, whose
    /// `dataset_description.json` may have been added or corrected since. It is *not* the
    /// default, because the guarded insert is what lets the walk issue stub rows freely —
    /// an implicit `participants` row must never overwrite one read from `participants.tsv`.
    pub fn insert_or_replace(
        &self,
        conn: &Connection,
        table_name: &str,
        data: &Value,
    ) -> Result<()> {
        self.write_row(conn, table_name, data, &self.replace_sql)
    }

    fn write_row(
        &self,
        conn: &Connection,
        table_name: &str,
        data: &Value,
        statements: &HashMap<String, String>,
    ) -> Result<()> {
        // Use the statement built once at load time.
        // Was a `duckdb::Error::ToSqlConversionFailure` wrapping a fabricated
        // `io::Error(NotFound)` -- an I/O error for a table that is missing from an
        // in-memory map, which is neither I/O nor a conversion. It typed as `duckdb::Result`
        // and that was its only reason to exist.
        let sql = statements
            .get(table_name)
            .ok_or_else(|| anyhow!("no insert statement for table {table_name}"))?;
        let params = self.row_values(table_name, data)?;
        // Convert Option<Value> to &dyn ToSql
        let params_refs: Vec<&dyn duckdb::ToSql> =
            params.iter().map(|p| p as &dyn duckdb::ToSql).collect();
        // prepare_cached reuses the compiled plan across every row of this table.
        let mut stmt = conn
            .prepare_cached(sql)
            .map_err(duck)
            .with_context(|| format!("preparing insert into {table_name}"))?;
        // The context a call site cannot supply: it knows it is writing `sidecars`, not
        // which of the row's values DuckDB rejected. Naming the column count and the
        // offending row's keys turns "Conversion Error" into something locatable.
        stmt.execute(params_refs.as_slice())
            .map_err(duck)
            .with_context(|| {
                format!(
                    "inserting a row into {table_name} ({} bound values; keys: {})",
                    params_refs.len(),
                    summarize_keys(data),
                )
            })?;
        Ok(())
    }

    /// Build the ordered bind values for one row of `table_name` — the write columns
    /// in `table_columns` order, with the same type coercion the insert path uses.
    /// Shared by the prepared-statement `insert` and the bulk Appender path
    /// ([`crate::db::BidsDb::append_rows`]), so bulk inserts stay identical to
    /// single-row ones.
    pub fn row_values(
        &self,
        table_name: &str,
        data: &Value,
    ) -> Result<Vec<Option<duckdb::types::Value>>> {
        let fields = self
            .table_columns
            .get(table_name)
            .ok_or_else(|| anyhow!("table {table_name} not found in schema"))?;

        // Which source keys have a dedicated column. Case-insensitive, and precomputed
        // per table rather than rebuilt per row — see `declares` for why both matter.
        let schema_keys = self.table_keys_lower.get(table_name);
        let json_keys_lower = self.table_json_keys_lower.get(table_name);

        // The row's keys, folded to lowercase once, so the last-resort column match below is
        // a hash lookup. That match runs for every column the row does not carry — for a
        // table as wide as `sidecars` that is nearly all of them — so folding on the spot
        // meant rescanning and re-folding the whole row once per column, which made this the
        // dominant cost of shaping a sidecar row.
        //
        // This index is lossy by construction: two keys differing only in case collapse to
        // one entry. That is harmless for a lookup keyed on a column name, but it is why the
        // `other_data` overflow below iterates the row itself instead.
        let obj = data.as_object();
        let mut lower_keys: HashMap<String, (&String, &Value)> = HashMap::new();
        if let Some(obj) = obj {
            for (key, value) in obj {
                lower_keys.insert(key.to_lowercase(), (key, value));
            }
        }

        let mut params: Vec<Option<duckdb::types::Value>> = Vec::new();

        for (idx, (col_name, col_type, json_key)) in fields.iter().enumerate() {
            // Special handling for other_data column
            if col_name == "other_data" {
                if let Some(obj) = obj {
                    // Only the keys with no dedicated column reach the overflow.
                    //
                    // Iterated over the row, not over `lower_keys`: that index is keyed on the
                    // folded spelling, so a row carrying two undeclared keys that differ only
                    // in case keeps one of them and drops the other's value on the floor —
                    // silently, and against this module's promise that nothing is dropped. The
                    // membership test folds with `to_lowercase`, the same rule `declares` and
                    // `set_table_columns` use, and runs once per row over the row's own keys
                    // rather than once per column — which is the cost `lower_keys` removed.
                    //
                    // Borrowed, not cloned: serializing a map of references produces the same
                    // JSON without copying the values, and sidecar values are not always small.
                    // `BTreeMap<&str, _>` orders by key exactly as `serde_json::Map` (a
                    // `BTreeMap<String, _>` here) does, so the output is byte-identical.
                    let mut overflow: std::collections::BTreeMap<&str, &Value> =
                        std::collections::BTreeMap::new();
                    for (key, value) in obj {
                        if !schema_keys.is_some_and(|keys| keys.contains(&key.to_lowercase())) {
                            overflow.insert(key.as_str(), value);
                        }
                    }

                    if !overflow.is_empty() {
                        let json_str = serde_json::to_string(&overflow).unwrap();
                        params.push(Some(duckdb::types::Value::Text(json_str)));
                    } else {
                        params.push(None);
                    }
                } else {
                    params.push(None);
                }
                continue;
            }

            let val = obj.and_then(|obj| {
                obj.get(json_key).or_else(|| obj.get(col_name)).or_else(|| {
                    // Case-insensitive last resort, for the spelling the DDL dropped
                    // (see `table_keys_lower`). One hash lookup against the row's
                    // pre-folded keys — this path is taken for every column the row
                    // does not carry, so it must not scan.
                    json_keys_lower
                        .and_then(|lower| lower.get(idx))
                        .and_then(|want| lower_keys.get(want.as_str()))
                        .map(|(_, v)| *v)
                })
            });

            if col_type == "JSON" {
                match val.map(|v| v.to_string()) {
                    Some(s_val) => params.push(Some(duckdb::types::Value::Text(s_val))),
                    None => params.push(None),
                }
            } else {
                // A dedicated column has a fixed scalar type, but BIDS metadata is
                // messy: numeric columns receive non-numeric strings (censored ages
                // like "89+", ranges like "35-40") and even arrays (ASL
                // PostLabelingDelay, some MRS fields can be scalar OR a list). None
                // of those fit a numeric column. Rather than let DuckDB's implicit
                // cast abort the whole row insert — dropping every other field on
                // that row — store NULL for the mismatched value and keep the row.
                let is_numeric_col = matches!(
                    col_type.as_str(),
                    "DOUBLE" | "BIGINT" | "FLOAT" | "REAL" | "INTEGER" | "UBIGINT"
                );
                let is_bool_col = col_type == "BOOLEAN";
                match val {
                    // A number can't go into a BOOLEAN column.
                    Some(Value::Number(_)) if is_bool_col => params.push(None),
                    // The registry key, before the generic numeric arm. A `u64` is exactly
                    // what `serde_json::Number` holds, so an id makes the trip as a JSON
                    // *number* and arrives needing no parse — which is most of the
                    // simplification of moving off `HUGEINT` (see [`crate::bids::file_id`]).
                    //
                    // A negative or fractional value is not an id and cannot be coerced into
                    // one. NULL rather than a wrap-around: `file_registry.file_id` is a NOT
                    // NULL primary key, so such a row fails loudly instead of landing under
                    // a corrupted id.
                    Some(Value::Number(n)) if col_type == "UBIGINT" => match n.as_u64() {
                        Some(u) => params.push(Some(duckdb::types::Value::UBigInt(u))),
                        None => params.push(None),
                    },
                    Some(Value::Number(n)) => {
                        if n.is_i64() {
                            params.push(Some(duckdb::types::Value::BigInt(n.as_i64().unwrap())));
                        } else if n.is_f64() {
                            params.push(Some(duckdb::types::Value::Double(n.as_f64().unwrap())));
                        } else {
                            params.push(Some(duckdb::types::Value::Text(n.to_string())));
                        }
                    }
                    // An id that still arrives as a string — a hand-built row, or a caller
                    // outside the ingest. Kept because the generic arm below would try
                    // `parse::<i64>()`, fall through to `parse::<f64>()`, and silently round
                    // a primary key to 53 bits of mantissa for any id above 2^53.
                    Some(Value::String(s)) if col_type == "UBIGINT" => match s.parse::<u64>() {
                        Ok(u) => params.push(Some(duckdb::types::Value::UBigInt(u))),
                        Err(_) => params.push(None),
                    },
                    Some(Value::String(s)) if is_numeric_col => {
                        if let Ok(i) = s.parse::<i64>() {
                            params.push(Some(duckdb::types::Value::BigInt(i)));
                        } else if let Ok(f) = s.parse::<f64>() {
                            params.push(Some(duckdb::types::Value::Double(f)));
                        } else {
                            params.push(None);
                        }
                    }
                    Some(Value::String(s)) if is_bool_col => match s.as_str() {
                        "true" | "True" | "TRUE" => {
                            params.push(Some(duckdb::types::Value::Boolean(true)))
                        }
                        "false" | "False" | "FALSE" => {
                            params.push(Some(duckdb::types::Value::Boolean(false)))
                        }
                        _ => params.push(None),
                    },
                    Some(Value::String(s)) => {
                        params.push(Some(duckdb::types::Value::Text(s.clone())));
                    }
                    // Non-scalar (array/object) value in a scalar (numeric or bool)
                    // column: cannot represent it, so store NULL and keep the row.
                    Some(Value::Array(_)) | Some(Value::Object(_))
                        if is_numeric_col || is_bool_col =>
                    {
                        params.push(None);
                    }
                    // A bool can't go into a numeric column.
                    Some(Value::Bool(_)) if is_numeric_col => params.push(None),
                    Some(Value::Bool(b)) => {
                        params.push(Some(duckdb::types::Value::Boolean(*b)));
                    }
                    Some(Value::Null) | None => {
                        params.push(None);
                    }
                    Some(v) => {
                        // Non-numeric column: arrays/objects serialize to JSON text.
                        if v.is_array() || v.is_object() {
                            let json_str = v.to_string();
                            params.push(Some(duckdb::types::Value::Text(json_str)));
                        } else {
                            params.push(Some(duckdb::types::Value::Text(v.to_string())));
                        }
                    }
                }
            }
        }

        Ok(params)
    }
}

/// One BIDS-concept column of the `all_files` view: its name and the expression computing it
/// from `file_path` (optionally falling back from a stored term-map projection).
///
/// No SQL type, because these are select items rather than declared columns — DuckDB infers
/// the type from the expression. They used to be `GENERATED ALWAYS AS (…) VIRTUAL` columns on
/// every file-keyed table; once those tables key on `file_id` they have no `file_path` to
/// compute from, so the concepts are computed once, on the one relation that holds every file.
struct ConceptColumn {
    name: String,
    expr: String,
}

impl ConceptColumn {
    fn varchar(name: impl Into<String>, expr: String) -> Self {
        Self {
            name: name.into(),
            expr,
        }
    }

    /// As a select item in a view. DuckDB resolves a later expression against an earlier
    /// alias in the same `SELECT`, which is what lets `modality` key off `datatype`.
    fn as_select_item(&self) -> String {
        format!("{} AS {}", self.expr, quote_ident(&self.name))
    }
}

/// Whether a DuckDB column type is temporal (needs `TRY_CAST` on string insert).
fn is_temporal_type(col_type: &str) -> bool {
    matches!(col_type, "TIMESTAMP" | "DATE" | "TIME")
}

/// Quote a SQL identifier for DuckDB (double quotes, `"` doubled). Tabular column
/// names are BIDS TSV headers, which can be reserved words (`type`, `index`) or
/// case-sensitive (`HED`), so they must be quoted wherever they appear in SQL.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A SQL string literal: single-quoted, with embedded `'` doubled. Lives next to
/// [`quote_ident`] so one place owns SQL literal- and identifier-escaping.
pub(crate) fn sql_lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Map a BIDS schema field/column definition to a DuckDB column type.
///
/// Handles the three shapes a definition takes in `objects.columns` /
/// `objects.metadata`: a direct `type`, an `anyOf` union (first non-null wins),
/// and the `definition.Format` used by phenotype/physio columns (`age`, `sex`,
/// `cardiac`, …). Shared by [`Schema`] and [`crate::schema::tabular`].
pub(crate) fn json_type_to_sql(field_def: &Value) -> String {
    // Handle anyOf (union types) - take the first non-null type
    if let Some(any_of) = field_def.get("anyOf").and_then(|v| v.as_array()) {
        for type_def in any_of {
            if let Some(t) = type_def.get("type").and_then(|v| v.as_str())
                && t != "null"
            {
                return map_simple_type(t, type_def);
            }
        }
    }

    // Handle direct type
    if let Some(type_str) = field_def.get("type").and_then(|v| v.as_str()) {
        return map_simple_type(type_str, field_def);
    }

    // Handle BIDS "Format" field (e.g. for age)
    // Some fields like age have "Format": "number" inside definition
    if let Some(format) = field_def.get("Format").and_then(|v| v.as_str()) {
        match format {
            "number" | "float" => return "DOUBLE".to_string(),
            "integer" => return "BIGINT".to_string(),
            "boolean" => return "BOOLEAN".to_string(),
            _ => {} // Fallback to TEXT
        }
    }

    // Check inside "definition" object if present (common in BIDS schema)
    if let Some(def) = field_def.get("definition")
        && let Some(format) = def.get("Format").and_then(|v| v.as_str())
    {
        match format {
            "number" | "float" => return "DOUBLE".to_string(),
            "integer" => return "BIGINT".to_string(),
            "boolean" => return "BOOLEAN".to_string(),
            _ => {}
        }
    }

    // Default to TEXT for unknown types
    "TEXT".to_string()
}

/// Map a simple JSON-schema `type` to a DuckDB type.
fn map_simple_type(json_type: &str, field_def: &Value) -> String {
    match json_type {
        "string" => {
            // Check for special formats
            if let Some(format) = field_def.get("format").and_then(|v| v.as_str()) {
                match format {
                    "datetime" => "TIMESTAMP",
                    "date" => "DATE",
                    _ => "TEXT",
                }
            } else {
                "TEXT"
            }
        }
        "number" => "DOUBLE",
        "integer" => "BIGINT",
        "boolean" => "BOOLEAN",
        "array" => "TEXT", // Store as JSON text
        "object" => "JSON",
        _ => "TEXT",
    }
    .to_string()
}

pub(crate) fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = s.chars().collect();

    for (i, c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            // Add underscore before uppercase if:
            // 1. Not at start (i > 0)
            // 2. Previous char was lowercase, OR
            // 3. Next char exists and is lowercase (end of acronym like "XMLParser" -> "xml_parser")
            if i > 0 {
                let prev_is_lower = chars.get(i - 1).is_some_and(|ch| ch.is_lowercase());
                let next_is_lower = chars.get(i + 1).is_some_and(|ch| ch.is_lowercase());

                if prev_is_lower || next_is_lower {
                    result.push('_');
                }
            }
            result.push(c.to_lowercase().next().unwrap());
        } else {
            result.push(*c);
        }
    }

    // Normalize multiple underscores
    let mut normalized = String::new();
    let mut prev_was_underscore = false;
    for c in result.chars() {
        if c == '_' {
            if !prev_was_underscore {
                normalized.push(c);
            }
            prev_was_underscore = true;
        } else {
            normalized.push(c);
            prev_was_underscore = false;
        }
    }
    normalized
}

/// A few of a row's keys, for an error message.
///
/// The row itself is the wrong thing to put in an error: a `sidecars` row is 451 columns
/// wide and a `scans` row can carry an arbitrary `other_data` blob, so printing it buries
/// the message and can leak participant data into a log. The keys are enough to identify
/// which row failed, and are bounded.
fn summarize_keys(data: &Value) -> String {
    const MAX: usize = 8;
    let Some(obj) = data.as_object() else {
        return format!("<{}>", type_name(data));
    };
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let shown = keys.len().min(MAX);
    let mut out = keys[..shown].join(", ");
    if keys.len() > shown {
        out.push_str(&format!(", … {} more", keys.len() - shown));
    }
    out
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
