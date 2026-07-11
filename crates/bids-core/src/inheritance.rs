//! BIDS Inheritance Principle implementation.
//!
//! Metadata in JSON sidecars is inherited from parent directories to children.
//! This module walks up the file tree to find and merge sidecar files.

use crate::entities::read_entities;
use crate::filetree::{BidsFile, FileTree};
use serde_json::Value;
use std::collections::HashMap;

/// A sidecar key whose inherited value is overridden by a more-specific sidecar with a
/// different value (mirrors the TS validator's `SIDECAR_FIELD_OVERRIDE`).
#[derive(Debug, Clone)]
pub struct SidecarOverride {
    /// The overridden key (issue `subCode`).
    pub key: String,
    /// Path of the winning (nearer) sidecar that supplied the effective value (issue `location`).
    pub location: String,
    /// Human-readable detail (issue `issueMessage`).
    pub message: String,
}

/// Read and merge sidecar JSON files following the BIDS inheritance principle.
///
/// Walks from the file's directory up to the dataset root, collecting JSON sidecars
/// that match the file's suffix and a subset of its entities. Child values override
/// parent values. Also returns any [`SidecarOverride`]s, where a farther sidecar holds a
/// different value for a key already set by a nearer one.
pub async fn read_sidecars(file: &BidsFile, tree: &FileTree) -> (Value, Vec<SidecarOverride>) {
    let file_parts = read_entities(&file.name);
    let target_suffix = &file_parts.suffix;
    let file_entities = &file_parts.entities;

    // Collect (path, contents) for each matching sidecar.
    let mut sidecars: Vec<(String, Value)> = Vec::new();

    // Walk up the directory hierarchy
    let path_parts: Vec<&str> = file.path.split('/').filter(|s| !s.is_empty()).collect();

    // Build directory paths from root to the file's parent
    let mut dir_paths: Vec<String> = vec!["/".to_string()];
    for i in 0..path_parts.len().saturating_sub(1) {
        let dir = format!("/{}", path_parts[..=i].join("/"));
        dir_paths.push(dir);
    }

    // Process from shallowest to deepest so that deeper files override shallower ones.
    for dir_path in &dir_paths {
        let dir_tree = if dir_path == "/" {
            Some(tree)
        } else {
            tree.subtree(dir_path)
        };

        if let Some(dir) = dir_tree {
            for candidate in &dir.files {
                if !candidate.name.ends_with(".json") {
                    continue;
                }
                if matches_sidecar(&candidate.name, target_suffix, file_entities)
                    && let Ok(content) = candidate.read_string().await
                    && let Ok(val) = serde_json::from_str::<Value>(&content)
                {
                    sidecars.push((candidate.path.clone(), val));
                }
            }
        }
    }

    // Process nearest-first (deepest → root), mirroring the TS validator: the nearer value
    // wins, and a farther sidecar holding a *different* value for an already-set key is an
    // override. `sidecars` is currently shallowest-first, so reverse it.
    sidecars.reverse();
    let mut merged = serde_json::Map::new();
    let mut origin: HashMap<String, String> = HashMap::new();
    let mut overrides: Vec<SidecarOverride> = Vec::new();
    for (path, json) in &sidecars {
        if let Some(obj) = json.as_object() {
            for (key, value) in obj {
                match merged.get(key) {
                    Some(existing) => {
                        if is_override(existing, value) {
                            let location = origin.get(key).cloned().unwrap_or_default();
                            let prev = match value {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            overrides.push(SidecarOverride {
                                key: key.clone(),
                                location: location.clone(),
                                message: format!(
                                    "Sidecar key defined in {} overrides previous value ({}) from {}",
                                    path, prev, location
                                ),
                            });
                        }
                        // Nearer value already recorded — keep it.
                    }
                    None => {
                        merged.insert(key.clone(), value.clone());
                        origin.insert(key.clone(), path.clone());
                    }
                }
            }
        }
    }

    (Value::Object(merged), overrides)
}

/// Whether a key re-declared in a farther (less-specific) sidecar counts as an override of the
/// value already set by a nearer one.
///
/// Matches the reference validator's `json[key] !== this.sidecar[key]`
/// (`lib/bids-validator/src/schema/context.ts:218`). JavaScript `!==` is *reference* inequality for
/// objects and arrays, so any object- or array-valued key that appears in two sidecars is always an
/// override — even when the two values are structurally identical (as `SoftwareFilters` is in
/// `eeg_ds003645s_hed_demo`). The intent is to flag a field defined in more than one place at all,
/// not only one whose effective value changed. Primitives compare by value, with `2 === 2.0` (JS
/// numbers are all `f64`), unlike serde's representation-sensitive `==`.
fn is_override(nearer: &Value, farther: &Value) -> bool {
    match (nearer, farther) {
        (Value::Object(_) | Value::Array(_), _) | (_, Value::Object(_) | Value::Array(_)) => true,
        (Value::Number(a), Value::Number(b)) => a.as_f64() != b.as_f64(),
        _ => nearer != farther,
    }
}

/// Check if a JSON filename matches as a sidecar for the given suffix and entities.
///
/// A sidecar matches if:
/// 1. It has `.json` extension
/// 2. Its suffix matches the target suffix (or it has no suffix — root-level sidecar)
/// 3. Its entities are a subset of the file's entities
fn matches_sidecar(
    sidecar_name: &str,
    target_suffix: &str,
    file_entities: &HashMap<String, String>,
) -> bool {
    let sidecar_parts = read_entities(sidecar_name);

    // Extension must be .json
    if sidecar_parts.extension != ".json" {
        return false;
    }

    // Suffix must match (or sidecar has no entities and matching suffix — e.g. "task-rest_bold.json")
    if !sidecar_parts.suffix.is_empty() && sidecar_parts.suffix != target_suffix {
        return false;
    }

    // All sidecar entities must be present in the file entities with matching values
    for (key, value) in &sidecar_parts.entities {
        match file_entities.get(key) {
            Some(file_value) if file_value == value => continue,
            _ => return false,
        }
    }

    true
}

/// The directories to search for an associated file, nearest first.
///
/// Always starts at the source file's own directory. When `inherit` is set, every ancestor
/// directory follows, ending at the dataset root — mirroring the TS validator's `walkBack`,
/// which ascends the full parent chain (`lib/bids-validator/src/files/inheritance.ts`).
/// Root is always included: some associations (e.g. `atlas_description`) live there
/// regardless of `inherit`.
fn search_dirs(path_parts: &[&str], inherit: bool) -> Vec<String> {
    let mut dir_paths: Vec<String> = Vec::new();

    if path_parts.len() >= 2 {
        dir_paths.push(format!("/{}", path_parts[..path_parts.len() - 1].join("/")));
    } else {
        dir_paths.push("/".to_string());
    }

    if inherit {
        // `i` indexes the deepest directory component of the ancestor, so `path_parts[..=i]`
        // is the ancestor's path. Index 0 is the top-level directory (e.g. `/sub-01`), not the
        // dataset root — root is appended separately below.
        for i in (0..path_parts.len().saturating_sub(2)).rev() {
            let dir = format!("/{}", path_parts[..=i].join("/"));
            if !dir_paths.contains(&dir) {
                dir_paths.push(dir);
            }
        }
    }

    if !dir_paths.contains(&"/".to_string()) {
        dir_paths.push("/".to_string());
    }

    dir_paths
}

/// Find an associated file for a given source file.
///
/// This is used for association types defined in `meta.associations` in the schema,
/// such as events files, channels files, etc.
pub fn find_associated_file(
    source: &BidsFile,
    tree: &FileTree,
    target_suffix: Option<&str>,
    target_extensions: &[&str],
    inherit: bool,
) -> Option<BidsFile> {
    let source_parts = read_entities(&source.name);
    let suffix = target_suffix.unwrap_or(&source_parts.suffix);

    let path_parts: Vec<&str> = source.path.split('/').filter(|s| !s.is_empty()).collect();

    let dir_paths = search_dirs(&path_parts, inherit);

    for dir_path in &dir_paths {
        let dir_tree = if dir_path == "/" {
            Some(tree)
        } else {
            tree.subtree(dir_path)
        };

        if let Some(dir) = dir_tree {
            for candidate in &dir.files {
                let cand_parts = read_entities(&candidate.name);

                // Check suffix
                if cand_parts.suffix != suffix {
                    continue;
                }

                // Check extension
                let ext_match = target_extensions.is_empty()
                    || target_extensions.iter().any(|e| *e == cand_parts.extension);
                if !ext_match {
                    continue;
                }

                // Check entities: candidate must have subset of source entities
                let mut entity_match = true;
                for (key, value) in &cand_parts.entities {
                    match source_parts.entities.get(key) {
                        Some(src_val) if src_val == value => continue,
                        _ => {
                            entity_match = false;
                            break;
                        }
                    }
                }

                if entity_match {
                    return Some(candidate.clone());
                }
            }
        }
    }

    None
}

/// Find ALL associated files matching the target, with specified entities
/// treated as "free" (not required to match the source).
///
/// This is used for associations like `coordsystems` where multiple files
/// with varying `space` entities should be collected.
pub fn find_all_associated_files(
    source: &BidsFile,
    tree: &FileTree,
    target_suffix: Option<&str>,
    target_extensions: &[&str],
    free_entities: &[&str],
    inherit: bool,
) -> Vec<BidsFile> {
    let source_parts = read_entities(&source.name);
    let suffix = target_suffix.unwrap_or(&source_parts.suffix);

    let path_parts: Vec<&str> = source.path.split('/').filter(|s| !s.is_empty()).collect();

    let dir_paths = search_dirs(&path_parts, inherit);

    let mut results = Vec::new();

    for dir_path in &dir_paths {
        let dir_tree = if dir_path == "/" {
            Some(tree)
        } else {
            tree.subtree(dir_path)
        };

        if let Some(dir) = dir_tree {
            for candidate in &dir.files {
                let cand_parts = read_entities(&candidate.name);

                if cand_parts.suffix != suffix {
                    continue;
                }

                let ext_match = target_extensions.is_empty()
                    || target_extensions.iter().any(|e| *e == cand_parts.extension);
                if !ext_match {
                    continue;
                }

                // Check entities: candidate entities must match source,
                // EXCEPT for "free" entities which are allowed to differ.
                let mut entity_match = true;
                for (key, value) in &cand_parts.entities {
                    if free_entities.contains(&key.as_str()) {
                        continue; // Skip free entities
                    }
                    match source_parts.entities.get(key) {
                        Some(src_val) if src_val == value => continue,
                        _ => {
                            entity_match = false;
                            break;
                        }
                    }
                }

                if entity_match {
                    results.push(candidate.clone());
                }
            }
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn test_is_override_matches_js_strict_inequality() {
        // Objects/arrays: reference inequality in JS — always an override, even when equal.
        assert!(is_override(&json!({"a": 1}), &json!({"a": 1})));
        assert!(is_override(&json!([1, 2]), &json!([1, 2])));
        assert!(is_override(&json!({"a": 1}), &json!({"a": 2})));
        // Primitives: compared by value.
        assert!(!is_override(&json!("x"), &json!("x")));
        assert!(is_override(&json!("x"), &json!("y")));
        assert!(!is_override(&json!(true), &json!(true)));
        assert!(!is_override(&Value::Null, &Value::Null));
        // JS numbers are all f64, so 2 === 2.0.
        assert!(!is_override(&json!(2), &json!(2.0)));
        assert!(is_override(&json!(2), &json!(3)));
    }

    #[test]
    fn test_matches_sidecar() {
        let mut entities = HashMap::new();
        entities.insert("sub".to_string(), "01".to_string());
        entities.insert("ses".to_string(), "pre".to_string());
        entities.insert("task".to_string(), "rest".to_string());

        // Exact match
        assert!(matches_sidecar(
            "sub-01_ses-pre_task-rest_bold.json",
            "bold",
            &entities,
        ));

        // Subset of entities
        assert!(matches_sidecar("task-rest_bold.json", "bold", &entities));

        // Root level (no entities)
        assert!(matches_sidecar("bold.json", "bold", &entities));

        // Wrong suffix
        assert!(!matches_sidecar("sub-01_T1w.json", "bold", &entities));

        // Extra entity not in file
        assert!(!matches_sidecar(
            "sub-01_ses-pre_task-rest_run-01_bold.json",
            "bold",
            &entities,
        ));
    }
}
