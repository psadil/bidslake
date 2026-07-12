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
//! pulls the value at `json_key` (falling back to `col_name`). Any input key that
//! is *not* claimed by a column lands in the table's `other_data JSON` column, so
//! nothing is dropped. The INSERT SQL is built once per table
//! (`build_insert_statements`) and executed via `prepare_cached`, so
//! DuckDB reuses one compiled plan across every row of that table.
//!
//! ## Generated (virtual) BIDS-concept columns
//!
//! On top of the written columns, the `scans` table carries generated columns —
//! `task`, `sub`, `ses`, `run`, …, plus `datatype`, `suffix`, `extension`, and
//! `modality` — produced by `generated_bids_columns`. These are
//! `GENERATED ALWAYS AS (…) VIRTUAL`, computed from `file_path` on read, so they
//! let callers query by BIDS concept (`WHERE task = 'rest'`) instead of by path
//! regex, and are *not* part of `table_columns` (the write path never touches
//! them). They too are generated from `objects.entities` / `objects.datatypes` /
//! `rules.modalities`.
//!
//! The static tables `diffusion` and `file_associations` live in the parent
//! [`crate::schema`] module, not here.

use super::tabular::{RowIdentity, TableSpec, Tabular};
use duckdb::{Connection, Result};
use serde_json::Value;
use std::collections::HashMap;

/// The vendored BIDS schema, embedded at compile time. An instance of the BIDS
/// metaschema; its `objects.*` and `rules.*` sections drive table generation.
const SCHEMA_JSON: &str = bids_schema::SCHEMA_JSON;

#[derive(Clone, Debug)]
pub struct Schema {
    schema: Value,
    /// The schema-driven tabular model (which tables exist, their columns). Drives
    /// generation of every table that comes from `rules.tabular_data`.
    tabular: Tabular,
    table_definitions: HashMap<String, String>,
    table_columns: HashMap<String, Vec<(String, String, String)>>, // table -> [(col_name, col_type, json_key)]
    primary_keys: HashMap<String, Vec<String>>,                    // table -> [pk_col_names]
    insert_sql: HashMap<String, String>, // table -> prebuilt INSERT statement
}

impl Schema {
    pub fn load(schema_path: Option<&str>) -> Self {
        let schema: Value = match schema_path {
            Some(path) => {
                let content = std::fs::read_to_string(path)
                    .unwrap_or_else(|e| panic!("Failed to read schema file {}: {}", path, e));
                serde_json::from_str(&content)
                    .unwrap_or_else(|e| panic!("Failed to parse schema file {}: {}", path, e))
            }
            None => {
                serde_json::from_str(SCHEMA_JSON).expect("Failed to parse embedded schema.json")
            }
        };

        let tabular = Tabular::load(&schema);
        let mut instance = Schema {
            schema,
            tabular,
            table_definitions: HashMap::new(),
            table_columns: HashMap::new(),
            primary_keys: HashMap::new(),
            insert_sql: HashMap::new(),
        };
        instance.generate_definitions();
        instance.build_insert_statements();
        instance
    }

    /// The schema-driven tabular model — which tables exist and their columns,
    /// plus selector-based routing. Used by the ingest pipeline.
    pub fn tabular(&self) -> &Tabular {
        &self.tabular
    }

    /// The BIDS datatype directory names (`func`, `anat`, `eeg`, `phenotype`, …)
    /// from `objects.datatypes`. Ingestion uses these to derive a file's datatype
    /// from its path for selector evaluation.
    pub fn datatypes(&self) -> Vec<String> {
        self.schema["objects"]["datatypes"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The raw parsed BIDS schema JSON — for the shared `bids_schema` helpers
    /// (`build_file_context`, `find_datatype`, association resolution).
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
            if let Some(sql) = self.build_insert_sql(&table) {
                self.insert_sql.insert(table, sql);
            }
        }
    }

