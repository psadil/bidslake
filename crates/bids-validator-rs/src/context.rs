//! BIDS validation context.
//!
//! For each file in the dataset, a `BidsContext` is constructed that contains
//! all the information needed to evaluate schema rules and checks. This mirrors
//! the `meta.context` structure defined in the BIDS schema.

use crate::associations::{BidsAssociations, CoordsystemsAssociation};
use crate::files::bval::{BFileMeta, parse_bfile_meta_from_file};
use crate::files::json::load_json;
use crate::files::nifti::NiftiHeader;
use crate::files::nifti::load_nifti_header;
use crate::files::tiff::{Ome, Tiff, parse_tiff};
use crate::files::tsv::TsvColumns;
use crate::filetree::{BidsFile, FileTree};
use crate::inheritance::SidecarOverride;
use crate::inheritance::read_sidecars;
use crate::issues::DatasetIssues;
use crate::schema::BidsSchema;
use bids_schema::context::FileContext;
use hed_validator_rs::schema::{SchemaCollection, load_schema_version};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

/// Dataset-level context, shared across all files.
#[derive(Debug, Clone, Serialize)]
pub struct DatasetContext {
    /// Contents of `/dataset_description.json`.
    pub dataset_description: Value,
    /// Reference to the file tree.
    #[serde(skip)] // FileTree probably shouldn't be serialized for expressions
    pub tree: FileTree,
    /// Set of ignored file patterns.
    pub ignored: Vec<String>,
    /// Data types present in the dataset.
    pub datatypes: Vec<String>,
    /// Modalities present in the dataset.
    pub modalities: Vec<String>,
    /// Subject information.
    pub subjects: SubjectsContext,
    /// e.g., "raw", "study", "derivatives"
    pub dataset_type: String,
    /// Pre-computed list of all file and directory paths in the tree (for expression evaluation).
    #[serde(skip)]
    pub tree_paths: Vec<String>,
    /// HED schemas built once for the whole dataset from `HEDVersion`, if present.
    /// `None` when `HEDVersion` is absent or the build failed (see `hed_schema_error`).
    #[serde(skip)]
    pub hed_schemas: Option<SchemaCollection>,
    /// Error message if a HED schema build was attempted (i.e. `HEDVersion` present) but failed.
    #[serde(skip)]
    pub hed_schema_error: Option<String>,
}

/// Subject information at the dataset level.
#[derive(Debug, Clone, Serialize)]
pub struct SubjectsContext {
    /// Subject directories found (e.g. ["sub-01", "sub-02"]).
    pub sub_dirs: Vec<String>,
    /// participant_id column from participants.tsv, if present.
    pub participant_id: Option<Vec<String>>,
}

/// The full validation context for a single file.
#[derive(Debug, Clone, Serialize)]
pub struct BidsContext {
    /// Path of the current file (relative to dataset root).
    pub path: String,
    /// File size in bytes.
    pub size: u64,
    /// Parsed entities (schema keys, e.g. "subject" -> "01").
    pub entities: HashMap<String, String>,
    /// Raw entities as they appear in filename (e.g. "sub" -> "01").
    pub raw_entities: HashMap<String, String>,
    /// The keys of the raw entities in the order they appear in the filename.
    pub entity_keys: Vec<String>,
    /// Datatype (e.g. "anat", "func").
    pub datatype: Option<String>,
    /// Suffix (e.g. "T1w", "bold").
    pub suffix: String,
    /// Extension (e.g. ".nii.gz", ".json").
    pub extension: String,
    /// The stem of the filename (everything before the extension).
    pub stem: String,
    /// Modality (e.g. "mri", "eeg").
    pub modality: Option<String>,
    /// Sidecar metadata accumulated via inheritance principle.
    pub sidecar: Value,
    /// Inherited sidecar keys overridden by a more-specific sidecar with a different value.
    #[serde(skip)]
    pub sidecar_overrides: Vec<SidecarOverride>,
    /// Metadata of associated files
    pub associations: BidsAssociations,
    /// TSV columns (if the file is a TSV).
    pub columns: TsvColumns,
    /// JSON file contents (if the file is a JSON).
    pub json: Value,
    /// GZIP header info (if the file is gzipped).
    pub gzip: Value,
    /// Some metadata when we're dealing with a bval/bvec
    pub bfile_meta: Option<BFileMeta>,
    /// NIfTI header info (if the file is a NIfTI).
    pub nifti_header: Option<NiftiHeader>,
    /// TIFF header info (if the file is a TIFF).
    pub tiff: Option<Tiff>,
    /// OME-XML physical sizes (if the file is an OME-TIFF).
    pub ome: Option<Ome>,
    /// Schema rules that matched this file during identification.
    pub filename_rules: Vec<String>,
    /// Whether this file was identified as a directory pseudofile.
    pub directory: bool,
    /// Datatypes present in the entire dataset.
    pub dataset_datatypes: Vec<String>,
}

