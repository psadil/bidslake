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

/// One BIDS entity's short `name` and value `format` (e.g. `"index"` / `"label"`),
/// from `objects.entities`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaEntity {
    /// The short entity key used in filenames, e.g. `sub`, `ses`, `task`.
    pub name: String,
    /// The entity's declared value `format` (`"index"` for integer entities,
    /// `"label"` otherwise), if the schema specifies one.
    pub format: Option<String>,
}

/// Every entity in `objects.entities`, sorted by `name` and de-duplicated by name.
/// The single source of truth for the set of BIDS entities, shared by bidslake's
/// generated columns and the Python type codegen so they cannot disagree.
pub fn entities(schema: &Value) -> Vec<SchemaEntity> {
    let mut v: Vec<SchemaEntity> = schema
        .get("objects")
        .and_then(|o| o.get("entities"))
        .and_then(|e| e.as_object())
        .map(|m| {
            m.values()
                .filter_map(|e| {
                    let name = e.get("name")?.as_str()?.to_string();
                    let format = e.get("format").and_then(|f| f.as_str()).map(String::from);
                    Some(SchemaEntity { name, format })
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v.dedup_by(|a, b| a.name == b.name);
    v
}

/// The BIDS datatype directory names (`func`, `anat`, `eeg`, `phenotype`, …) from
/// `objects.datatypes`, sorted. The single source of truth for the datatype set.
pub fn datatypes(schema: &Value) -> Vec<String> {
    let mut v: Vec<String> = schema
        .get("objects")
        .and_then(|o| o.get("datatypes"))
        .and_then(|d| d.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// The modality names (`mri`, `eeg`, …) from `rules.modalities`, sorted. The
/// single source of truth for the modality set.
pub fn modalities(schema: &Value) -> Vec<String> {
    let mut v: Vec<String> = schema
        .get("rules")
        .and_then(|r| r.get("modalities"))
        .and_then(|m| m.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    v.sort();
    v
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
