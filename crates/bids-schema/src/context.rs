//! The per-file selector context used to evaluate BIDS schema expressions.
//!
//! [`FileContext`] is everything about a file that follows from its name, its path and the
//! schema — no I/O. Both the validator and bidslake derive one per file and then read the
//! fields they need, rather than each re-deriving the same facts; and
//! [`FileContext::to_selector_value`] renders the single `{ path, extension, suffix, datatype,
//! modality, entities }` object they feed to
//! [`expression::do_selectors_select`](crate::expression::do_selectors_select).
//!
//! The lookups that derivation needs depend only on the schema, so they live in
//! [`SchemaIndex`], built once and passed in.

use bids_core::entities::{BidsFileParts, read_entities, resolve_entities};
use bids_core::filetree::BidsFile;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

/// Map each entity's abbreviation (`entity`, falling back to `name`) → its schema object-key,
/// from `schema.objects.entities`. e.g. `"sub" → "subject"`. Used to lift a filename's raw
/// entity keys into the schema's namespace, matching the reference validator.
pub fn entity_name_to_key(schema: &Value) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(ents) = schema
        .get("objects")
        .and_then(|o| o.get("entities"))
        .and_then(|e| e.as_object())
    {
        for (key, def) in ents {
            let abbr = def
                .get("entity")
                .and_then(|v| v.as_str())
                .or_else(|| def.get("name").and_then(|v| v.as_str()));
            if let Some(abbr) = abbr {
                map.insert(abbr.to_string(), key.clone());
            }
        }
    }
    map
}

/// The schema-derived lookups [`FileContext::derive`] needs, in a form that answers in one hash
/// probe.
///
/// Every field depends only on the schema, never on the file, so callers build this **once**
/// and pass it in for every file rather than re-walking the schema JSON per file. Without it,
/// each file costs two B-tree descents for its datatype and a linear scan of `rules.modalities`
/// for its modality.
#[derive(Debug, Clone)]
pub struct SchemaIndex {
    entity_name_to_key: HashMap<String, String>,
    datatypes: HashSet<String>,
    datatype_to_modality: HashMap<String, String>,
}

impl SchemaIndex {
    /// Derive the lookups from the raw schema JSON.
    pub fn new(schema: &Value) -> Self {
        let datatypes: HashSet<String> = crate::datatypes::datatypes(schema).into_iter().collect();

        // `rules.modalities` is modality → datatypes, so this inverts it; nothing in the schema
        // forbids a datatype appearing under two modalities, and the inversion has to answer
        // deterministically if one ever does. `or_insert` keeps the first in iteration order,
        // and `serde_json::Map` is a `BTreeMap` here (the workspace does not enable
        // `preserve_order`), so that means the alphabetically-first modality name.
        let mut datatype_to_modality = HashMap::new();
        if let Some(mods) = schema
            .get("rules")
            .and_then(|r| r.get("modalities"))
            .and_then(|m| m.as_object())
        {
            for (mod_name, def) in mods {
                let Some(dts) = def.get("datatypes").and_then(|d| d.as_array()) else {
                    continue;
                };
                for dt in dts.iter().filter_map(|d| d.as_str()) {
                    datatype_to_modality
                        .entry(dt.to_string())
                        .or_insert_with(|| mod_name.clone());
                }
            }
        }

        Self {
            entity_name_to_key: entity_name_to_key(schema),
            datatypes,
            datatype_to_modality,
        }
    }

    /// The entity abbreviation → schema-key map, e.g. `"sub" → "subject"`.
    pub fn entity_name_to_key(&self) -> &HashMap<String, String> {
        &self.entity_name_to_key
    }

    /// The schema's datatype directory names (`anat`, `func`, `eeg`, …).
    pub fn datatypes(&self) -> &HashSet<String> {
        &self.datatypes
    }

    /// The datatype `path` sits in, if its immediate parent directory names one. Borrowed from
    /// `path`, so a per-file loop pays no allocation; the rule itself is
    /// [`bids_core::datatype::parent_datatype`].
    pub fn datatype<'a>(&self, path: &'a str) -> Option<&'a str> {
        bids_core::datatype::parent_datatype(path, &self.datatypes)
    }

    /// The modality owning `datatype` — the reverse of `rules.modalities`, as one hash lookup.
    pub fn modality(&self, datatype: &str) -> Option<&str> {
        self.datatype_to_modality.get(datatype).map(String::as_str)
    }
}

/// Everything about a file that follows from its name, its path and the schema — no I/O.
///
/// Derived **once** per file. The caller reads the typed fields it needs and renders the
/// selector subset with [`to_selector_value`](Self::to_selector_value); deriving the facts a
/// second time to build that JSON is the bug this type exists to prevent.
#[derive(Debug, Clone)]
pub struct FileContext {
    /// The file's path relative to the dataset root.
    pub path: String,
    /// The parsed filename: stem, suffix, extension, raw entities, and their order.
    pub parts: BidsFileParts,
    /// `parts.entities` lifted into the schema's key namespace (`subject`, not `sub`).
    pub entities: HashMap<String, String>,
    /// The datatype directory the file sits in, if it names a known datatype.
    pub datatype: Option<String>,
    /// The modality owning `datatype`.
    pub modality: Option<String>,
}