impl DatasetContext {
    /// Build the dataset-level context from a file tree and schema.
    ///
    /// `hed_schema_dir` optionally points at a local `hed-standard/hed-schemas` checkout used
    /// to resolve HED schemas offline; when `None`, HED schemas fall back to the on-disk cache
    /// and a network fetch (mirroring hed-python).
    pub async fn new(
        tree: FileTree,
        schema: &BidsSchema,
        hed_schema_dir: Option<&std::path::Path>,
        issues: &mut DatasetIssues,
    ) -> Self {
        // Load dataset_description.json
        let mut dataset_description = match tree.find_file("/dataset_description.json") {
            Some(f) => load_json(f).await.ok(),
            None => None,
        }
        .unwrap_or_else(|| {
            issues.add_issue(
                "MISSING_DATASET_DESCRIPTION",
                "dataset_description.json is missing",
                crate::issues::Severity::Error,
                "/dataset_description.json",
                None,
                None,
            );
            Value::Object(serde_json::Map::new())
        });

        // Default `DatasetType` (derivative if `GeneratedBy` is present, else raw), matching the
        // TS validator's dataset-description setter. Because it always has a value, the
        // recommended-field check never reports it as missing.
        if let Value::Object(map) = &mut dataset_description
            && !map.contains_key("DatasetType")
        {
            let dt = if map.contains_key("GeneratedBy") {
                "derivative"
            } else {
                "raw"
            };
            map.insert("DatasetType".to_string(), Value::String(dt.to_string()));
        }

        // Collect subject directories
        let sub_dirs: Vec<String> = tree
            .directories
            .iter()
            .filter(|d| d.name.starts_with("sub-"))
            .map(|d| d.name.clone())
            .collect();

        // Load participants.tsv if present
        let participant_id = match tree.find_file("/participants.tsv") {
            Some(f) => crate::files::tsv::load_tsv_column(f, "participant_id")
                .await
                .ok(),
            None => None,
        };

        // Collect all datatypes present in the dataset
        // and pre-compute tree paths in a single walk
        let mut datatypes = Vec::new();
        let mut tree_paths = Vec::new();
        for file in tree.walk_files() {
            tree_paths.push(file.path.clone());
            if let Some(dt) = schema.index.datatype(&file.path)
                && !datatypes.iter().any(|d| d == dt)
            {
                datatypes.push(dt.to_string());
            }
        }
        for dir in tree.walk_directories() {
            tree_paths.push(dir.path.clone());
        }

        // Determine modalities from datatypes
        let mut modalities = Vec::new();
        for dt in &datatypes {
            if let Some(m) = schema.index.modality(dt)
                && !modalities.iter().any(|x| x == m)
            {
                modalities.push(m.to_string());
            }
        }

        let dataset_type = dataset_description
            .get("DatasetType")
            .and_then(|v| v.as_str())
            .unwrap_or("raw")
            .to_lowercase();

        // Build HED schemas once for the whole dataset, from `HEDVersion`.
        let (hed_schemas, hed_schema_error) =
            build_hed_schemas(&dataset_description, hed_schema_dir).await;

        DatasetContext {
            dataset_description,
            tree,
            ignored: Vec::new(),
            datatypes,
            modalities,
            subjects: SubjectsContext {
                sub_dirs,
                participant_id,
            },
            dataset_type,
            tree_paths,
            hed_schemas,
            hed_schema_error,
        }
    }
}

