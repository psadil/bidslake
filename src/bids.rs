use crate::db::{BidsDb, FileAssociation};
use crate::fs::BidsFileSystem;
use crate::schema::Schema;
use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

pub struct BidsParser {
    fs: Box<dyn BidsFileSystem>,
    dataset_id: Option<String>,
    ignore_set: GlobSet,
    pending_associations: Vec<FileAssociation>,
    pending_diffusion: HashMap<String, PendingDiffusion>,
    schema: Schema,
    imaging_files: Vec<ImagingFile>,
    has_scans_tsv: bool,
    sidecars: Vec<SidecarInfo>,
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

impl BidsParser {
    pub fn new(fs: Box<dyn BidsFileSystem>, dataset_id: Option<String>, schema: Schema) -> Self {
        Self {
            fs,
            dataset_id,
            ignore_set: GlobSet::empty(),
            pending_associations: Vec::new(),
            pending_diffusion: HashMap::new(),
            schema,
            imaging_files: Vec::new(),
            has_scans_tsv: false,
            sidecars: Vec::new(),
        }
    }

    pub async fn parse(&mut self, db: &BidsDb) -> Result<()> {
        // Load .bidsignore patterns before parsing
        self.load_bidsignore().await?;

        // Pre-compile regex for extracting entities
        let entity_re: Regex = Regex::new(r"([a-zA-Z0-9]+)-([a-zA-Z0-9]+)")?;

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

            // Skip files matching .bidsignore patterns
            if self.ignore_set.is_match(&path) {
                continue;
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

        // Pass 0: Process dataset_description.json first to resolve dataset_id
        for path in &dataset_description {
            if self.dataset_id.is_none() {
                let content = self.fs.read_to_string(path).await?;
                let dataset_desc: Value = serde_json::from_str(&content)?;
                if let Some(name) = dataset_desc.get("Name").and_then(|v| v.as_str()) {
                    println!("Using dataset name from dataset_description.json: {}", name);
                    self.dataset_id = Some(name.to_string());
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
                    .last()
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
            self.process_file(&path, db, &entity_re, &dataset_id)
                .await?;
        }

        // Pass 1: Process participants.tsv files
        for path in participants_tsv {
            self.process_file(&path, db, &entity_re, &dataset_id)
                .await?;
        }

        // Pass 2: Process sessions.tsv files
        for path in sessions_tsv {
            self.process_file(&path, db, &entity_re, &dataset_id)
                .await?;
        }

        // Pass 3: Process all other files
        for path in other_files {
            self.process_file(&path, db, &entity_re, &dataset_id)
                .await?;
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

        // Insert pending diffusion data
        for (nifti_path, diff) in &self.pending_diffusion {
            // Only insert if we have both bval and bvec data
            if diff.bval.is_some()
                && diff.bvec_x.is_some()
                && diff.bvec_y.is_some()
                && diff.bvec_z.is_some()
            {
                // Files table removed, so just insert directly
                if let Err(e) = db.insert_diffusion(
                    &diff.dataset_id,
                    nifti_path,
                    diff.bval.as_ref().unwrap(),
                    diff.bvec_x.as_ref().unwrap(),
                    diff.bvec_y.as_ref().unwrap(),
                    diff.bvec_z.as_ref().unwrap(),
                ) {
                    eprintln!("Failed to insert diffusion data for {}: {}", nifti_path, e);
                }
            }
        }

        if !self.has_scans_tsv && !self.imaging_files.is_empty() {
            println!(
                "No scans.tsv files found. Auto-populating scans table with {} imaging files.",
                self.imaging_files.len()
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
                if let Some(filename) = img_file.file_path.split('/').last() {
                    scan_data.insert("filename".to_string(), Value::String(filename.to_string()));
                }

                // Build other_data without file_path and dataset_id
                let mut other_data = serde_json::Map::new();
                // Only include filename in other_data (exclude file_path and dataset_id)
                if let Some(filename) = img_file.file_path.split('/').last() {
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

        // Apply BIDS inheritance to populate sidecars table
        println!(
            "Applying BIDS inheritance for {} imaging files...",
            self.imaging_files.len()
        );
        let entity_re = Regex::new(r"([a-zA-Z0-9]+)-([a-zA-Z0-9]+)")?;

        for img_file in &self.imaging_files {
            let mut merged_metadata = serde_json::Map::new();

            // Extract entities and suffix from imaging file
            let file_name = img_file.file_path.split('/').last().unwrap();
            let mut img_entities = HashMap::new();
            for cap in entity_re.captures_iter(file_name) {
                img_entities.insert(cap[1].to_string(), cap[2].to_string());
            }

            let img_suffix = if let Some(last_underscore) = file_name.rfind('_') {
                if let Some(first_dot) = file_name[last_underscore..].find('.') {
                    file_name[last_underscore + 1..last_underscore + first_dot].to_string()
                } else {
                    file_name[last_underscore + 1..].to_string()
                }
            } else {
                // Should have a suffix if it's BIDS, but fallback
                if let Some(first_dot) = file_name.find('.') {
                    file_name[..first_dot].to_string()
                } else {
                    file_name.to_string()
                }
            };

            // Find applicable sidecars
            let mut applicable_sidecars: Vec<&SidecarInfo> = self
                .sidecars
                .iter()
                .filter(|s| {
                    // Check dataset_id match
                    if s.dataset_id != img_file.dataset_id {
                        return false;
                    }

                    // 1. Must match suffix
                    if s.suffix != img_suffix {
                        return false;
                    }

                    // 2. Must be in same directory or parent directory
                    let img_dir = std::path::Path::new(&img_file.file_path)
                        .parent()
                        .unwrap_or(std::path::Path::new(""));
                    let sidecar_dir = std::path::Path::new(&s.file_path)
                        .parent()
                        .unwrap_or(std::path::Path::new(""));

                    if !img_dir.starts_with(sidecar_dir) {
                        return false;
                    }

                    // 3. Entities must be a subset of image entities
                    for (key, value) in &s.entities {
                        if img_entities.get(key) != Some(value) {
                            return false;
                        }
                    }

                    true
                })
                .collect();

            // Sort by specificity (number of entities) - least specific first for merging
            // BIDS Principle of Inheritance: values from more specific files override less specific ones.
            // So we want to merge from top (least specific) to bottom (most specific).
            applicable_sidecars.sort_by_key(|s| s.entities.len());

            // Merge metadata
            for sidecar in applicable_sidecars {
                if let Value::Object(map) = &sidecar.content {
                    for (k, v) in map {
                        merged_metadata.insert(k.clone(), v.clone());
                    }
                }
            }

            // Insert into sidecars table if we have metadata
            if !merged_metadata.is_empty() {
                let mut sidecar_entry = serde_json::Map::new();
                sidecar_entry.insert(
                    "dataset_id".to_string(),
                    Value::String(img_file.dataset_id.clone()),
                );
                sidecar_entry.insert(
                    "file_path".to_string(),
                    Value::String(img_file.file_path.clone()),
                );
                sidecar_entry.insert(
                    "other_data".to_string(),
                    Value::Object(merged_metadata.clone()),
                );

                // Also flatten metadata into top-level fields for known columns
                for (k, v) in &merged_metadata {
                    sidecar_entry.insert(k.clone(), v.clone());
                }

                if let Err(e) = db.insert(&self.schema, "sidecars", &Value::Object(sidecar_entry)) {
                    eprintln!(
                        "Failed to insert sidecar entry for {}: {}",
                        img_file.file_path, e
                    );
                }
            }
        }

        Ok(())
    }

    async fn process_file(
        &mut self,
        path: &Path,
        db: &BidsDb,
        entity_re: &Regex,
        dataset_id: &str,
    ) -> Result<()> {
        let file_name = path.file_name().unwrap().to_str().unwrap();

        // path from walk() is already relative to dataset root
        let rel_path = path.to_str().unwrap();

        if file_name.starts_with('.') {
            return Ok(());
        }

        // Extract entities from filename
        let mut entities = HashMap::new();
        for cap in entity_re.captures_iter(file_name) {
            entities.insert(cap[1].to_string(), cap[2].to_string());
        }

        let participant_id = entities.get("sub").map(|s| format!("sub-{}", s));
        let session_id = entities.get("ses").map(|s| format!("ses-{}", s));

        // Auto-create participant/session if they don't exist (implicit)
        if let Some(ref pid) = participant_id {
            // Try to insert participant
            let mut participant_data = serde_json::Map::new();
            participant_data.insert(
                "dataset_id".to_string(),
                Value::String(dataset_id.to_string()),
            );
            participant_data.insert("participant_id".to_string(), Value::String(pid.clone()));

            // Ignore errors - participant might already exist
            let _ = db.insert(
                &self.schema,
                "participants",
                &Value::Object(participant_data),
            );

            if let Some(ref sid) = session_id {
                // Try to insert the session
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

        // Determine datatype, suffix, extension
        // Simple heuristic for now
        let parts: Vec<&str> = file_name.split('_').collect();
        let suffix_parts: Vec<&str> = parts.last().unwrap().split('.').collect();
        let _suffix = suffix_parts[0];
        let _extension = if suffix_parts.len() > 1 {
            Some(suffix_parts[1..].join("."))
        } else {
            None
        };

        // Datatype is usually the parent directory name if it's a standard BIDS folder
        let parent_dir_name = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let _datatype = if ["anat", "func", "dwi", "fmap", "beh"].contains(&parent_dir_name) {
            Some(parent_dir_name)
        } else {
            None
        };

        // Specific file processing
        if file_name == "dataset_description.json" {
            self.process_dataset_description(path, db, dataset_id)
                .await?;
        } else if file_name == "participants.tsv" {
            self.process_participants_tsv(path, db, dataset_id).await?;
        } else if file_name == "sessions.tsv" {
            self.process_sessions_tsv(path, db, dataset_id).await?;
        } else if file_name.ends_with("_scans.tsv") {
            self.process_scans_tsv(
                path,
                db,
                dataset_id,
                participant_id.as_deref(),
                session_id.as_deref(),
            )
            .await?;
        } else if file_name.ends_with("_events.tsv") {
            self.process_events_tsv(
                path,
                db,
                rel_path,
                participant_id.as_deref(),
                session_id.as_deref(),
                dataset_id,
            )
            .await?;
        } else if file_name.ends_with(".bval") || file_name.ends_with(".bvec") {
            // For bval/bvec, we need to find the corresponding NIfTI file
            self.process_diffusion_file(path, db, rel_path, file_name, dataset_id)
                .await?;
        } else if file_name.ends_with(".json") {
            self.process_json_file(path, db, dataset_id, rel_path, &entities)
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

        // Extract suffix from filename (part after last underscore, before extension)
        let file_name = path.file_name().unwrap().to_str().unwrap();
        let suffix = if let Some(last_underscore) = file_name.rfind('_') {
            if let Some(first_dot) = file_name[last_underscore..].find('.') {
                file_name[last_underscore + 1..last_underscore + first_dot].to_string()
            } else {
                // No extension?
                file_name[last_underscore + 1..].to_string()
            }
        } else {
            // No underscore, maybe top level like "dwi.json"?
            if let Some(first_dot) = file_name.find('.') {
                file_name[..first_dot].to_string()
            } else {
                file_name.to_string()
            }
        };

        // Store sidecar info for later inheritance processing
        self.sidecars.push(SidecarInfo {
            dataset_id: dataset_id.to_string(),
            file_path: rel_path.to_string(),
            entities: entities.clone(),
            suffix,
            content: json_value.clone(),
        });

        // Check for IntendedFor field to create associations
        self.process_intended_for(rel_path, &content, dataset_id, &entities)?;

        Ok(())
    }

    async fn process_dataset_description(
        &self,
        path: &Path,
        db: &BidsDb,
        dataset_id: &str,
    ) -> Result<()> {
        let content = self.fs.read_to_string(path).await?;
        let mut json_value: Value = serde_json::from_str(&content)?;

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

        // Find the base name (remove .bval or .bvec extension)
        let base_name = if file_name.ends_with(".bval") {
            &rel_path[..rel_path.len() - 5]
        } else {
            &rel_path[..rel_path.len() - 5]
        };

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

    async fn process_participants_tsv(
        &self,
        path: &Path,
        db: &BidsDb,
        dataset_id: &str,
    ) -> Result<()> {
        let content = self.fs.read_to_string(path).await?;
        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(b'\t')
            .from_reader(content.as_bytes());

        for result in rdr.deserialize() {
            let record: HashMap<String, String> = result?;
            let mut value = serde_json::to_value(&record)?;
            let value_copy = value.clone();

            if let Value::Object(ref mut map) = value {
                map.insert(
                    "dataset_id".to_string(),
                    Value::String(dataset_id.to_string()),
                );
                map.insert("other_data".to_string(), value_copy);

                // Normalize participant_id
                if let Some(pid) = map.get("participant_id").and_then(|v| v.as_str()) {
                    if !pid.starts_with("sub-") {
                        map.insert(
                            "participant_id".to_string(),
                            Value::String(format!("sub-{}", pid)),
                        );
                    }
                }
            }

            db.insert(&self.schema, "participants", &value)?;
        }
        Ok(())
    }

    async fn process_sessions_tsv(&self, path: &Path, db: &BidsDb, dataset_id: &str) -> Result<()> {
        let content = self.fs.read_to_string(path).await?;
        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(b'\t')
            .from_reader(content.as_bytes());

        for result in rdr.deserialize() {
            let record: HashMap<String, String> = result?;
            let mut value = serde_json::to_value(&record)?;
            let value_copy = value.clone();

            if let Value::Object(ref mut map) = value {
                map.insert(
                    "dataset_id".to_string(),
                    Value::String(dataset_id.to_string()),
                );
                map.insert("other_data".to_string(), value_copy);

                // Normalize session_id
                if let Some(sid) = map.get("session_id").and_then(|v| v.as_str()) {
                    if !sid.starts_with("ses-") {
                        map.insert(
                            "session_id".to_string(),
                            Value::String(format!("ses-{}", sid)),
                        );
                    }
                }
            }

            db.insert(&self.schema, "sessions", &value)?;
        }
        Ok(())
    }

    async fn process_scans_tsv(
        &mut self,
        path: &Path,
        db: &BidsDb,
        dataset_id: &str,
        participant_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<()> {
        self.has_scans_tsv = true; // Mark that we found a scans.tsv file

        let content = self.fs.read_to_string(path).await?;
        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(b'\t')
            .from_reader(content.as_bytes());

        for result in rdr.deserialize() {
            let record: HashMap<String, String> = result?;
            let mut value = serde_json::to_value(&record)?;
            let value_copy = value.clone();

            if let Value::Object(ref mut map) = value {
                map.insert(
                    "dataset_id".to_string(),
                    Value::String(dataset_id.to_string()),
                );

                // Map 'filename' column to 'file_path' for database
                if let Some(filename) = record.get("filename") {
                    // Construct full relative path
                    let full_path = if let (Some(pid), Some(sid)) = (participant_id, session_id) {
                        format!("{}/{}/{}", pid, sid, filename)
                    } else if let Some(pid) = participant_id {
                        format!("{}/{}", pid, filename)
                    } else {
                        filename.to_string()
                    };
                    map.insert("file_path".to_string(), Value::String(full_path));
                }

                // Build other_data excluding file_path and dataset_id
                let mut other_data = value_copy.as_object().unwrap().clone();
                other_data.remove("file_path");
                other_data.remove("dataset_id");
                map.insert("other_data".to_string(), Value::Object(other_data));
            }

            db.insert(&self.schema, "scans", &value)?;
        }
        Ok(())
    }

    async fn process_events_tsv(
        &self,
        path: &Path,
        db: &BidsDb,
        rel_path: &str,
        participant_id: Option<&str>,
        session_id: Option<&str>,
        dataset_id: &str,
    ) -> Result<()> {
        let content = self.fs.read_to_string(path).await?;
        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(b'\t')
            .from_reader(content.as_bytes());

        for result in rdr.deserialize() {
            let record: HashMap<String, String> = result?;

            // Convert the record to a Value, converting numeric strings to numbers
            let mut value_map = serde_json::Map::new();
            for (key, val) in &record {
                // Try to parse as f64, otherwise keep as string
                let value = if let Ok(num) = val.parse::<f64>() {
                    serde_json::Value::Number(
                        serde_json::Number::from_f64(num)
                            .unwrap_or_else(|| serde_json::Number::from(0)),
                    )
                } else {
                    serde_json::Value::String(val.clone())
                };
                value_map.insert(key.clone(), value);
            }

            value_map.insert(
                "dataset_id".to_string(),
                Value::String(dataset_id.to_string()),
            );
            value_map.insert("file_path".to_string(), Value::String(rel_path.to_string()));
            if let Some(pid) = participant_id {
                value_map.insert("participant_id".to_string(), Value::String(pid.to_string()));
            }
            if let Some(sid) = session_id {
                value_map.insert("session_id".to_string(), Value::String(sid.to_string()));
            }
            value_map.insert("other_data".to_string(), Value::Object(value_map.clone()));

            let value = serde_json::Value::Object(value_map);
            db.insert(&self.schema, "events", &value)?;
        }
        Ok(())
    }

    /// Load .bidsignore file and build GlobSet for pattern matching
    /// .bidsignore follows gitignore-style patterns
    async fn load_bidsignore(&mut self) -> Result<()> {
        use std::path::PathBuf;

        let bidsignore_path = PathBuf::from(".bidsignore");

        // Try to read .bidsignore file
        let content = match self.fs.read_to_string(&bidsignore_path).await {
            Ok(c) => c,
            Err(_) => {
                // .bidsignore doesn't exist, use empty GlobSet
                return Ok(());
            }
        };

        let mut builder = GlobSetBuilder::new();
        for line in content.lines() {
            let line = line.trim();
            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Add glob pattern to builder
            match Glob::new(line) {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(e) => {
                    eprintln!("Warning: Invalid .bidsignore pattern '{}': {}", line, e);
                }
            }
        }

        self.ignore_set = builder.build()?;
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
        if let Ok(json) = serde_json::from_str::<Value>(sidecar_content) {
            if let Some(intended_for) = json.get("IntendedFor") {
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
                                let normalized_target =
                                    self.normalize_path(target_str, source_file);
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

    /// Normalize IntendedFor path (handle session-relative, dataset-relative)
    fn normalize_path(&self, target: &str, _source_file: &str) -> String {
        // Remove leading slashes and "bids::" prefix if present
        let target = target.trim_start_matches("bids::").trim_start_matches('/');

        // If target starts with "ses-", it's session-relative
        // Otherwise it's dataset-relative and already correct
        target.to_string()
    }

    /// Detect sbref associations based on naming patterns
    fn detect_sbref_associations(&self) -> Result<Vec<FileAssociation>> {
        // This would need database querying or tracking file list
        // For now, return empty - we'll enhance this in a future iteration
        Ok(Vec::new())
    }
}

/// Determine if a file is an imaging data file that should go in scans table
fn is_imaging_file(filename: &str) -> bool {
    let imaging_extensions = [
        ".nii.gz", ".nii", ".img", ".hdr", // Analyze format
        ".img.gz", ".hdr.gz",
    ];

    imaging_extensions.iter().any(|ext| filename.ends_with(ext))
}
