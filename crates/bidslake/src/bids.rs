//! The ingestion pipeline: BIDS dataset → DuckDB rows.
//!
//! [`BidsParser::parse`] runs the whole ingest against a [`BidsFileSystem`]
//! (local or S3), driven by a [`Schema`]. The steps:
//!
//! 1. **Walk & categorize.** List every file and bucket it: `dataset_description.json`,
//!    `participants.tsv`, `sessions.tsv`, and everything else.
//! 2. **Resolve the dataset id** from the root `dataset_description.json` (nested
//!    ones under `derivatives/` are sorted shallowest-first so the root wins).
//! 3. **Process in passes** — dataset_description, then participants, then
//!    sessions, then all other files — via `process_file`. Filename
//!    entities are parsed here (`sub-01` → `sub`), participants/sessions are
//!    implicitly created (deduped in-memory via `seen_participants`/
//!    `seen_sessions`), and TSV/JSON/bval-bvec files are dispatched to handlers.
//! 4. **Flush deferred work**: `IntendedFor` file associations, parsed
//!    `.bval`/`.bvec` diffusion arrays, and the `scans` table (every imaging file
//!    gets a row, whether or not a `*_scans.tsv` listed it).
//! 5. **Apply BIDS inheritance** to build `sidecars`: for each imaging file, the
//!    applicable dataset-/subject-level JSON sidecars are merged (more-specific
//!    wins), indexed by `(dataset_id, suffix)` to keep matching near-linear.
//!
//! Two performance notes worth knowing: `events` rows (by far the highest row
//! count) are written with the DuckDB `Appender` (bulk path, bypassing SQL
//! planning), and the entire parse runs inside a single transaction (opened by
//! the caller in `main`), so it commits atomically.

use crate::db::{BidsDb, FileAssociation};
use crate::fs::BidsFileSystem;
use crate::schema::Schema;
use crate::schema::dynamic::quote_ident;
use crate::schema::tabular::{ColumnSpec, FileContext, RowIdentity, TableSpec};
use anyhow::Result;
use duckdb::Connection;
use bids_core::entities::read_entities;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub struct BidsParser {
    fs: Box<dyn BidsFileSystem>,
    dataset_id: Option<String>,
    ignore_set: Gitignore,
    pending_associations: Vec<FileAssociation>,
    pending_diffusion: HashMap<String, PendingDiffusion>,
    schema: Schema,
    imaging_files: Vec<ImagingFile>,
    has_scans_tsv: bool,
    sidecars: Vec<SidecarInfo>,
    /// `dataset_description.json`'s `DatasetType` (`raw`/`derivative`), needed to
    /// evaluate the `derivatives.*` tabular selectors (e.g. `dseg` lookups).
    dataset_type: Option<String>,
    /// The BIDS datatype directory names, cached from the schema for classifying
    /// each file's datatype from its path.
    datatypes: HashSet<String>,
    /// Every datatype directory in the dataset, as `(dir_path, datatype)` — e.g.
    /// (`sub-01/ses-meg/meg`, `meg`). Used to infer the datatype of a tabular file
    /// that sits *above* a datatype directory (a session- or subject-level
    /// `channels.tsv` that applies to the `meg/` runs below it) so it can still be
    /// routed.
    datatype_dirs: HashSet<(String, String)>,
    /// Headerless recordings, ingested in the flush once all sidecars are known.
    pending_recordings: Vec<PendingRecording>,
    /// A throwaway in-memory connection used to pre-flight `read_csv` on a file
    /// before the real INSERT. A malformed TSV (empty, truncated, or a non-gzip
    /// git-annex placeholder with a `.gz` name) makes `read_csv` error, and inside
    /// the ingest transaction that error would poison every later statement. Testing
    /// the read here first — off the main connection — keeps a bad file from
    /// aborting the whole dataset's ingest.
    validator: Connection,
    // Track which implicit participants/sessions we've already inserted so the
    // per-file loop doesn't re-issue an insert for every file of a subject.
    seen_participants: HashSet<(String, String)>, // (dataset_id, participant_id)
    seen_sessions: HashSet<(String, String, String)>, // (dataset_id, participant_id, session_id)
}

#[derive(Clone)]
struct ImagingFile {
    dataset_id: String,
    file_path: String,
}

struct PendingDiffusion {
    dataset_id: String,
    bval: Option<Vec<f64>>,
    bvec_x: Option<Vec<f64>>,
    bvec_y: Option<Vec<f64>>,
    bvec_z: Option<Vec<f64>>,
}

struct SidecarInfo {
    dataset_id: String,
    file_path: String, // Relative path in dataset
    entities: HashMap<String, String>,
    suffix: String,
    content: Value,
}

/// A headerless continuous recording (`*_physio.tsv.gz`, `*_stim.tsv.gz`,
/// `*_physioevents.tsv.gz`, `*_motion.tsv`) deferred to the flush phase: its column
/// names come from the merged sidecar's `Columns` (or, for motion, the associated
/// `_channels.tsv`), which is only known once every sidecar has been collected.
struct PendingRecording {
    dataset_id: String,
    rel_path: String,
    suffix: String,
    entities: HashMap<String, String>,
}

impl BidsParser {
    pub fn new(fs: Box<dyn BidsFileSystem>, dataset_id: Option<String>, schema: Schema) -> Self {
        let datatypes = schema.datatypes().into_iter().collect();
        Self {
            fs,
            dataset_id,
            ignore_set: Gitignore::empty(),
            pending_associations: Vec::new(),
            pending_diffusion: HashMap::new(),
            schema,
            imaging_files: Vec::new(),
            has_scans_tsv: false,
            sidecars: Vec::new(),
            dataset_type: None,
            datatypes,
            datatype_dirs: HashSet::new(),
            pending_recordings: Vec::new(),
            validator: Connection::open_in_memory()
                .expect("open in-memory validator connection"),
            seen_participants: HashSet::new(),
            seen_sessions: HashSet::new(),
        }
    }

    /// Whether DuckDB can read this `read_csv(...)` call — tested on the throwaway
    /// [`Self::validator`] connection so a parse error can't poison the main ingest
    /// transaction. A readable-but-empty file returns `true` (it just yields no
    /// rows); an unreadable one (bad gzip, malformed) returns `false`.
    fn read_csv_ok(&self, read_csv_from: &str) -> bool {
        let sql = format!("SELECT 1 FROM {read_csv_from} LIMIT 1");
        match self.validator.prepare(&sql) {
            Ok(mut stmt) => stmt.query([]).map(|_| ()).is_ok(),
            Err(_) => false,
        }
    }

    /// Column names of a header-bearing file, sniffed on the validator connection.
    /// `None` if the file can't be read (so it is skipped, not ingested).
    fn sniff_columns(&self, read_csv_from: &str) -> Option<Vec<String>> {
        let sql = format!("DESCRIBE SELECT * FROM {read_csv_from}");
        let mut stmt = self.validator.prepare(&sql).ok()?;
        let cols = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .ok()?
            .filter_map(|r| r.ok())
            .collect();
        Some(cols)
    }