/// Column names for gzipped tabular files, taken from the sidecar `Columns` array (there is no
/// header row in the compressed file). Values are empty — only column presence is needed for
/// rule checks. Mirrors the TS validator's handling of `.tsv.gz`.
fn columns_from_sidecar(sidecar: &Value) -> TsvColumns {
    sidecar
        .get("Columns")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| (s.to_string(), Vec::new()))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse `HEDVersion` (string or array of strings) into loader specs.
fn hed_version_specs(dataset_description: &Value) -> Option<Vec<String>> {
    match dataset_description.get("HEDVersion") {
        Some(Value::String(s)) => Some(vec![s.clone()]),
        Some(Value::Array(items)) => {
            let specs: Vec<String> = items
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            if specs.is_empty() { None } else { Some(specs) }
        }
        _ => None,
    }
}

/// Load the HED schemas named by `HEDVersion`. Returns `(None, None)` when no version is
/// declared, `(Some(schemas), None)` on success, and `(None, Some(error))` on failure.
/// `load_schema_version` is blocking (cache/network I/O), so it runs on a blocking thread.
async fn build_hed_schemas(
    dataset_description: &Value,
    hed_schema_dir: Option<&std::path::Path>,
) -> (Option<SchemaCollection>, Option<String>) {
    let Some(specs) = hed_version_specs(dataset_description) else {
        return (None, None);
    };
    let schema_dir: Option<PathBuf> = hed_schema_dir.map(|p| p.to_path_buf());
    let result =
        tokio::task::spawn_blocking(move || load_schema_version(&specs, schema_dir.as_deref()))
            .await;

    match result {
        Ok(Ok(schemas)) => (Some(schemas), None),
        Ok(Err(e)) => (None, Some(e.to_string())),
        Err(join_err) => (
            None,
            Some(format!("HED schema build panicked: {}", join_err)),
        ),
    }
}

