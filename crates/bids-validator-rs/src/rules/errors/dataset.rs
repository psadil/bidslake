use super::ErrorValidator;
use crate::context::{BidsContext, DatasetContext};
use std::collections::HashSet;

pub struct MissingSession;

#[async_trait::async_trait]
impl ErrorValidator for MissingSession {
    fn key(&self) -> &'static str {
        "MissingSession"
    }

    async fn validate_dataset(&self, dataset: &DatasetContext) -> Vec<String> {
        let mut failing = Vec::new();
        // If there are multiple subjects, do they all have the exact same session directories?
        if dataset.subjects.sub_dirs.is_empty() {
            return failing;
        }

        let mut subject_sessions: Vec<(String, HashSet<String>)> = Vec::new();

        for sub in &dataset.subjects.sub_dirs {
            let mut ses_set = HashSet::new();
            if let Some(sub_dir) = dataset.tree.find_dir(sub) {
                for d in &sub_dir.directories {
                    if d.name.starts_with("ses-") {
                        ses_set.insert(d.name.clone());
                    }
                }
            }
            subject_sessions.push((sub.clone(), ses_set));
        }

        if subject_sessions.is_empty() {
            return failing;
        }

        // Compare all sets against the first subject's set
        let first_set = &subject_sessions[0].1;
        for (sub, ses_set) in &subject_sessions[1..] {
            if ses_set != first_set {
                // If they don't match, we return the paths of subjects missing sessions
                // We'll just return the subject path
                failing.push(format!("/{}", sub));
            }
        }

        // Return paths for the first subject too if it was missing sessions others had
        for (_sub, ses_set) in &subject_sessions[1..] {
            if ses_set != first_set && !failing.contains(&format!("/{}", subject_sessions[0].0)) {
                failing.push(format!("/{}", subject_sessions[0].0));
            }
        }

        failing
    }
}

pub struct NoValidDataFoundForSubject;

#[async_trait::async_trait]
impl ErrorValidator for NoValidDataFoundForSubject {
    fn key(&self) -> &'static str {
        "NoValidDataFoundForSubject"
    }

    async fn validate_dataset(&self, dataset: &DatasetContext) -> Vec<String> {
        let mut failing = Vec::new();
        // We consider data valid if it's in a subject directory and has at least one file.
        for sub in &dataset.subjects.sub_dirs {
            let has_files = dataset
                .tree
                .find_dir(sub)
                .map(|sub_dir| {
                    // Check if the subject subtree has any files (recursively)
                    sub_dir.walk_files().next().is_some()
                })
                .unwrap_or(false);
            if !has_files {
                failing.push(format!("/{}", sub));
            }
        }
        failing
    }
}

pub struct SidecarWithoutDatafile;

#[async_trait::async_trait]
impl ErrorValidator for SidecarWithoutDatafile {
    fn key(&self) -> &'static str {
        "SidecarWithoutDatafile"
    }

    async fn validate_file(&self, context: &BidsContext, dataset: &DatasetContext) -> bool {
        // "A json sidecar file was found without a corresponding data file."
        // We only trigger this if the file is a .json and is not a top-level file like dataset_description.json
        if context.extension != ".json" {
            return false;
        }

        if context.path == "/dataset_description.json" || context.path == "/genetic_info.json" {
            return false; // exceptions
        }

        // Description files (e.g. atlas-<label>_description.json) are standalone
        // metadata, not sidecar JSON files.
        if context.suffix == "description" {
            return false;
        }

        // Navigate directly to the parent directory of this sidecar
        let dir = context.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let dir_path = dir.strip_prefix('/').unwrap_or(dir);

        // Get the directory subtree to search within
        let search_dir = if dir_path.is_empty() {
            Some(&dataset.tree)
        } else {
            dataset.tree.subtree(dir_path)
        };

        let Some(search_dir) = search_dir else {
            return true; // Directory not found, so no data file
        };

        let has_datafile = search_dir.walk_files().any(|f| {
            if f.path == context.path {
                return false; // Skip the sidecar itself
            }
            // A corresponding data file cannot be a JSON file
            if f.name.ends_with(".json") {
                return false;
            }

            // Check if suffix matches. Some JSON files like coordsystem.json don't have a data file
            // with the same suffix, they associate with the main data file (e.g. meg.fif).
            let standalone_json_suffixes = ["coordsystem"];
            let suffix_match = if standalone_json_suffixes.contains(&context.suffix.as_str()) {
                true
            } else {
                f.name.contains(&format!("_{}.", context.suffix))
                    || f.name.starts_with(&format!("{}.", context.suffix))
            };

            if !suffix_match {
                return false;
            }

            // Check if all entities from the sidecar are present in the data file
            for (k, v) in &context.raw_entities {
                if context.suffix == "coordsystem" && k == "space" {
                    continue;
                }
                let entity_str = format!("{}-{}", k, v);
                if !f.name.contains(&entity_str) {
                    return false;
                }
            }

            true
        });

        !has_datafile
    }
}