    pub async fn parse(&mut self, db: &BidsDb) -> Result<()> {
        // Load .bidsignore patterns before parsing
        self.load_bidsignore().await?;

        // Collect all file paths first
        let mut dataset_description: Vec<std::path::PathBuf> = Vec::new();
        let mut participants_tsv: Vec<std::path::PathBuf> = Vec::new();
        let mut sessions_tsv: Vec<std::path::PathBuf> = Vec::new();
        let mut other_files: Vec<std::path::PathBuf> = Vec::new();

        let files: Vec<std::path::PathBuf> = self.fs.walk().await?;

        for path in files {
            let file_name = path.file_name().unwrap().to_str().unwrap();

            // Skip dotfiles
            if file_name.starts_with('.') {
                continue;
            }

            // Skip files matching .bidsignore patterns. `matched_path_or_any_parents`
            // applies gitignore semantics — crucially it also tests parent dirs, so a
            // directory pattern like `logs/` excludes everything beneath it.
            if self
                .ignore_set
                .matched_path_or_any_parents(&path, false)
                .is_ignore()
            {
                continue;
            }

            // Record datatype directories so a tabular file sitting above one (a
            // session- or subject-level channels.tsv) can still be routed. For a
            // path like `sub-01/ses-meg/meg/…`, note the datatype dir
            // (`sub-01/ses-meg/meg`, `meg`).
            let comps: Vec<&str> = path.iter().filter_map(|c| c.to_str()).collect();
            for (i, comp) in comps.iter().enumerate() {
                if self.datatypes.contains(*comp) {
                    self.datatype_dirs
                        .insert((comps[..=i].join("/"), comp.to_string()));
                }
            }

            // Categorize files
            if file_name == "dataset_description.json" {
                dataset_description.push(path);
            } else if file_name == "participants.tsv" {
                participants_tsv.push(path);
            } else if file_name == "sessions.tsv" {
                sessions_tsv.push(path);
            } else {
                other_files.push(path);
            }
        }

        // Datasets can carry nested dataset_description.json files (e.g. under
        // derivatives/). Sort shallowest-first so the dataset root wins when we
        // resolve the dataset_id and insert the description.
        dataset_description.sort_by_key(|p| p.components().count());

        // Pass 0: Process dataset_description.json first to resolve dataset_id
        for path in &dataset_description {
            if self.dataset_id.is_none() {
                let content = self.fs.read_to_string(path).await?;
                match serde_json::from_str::<Value>(&content) {
                    Ok(dataset_desc) => {
                        if let Some(name) = dataset_desc.get("Name").and_then(|v| v.as_str()) {
                            println!("Using dataset name from dataset_description.json: {}", name);
                            self.dataset_id = Some(name.to_string());
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "Warning: skipping unparseable dataset_description.json at {}: {}",
                            path.display(),
                            e
                        );
                    }
                }
            }
        }

        // If still no dataset_id, use root name or default
        if self.dataset_id.is_none() {
            // For S3, root might be s3://bucket/prefix/
            // We can try to extract the last part of the prefix
            let root = self.fs.root();
            let dir_name = if root.starts_with("s3://") {
                root.trim_end_matches('/')
                    .split('/')
                    .next_back()
                    .unwrap_or("unknown")
                    .to_string()
            } else {
                Path::new(&root)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string()
            };

            println!("Using directory/prefix name as dataset_id: {}", dir_name);
            self.dataset_id = Some(dir_name);
        }

        let dataset_id = self.dataset_id.as_ref().unwrap().clone();

        // Process dataset_description.json again to insert it
        for path in dataset_description {
            self.process_file(&path, db, &dataset_id).await?;
        }

        // Pass 1: Process participants.tsv files
        for path in participants_tsv {
            self.process_file(&path, db, &dataset_id).await?;
        }

        // Pass 2: Process sessions.tsv files
        for path in sessions_tsv {
            self.process_file(&path, db, &dataset_id).await?;
        }

        // Pass 3: Process all other files
        for path in other_files {
            self.process_file(&path, db, &dataset_id).await?;
        }

        // Process file associations after all files are indexed
        let mut associations = self.pending_associations.clone();

        // Detect sbref associations
        associations.extend(self.detect_sbref_associations()?);

        // Insert all associations
        for assoc in associations {
            if let Err(e) = db.insert_file_association(&assoc) {
                eprintln!("Failed to insert file association {:?}: {}", assoc, e);
            }
        }

        // Insert pending diffusion data — only when we have both bval and bvec.
        for (nifti_path, diff) in &self.pending_diffusion {
            if let (Some(bval), Some(bvec_x), Some(bvec_y), Some(bvec_z)) =
                (&diff.bval, &diff.bvec_x, &diff.bvec_y, &diff.bvec_z)
                && let Err(e) =
                    db.insert_diffusion(&diff.dataset_id, nifti_path, bval, bvec_x, bvec_y, bvec_z)
            {
                eprintln!("Failed to insert diffusion data for {}: {}", nifti_path, e);
            }
        }

        // Ensure every discovered imaging file has a scans row. scans.tsv rows
        // (inserted earlier, with richer metadata) win via insert-if-not-exists;
        // this adds any imaging files a scans.tsv omitted (derivatives, files not
        // listed) so their sidecars/associations have a referent. Running it
        // unconditionally is what keeps the sidecars FK satisfied.
        if !self.imaging_files.is_empty() {
            println!(
                "Populating scans table with {} imaging files ({}scans.tsv present).",
                self.imaging_files.len(),
                if self.has_scans_tsv { "" } else { "no " }
            );
            for img_file in &self.imaging_files {
                let mut scan_data = serde_json::Map::new();
                scan_data.insert(
                    "dataset_id".to_string(),
                    Value::String(img_file.dataset_id.clone()),
                );
                // file_path contains the full relative path (e.g., sub-01/anat/sub-01_T1w.nii.gz)
                scan_data.insert(
                    "file_path".to_string(),
                    Value::String(img_file.file_path.clone()),
                );

                // Extract filename from file_path for the 'filename' field
                if let Some(filename) = img_file.file_path.split('/').next_back() {
                    scan_data.insert("filename".to_string(), Value::String(filename.to_string()));
                }

                // Build other_data without file_path and dataset_id
                let mut other_data = serde_json::Map::new();
                // Only include filename in other_data (exclude file_path and dataset_id)
                if let Some(filename) = img_file.file_path.split('/').next_back() {
                    other_data.insert("filename".to_string(), Value::String(filename.to_string()));
                }
                scan_data.insert("other_data".to_string(), Value::Object(other_data));

                if let Err(e) = db.insert(&self.schema, "scans", &Value::Object(scan_data)) {
                    eprintln!(
                        "Failed to insert auto-generated scan entry for {}: {}",
                        img_file.file_path, e
                    );
                }
            }
        }

        // Apply BIDS inheritance to populate the `sidecars` table. On local disk we
        // reuse bids-core's tree-based resolver (nearest-wins, matching the BIDS spec
        // / reference validator); other backends (S3) fall back to the in-memory
        // resolver over the sidecars collected during the walk.
        println!(
            "Applying BIDS inheritance for {} imaging files...",
            self.imaging_files.len()
        );
        if let Some(tree) = self.fs.file_tree() {
            self.apply_inheritance_tree(db, &tree).await;
        } else {
            self.apply_inheritance_collected(db);
        }

        // Ingest the deferred headerless recordings now that every sidecar and
        // channels file is available (their columns come from those).
        self.flush_recordings(db).await?;

        Ok(())
    }