impl BidsContext {
    /// Build a context for a specific file.
    pub async fn new(file: &BidsFile, dataset: &DatasetContext, schema: &BidsSchema) -> Self {
        // The walk no longer `stat`s every entry to carry a size — ingestion never
        // reads it, and validation is the one consumer, once per file, here.
        let size = file.size_bytes();
        // Derived once: these same facts render the selector context that resolves this file's
        // associations further down, and then become this struct's fields.
        let file_ctx = FileContext::derive(file, &schema.index);

        // Read sidecar metadata via inheritance. A `.json` file has no sidecar of its own —
        // its contents are bound to `json`, not `sidecar`. Without this guard a sidecar's own
        // keys would satisfy `sidecar.*` selectors and the file would be reported alongside the
        // data file it describes (mirrors the TS validator's `loadSidecar` early return,
        // lib/bids-validator/src/schema/context.ts:200-204).
        let (sidecar, sidecar_overrides) = if file_ctx.parts.extension == ".json" {
            (Value::Object(Default::default()), Vec::new())
        } else {
            read_sidecars(file, &dataset.tree).await
        };

        // Read TSV columns if applicable. Gzipped tabular files have no header row: their
        // columns are declared in the sidecar `Columns` field. An empty file has no columns to
        // check — reporting on names we never read would be unsound, and `EMPTY_FILE` already
        // covers it (mirrors the TS validator, context.ts:270-276).
        let columns = if file_ctx.parts.extension == ".tsv" {
            crate::files::tsv::load_tsv_columns(file)
                .await
                .unwrap_or_default()
        } else if file_ctx.parts.extension == ".tsv.gz" && size != 0 {
            columns_from_sidecar(&sidecar)
        } else {
            HashMap::new()
        };

        // Read JSON contents if applicable
        let json = if file_ctx.parts.extension == ".json" {
            let mut j = load_json(file).await.unwrap_or(Value::Null);
            // For dataset_description.json, default `DatasetType` (matching the TS validator) so
            // the recommended-field check doesn't flag it — but only when the file actually
            // parsed as an object, so malformed JSON still surfaces via JSON_INVALID.
            if file.path == "/dataset_description.json"
                && let Value::Object(map) = &mut j
                && !map.contains_key("DatasetType")
            {
                let dt = if map.contains_key("GeneratedBy") {
                    "derivative"
                } else {
                    "raw"
                };
                map.insert("DatasetType".to_string(), Value::String(dt.to_string()));
            }
            j
        } else {
            Value::Null
        };

        // Read NIfTI header if applicable
        let nifti_header =
            if file_ctx.parts.extension == ".nii" || file_ctx.parts.extension == ".nii.gz" {
                load_nifti_header(file).await
            } else {
                None
            };

        // Read bfile meta if applicable
        let bfile_meta =
            if file_ctx.parts.extension == ".bval" || file_ctx.parts.extension == ".bvec" {
                parse_bfile_meta_from_file(file).await
            } else {
                None
            };

        // Read gzip header if applicable
        let gzip = if file.name.ends_with(".gz") {
            crate::files::gzip::parse_gzip_header(file)
                .await
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };

        // Read TIFF / OME-TIFF header if applicable
        let (tiff, ome) = if file_ctx.parts.extension.ends_with(".tif")
            || file_ctx.parts.extension.ends_with(".btf")
        {
            parse_tiff(file, file_ctx.parts.extension.starts_with(".ome")).await
        } else {
            (None, None)
        };

        // Resolve the schema's `meta.associations` for this file via the shared, pure resolver
        // in `bids-schema` (selector eval + tree search, no content reads), then build the typed
        // `BidsAssociations` on top (the content reads stay here). The selector context is
        // rendered from the facts derived at the top of this function, not derived a second time.
        let ctx_value = file_ctx.to_selector_value();
        let hits = bids_schema::associations::resolve_associations(
            schema.associations(),
            file,
            &dataset.tree,
            &ctx_value,
        );

        let mut associations = BidsAssociations::default();
        // Multi-file associations: only `coordsystems` is wired into the typed context today
        // (preserves prior behavior — `electrodes` is also multi-file but stays unpopulated).
        let coordsystem_files: Vec<BidsFile> = hits
            .iter()
            .filter(|h| h.multi && h.name == "coordsystems")
            .map(|h| h.target_file.clone())
            .collect();
        if !coordsystem_files.is_empty() {
            associations.coordsystems =
                Some(CoordsystemsAssociation::from_files(&coordsystem_files));
        }
        // Single-file associations: first hit per name → typed load (reads file content).
        let mut seen = std::collections::HashSet::new();
        for h in hits.iter().filter(|h| !h.multi) {
            if seen.insert(h.name.clone()) {
                associations
                    .load(&h.name, &h.target_file, &dataset.tree)
                    .await;
            }
        }

        BidsContext {
            path: file_ctx.path,
            size,
            raw_entities: file_ctx.parts.entities,
            entities: file_ctx.entities,
            entity_keys: file_ctx.parts.entity_keys,
            datatype: file_ctx.datatype,
            suffix: file_ctx.parts.suffix,
            extension: file_ctx.parts.extension,
            stem: file_ctx.parts.stem,
            modality: file_ctx.modality,
            sidecar,
            sidecar_overrides,
            associations,
            columns,
            json,
            gzip,
            bfile_meta,
            nifti_header,
            tiff,
            ome,
            filename_rules: Vec::new(),
            directory: false,
            dataset_datatypes: dataset.datatypes.clone(),
        }
    }

    /// Build the per-file bindings for expression evaluation: `path`, `suffix`,
    /// `sidecar`, `associations`, `nifti_header`, and the rest of this file's context.
    ///
    /// These are the `file`-scope bindings of an [`crate::expression::EvalContext`]. The
    /// dataset-wide bindings (`dataset` / `schema` / `subject`) come from
    /// [`DatasetContext::dataset_context_value`] and its siblings, which build them once
    /// per dataset.
    pub fn to_file_value(&self) -> Value {
        let entities_val: Value = serde_json::to_value(&self.entities).unwrap_or(Value::Null);

        let columns_val: Value = {
            let mut map = serde_json::Map::new();
            for (key, values) in &self.columns {
                map.insert(
                    key.clone(),
                    Value::Array(values.iter().map(|v| Value::String(v.clone())).collect()),
                );
            }
            Value::Object(map)
        };

        serde_json::json!({
            "path": self.path,
            "size": self.size,
            "entities": entities_val,
            "datatype": self.datatype,
            "suffix": self.suffix,
            "extension": self.extension,
            "stem": self.stem,
            "modality": self.modality,
            "sidecar": self.sidecar,
            "associations": self.associations,
            "columns": columns_val,
            "json": self.json,
            "gzip": self.gzip,
            "nifti_header": self.nifti_header,
            "tiff": self.tiff,
            "ome": self.ome,
        })
    }
}

