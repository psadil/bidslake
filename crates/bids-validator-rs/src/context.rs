//! BIDS validation context.
//!
//! For each file in the dataset, a `BidsContext` is constructed that contains
//! all the information needed to evaluate schema rules and checks. This mirrors
//! the `meta.context` structure defined in the BIDS schema.

use crate::associations::{BidsAssociations, CoordsystemsAssociation};
use bids_schema::datatypes::{find_datatype, find_modality};
use crate::entities::{read_entities, resolve_entities};
use crate::files::bval::{BFileMeta, parse_bfile_meta_from_file};
use crate::files::json::load_json;
use crate::files::nifti::NiftiHeader;
use crate::files::nifti::load_nifti_header;
use crate::files::tiff::{Ome, Tiff, parse_tiff};
use crate::files::tsv::TsvColumns;
use crate::filetree::{BidsFile, FileTree};
use crate::inheritance::read_sidecars;
use crate::inheritance::SidecarOverride;
use crate::issues::DatasetIssues;
use crate::schema::BidsSchema;
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

/// Per-subject context.
#[derive(Debug, Clone, Serialize)]
pub struct SubjectContext {
    /// Session information for this subject.
    pub sessions: SessionsContext,
}

/// Session information for a subject.
#[derive(Debug, Clone, Serialize)]
pub struct SessionsContext {
    /// Session directories found (e.g. ["ses-01", "ses-02"]).
    pub ses_dirs: Vec<String>,
    /// session_id column from sessions.tsv, if present.
    pub session_id: Option<Vec<String>>,
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
            if let Some(dt) = find_datatype(&file.path, &schema.raw)
                && !datatypes.contains(&dt)
            {
                datatypes.push(dt);
            }
        }
        for dir in tree.walk_directories() {
            tree_paths.push(dir.path.clone());
        }

        // Determine modalities from datatypes
        let mut modalities = Vec::new();
        for dt in &datatypes {
            if let Some(m) = find_modality(dt, &schema.raw)
                && !modalities.contains(&m)
            {
                modalities.push(m);
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
        let file_parts = read_entities(&file.name);
        let entities = resolve_entities(&file_parts.entities, &schema.entity_name_to_key);
        let datatype = find_datatype(&file.path, &schema.raw);
        let modality = datatype.as_ref().and_then(|dt| find_modality(dt, &schema.raw));

        // Read sidecar metadata via inheritance. A `.json` file has no sidecar of its own —
        // its contents are bound to `json`, not `sidecar`. Without this guard a sidecar's own
        // keys would satisfy `sidecar.*` selectors and the file would be reported alongside the
        // data file it describes (mirrors the TS validator's `loadSidecar` early return,
        // lib/bids-validator/src/schema/context.ts:200-204).
        let (sidecar, sidecar_overrides) = if file_parts.extension == ".json" {
            (Value::Object(Default::default()), Vec::new())
        } else {
            read_sidecars(file, &dataset.tree).await
        };

        // Read TSV columns if applicable. Gzipped tabular files have no header row: their
        // columns are declared in the sidecar `Columns` field. An empty file has no columns to
        // check — reporting on names we never read would be unsound, and `EMPTY_FILE` already
        // covers it (mirrors the TS validator, context.ts:270-276).
        let columns = if file_parts.extension == ".tsv" {
            crate::files::tsv::load_tsv_columns(file)
                .await
                .unwrap_or_default()
        } else if file_parts.extension == ".tsv.gz" && file.size != 0 {
            columns_from_sidecar(&sidecar)
        } else {
            HashMap::new()
        };

        // Read JSON contents if applicable
        let json = if file_parts.extension == ".json" {
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
        let nifti_header = if file_parts.extension == ".nii" || file_parts.extension == ".nii.gz" {
            load_nifti_header(file).await
        } else {
            None
        };

        // Read bfile meta if applicable
        let bfile_meta = if file_parts.extension == ".bval" || file_parts.extension == ".bvec" {
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
        let (tiff, ome) =
            if file_parts.extension.ends_with(".tif") || file_parts.extension.ends_with(".btf") {
                parse_tiff(file, file_parts.extension.starts_with(".ome")).await
            } else {
                (None, None)
            };

        // Build subject context
        let subject_dir = entities.get("subject").map(|s| format!("sub-{}", s));
        let _subject = subject_dir
            .as_ref()
            .and_then(|sd| dataset.tree.find_dir(sd));

        // Resolve the schema's `meta.associations` for this file via the shared, pure resolver
        // in `bids-schema` (selector eval + tree search, no content reads), then build the typed
        // `BidsAssociations` on top (the content reads stay here).
        let ctx_value = bids_schema::context::build_file_context(file, &schema.raw);
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
            associations.coordsystems = Some(CoordsystemsAssociation::from_files(&coordsystem_files));
        }
        // Single-file associations: first hit per name → typed load (reads file content).
        let mut seen = std::collections::HashSet::new();
        for h in hits.iter().filter(|h| !h.multi) {
            if seen.insert(h.name.clone()) {
                associations.load(&h.name, &h.target_file, &dataset.tree).await;
            }
        }

        BidsContext {
            path: file.path.clone(),
            size: file.size,
            raw_entities: file_parts.entities,
            entities,
            entity_keys: file_parts.entity_keys,
            datatype,
            suffix: file_parts.suffix,
            extension: file_parts.extension,
            stem: file_parts.stem,
            modality,
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

    /// Build the `subject` binding shared by every file. Currently a stub: sessions are
    /// not yet populated, so `subject.sessions.ses_dirs` is empty and `session_id` is null.
    pub fn subject_context_value(&self) -> Value {
        serde_json::json!({
            "sessions": {
                "ses_dirs": [],
                "session_id": null,
            }
        })
    }
}