    /// BIDS inheritance for the `sidecars` table using the local [`bids_core`]
    /// `FileTree` and `read_sidecars` (nearest-wins, matching the BIDS spec /
    /// reference validator). Local-disk backends only — it needs the tree and reads
    /// each applicable sidecar from disk.
    async fn apply_inheritance_tree(&self, db: &BidsDb, tree: &bids_core::filetree::FileTree) {
        for img_file in &self.imaging_files {
            // Tree paths carry a leading `/`; our `file_path` does not.
            let tree_path = format!("/{}", img_file.file_path);
            let Some(bids_file) = tree.find_file(&tree_path) else {
                continue;
            };
            let (merged, _overrides) =
                bids_core::inheritance::read_sidecars(bids_file, tree).await;
            if let Value::Object(map) = merged {
                self.insert_sidecar_row(db, &img_file.dataset_id, &img_file.file_path, map);
            }
        }
    }

    /// Insert one merged-sidecar row: the merged metadata as `other_data`, plus each
    /// field also flattened to its own column (schema-known fields get typed
    /// columns). A no-op when `merged` is empty.
    fn insert_sidecar_row(
        &self,
        db: &BidsDb,
        dataset_id: &str,
        file_path: &str,
        merged: serde_json::Map<String, Value>,
    ) {
        if merged.is_empty() {
            return;
        }
        let mut sidecar_entry = serde_json::Map::new();
        sidecar_entry.insert(
            "dataset_id".to_string(),
            Value::String(dataset_id.to_string()),
        );
        sidecar_entry.insert("file_path".to_string(), Value::String(file_path.to_string()));
        sidecar_entry.insert("other_data".to_string(), Value::Object(merged.clone()));
        // Also flatten metadata into top-level fields for known columns.
        for (k, v) in &merged {
            sidecar_entry.insert(k.clone(), v.clone());
        }
        if let Err(e) = db.insert(&self.schema, "sidecars", &Value::Object(sidecar_entry)) {
            eprintln!("Failed to insert sidecar entry for {}: {}", file_path, e);
        }
    }

    /// In-memory BIDS inheritance for the `sidecars` table over the JSON sidecars
    /// collected during the walk, keyed by `(dataset_id, suffix)` with a
    /// directory-prefix + entity-subset match. Used by non-local backends (S3),
    /// which have no `FileTree`.
    fn apply_inheritance_collected(&self, db: &BidsDb) {
        // A sidecar can only apply to an imaging file of the same dataset and
        // suffix, so index sidecars by (dataset_id, suffix) and precompute each
        // one's parent directory. This turns inheritance matching from
        // O(imaging_files x all_sidecars) into O(imaging_files x same-suffix
        // sidecars) — the dominant ingestion cost for sidecar-heavy datasets.
        let sidecar_dirs: Vec<&Path> = self
            .sidecars
            .iter()
            .map(|s| {
                Path::new(&s.file_path)
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
            })
            .collect();
        let mut sidecar_index: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
        for (i, s) in self.sidecars.iter().enumerate() {
            sidecar_index
                .entry((s.dataset_id.as_str(), s.suffix.as_str()))
                .or_default()
                .push(i);
        }

        for img_file in &self.imaging_files {
            let mut merged_metadata = serde_json::Map::new();

            // Extract entities and suffix from imaging file
            let file_name = img_file.file_path.split('/').next_back().unwrap();
            let img_parts = read_entities(file_name);
            let img_entities = img_parts.entities;
            let img_suffix = img_parts.suffix;

            // Candidates already share dataset_id + suffix; keep those whose
            // directory is a prefix of the image's and whose entities are a
            // subset of the image's.
            let img_dir = Path::new(&img_file.file_path)
                .parent()
                .unwrap_or_else(|| Path::new(""));
            let mut applicable: Vec<usize> = Vec::new();
            if let Some(candidates) =
                sidecar_index.get(&(img_file.dataset_id.as_str(), img_suffix.as_str()))
            {
                for &i in candidates {
                    if !img_dir.starts_with(sidecar_dirs[i]) {
                        continue;
                    }
                    let entities = &self.sidecars[i].entities;
                    if entities
                        .iter()
                        .all(|(key, value)| img_entities.get(key) == Some(value))
                    {
                        applicable.push(i);
                    }
                }
            }

            // Sort by specificity (number of entities) - least specific first for merging
            // BIDS Principle of Inheritance: values from more specific files override less specific ones.
            // So we want to merge from top (least specific) to bottom (most specific).
            applicable.sort_by_key(|&i| self.sidecars[i].entities.len());

            // Merge metadata
            for &i in &applicable {
                if let Value::Object(map) = &self.sidecars[i].content {
                    for (k, v) in map {
                        merged_metadata.insert(k.clone(), v.clone());
                    }
                }
            }

            self.insert_sidecar_row(
                db,
                &img_file.dataset_id,
                &img_file.file_path,
                merged_metadata,
            );
        }
    }

