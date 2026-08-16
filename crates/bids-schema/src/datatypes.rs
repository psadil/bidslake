//! The BIDS vocabulary sets, read out of the raw schema `Value`: which entities, datatypes and
//! modalities exist.
//!
//! These are the *sets*, derived once per schema. Resolving a particular file against them —
//! which datatype a path sits in, which modality owns that datatype — belongs to
//! [`crate::context::SchemaIndex`], which turns these into hash lookups, over the rule in
//! [`bids_core::datatype`].

use serde_json::Value;

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

    /// The vocabulary the rest of the workspace keys on. A datatype or modality silently
    /// leaving these sets would make files stop resolving rather than fail loudly.
    #[test]
    fn the_schema_declares_the_expected_vocabulary() {
        let s = schema();

        let dts = datatypes(&s);
        for expected in ["anat", "func", "dwi", "eeg", "meg", "fmap"] {
            assert!(
                dts.iter().any(|d| d == expected),
                "missing datatype {expected}"
            );
        }

        let mods = modalities(&s);
        for expected in ["mri", "eeg", "meg"] {
            assert!(
                mods.iter().any(|m| m == expected),
                "missing modality {expected}"
            );
        }

        let ents = entities(&s);
        for expected in ["sub", "ses", "task", "run"] {
            assert!(
                ents.iter().any(|e| e.name == expected),
                "missing entity {expected}"
            );
        }
    }
}
