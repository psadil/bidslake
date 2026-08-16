//! Datatype and modality resolution from a file path + the raw BIDS schema `Value`.
//!
//! These read the schema directly (`objects.datatypes`, `rules.modalities`) rather than a
//! typed struct, so any consumer holding the raw schema JSON can use them.

use serde_json::Value;

/// The datatype of a file: the directory name directly above it, if that name is a known
/// datatype (`schema.objects.datatypes`). e.g. `/sub-01/anat/sub-01_T1w.nii.gz` → `anat`;
/// `/dataset_description.json` and `/participants.tsv` → `None`.
///
/// The path half of the rule is [`bids_core::datatype::parent_dir`] — the one place it is
/// spelled. This form reads the datatype set out of the schema on every call, which suits a
/// caller holding only the raw schema and looking up a handful of paths. A per-file loop should
/// derive the set once and use [`bids_core::datatype::parent_datatype`] instead (which is what
/// [`crate::context::SchemaIndex`] does).
pub fn find_datatype(path: &str, schema: &Value) -> Option<String> {
    let parent = bids_core::datatype::parent_dir(path)?;
    schema
        .get("objects")
        .and_then(|o| o.get("datatypes"))
        .and_then(|d| d.as_object())
        .filter(|dts| dts.contains_key(parent))
        .map(|_| parent.to_string())
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

    /// The schema-reading form and the set-reading form are the same rule, so they must agree
    /// everywhere. This replaces bidslake's `is_datafile_agrees_with_find_datatype`, which
    /// pinned a *second* implementation of the rule; there is only one now, and this asserts
    /// that the two entry points onto it stay interchangeable.
    #[test]
    fn find_datatype_agrees_with_parent_datatype() {
        let s = schema();
        let set: std::collections::HashSet<String> = datatypes(&s).into_iter().collect();
        for path in [
            "sub-01/anat/sub-01_T1w.nii.gz",
            "sub-01/func/sub-01_task-rest_bold.nii.gz",
            "sub-01/ses-1/eeg/sub-01_ses-1_task-x_eeg.vhdr",
            "sub-01/meg/sub-01_task-x_meg.ds",
            "derivatives/fmriprep/sub-01/anat/sub-01_desc-preproc_T1w.nii.gz",
            "anat/loose.nii.gz",
            "sub-01/anat/extra/nested.nii.gz",
            "sub-01/sub-01_scans.tsv",
            "dataset_description.json",
            "README",
        ] {
            assert_eq!(
                find_datatype(path, &s).as_deref(),
                bids_core::datatype::parent_datatype(path, &set),
                "disagreement on {path}"
            );
        }
    }
}