    async fn process_file(&mut self, path: &Path, db: &BidsDb, dataset_id: &str) -> Result<()> {
        let file_name = path.file_name().unwrap().to_str().unwrap();

        // path from walk() is already relative to dataset root
        let rel_path = path.to_str().unwrap();

        if file_name.starts_with('.') {
            return Ok(());
        }

        // Parse BIDS filename entities via the shared bids-core parser.
        let entities = read_entities(file_name).entities;

        let participant_id = entities.get("sub").map(|s| format!("sub-{}", s));
        let session_id = entities.get("ses").map(|s| format!("ses-{}", s));

        // Auto-create participant/session if they don't exist (implicit).
        // Only hit the DB the first time we see each one; every file of a subject
        // would otherwise re-issue an identical (guarded, no-op) insert.
        if let Some(ref pid) = participant_id {
            if self
                .seen_participants
                .insert((dataset_id.to_string(), pid.clone()))
            {
                let mut participant_data = serde_json::Map::new();
                participant_data.insert(
                    "dataset_id".to_string(),
                    Value::String(dataset_id.to_string()),
                );
                participant_data.insert("participant_id".to_string(), Value::String(pid.clone()));

                // Ignore errors - participant might already exist (e.g. from participants.tsv)
                let _ = db.insert(
                    &self.schema,
                    "participants",
                    &Value::Object(participant_data),
                );
            }

            if let Some(ref sid) = session_id
                && self
                    .seen_sessions
                    .insert((dataset_id.to_string(), pid.clone(), sid.clone()))
            {
                let mut session_data = serde_json::Map::new();
                session_data.insert(
                    "dataset_id".to_string(),
                    Value::String(dataset_id.to_string()),
                );
                session_data.insert("session_id".to_string(), Value::String(sid.clone()));
                session_data.insert("participant_id".to_string(), Value::String(pid.clone()));

                // Ignore errors - session might already exist
                let _ = db.insert(&self.schema, "sessions", &Value::Object(session_data));
            }
        }

        // Specific file processing
        if file_name == "dataset_description.json" {
            self.process_dataset_description(path, db, dataset_id)
                .await?;
        } else if file_name.ends_with(".bval") || file_name.ends_with(".bvec") {
            // For bval/bvec, we need to find the corresponding NIfTI file
            self.process_diffusion_file(path, db, rel_path, file_name, dataset_id)
                .await?;
        } else if file_name.ends_with(".json") {
            self.process_json_file(path, db, dataset_id, rel_path, &entities)
                .await?;
        } else if is_tabular_file(file_name) {
            // Every .tsv/.tsv.gz is routed to a table by the schema-driven tabular
            // model — participants, scans, events, channels, … — so all tabular
            // data lives in the database. See `process_tabular_file`.
            self.process_tabular_file(db, rel_path, file_name, dataset_id, &entities)
                .await?;
        }

        // Track imaging files for auto-populating scans table if needed
        if is_imaging_file(file_name) {
            self.imaging_files.push(ImagingFile {
                dataset_id: dataset_id.to_string(),
                file_path: rel_path.to_string(), // Use rel_path not file_name
            });
        }

        Ok(())
    }

    async fn process_json_file(
        &mut self,
        path: &Path,
        _db: &BidsDb, // db not used here anymore
        dataset_id: &str,
        rel_path: &str,
        entities: &HashMap<String, String>,
    ) -> Result<()> {
        let content = self.fs.read_to_string(path).await?;
        let json_value: Value = serde_json::from_str(&content).unwrap_or(Value::Null);

        // Extract the BIDS suffix from the filename via the shared bids-core parser.
        let file_name = path.file_name().unwrap().to_str().unwrap();
        let suffix = read_entities(file_name).suffix;

        // Store sidecar info for later inheritance processing
        self.sidecars.push(SidecarInfo {
            dataset_id: dataset_id.to_string(),
            file_path: rel_path.to_string(),
            entities: entities.clone(),
            suffix,
            content: json_value.clone(),
        });

        // Check for IntendedFor field to create associations
        self.process_intended_for(rel_path, &content, dataset_id, entities)?;

        Ok(())
    }