impl FileContext {
    /// Derive the facts for `file` against a pre-built [`SchemaIndex`].
    pub fn derive(file: &BidsFile, index: &SchemaIndex) -> Self {
        let datatype = index.datatype(&file.path).map(str::to_string);
        let modality = datatype
            .as_deref()
            .and_then(|dt| index.modality(dt))
            .map(str::to_string);
        let parts = read_entities(&file.name);
        let entities = resolve_entities(&parts.entities, index.entity_name_to_key());

        Self {
            path: file.path.clone(),
            parts,
            entities,
            datatype,
            modality,
        }
    }

    /// The `{ path, extension, suffix, datatype, modality, entities }` object fed to
    /// [`do_selectors_select`](crate::expression::do_selectors_select) — the *selector subset*
    /// of these facts.
    ///
    /// Contrast the validator's `BidsContext::to_file_value`, which renders the full `file`
    /// scope (sidecar, columns, headers, associations) once those have been read. The shared
    /// `dataset`/`schema`/`subject` scopes are left to the caller's
    /// [`EvalContext`](crate::expression::EvalContext).
    pub fn to_selector_value(&self) -> Value {
        json!({
            "path": self.path,
            "extension": self.parts.extension,
            "suffix": self.parts.suffix,
            "datatype": self.datatype,
            "modality": self.modality,
            "entities": self.entities,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::path::PathBuf;

    fn schema() -> Value {
        serde_json::from_str(crate::SCHEMA_JSON).unwrap()
    }

    fn file(path: &str) -> BidsFile {
        BidsFile {
            name: path.rsplit('/').next().unwrap().to_string(),
            path: path.to_string(),
            absolute_path: PathBuf::new(),
        }
    }

    /// The selector context's shape is a contract with every `meta.associations` and
    /// `rules.checks` selector in the schema: the key names, the fact that entities arrive in
    /// the schema's *key* namespace (`subject`, not `sub`), and — for a file outside a datatype
    /// directory — that `datatype`/`modality` are present and **null** rather than absent.
    /// `EvalContext::get` distinguishes those two (`Some(&Null)` vs `None`), and selectors such
    /// as `datatype == "func"` see different things, so the null case is pinned deliberately.
    #[test]
    fn selector_value_shape_is_pinned() {
        let index = SchemaIndex::new(&schema());

        let bold = file("/sub-01/func/sub-01_task-rest_run-01_bold.nii.gz");
        assert_eq!(
            FileContext::derive(&bold, &index).to_selector_value(),
            json!({
                "path": "/sub-01/func/sub-01_task-rest_run-01_bold.nii.gz",
                "extension": ".nii.gz",
                "suffix": "bold",
                "datatype": "func",
                "modality": "mri",
                "entities": { "subject": "01", "task": "rest", "run": "01" },
            })
        );

        let desc = file("/dataset_description.json");
        assert_eq!(
            FileContext::derive(&desc, &index).to_selector_value(),
            json!({
                "path": "/dataset_description.json",
                "extension": ".json",
                "suffix": "description",
                "datatype": null,
                "modality": null,
                "entities": {},
            })
        );

        // `.bval` must survive as the extension intact — association resolution keys on it.
        let bval = file("/sub-01/dwi/sub-01_dwi.bval");
        assert_eq!(
            FileContext::derive(&bval, &index).to_selector_value(),
            json!({
                "path": "/sub-01/dwi/sub-01_dwi.bval",
                "extension": ".bval",
                "suffix": "dwi",
                "datatype": "dwi",
                "modality": "mri",
                "entities": { "subject": "01" },
            })
        );
    }

    // `the_index_resolves_datatype` lived here, restating the eight corpus shapes that
    // `bids_core::datatype::tests::parent_datatype_over_the_corpus_shapes` already covers —
    // `SchemaIndex::datatype` is a one-line delegation to it. What is specific to this crate
    // is that the *real* schema fills `SchemaIndex.datatypes`, and that is pinned by
    // `datatypes::tests::the_schema_declares_the_expected_vocabulary` plus the end-to-end
    // `selector_value_shape_is_pinned` below.

    #[rstest]
    #[case("anat", Some("mri"))]
    #[case("func", Some("mri"))]
    #[case("dwi", Some("mri"))]
    #[case("fmap", Some("mri"))]
    #[case("eeg", Some("eeg"))]
    #[case("meg", Some("meg"))]
    #[case::not_a_datatype("not-a-datatype", None)]
    fn the_index_resolves_modality(#[case] datatype: &str, #[case] expected: Option<&str>) {
        let index = SchemaIndex::new(&schema());

        assert_eq!(index.modality(datatype), expected, "modality of {datatype}");
    }

    /// `objects.datatypes` and `rules.modalities` do not cover the same set: `phenotype` is
    /// a datatype that belongs to no modality, so `modality()` answers `None` for a file
    /// that has a perfectly good datatype. Pinned because it is easy to assume otherwise,
    /// and because a *second* uncovered datatype appearing should be a decision, not a
    /// silent `None` reaching `dataset.modalities` and the rules gated on it.
    #[test]
    fn phenotype_is_the_only_datatype_with_no_modality() {
        let schema = schema();
        let index = SchemaIndex::new(&schema);

        let uncovered: Vec<String> = crate::datatypes::datatypes(&schema)
            .into_iter()
            .filter(|dt| index.modality(dt).is_none())
            .collect();

        assert_eq!(uncovered, ["phenotype"]);
    }
}
