//! BIDS filename parsing — extract entities, suffix, extension from filenames.
//!
//! A BIDS filename has the form:
//! ```text
//! key1-value1_key2-value2_..._suffix.extension
//! ```
//! where keys are entity abbreviations (e.g. `sub`, `ses`, `acq`) and the
//! suffix describes the data type (e.g. `T1w`, `bold`, `events`).

use std::collections::HashMap;

/// The parsed components of a BIDS filename.
#[derive(Debug, Clone, PartialEq)]
pub struct BidsFileParts {
    /// The stem (filename without extension).
    pub stem: String,
    /// The suffix (last `_`-separated part of the stem, e.g. "T1w", "bold").
    pub suffix: String,
    /// The extension including the leading dot (e.g. ".nii.gz", ".json", ".tsv").
    pub extension: String,
    /// Entity key-value pairs parsed from the filename.
    /// Keys are the abbreviations as they appear in the filename (e.g. "sub", "ses", "acq").
    pub entities: HashMap<String, String>,
    /// The keys of the entities in the order they appear in the filename.
    pub entity_keys: Vec<String>,
}

/// Parse a BIDS filename into its component parts.
///
/// The extension is everything from the first `.` onward, matching the reference
/// TypeScript validator's `readEntities` (which does not consult the schema).
///
/// # Examples
/// ```
/// use bids_core::entities::read_entities;
///
/// let parts = read_entities("sub-01_ses-pre_T1w.nii.gz");
/// assert_eq!(parts.suffix, "T1w");
/// assert_eq!(parts.extension, ".nii.gz");
/// assert_eq!(parts.entities.get("sub"), Some(&"01".to_string()));
/// assert_eq!(parts.entities.get("ses"), Some(&"pre".to_string()));
/// ```
pub fn read_entities(filename: &str) -> BidsFileParts {
    let (stem, extension) = split_extension(filename);

    let parts: Vec<&str> = stem.split('_').collect();

    let mut entities = HashMap::new();
    let mut entity_keys = Vec::new();
    let suffix;

    if parts.is_empty() {
        suffix = String::new();
    } else {
        // The last underscore-separated part is the suffix (if it doesn't contain `-`).
        let last = parts[parts.len() - 1];
        if last.contains('-') {
            // No suffix — all parts are entities
            suffix = String::new();
            for part in &parts {
                parse_entity(part, &mut entities, &mut entity_keys);
            }
        } else {
            suffix = last.to_string();
            for part in &parts[..parts.len() - 1] {
                parse_entity(part, &mut entities, &mut entity_keys);
            }
        }
    }

    BidsFileParts {
        stem: stem.to_string(),
        suffix,
        extension: extension.to_string(),
        entities,
        entity_keys,
    }
}

/// Split a filename into stem and extension.
///
/// The extension is everything from the first `.` onward (so `.nii.gz` and
/// `.ome.tif` come through whole), mirroring the reference TS validator's
/// `readEntities`. BIDS labels and suffixes are alphanumeric, so the first dot
/// always begins the extension for valid filenames. Not schema-dependent.
fn split_extension(filename: &str) -> (&str, &str) {
    match filename.find('.') {
        Some(i) => (&filename[..i], &filename[i..]),
        None => (filename, ""),
    }
}

/// Parse a single `key-value` entity part.
fn parse_entity(part: &str, entities: &mut HashMap<String, String>, entity_keys: &mut Vec<String>) {
    if let Some(dash_pos) = part.find('-') {
        let key = &part[..dash_pos];
        let value = &part[dash_pos + 1..];
        if !key.is_empty() {
            entity_keys.push(key.to_string());
            if value.is_empty() {
                // Entity key with no value — mark as NOENTITY sentinel
                entities.insert(key.to_string(), "NOENTITY".to_string());
            } else {
                entities.insert(key.to_string(), value.to_string());
            }
        }
    }
    // Parts without `-` that aren't the last part are silently ignored
}

/// Resolve entity abbreviations from filename keys to schema keys.
///
/// Given a map like `{ "sub": "01", "ses": "pre" }` and a name-to-key mapping
/// like `{ "sub": "subject", "ses": "session" }`, returns `{ "subject": "01", "session": "pre" }`.
pub fn resolve_entities(
    file_entities: &HashMap<String, String>,
    name_to_key: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut resolved = HashMap::new();
    for (name, value) in file_entities {
        let key = name_to_key
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.clone());
        resolved.insert(key, value.clone());
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_filename() {
        let parts = read_entities("sub-01_T1w.nii.gz");
        assert_eq!(parts.suffix, "T1w");
        assert_eq!(parts.extension, ".nii.gz");
        assert_eq!(parts.entities.get("sub"), Some(&"01".to_string()));
        assert_eq!(parts.entities.len(), 1);
    }

    #[test]
    fn test_multiple_entities() {
        let parts = read_entities("sub-01_ses-pre_task-rest_run-02_bold.nii.gz");
        assert_eq!(parts.suffix, "bold");
        assert_eq!(parts.extension, ".nii.gz");
        assert_eq!(parts.entities.get("sub"), Some(&"01".to_string()));
        assert_eq!(parts.entities.get("ses"), Some(&"pre".to_string()));
        assert_eq!(parts.entities.get("task"), Some(&"rest".to_string()));
        assert_eq!(parts.entities.get("run"), Some(&"02".to_string()));
        assert_eq!(parts.entities.len(), 4);
    }

    #[test]
    fn test_json_sidecar() {
        let parts = read_entities("sub-01_T1w.json");
        assert_eq!(parts.suffix, "T1w");
        assert_eq!(parts.extension, ".json");
        assert_eq!(parts.entities.get("sub"), Some(&"01".to_string()));
    }

    #[test]
    fn test_tsv_file() {
        let parts = read_entities("sub-01_ses-01_task-rest_events.tsv");
        assert_eq!(parts.suffix, "events");
        assert_eq!(parts.extension, ".tsv");
    }

    #[test]
    fn test_no_entities() {
        let parts = read_entities("dataset_description.json");
        // "dataset_description" has no `-`, so entire stem becomes suffix
        assert_eq!(parts.suffix, "description");
        assert_eq!(parts.extension, ".json");
        // "dataset" part has no `-` either — it's not an entity
    }

    #[test]
    fn test_participants_tsv() {
        let parts = read_entities("participants.tsv");
        assert_eq!(parts.suffix, "participants");
        assert_eq!(parts.extension, ".tsv");
        assert!(parts.entities.is_empty());
    }

    #[test]
    fn test_entity_no_label() {
        let parts = read_entities("sub-_T1w.nii.gz");
        assert_eq!(parts.entities.get("sub"), Some(&"NOENTITY".to_string()));
    }

    #[test]
    fn test_compressed_tsv() {
        let parts = read_entities("sub-01_physio.tsv.gz");
        assert_eq!(parts.suffix, "physio");
        assert_eq!(parts.extension, ".tsv.gz");
    }

    #[test]
    fn test_resolve_entities() {
        let mut file_entities = HashMap::new();
        file_entities.insert("sub".to_string(), "01".to_string());
        file_entities.insert("ses".to_string(), "pre".to_string());

        let mut name_to_key = HashMap::new();
        name_to_key.insert("sub".to_string(), "subject".to_string());
        name_to_key.insert("ses".to_string(), "session".to_string());

        let resolved = resolve_entities(&file_entities, &name_to_key);
        assert_eq!(resolved.get("subject"), Some(&"01".to_string()));
        assert_eq!(resolved.get("session"), Some(&"pre".to_string()));
    }
}