impl DatasetContext {
    /// Build the `dataset` binding shared by every file: `dataset.tree`,
    /// `dataset.datatypes`, `dataset.modalities`, `dataset.subjects`, and the dataset
    /// description. The same for all files, so it is built once per dataset.
    ///
    /// `dataset.datatypes` and `dataset.modalities` are populated here (the reference TS
    /// validator leaves them empty), so rules gated on them — e.g.
    /// `intersects(dataset.modalities, ["pet"])` → PETMRISequenceSpecifics — are enforced.
    /// This is a deliberate, stricter-than-TS reading of the schema.
    pub fn dataset_context_value(&self) -> Value {
        let subjects_val = serde_json::json!({
            "sub_dirs": self.subjects.sub_dirs,
            "participant_id": self.subjects.participant_id,
        });
        serde_json::json!({
            "dataset_description": self.dataset_description,
            "tree": self.tree_paths,
            "ignored": self.ignored,
            "datatypes": self.datatypes,
            "modalities": self.modalities,
            "subjects": subjects_val,
        })
    }

    /// Build the `schema` binding shared by every file. Expressions reach only
    /// `schema.meta.*` and `schema.objects.enums.*`, so the binding carries just those
    /// subtrees rather than the whole (~600 KB) schema. Built once per dataset.
    pub fn schema_context_value(&self, schema: &BidsSchema) -> Value {
        serde_json::json!({
            "meta": schema.raw.get("meta").cloned().unwrap_or(Value::Null),
            "objects": {
                "enums": schema.objects().get("enums").cloned().unwrap_or(Value::Null),
            },
        })
    }