    async fn process_dataset_description(
        &mut self,
        path: &Path,
        db: &BidsDb,
        dataset_id: &str,
    ) -> Result<()> {
        let content = self.fs.read_to_string(path).await?;
        let mut json_value: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "Warning: skipping unparseable dataset_description.json at {}: {}",
                    path.display(),
                    e
                );
                return Ok(());
            }
        };

        // Remember DatasetType (raw/derivative) — the `derivatives.*` tabular
        // selectors (dseg lookups) gate on it. Only the root description sets it;
        // nested ones are processed later and must not clobber it.
        if self.dataset_type.is_none()
            && let Some(dt) = json_value.get("DatasetType").and_then(|v| v.as_str())
        {
            self.dataset_type = Some(dt.to_string());
        }

        if let Value::Object(ref mut map) = json_value {
            map.insert(
                "dataset_id".to_string(),
                Value::String(dataset_id.to_string()),
            );
            map.insert("root_uri".to_string(), Value::String(self.fs.root()));
        }

        db.insert(&self.schema, "dataset_description", &json_value)?;
        Ok(())
    }

    async fn process_diffusion_file(
        &mut self,
        path: &Path,
        _db: &BidsDb,
        rel_path: &str,
        file_name: &str,
        dataset_id: &str,
    ) -> Result<()> {
        // Read the bval or bvec file
        let content = self.fs.read_to_string(path).await?;

        // Find the base name (both ".bval" and ".bvec" are 5 chars).
        let base_name = &rel_path[..rel_path.len() - 5];

        // The NIfTI file path
        let nifti_path = format!("{}.nii.gz", base_name);

        // Parse content first (before borrowing self mutably)
        if file_name.ends_with(".bval") {
            let bval_vec = self.parse_bval(&content)?;

            // Get or create entry in HashMap
            let entry =
                self.pending_diffusion
                    .entry(nifti_path.clone())
                    .or_insert(PendingDiffusion {
                        dataset_id: dataset_id.to_string(),
                        bval: None,
                        bvec_x: None,
                        bvec_y: None,
                        bvec_z: None,
                    });

            entry.bval = Some(bval_vec);
        } else if file_name.ends_with(".bvec") {
            let (x, y, z) = self.parse_bvec(&content)?;

            // Get or create entry in HashMap
            let entry =
                self.pending_diffusion
                    .entry(nifti_path.clone())
                    .or_insert(PendingDiffusion {
                        dataset_id: dataset_id.to_string(),
                        bval: None,
                        bvec_x: None,
                        bvec_y: None,
                        bvec_z: None,
                    });

            entry.bvec_x = Some(x);
            entry.bvec_y = Some(y);
            entry.bvec_z = Some(z);
        }

        Ok(())
    }

    fn parse_bval(&self, content: &str) -> Result<Vec<f64>> {
        content
            .split_whitespace()
            .map(|s| {
                s.parse::<f64>()
                    .map_err(|e| anyhow::anyhow!("Failed to parse bval: {}", e))
            })
            .collect()
    }

    fn parse_bvec(&self, content: &str) -> Result<(Vec<f64>, Vec<f64>, Vec<f64>)> {
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

        if lines.len() != 3 {
            return Err(anyhow::anyhow!(
                "bvec file must have exactly 3 rows, found {}",
                lines.len()
            ));
        }

        let parse_row = |line: &str| -> Result<Vec<f64>> {
            line.split_whitespace()
                .map(|s| {
                    s.parse::<f64>()
                        .map_err(|e| anyhow::anyhow!("Failed to parse bvec: {}", e))
                })
                .collect()
        };

        let x = parse_row(lines[0])?;
        let y = parse_row(lines[1])?;
        let z = parse_row(lines[2])?;

        // Verify all rows have the same length
        if x.len() != y.len() || y.len() != z.len() {
            return Err(anyhow::anyhow!("bvec rows must have equal length"));
        }

        Ok((x, y, z))
    }

    /// Route one tabular file to its table and ingest it with DuckDB `read_csv`.
    ///
    /// The file's `(path, suffix, extension, datatype, dataset_type)` are matched
    /// against `rules.tabular_data`. Header-bearing `.tsv` files are ingested
    /// directly; gzipped continuous recordings (`*_physio.tsv.gz`, …) are headerless
    /// and handled separately. Every tabular file — ingested, deferred, or
    /// unmatched — is recorded in `tabular_files` so nothing is silently dropped.
    async fn process_tabular_file(
        &mut self,
        db: &BidsDb,
        rel_path: &str,
        file_name: &str,
        dataset_id: &str,
        entities: &HashMap<String, String>,
    ) -> Result<()> {
        let (suffix, extension) = split_suffix_ext(file_name);
        if suffix == "scans" {
            self.has_scans_tsv = true;
        }

        // Compressed continuous recordings (`*_physio.tsv.gz`, `*_stim.tsv.gz`,
        // `*_physioevents.tsv.gz`) are stored one row per sample and dwarf the
        // metadata, so as a deliberate size policy they are left on disk rather
        // than ingested — recorded here so they are still tracked. See the README
        // roadmap for where this line is likely to move.
        if extension == ".tsv.gz" {
            let table = recording_table_of(&suffix);
            db.record_tabular_file(dataset_id, rel_path, table, 0, "on_disk")?;
            return Ok(());
        }

        // Uncompressed headerless recordings — chiefly the motion time-series —
        // are still ingested. They have no header row; their column names come from
        // the merged sidecar `Columns` or the associated channels file, so they are
        // deferred to the flush once every sidecar has been collected.
        if is_recording_suffix(&suffix) {
            self.pending_recordings.push(PendingRecording {
                dataset_id: dataset_id.to_string(),
                rel_path: rel_path.to_string(),
                suffix,
                entities: entities.clone(),
            });
            return Ok(());
        }

        // Datatype from the path, or — for a file above a datatype directory, like
        // a session-level channels.tsv — inferred from the datatype dirs beneath it.
        let datatype = self
            .datatype_of(rel_path)
            .or_else(|| self.infer_datatype(rel_path, &suffix, &extension));
        // BIDS selector paths are dataset-relative with a leading slash.
        let path_with_slash = format!("/{rel_path}");
        let sidecar = Value::Null;
        let ctx = FileContext {
            path: &path_with_slash,
            datatype: datatype.as_deref(),
            suffix: Some(&suffix),
            extension: Some(&extension),
            sidecar: &sidecar,
            dataset_type: self.dataset_type.as_deref(),
        };

        let table = self.schema.tabular().route(&ctx).cloned();
        match table {
            Some(spec) => {
                let n = self
                    .ingest_tabular(db, &spec, rel_path, dataset_id, entities)
                    .await?;
                db.record_tabular_file(dataset_id, rel_path, Some(&spec.table), n, "ingested")?;
            }
            None => {
                // A validated dataset should not reach here (all its tabular files
                // are schema-described). Warn rather than fail so a newer BIDS
                // extension than the vendored schema doesn't abort ingest.
                eprintln!("Warning: no tabular_data rule for {rel_path}; skipping");
                db.record_tabular_file(dataset_id, rel_path, None, 0, "skipped")?;
            }
        }
        Ok(())
    }

    /// Ingest a header-bearing tabular file into `spec`'s table via a single
    /// `read_csv` INSERT. Returns the number of rows written.
    async fn ingest_tabular(
        &self,
        db: &BidsDb,
        spec: &TableSpec,
        rel_path: &str,
        dataset_id: &str,
        entities: &HashMap<String, String>,
    ) -> Result<i64> {
        let local = self.fs.materialize(Path::new(rel_path)).await?;
        let local = local.to_string_lossy().to_string();

        // Sniff the actual headers on the validator connection (so a parse error
        // can't poison the ingest transaction). An unreadable or column-less file
        // is classified but contributes no rows.
        let read_from = format!("read_csv({}, {HEADER_READ_OPTS})", sql_lit(&local));
        let sniffed = self.sniff_columns(&read_from).unwrap_or_default();
        if sniffed.is_empty() {
            return Ok(0);
        }

        // Re-index idempotency for keyless row tables: clear this file's prior rows
        // before re-inserting. (PK tables dedup via INSERT OR IGNORE.)
        if matches!(spec.identity, RowIdentity::PerRow) {
            let del = format!(
                "DELETE FROM {} WHERE dataset_id = {} AND file_path = {}",
                spec.table,
                sql_lit(dataset_id),
                sql_lit(rel_path)
            );
            db.conn.execute(&del, [])?;
        }

        let sub = entities.get("sub").map(|s| s.as_str());
        let sql =
            build_tabular_insert_sql(spec, &local, rel_path, dataset_id, sub, &sniffed, HEADER_READ_OPTS);
        // A single malformed TSV must not abort the whole dataset's ingest — log
        // and move on (the file is still recorded in `tabular_files`, with 0 rows).
        match db.conn.execute(&sql, []) {
            Ok(n) => Ok(n as i64),
            Err(e) => {
                eprintln!("Warning: failed to ingest tabular file {rel_path}: {e}");
                Ok(0)
            }
        }
    }

    /// The BIDS datatype for a file, taken from the datatype directory in its path.
    fn datatype_of(&self, rel_path: &str) -> Option<String> {
        rel_path
            .split('/')
            .find(|c| self.datatypes.contains(*c))
            .map(|s| s.to_string())
    }

    /// Infer the datatype of a tabular file that has no datatype directory of its
    /// own (a session-/subject-level `channels.tsv`/`electrodes.tsv` that applies
    /// to the runs below it). Among the datatypes appearing in directories *below*
    /// the file, pick the one under which the file actually routes — unique or
    /// nothing, so an ambiguous layout is left unrouted rather than guessed.
    fn infer_datatype(&self, rel_path: &str, suffix: &str, extension: &str) -> Option<String> {
        let dir = Path::new(rel_path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("");
        // Datatype directories beneath the file's directory. A root-level file
        // (dir empty) is above every datatype directory in the dataset.
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let mut candidates: Vec<&str> = self
            .datatype_dirs
            .iter()
            .filter(|(p, _)| p.starts_with(&prefix))
            .map(|(_, dt)| dt.as_str())
            .collect();
        candidates.sort_unstable();
        candidates.dedup();

        let path_with_slash = format!("/{rel_path}");
        let sidecar = Value::Null;
        let mut routable = candidates.into_iter().filter(|dt| {
            let ctx = FileContext {
                path: &path_with_slash,
                datatype: Some(dt),
                suffix: Some(suffix),
                extension: Some(extension),
                sidecar: &sidecar,
                dataset_type: self.dataset_type.as_deref(),
            };
            self.schema.tabular().route(&ctx).is_some()
        });
        let first = routable.next()?;
        match routable.next() {
            None => Some(first.to_string()), // exactly one datatype routes
            Some(_) => None,                 // ambiguous — leave unrouted
        }
    }

    /// Ingest the deferred headerless recordings. Run in the flush phase, after all
    /// sidecars are collected (physio/stim column names come from the merged
    /// sidecar `Columns`) and all channels are ingested (motion column names come
    /// from the associated `_channels.tsv`).
    async fn flush_recordings(&self, db: &BidsDb) -> Result<()> {
        if self.pending_recordings.is_empty() {
            return Ok(());
        }
        println!(
            "Ingesting {} continuous recordings (physio/stim/motion)...",
            self.pending_recordings.len()
        );
        for rec in &self.pending_recordings {
            let (table, n) = self.ingest_recording(db, rec).await.unwrap_or_else(|e| {
                eprintln!("Warning: failed to ingest recording {}: {}", rec.rel_path, e);
                (None, 0)
            });
            let status = if table.is_some() { "ingested" } else { "skipped" };
            db.record_tabular_file(&rec.dataset_id, &rec.rel_path, table.as_deref(), n, status)?;
        }
        Ok(())
    }

    /// Ingest one headerless recording. Returns `(table, rows)`, or `(None, 0)` if
    /// its column names could not be resolved (so it is recorded as skipped).
    async fn ingest_recording(
        &self,
        db: &BidsDb,
        rec: &PendingRecording,
    ) -> Result<(Option<String>, i64)> {
        // Map suffix → target table and the table's schema-declared columns.
        let (table, columns): (&str, Vec<ColumnSpec>) = match rec.suffix.as_str() {
            "physio" => ("physio", self.recording_columns("physio")),
            "physioevents" => ("physio_events", self.recording_columns("physio_events")),
            "stim" => ("stim", Vec::new()),
            "motion" => ("motion", Vec::new()),
            _ => return Ok((None, 0)),
        };

        // Column names, in file order: from the associated channels file (motion) or
        // the merged sidecar `Columns` (physio/stim/physioevents).
        let colnames = if rec.suffix == "motion" {
            self.motion_columns(db, rec)?
        } else {
            self.sidecar_columns(rec)
        };
        if colnames.is_empty() {
            return Ok((None, 0)); // headerless file with no column names → skip
        }

        let local = self.fs.materialize(Path::new(&rec.rel_path)).await?;
        let local = local.to_string_lossy().to_string();

        let spec = TableSpec {
            table: table.to_string(),
            columns,
            identity: RowIdentity::PerRow,
            file_based: true,
            rule_ids: Vec::new(),
        };

        // Headerless read: supply the column names explicitly, all as VARCHAR (the
        // SELECT TRY_CASTs the schema-typed ones). `auto_detect=false` skips the
        // dialect sniffer — it trusts our explicit spec, and (crucially) an empty
        // or truncated file then yields zero rows instead of a sniff error that
        // would poison the whole ingest transaction.
        let cols_spec: Vec<String> = colnames
            .iter()
            .map(|c| format!("{}: 'VARCHAR'", sql_lit(c)))
            .collect();
        let read_opts = format!(
            "delim='\\t', header=false, auto_detect=false, all_varchar=true, nullstr='n/a', columns={{{}}}",
            cols_spec.join(", ")
        );

        // Pre-flight on the validator connection: many recordings are non-gzip
        // git-annex placeholders whose read errors would otherwise poison the
        // transaction. A readable-but-empty file passes and simply yields 0 rows.
        let read_from = format!("read_csv({}, {read_opts})", sql_lit(&local));
        if !self.read_csv_ok(&read_from) {
            return Ok((Some(table.to_string()), 0));
        }

        // Re-index idempotency.
        let del = format!(
            "DELETE FROM {} WHERE dataset_id = {} AND file_path = {}",
            table,
            sql_lit(&rec.dataset_id),
            sql_lit(&rec.rel_path)
        );
        db.conn.execute(&del, [])?;

        let sub = rec.entities.get("sub").map(|s| s.as_str());
        let sql = build_tabular_insert_sql(
            &spec,
            &local,
            &rec.rel_path,
            &rec.dataset_id,
            sub,
            &colnames,
            &read_opts,
        );
        match db.conn.execute(&sql, []) {
            Ok(n) => Ok((Some(table.to_string()), n as i64)),
            Err(e) => {
                eprintln!("Warning: failed to ingest recording {}: {}", rec.rel_path, e);
                Ok((Some(table.to_string()), 0))
            }
        }
    }

    /// The schema-declared columns of a rule-based recording table (`physio`,
    /// `physio_events`).
    fn recording_columns(&self, table: &str) -> Vec<ColumnSpec> {
        self.schema
            .tabular()
            .tables()
            .iter()
            .find(|t| t.table == table)
            .map(|t| t.columns.clone())
            .unwrap_or_default()
    }

    /// Column names for a physio/stim/physioevents recording, from the merged
    /// sidecar's `Columns` array (BIDS requires it for these files).
    fn sidecar_columns(&self, rec: &PendingRecording) -> Vec<String> {
        let merged =
            self.merged_sidecar_map(&rec.dataset_id, &rec.rel_path, &rec.suffix, &rec.entities);
        merged
            .get("Columns")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Column names for a motion recording: the `name` column of its associated
    /// `_channels.tsv` (per `meta.associations.channels`), already ingested into
    /// `motion_channels`.
    fn motion_columns(&self, db: &BidsDb, rec: &PendingRecording) -> Result<Vec<String>> {
        let Some(base) = rec.rel_path.strip_suffix("_motion.tsv") else {
            return Ok(Vec::new());
        };
        let channels_path = format!("{base}_channels.tsv");
        let sql = format!(
            "SELECT name FROM motion_channels WHERE dataset_id = {} AND file_path = {} ORDER BY row_idx",
            sql_lit(&rec.dataset_id),
            sql_lit(&channels_path)
        );
        let mut stmt = db.conn.prepare(&sql)?;
        let names = stmt
            .query_map([], |r| r.get::<_, Option<String>>(0))?
            .filter_map(|r| r.ok().flatten())
            .collect();
        Ok(names)
    }

    /// The merged sidecar for a file via BIDS inheritance: every collected `*.json`
    /// sidecar of the same dataset and suffix whose directory is a prefix of the
    /// file's and whose entities are a subset, merged least-specific-first.
    fn merged_sidecar_map(
        &self,
        dataset_id: &str,
        rel_path: &str,
        suffix: &str,
        entities: &HashMap<String, String>,
    ) -> serde_json::Map<String, Value> {
        let file_dir = Path::new(rel_path).parent().unwrap_or_else(|| Path::new(""));
        let mut applicable: Vec<&SidecarInfo> = self
            .sidecars
            .iter()
            .filter(|s| s.dataset_id == dataset_id && s.suffix == suffix)
            .filter(|s| {
                let sdir = Path::new(&s.file_path)
                    .parent()
                    .unwrap_or_else(|| Path::new(""));
                file_dir.starts_with(sdir)
                    && s.entities.iter().all(|(k, v)| entities.get(k) == Some(v))
            })
            .collect();
        applicable.sort_by_key(|s| s.entities.len());
        let mut merged = serde_json::Map::new();
        for s in applicable {
            if let Value::Object(m) = &s.content {
                for (k, v) in m {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }
        merged
    }

    /// Load the dataset-root `.bidsignore` and compile it with full gitignore
    /// semantics.
    ///
    /// BIDS specifies that `.bidsignore` follows gitignore rules, so we use the
    /// `ignore` crate rather than a bare glob set. This is what makes directory
    /// patterns (`logs/`, `figures/`), anchoring (`/derivatives`), and negation
    /// (`!keep.tsv`) behave correctly — a plain `GlobSet` silently mishandled all
    /// three. Only the root `.bidsignore` is consulted, per spec (nested datasets'
    /// ignore files are not applied when walking a parent).
    async fn load_bidsignore(&mut self) -> Result<()> {
        let bidsignore_path = Path::new(".bidsignore");

        let content = match self.fs.read_to_string(bidsignore_path).await {
            Ok(c) => c,
            Err(_) => return Ok(()), // no .bidsignore → nothing ignored
        };

        self.ignore_set = build_bidsignore(&content)?;
        Ok(())
    }

    /// Process IntendedFor field in sidecar to create file associations
    fn process_intended_for(
        &mut self,
        source_file: &str,
        sidecar_content: &str,
        dataset_id: &str,
        entities: &HashMap<String, String>,
    ) -> Result<()> {
        // Parse JSON to extract IntendedFor
        if let Ok(json) = serde_json::from_str::<Value>(sidecar_content)
            && let Some(intended_for) = json.get("IntendedFor")
        {
            // Determine association type from source file path
            let assoc_type = self.infer_association_type(source_file, entities);

            match intended_for {
                Value::String(target) => {
                    // Single target
                    let normalized_target = self.normalize_path(target, source_file);
                    self.pending_associations.push(FileAssociation {
                        dataset_id: dataset_id.to_string(),
                        source_file: source_file.to_string(),
                        target_file: normalized_target,
                        assoc_type: assoc_type.clone(),
                    });
                }
                Value::Array(targets) => {
                    // Multiple targets
                    for target in targets {
                        if let Some(target_str) = target.as_str() {
                            let normalized_target = self.normalize_path(target_str, source_file);
                            self.pending_associations.push(FileAssociation {
                                dataset_id: dataset_id.to_string(),
                                source_file: source_file.to_string(),
                                target_file: normalized_target,
                                assoc_type: assoc_type.clone(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Infer association type from file path and entities
    fn infer_association_type(
        &self,
        file_path: &str,
        entities: &HashMap<String, String>,
    ) -> String {
        if file_path.contains("/fmap/") || file_path.contains("\\fmap\\") {
            "fieldmap".to_string()
        } else if file_path.contains("mask") || entities.get("label").is_some() {
            "mask".to_string()
        } else if entities.get("suffix") == Some(&"sbref".to_string()) {
            "sbref".to_string()
        } else {
            "derivative".to_string()
        }
    }

    /// Normalize an IntendedFor target into a full dataset-relative path so it
    /// matches `scans.file_path`.
    ///
    /// BIDS allows two forms:
    /// - dataset-relative, e.g. `bids::sub-01/ses-mri/func/..._bold.nii.gz`
    ///   (optionally with a `bids::` prefix or leading `/`);
    /// - subject-relative (legacy), e.g. `ses-mri/func/..._bold.nii.gz`, which is
    ///   relative to the declaring file's subject directory.
    ///
    /// We strip URI decoration, and for the subject-relative form prepend the
    /// `sub-XX/` taken from `source_file`.
    fn normalize_path(&self, target: &str, source_file: &str) -> String {
        let target = target.trim_start_matches("bids::").trim_start_matches('/');

        // Already dataset-relative.
        if target.starts_with("sub-") {
            return target.to_string();
        }

        // Subject-relative: prepend the source file's subject directory.
        if let Some(sub) = source_file.split('/').next()
            && sub.starts_with("sub-")
        {
            return format!("{}/{}", sub, target);
        }

        target.to_string()
    }

    /// Detect sbref associations based on naming patterns
    fn detect_sbref_associations(&self) -> Result<Vec<FileAssociation>> {
        // This would need database querying or tracking file list
        // For now, return empty - we'll enhance this in a future iteration
        Ok(Vec::new())
    }
}

/// Whether a file is a BIDS tabular data file (all such data lives in the database).
fn is_tabular_file(file_name: &str) -> bool {
    file_name.ends_with(".tsv") || file_name.ends_with(".tsv.gz")
}

/// Whether a suffix names a headerless continuous recording (columns come from the
/// sidecar `Columns` or the associated channels file, not a header row).
fn is_recording_suffix(suffix: &str) -> bool {
    matches!(suffix, "physio" | "physioevents" | "stim" | "motion")
}

/// The table a recording suffix maps to (for provenance), or `None` for suffixes
/// with no dedicated table.
fn recording_table_of(suffix: &str) -> Option<&'static str> {
    match suffix {
        "physio" => Some("physio"),
        "physioevents" => Some("physio_events"),
        "stim" => Some("stim"),
        "motion" => Some("motion"),
        _ => None,
    }
}

/// Split a tabular filename into its BIDS `(suffix, extension)` via the shared
/// bids-core parser. The suffix is the trailing token (or the stem for
/// `participants.tsv` / `samples.tsv`); the extension is `.tsv` or `.tsv.gz`.
fn split_suffix_ext(file_name: &str) -> (String, String) {
    let parts = read_entities(file_name);
    (parts.suffix, parts.extension)
}

/// A SQL string literal: single-quoted, with embedded `'` doubled.
fn sql_lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Whether a DuckDB type needs a `TRY_CAST` when read from an all-varchar TSV
/// (so a `n/a` or otherwise unparseable cell becomes NULL rather than erroring).
fn needs_try_cast(sql_type: &str) -> bool {
    matches!(
        sql_type,
        "DOUBLE" | "BIGINT" | "FLOAT" | "REAL" | "INTEGER" | "HUGEINT" | "BOOLEAN" | "TIMESTAMP"
            | "DATE" | "TIME"
    )
}

/// Build the `INSERT … SELECT … FROM read_csv(…)` that ingests one tabular file
/// into its table. DuckDB does the parsing (gzip, `n/a`→NULL, typing); we shape
/// the SELECT so that:
/// - structural columns are filled from the file's location/identity (`dataset_id`
///   constant; `scans.file_path` from the `filename` column + directory prefix;
///   `participant_id`/`session_id` normalized; `row_idx` an ordinal for row tables);
/// - each schema-declared column present in the file is `TRY_CAST` to its type;
/// - every other column is folded into `other_data` as JSON.
///
/// `sniffed` is the file's column names — from a `DESCRIBE` for a header-bearing
/// file, or the sidecar `Columns` / associated channels for a headerless one.
/// Columns the schema declares but the file lacks are simply omitted — `INSERT …
/// BY NAME` leaves them NULL. `read_opts` is the `read_csv` argument list after the
/// path (it differs only by `header=`/`columns=` between the two cases).
fn build_tabular_insert_sql(
    spec: &TableSpec,
    local_path: &str,
    rel_path: &str,
    dataset_id: &str,
    sub: Option<&str>,
    sniffed: &[String],
    read_opts: &str,
) -> String {
    let present: HashSet<&str> = sniffed.iter().map(|s| s.as_str()).collect();
    let mut selects: Vec<String> = vec![format!("{} AS dataset_id", sql_lit(dataset_id))];
    // TSV headers consumed structurally (become a key/path, not a data column and
    // not `other_data`).
    let mut structural: HashSet<&str> = HashSet::new();

    match spec.identity {
        RowIdentity::PerFile => {
            structural.insert("filename");
            if present.contains("filename") {
                // scans.tsv `filename` is relative to the file's directory.
                let prefix = rel_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                let expr = if prefix.is_empty() {
                    "filename".to_string()
                } else {
                    format!("{} || '/' || filename", sql_lit(prefix))
                };
                selects.push(format!("{expr} AS file_path"));
            }
        }
        RowIdentity::PerEntity if spec.table == "participants" => {
            structural.insert("participant_id");
            if present.contains("participant_id") {
                selects.push(
                    "CASE WHEN participant_id LIKE 'sub-%' THEN participant_id ELSE 'sub-' || participant_id END AS participant_id"
                        .to_string(),
                );
            }
        }
        RowIdentity::PerEntity => {
            // sessions: session_id from the file, participant_id from its location.
            structural.insert("session_id");
            if present.contains("session_id") {
                selects.push(
                    "CASE WHEN session_id LIKE 'ses-%' THEN session_id ELSE 'ses-' || session_id END AS session_id"
                        .to_string(),
                );
            }
            if let Some(s) = sub {
                selects.push(format!("{} AS participant_id", sql_lit(&format!("sub-{s}"))));
            }
        }
        RowIdentity::PerRow => {
            selects.push(format!("{} AS file_path", sql_lit(rel_path)));
            selects.push("(row_number() OVER () - 1)::BIGINT AS row_idx".to_string());
        }
    }

    // Schema-declared data columns present in the file, TRY_CAST to their type.
    let mut known: HashSet<&str> = HashSet::new();
    for c in &spec.columns {
        if structural.contains(c.name.as_str()) {
            continue;
        }
        known.insert(c.name.as_str());
        if !present.contains(c.name.as_str()) {
            continue; // omitted → BY NAME leaves it NULL
        }
        let q = quote_ident(&c.name);
        if needs_try_cast(&c.sql_type) {
            selects.push(format!("TRY_CAST({q} AS {}) AS {q}", c.sql_type));
        } else {
            selects.push(format!("{q} AS {q}"));
        }
    }

    // Everything else → other_data JSON (in file order).
    let extras: Vec<&str> = sniffed
        .iter()
        .map(|s| s.as_str())
        .filter(|c| !structural.contains(c) && !known.contains(c))
        .collect();
    if !extras.is_empty() {
        let pairs: Vec<String> = extras
            .iter()
            .map(|c| format!("{}, {}", sql_lit(c), quote_ident(c)))
            .collect();
        selects.push(format!("json_object({}) AS other_data", pairs.join(", ")));
    }

    // PK tables (participants/sessions/scans) dedup by key so an explicit TSV row
    // and an implicit one don't collide; row tables have no key (re-index dedup is
    // a DELETE by file_path in the caller).
    let verb = match spec.identity {
        RowIdentity::PerRow => "INSERT",
        _ => "INSERT OR IGNORE",
    };
    format!(
        "{verb} INTO {} BY NAME SELECT {} FROM read_csv({}, {read_opts})",
        spec.table,
        selects.join(", "),
        sql_lit(local_path),
    )
}

/// The `read_csv` options for a header-bearing tabular file.
const HEADER_READ_OPTS: &str = "delim='\\t', header=true, all_varchar=true, nullstr='n/a'";

/// Determine if a file is an imaging data file that should go in scans table
fn is_imaging_file(filename: &str) -> bool {
    let imaging_extensions = [
        ".nii.gz", ".nii", ".img", ".hdr", // Analyze format
        ".img.gz", ".hdr.gz",
    ];

    imaging_extensions.iter().any(|ext| filename.ends_with(ext))
}

/// Compile `.bidsignore` file content into a [`Gitignore`] matcher.
///
/// Patterns are relative to the dataset root; `walk()` yields root-relative paths,
/// so an empty builder root keeps both sides in the same frame. Comments and blank
/// lines are skipped; an individually malformed pattern is warned about and
/// skipped rather than failing the whole load.
///
/// Public so the tabular-coverage test can reproduce exactly which files ingest
/// would ignore.
pub fn build_bidsignore(content: &str) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new("");
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Err(e) = builder.add_line(None, trimmed) {
            eprintln!("Warning: invalid .bidsignore pattern '{}': {}", trimmed, e);
        }
    }
    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::build_bidsignore;
    use std::path::Path;

    fn ignored(content: &str, path: &str) -> bool {
        build_bidsignore(content)
            .unwrap()
            .matched_path_or_any_parents(Path::new(path), false)
            .is_ignore()
    }

    /// A directory pattern must exclude everything beneath it — the case the old
    /// bare-GlobSet handling silently got wrong.
    #[test]
    fn directory_pattern_excludes_contents() {
        assert!(ignored("logs/\n", "logs/run-01.log"));
        assert!(ignored("logs/\n", "sub-01/logs/x.txt"));
        assert!(ignored("figures/\n", "derivatives/figures/a.svg"));
        assert!(!ignored("logs/\n", "sub-01/func/sub-01_bold.nii.gz"));
    }

    /// `*` glob still works, and matches across directories for a bare pattern.
    #[test]
    fn glob_patterns_match() {
        assert!(ignored("*_mixing.tsv\n", "sub-16/func/sub-16_desc-x_mixing.tsv"));
        assert!(ignored("*.html\n", "sub-01/report.html"));
        assert!(!ignored("*_mixing.tsv\n", "sub-16/func/sub-16_bold.nii.gz"));
    }

    /// A leading slash anchors a pattern to the dataset root.
    #[test]
    fn anchored_pattern_matches_only_at_root() {
        assert!(ignored("/derivatives\n", "derivatives/sub-01/x.nii.gz"));
        // Anchored at root, so a nested `derivatives` is NOT matched.
        assert!(!ignored("/derivatives\n", "sub-01/derivatives/x.nii.gz"));
    }

    /// Negation re-includes a file excluded by an earlier pattern.
    #[test]
    fn negation_reincludes() {
        let content = "*.tsv\n!keep.tsv\n";
        assert!(ignored(content, "sub-01/drop.tsv"));
        assert!(!ignored(content, "sub-01/keep.tsv"));
    }
}
