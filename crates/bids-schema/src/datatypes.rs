//! Datatype and modality resolution from a file path + the raw BIDS schema `Value`.
//!
//! These read the schema directly (`objects.datatypes`, `rules.modalities`) rather than a
//! typed struct, so any consumer holding the raw schema JSON can use them.

use serde_json::Value;

/// The datatype of a file: the directory name directly above it, if that name is a known
/// datatype (`schema.objects.datatypes`). e.g. `/sub-01/anat/sub-01_T1w.nii.gz` → `anat`;
/// `/dataset_description.json` and `/participants.tsv` → `None`.
pub fn find_datatype(path: &str, schema: &Value) -> Option<String> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    let parent = parts[parts.len() - 2];
    let known = schema
        .get("objects")
        .and_then(|o| o.get("datatypes"))
        .and_then(|d| d.as_object());
    match known {
        Some(dts) if dts.contains_key(parent) => Some(parent.to_string()),
        _ => None,
    }
}

/// The modality whose `datatypes` list (`schema.rules.modalities`) contains `dt_name`
/// (reverse lookup). e.g. `anat` → `mri`, `eeg` → `eeg`.
pub fn find_modality(dt_name: &str, schema: &Value) -> Option<String> {
    let mods = schema
        .get("rules")
        .and_then(|r| r.get("modalities"))
        .and_then(|m| m.as_object())?;
    for (mod_name, def) in mods {
        if let Some(dts) = def.get("datatypes").and_then(|d| d.as_array())
            && dts.iter().any(|d| d.as_str() == Some(dt_name))
        {
            return Some(mod_name.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        serde_json::from_str(crate::SCHEMA_JSON).unwrap()
    }

    #[test]
    fn test_find_datatype() {
        let s = schema();
        assert_eq!(
            find_datatype("/sub-01/anat/sub-01_T1w.nii.gz", &s),
            Some("anat".to_string())
        );
        assert_eq!(
            find_datatype("/sub-01/func/sub-01_task-rest_bold.nii.gz", &s),
            Some("func".to_string())
        );
        assert_eq!(find_datatype("/dataset_description.json", &s), None);
        assert_eq!(find_datatype("/participants.tsv", &s), None);
    }

    #[test]
    fn test_find_modality() {
        let s = schema();
        assert_eq!(find_modality("anat", &s), Some("mri".to_string()));
        assert_eq!(find_modality("func", &s), Some("mri".to_string()));
    }
}