    /// Build the `INSERT ... SELECT ... [WHERE NOT EXISTS ...]` statement for a
    /// table from its column and primary-key metadata.
    fn build_insert_sql(&self, table_name: &str) -> Option<String> {
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

        let mut sql = format!(
            "INSERT INTO {} ({}) SELECT {}",
            table_name,
            col_names.join(", "),
            selects.join(", ")
        );

        // Idempotency guard: skip the row if a matching primary key already
        // exists (safe re-indexing). Kept for correctness; made cheap by caching.
        if let Some(pks) = self.primary_keys.get(table_name)
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

        // `motion` and `stim` are continuous recordings the schema declares
        // (`rules.files`) but gives no `tabular_data` column rule: their columns
        // are data-defined (from the sidecar `Columns` array, or the associated
        // `_channels.tsv`). So they are bare row tables — everything lands in
        // `other_data`, alongside the generated virtual BIDS columns. (`physio` and
        // `physio_events` do have column rules and are generated above.)
        for name in ["motion", "stim"] {
            self.generate_tabular_table(&TableSpec {
                table: name.to_string(),
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

        match spec.identity {
            RowIdentity::PerFile => {
                base(&mut columns, &mut fields, "dataset_id");
                base(&mut columns, &mut fields, "file_path");
                pk = vec!["dataset_id".to_string(), "file_path".to_string()];
                skip.insert("filename"); // the `filename` column IS file_path
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
                base(&mut columns, &mut fields, "dataset_id");
                base(&mut columns, &mut fields, "file_path");
                columns.push("row_idx BIGINT".to_string());
                fields.push((
                    "row_idx".to_string(),
                    "BIGINT".to_string(),
                    "row_idx".to_string(),
                ));
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

        // Overflow for any header without a dedicated column.
        columns.push("other_data JSON".to_string());
        fields.push((
            "other_data".to_string(),
            "JSON".to_string(),
            "other_data".to_string(),
        ));

        // Virtual BIDS-concept columns (derived from file_path) for file-based
        // tables, computed on read and never written.
        if spec.file_based {
            columns.extend(self.generated_bids_columns());
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
        self.table_columns.insert(spec.table.clone(), fields);
        if !pk.is_empty() {
            self.primary_keys.insert(spec.table.clone(), pk);
        }
    }

    /// Extract all metadata field definitions from schema.json
    fn extract_metadata_fields(&self) -> HashMap<String, (String, bool)> {
        let mut fields = HashMap::new();

        if let Some(metadata) = self.schema["objects"]["metadata"].as_object() {
            for (field_name, field_def) in metadata {
                let sql_type = json_type_to_sql(field_def);
                // For now, assume all fields are nullable (we'll refine this later based on REQUIRED/RECOMMENDED)
                fields.insert(field_name.clone(), (sql_type, true));
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
    fn generated_bids_columns(&self) -> Vec<String> {
        let mut cols = Vec::new();

        // One column per BIDS entity, keyed by its short `name`.
        if let Some(entities) = self.schema["objects"]["entities"].as_object() {
            let mut ents: Vec<(&str, &str)> = entities
                .values()
                .filter_map(|e| {
                    let name = e.get("name")?.as_str()?;
                    let valpat = match e.get("format").and_then(|f| f.as_str()) {
                        Some("index") => "[0-9]+",
                        _ => "[0-9A-Za-z]+",
                    };
                    Some((name, valpat))
                })
                .collect();
            ents.sort_by(|a, b| a.0.cmp(b.0)); // deterministic DDL
            for (name, valpat) in ents {
                cols.push(format!(
                    "\"{name}\" VARCHAR GENERATED ALWAYS AS (NULLIF(regexp_extract(file_path, '(?:^|[_/]){name}-({valpat})', 1), '')) VIRTUAL"
                ));
            }
        }

        // datatype directory (func/anat/dwi/...).
        if let Some(dts) = self.schema["objects"]["datatypes"].as_object() {
            let mut keys: Vec<&str> = dts.keys().map(|k| k.as_str()).collect();
            keys.sort();
            if !keys.is_empty() {
                let alt = keys.join("|");
                cols.push(format!(
                    "datatype VARCHAR GENERATED ALWAYS AS (NULLIF(regexp_extract(file_path, '/({alt})/', 1), '')) VIRTUAL"
                ));
            }
        }

        // suffix (trailing _<suffix> before the extension) and extension.
        cols.push(
            "suffix VARCHAR GENERATED ALWAYS AS (NULLIF(regexp_extract(file_path, '_([A-Za-z0-9]+)\\.[^/]+$', 1), '')) VIRTUAL"
                .to_string(),
        );
        cols.push(
            "extension VARCHAR GENERATED ALWAYS AS (NULLIF(regexp_extract(file_path, '(\\.[^/]+)$', 1), '')) VIRTUAL"
                .to_string(),
        );

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
            cols.push(format!(
                "pseudofile BOOLEAN GENERATED ALWAYS AS (regexp_matches(file_path, '({})$')) VIRTUAL",
                pseudo_alt.join("|")
            ));
        }

        // modality (mri/eeg/...) mapped from the datatype dir via rules.modalities.
        if let Some(mods) = self.schema["rules"]["modalities"].as_object() {
            let mut keys: Vec<&str> = mods.keys().map(|k| k.as_str()).collect();
            keys.sort();
            let mut whens = Vec::new();
            for m in keys {
                if let Some(dts) = mods[m]["datatypes"].as_array() {
                    let alt: Vec<&str> = dts.iter().filter_map(|d| d.as_str()).collect();
                    if !alt.is_empty() {
                        whens.push(format!(
                            "WHEN regexp_matches(file_path, '/({})/') THEN '{}'",
                            alt.join("|"),
                            m
                        ));
                    }
                }
            }
            if !whens.is_empty() {
                cols.push(format!(
                    "modality VARCHAR GENERATED ALWAYS AS (CASE {} ELSE NULL END) VIRTUAL",
                    whens.join(" ")
                ));
            }
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

        // Base columns for sidecars
        sidecar_columns.push("dataset_id TEXT".to_string());
        sidecar_fields.push((
            "dataset_id".to_string(),
            "TEXT".to_string(),
            "dataset_id".to_string(),
        ));
        seen_lowercase_names.insert("dataset_id".to_string());

        sidecar_columns.push("file_path TEXT".to_string());
        sidecar_fields.push((
            "file_path".to_string(),
            "TEXT".to_string(),
            "file_path".to_string(),
        ));
        seen_lowercase_names.insert("file_path".to_string());

        // Extract ALL metadata fields from objects.metadata
        let metadata_fields = self.extract_metadata_fields();
        let mut sorted_fields: Vec<_> = metadata_fields.into_iter().collect();
        sorted_fields.sort_by(|a, b| a.0.cmp(&b.0));

        for (field_name, (sql_type, _nullable)) in sorted_fields {
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

        // Constraints for sidecars
        sidecar_columns.push("PRIMARY KEY (dataset_id, file_path)".to_string());
        sidecar_columns.push(
            "FOREIGN KEY (dataset_id, file_path) REFERENCES scans(dataset_id, file_path)"
                .to_string(),
        );

        let sidecar_sql = format!(
            "CREATE TABLE IF NOT EXISTS sidecars (\n    {}\n);",
            sidecar_columns.join(",\n    ")
        );
        self.table_definitions
            .insert("sidecars".to_string(), sidecar_sql);
        self.table_columns
            .insert("sidecars".to_string(), sidecar_fields);
        self.primary_keys.insert(
            "sidecars".to_string(),
            vec!["dataset_id".to_string(), "file_path".to_string()],
        );
    }

    fn generate_dataset_description_def(&mut self) {
        let table_name = "dataset_description";
        let mut columns = vec![
            "dataset_id TEXT PRIMARY KEY".to_string(),
            "root_uri TEXT".to_string(),
        ];
        let mut fields = vec![
            (
                "dataset_id".to_string(),
                "TEXT".to_string(),
                "dataset_id".to_string(),
            ),
            (
                "root_uri".to_string(),
                "TEXT".to_string(),
                "root_uri".to_string(),
            ),
        ];
        let mut seen_lowercase_names = std::collections::HashSet::new();
        seen_lowercase_names.insert("dataset_id".to_lowercase());
        seen_lowercase_names.insert("root_uri".to_lowercase());

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
        // bidslake-internal columns (`dataset_id`, `root_uri`) are snake_case.
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
        self.table_columns.insert(table_name.to_string(), fields);
        self.primary_keys
            .insert(table_name.to_string(), vec!["dataset_id".to_string()]);
    }

    pub fn create_tables_sql(&self) -> Vec<String> {
        // Foreign keys constrain the order: `sessions` references `participants`,
        // and `sidecars` references `scans`. Everything else is order-free, so
        // emit the FK-constrained tables first, then the rest deterministically.
        let mut order: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for t in ["participants", "sessions", "scans", "sidecars"] {
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

    #[allow(dead_code)]
    pub fn get_create_sql(&self, table_name: &str) -> Option<&String> {
        self.table_definitions.get(table_name)
    }

    pub fn insert(&self, conn: &Connection, table_name: &str, data: &Value) -> Result<()> {
        let fields = self.table_columns.get(table_name).ok_or_else(|| {
            duckdb::Error::ToSqlConversionFailure(Box::new(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Table {} not found in schema", table_name),
            )))
        })?;

        // Build set of all json_keys that have dedicated columns
        let schema_keys: std::collections::HashSet<&str> = fields
            .iter()
            .map(|(_, _, json_key)| json_key.as_str())
            .collect();

        // Use the statement built once at load time.
        let sql = self.insert_sql.get(table_name).ok_or_else(|| {
            duckdb::Error::ToSqlConversionFailure(Box::new(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("No insert statement for table {}", table_name),
            )))
        })?;

        let mut params = Vec::new();
        // We need to keep the values alive until the end of the function
        let mut string_values: Vec<String> = Vec::new();

        for (col_name, col_type, json_key) in fields {
            // Special handling for other_data column
            if col_name == "other_data" {
                if let Some(obj) = data.as_object() {
                    // Filter: only include keys NOT in schema_keys
                    let mut custom_data = serde_json::Map::new();
                    for (key, value) in obj {
                        if !schema_keys.contains(key.as_str()) {
                            custom_data.insert(key.clone(), value.clone());
                        }
                    }

                    if !custom_data.is_empty() {
                        let json_str = serde_json::to_string(&custom_data).unwrap();
                        string_values.push(json_str);
                        params.push(Some(duckdb::types::Value::Text(
                            string_values.last().unwrap().clone(),
                        )));
                    } else {
                        params.push(None);
                    }
                } else {
                    params.push(None);
                }
                continue;
            }

            let val = if let Some(obj) = data.as_object() {
                obj.get(json_key).or_else(|| obj.get(col_name))
            } else {
                None
            };

            if col_type == "JSON" {
                let s = val.map(|v| v.to_string());
                if let Some(s_val) = s {
                    string_values.push(s_val);
                    params.push(Some(duckdb::types::Value::Text(
                        string_values.last().unwrap().clone(),
                    )));
                } else {
                    params.push(None);
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
                    "DOUBLE" | "BIGINT" | "FLOAT" | "REAL" | "INTEGER" | "HUGEINT"
                );
                let is_bool_col = col_type == "BOOLEAN";
                match val {
                    // A number can't go into a BOOLEAN column.
                    Some(Value::Number(_)) if is_bool_col => params.push(None),
                    Some(Value::Number(n)) => {
                        if n.is_i64() {
                            params.push(Some(duckdb::types::Value::BigInt(n.as_i64().unwrap())));
                        } else if n.is_f64() {
                            params.push(Some(duckdb::types::Value::Double(n.as_f64().unwrap())));
                        } else {
                            params.push(Some(duckdb::types::Value::Text(n.to_string())));
                        }
                    }
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
                            string_values.push(json_str);
                            params.push(Some(duckdb::types::Value::Text(
                                string_values.last().unwrap().clone(),
                            )));
                        } else {
                            params.push(Some(duckdb::types::Value::Text(v.to_string())));
                        }
                    }
                }
            }
        }

        // Convert Option<Value> to &dyn ToSql
        let params_refs: Vec<&dyn duckdb::ToSql> =
            params.iter().map(|p| p as &dyn duckdb::ToSql).collect();

        // prepare_cached reuses the compiled plan across every row of this table.
        let mut stmt = conn.prepare_cached(sql)?;
        stmt.execute(params_refs.as_slice())?;
        Ok(())
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
