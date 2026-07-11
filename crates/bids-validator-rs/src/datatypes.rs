//! Datatype and modality resolution from file paths and schema.

use crate::schema::BidsSchema;

/// Extract the datatype from a file path.
///
/// The datatype is the directory name directly above the file (e.g. "anat", "func", "eeg")
/// if that directory name matches a known datatype in the schema.
///
/// For a path like `/sub-01/anat/sub-01_T1w.nii.gz`, the datatype is `anat`.
/// For a path like `/dataset_description.json`, there is no datatype.
pub fn find_datatype(path: &str, schema: &BidsSchema) -> Option<String> {
    let known_datatypes = &schema.known_datatypes;
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    // The datatype directory is typically the one right before the filename.
    // Walk from the bottom up, looking for a known datatype.
    if parts.len() < 2 {
        return None;
    }

    // Check the second-to-last part (directory containing the file)
    let parent_dir = parts[parts.len() - 2];
    if known_datatypes.iter().any(|dt| dt == parent_dir) {
        return Some(parent_dir.to_string());
    }

    None
}

/// Find the modality for a given datatype.
///
/// The schema defines `rules.modalities` as a mapping of modality names
/// to lists of datatypes. This function performs the reverse lookup.
///
/// Example: `anat` → `mri`, `eeg` → `eeg`, `pet` → `pet`.
pub fn find_modality(dt_name: &str, schema: &BidsSchema) -> Option<String> {
    let modalities = &schema.modalities;
    for (mod_name, modality) in modalities.iter() {
        if modality.datatypes.iter().any(|dt| dt == dt_name) {
            return Some(mod_name.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_datatype() {
        let schema = BidsSchema::bundled().unwrap();
        assert_eq!(
            find_datatype("/sub-01/anat/sub-01_T1w.nii.gz", &schema),
            Some("anat".to_string())
        );
        assert_eq!(
            find_datatype("/sub-01/func/sub-01_task-rest_bold.nii.gz", &schema),
            Some("func".to_string())
        );
        assert_eq!(find_datatype("/dataset_description.json", &schema), None);
        assert_eq!(find_datatype("/participants.tsv", &schema), None);
    }

    #[test]
    fn test_find_modality() {
        let schema = BidsSchema::bundled().unwrap();
        assert_eq!(find_modality("anat", &schema), Some("mri".to_string()));
        assert_eq!(find_modality("func", &schema), Some("mri".to_string()));
    }
}