    /// Build the `subject` binding. A deliberate stub — `ses_dirs` is empty and `session_id`
    /// is null for every file — because **no expression in the schema reads it**.
    ///
    /// `meta.context` declares the scope, so the binding exists to satisfy that shape, but a
    /// sweep of all ~1200 selector and check expressions finds only `dataset.subjects.*` and
    /// `entities.subject`; nothing references the `subject` scope itself, and none of the
    /// `meta.expression_tests` do either. Populating it would mean scanning each `sub-*`
    /// directory and reading one `sessions.tsv` per subject on every validation, to answer a
    /// question nothing asks. Compare `dataset.datatypes`/`modalities`, which this validator
    /// *does* populate where the reference leaves them empty — that was worth it because
    /// `intersects(dataset.modalities, ["pet"])` is a real rule.
    ///
    /// The hazard is that a stub answers `[]` where a real binding would answer something, and
    /// a selector cannot tell those apart. So the day the schema starts selecting on `subject`,
    /// this must be implemented rather than silently evaluated against an empty stub —
    /// `tests::no_schema_expression_reads_the_subject_scope` fails when that day comes.
    ///
    /// Implementing it means per-file state, not this dataset-wide value: `subject` is the
    /// *current file's* subject. The cheap shape is to resolve once per subject (there are
    /// few) and hand each file the entry for its own.
    pub fn subject_context_value(&self) -> Value {
        serde_json::json!({
            "sessions": {
                "ses_dirs": [],
                "session_id": null,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    /// Whether `expr` reads the top-level `subject` scope.
    ///
    /// Deliberately narrow, because the schema as it stands contains three lookalikes and a
    /// bare substring search would leave the tripwire permanently red:
    ///   - `dataset.subjects.sub_dirs` — a different scope, and a different word;
    ///   - `entities.subject` — the file's subject *entity*, not the subject scope;
    ///   - `exists(sidecar.IntendedFor, "subject")` — a quoted string naming the `subject`
    ///     path-resolution rule (see `expression.rs`'s `exists`), not an identifier at all.
    ///
    /// So a match is `subject` as a whole identifier, nothing preceding it with a `.`, and not
    /// wrapped in quotes.
    fn reads_subject_scope(expr: &str) -> bool {
        let bytes = expr.as_bytes();
        let mut from = 0;
        while let Some(offset) = expr[from..].find("subject") {
            let start = from + offset;
            let end = start + "subject".len();
            let is_start_of_identifier = start == 0 || {
                let prev = bytes[start - 1] as char;
                !prev.is_alphanumeric() && prev != '_' && prev != '.'
            };
            let is_end_of_identifier = end >= bytes.len() || {
                let next = bytes[end] as char;
                !next.is_alphanumeric() && next != '_'
            };
            let is_quoted_literal = start > 0
                && end < bytes.len()
                && matches!(bytes[start - 1] as char, '"' | '\'')
                && matches!(bytes[end] as char, '"' | '\'');
            if is_start_of_identifier && is_end_of_identifier && !is_quoted_literal {
                return true;
            }
            from = end;
        }
        false
    }

    /// Every expression string in the schema: the `selectors` and `checks` arrays of every
    /// rule, wherever they appear, plus the `meta.expression_tests` expressions.
    fn schema_expressions(schema: &Value) -> Vec<String> {
        fn walk(node: &Value, out: &mut Vec<String>) {
            match node {
                Value::Object(map) => {
                    for (key, value) in map {
                        if matches!(key.as_str(), "selectors" | "checks")
                            && let Some(items) = value.as_array()
                        {
                            out.extend(items.iter().filter_map(|v| v.as_str()).map(String::from));
                        } else {
                            walk(value, out);
                        }
                    }
                }
                Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(schema, &mut out);
        out.extend(
            schema
                .get("meta")
                .and_then(|m| m.get("expression_tests"))
                .and_then(|t| t.as_array())
                .into_iter()
                .flatten()
                .filter_map(|t| t.get("expression").and_then(|e| e.as_str()))
                .map(String::from),
        );
        out
    }

    /// `DatasetContext::subject_context_value` is a stub, and is only sound while nothing
    /// reads it. This is the tripwire: when a schema update starts selecting on `subject`,
    /// implement the binding rather than let the selector evaluate against an empty stub —
    /// which it would do silently, since a selector cannot tell "no sessions" from "not
    /// populated".
    #[test]
    fn no_schema_expression_reads_the_subject_scope() {
        let schema: Value = serde_json::from_str(bids_schema::SCHEMA_JSON).unwrap();
        let expressions = schema_expressions(&schema);

        // Guard the guard: if the walk stops finding expressions, it would pass vacuously.
        assert!(
            expressions.len() > 1000,
            "expected the schema's ~1200 expressions, found {} — the walk is looking in the \
             wrong place, not the schema shrinking",
            expressions.len()
        );
        assert!(
            expressions.iter().any(|e| e.contains("dataset.subjects")),
            "sanity: `dataset.subjects` should be present and must NOT count as a hit"
        );

        let readers: Vec<&String> = expressions
            .iter()
            .filter(|e| reads_subject_scope(e))
            .collect();
        assert!(
            readers.is_empty(),
            "the schema now reads the `subject` scope, which is still a stub \
             (`ses_dirs: []`, `session_id: null`) — populate it per subject before these \
             evaluate: {readers:#?}"
        );
    }

    #[test]
    fn reads_subject_scope_distinguishes_the_scope_from_its_lookalikes() {
        assert!(reads_subject_scope("length(subject.sessions.ses_dirs) > 0"));
        assert!(reads_subject_scope("subject.sessions.session_id"));
        assert!(reads_subject_scope("type(subject) != 'null'"));
        // The three forms that really are in the schema, none of which is the scope.
        assert!(!reads_subject_scope(
            "length(dataset.subjects.sub_dirs) > 0"
        ));
        assert!(!reads_subject_scope("entities.subject != \"emptyroom\""));
        assert!(!reads_subject_scope(
            "exists(sidecar.IntendedFor, \"subject\") == 1"
        ));
    }
}
