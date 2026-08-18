//! **Naming**: rendering a BIDS filename from concepts — the counterpart of
//! [`bids_core::entities::read_entities`].
//!
//! Every direction bidslake had was a reading one. `read_entities` parses a name, a
//! [term map](crate::term_map) projects a path, and a [layout](crate::layout) renders a *role* in
//! a tree that is not BIDS at all. Nothing rendered an ordinary BIDS filename, so anything that
//! needed one — a synthetic dataset, a fixture, a path a pipeline is about to write — spelled it
//! as a format string and carried the entity order in a programmer's head.
//!
//! This module is the missing half, and the law it exists to satisfy is
//! `read_entities(render(x)) == x`. That is a real law and not a tautology, because the two
//! implementations live in different crates and neither consults the other:
//! [`bids_core::entities`] is deliberately schema-agnostic, while everything here is read out of
//! the schema `Value`.
//!
//! ## Why it takes an index rather than the schema
//!
//! [`NameIndex::new`] walks `rules.entities` and `objects.entities` once and answers in one hash
//! probe thereafter, the same bargain [`crate::context::SchemaIndex`] makes. A caller rendering a
//! hundred thousand names would otherwise re-walk the schema a hundred thousand times.
//!
//! ## What "the entity order" means once overlays are in play
//!
//! `rules.entities` is the canonical order, and it lists entities by their **long** object key
//! (`subject`, `description`), while a filename carries the **short** one
//! (`objects.entities.subject.name == "sub"`). Bundled overlays key their additions *short*
//! already (`from`, `to`, `mode`, `seg`, `parc`) and none of them extends `rules.entities`, so an
//! overlay entity has no canonical position at all. Those sort after every canonical one,
//! lexicographically — lexicographically rather than in document order because
//! [`crate::overlay::merge_into`] promises nothing about key order, and a caller generating a tree
//! twice must get the same bytes both times.
//!
//! ## When two objects claim one key
//!
//! Two `objects.entities` objects can render the same filename key, and the merge is additive so
//! neither shadows the other. Whether that matters depends entirely on whether they *agree*:
//!
//! - `segmentation` (base) and `seg` (the freesurfer overlay) both render `seg` and declare the
//!   same constraints — `format: label`, no `enum`. Nothing about rendering depends on which one
//!   answers, so this resolves silently and keeps `segmentation`'s canonical position.
//! - Two claimants that *disagree* have no right answer, so [`NameIndex`] records the key as
//!   ambiguous and rendering it is [`NamingError::AmbiguousKey`] rather than a coin flip.
//!
//! The bundled overlays exhibit only the first case today, and the second is worth keeping
//! because they briefly exhibited it too. The freesurfer overlay used to declare an unconstrained
//! `hemi` beside base's `hemisphere`, whose `enum` is `["L", "R"]` — because FreeSurfer names a
//! hemisphere `lh`/`rh`. That was fixed where it belonged, in the projection: the term map now
//! *declares* `hemi: L` / `hemi: R` per mapping instead of capturing the filename token, so the
//! label reaching the catalog is one BIDS allows and one entity object covers both producers.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::Value;

/// An error rendering a BIDS name.
///
/// Every variant is a case where rendering anyway would produce a string
/// [`bids_core::entities::read_entities`] reads back as something else — a silently wrong
/// filename, which is worse than no filename at all.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NamingError {
    /// The effective schema declares no entity rendering as this key.
    ///
    /// Usually a missing overlay rather than a typo: `from`/`to`/`mode` exist only once the
    /// `fmriprep` (or `qsiprep`, or `feat`) overlay is merged, so a caller that forgot one asks
    /// for a transform's entities against a schema that has never heard of them. Failing here is
    /// how that gets noticed, instead of at index time when the column is missing.
    #[error("no entity in the effective schema renders as {key:?}")]
    UnknownEntity {
        /// The short key that was asked for, as it would have appeared in the filename.
        key: String,
    },
    /// Two `objects.entities` objects render this key with *different* constraints, so there is
    /// no single right answer. No bundled overlay produces one today — see the module docs for
    /// the case that used to, and how it was resolved.
    #[error(
        "entity key {key:?} is declared by more than one schema object with conflicting \
         constraints ({objects:?}); it cannot be rendered unambiguously"
    )]
    AmbiguousKey {
        /// The contested short key.
        key: String,
        /// The `objects.entities` keys that claim it, sorted, so the message names both documents
        /// an author would have to reconcile.
        objects: Vec<String>,
    },
    /// The entity declares an `enum` and the value is not in it.
    #[error("entity {key}-{value} is not one of {allowed:?}")]
    NotInEnum {
        /// The entity's short key.
        key: String,
        /// The rejected value.
        value: String,
        /// The declared `enum`, in schema order.
        allowed: Vec<String>,
    },
    /// The entity is `format: index` and the value is not a non-empty run of digits.
    ///
    /// Zero-padding is *not* checked, because BIDS does not specify it: `run-1` and `run-01` are
    /// both valid, and choosing between them is the caller's business.
    #[error("entity {key} has format `index`, so {value:?} must be one or more digits")]
    BadIndex {
        /// The entity's short key.
        key: String,
        /// The rejected value.
        value: String,
    },
    /// The value is not a non-empty BIDS label (`[0-9A-Za-z]+`).
    ///
    /// An empty label is rejected along with the rest: `sub-_T1w.nii.gz` parses back to the
    /// `NOENTITY` sentinel rather than to an empty string, so it breaks the round trip.
    #[error("entity {key} value {value:?} is not a BIDS label ([0-9A-Za-z]+)")]
    BadLabel {
        /// The entity's short key.
        key: String,
        /// The rejected value.
        value: String,
    },
    /// The suffix is empty or carries a character that changes how the name parses.
    ///
    /// `.` would be swallowed by the extension split, which takes everything from the *first*
    /// dot; `-` would make the last `_` segment read as a trailing entity and leave the file with
    /// no suffix; `_` would split it into two segments.
    #[error("suffix {suffix:?} is not a BIDS suffix ([0-9A-Za-z]+)")]
    BadSuffix {
        /// The rejected suffix.
        suffix: String,
    },
    /// The extension is non-empty and does not begin with `.`.
    ///
    /// Without the dot it fuses onto the suffix — `bold` plus `nii.gz` renders `boldnii.gz`, which
    /// reads back with suffix `boldnii`.
    #[error("extension {extension:?} must be empty or begin with `.`")]
    BadExtension {
        /// The rejected extension.
        extension: String,
    },
    /// The schema declares no datatype directory of this name.
    #[error("{datatype:?} is not a datatype in the effective schema")]
    UnknownDatatype {
        /// The rejected datatype.
        datatype: String,
    },
    /// A datatype directory was named but no `sub` entity, so there is no subject directory to
    /// put it under.
    ///
    /// The dataset-root shape is legitimate — `rules.files.deriv.atlas.atlas_description` and
    /// `rules.files.deriv.tables.descriptions` both name files with no subject — but those name
    /// no datatype either. A datatype without a subject is a caller that half-filled the name.
    #[error("datatype {datatype:?} was named without a `sub` entity, so the file has no home")]
    RootFileWithDatatype {
        /// The datatype that has nowhere to go.
        datatype: String,
    },
}

/// What rendering one entity has to know: where it sorts, and what values it admits.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EntitySpec {
    /// `objects.entities.<x>.format`, `None` when the schema omits it (treated as `label`).
    format: Option<String>,
    /// `objects.entities.<x>.enum`, in schema order.
    enum_values: Option<Vec<String>>,
}

/// The schema-derived lookups [`BidsName::render`] needs, in a form that answers in one hash
/// probe.
///
/// Built once per schema and shared across every name rendered against it. Holding the datatype
/// set here too, rather than reaching for [`crate::context::SchemaIndex`], keeps the write
/// direction from depending on the read one for a single `HashSet`.
#[derive(Debug, Clone)]
pub struct NameIndex {
    /// Short key → its index in `rules.entities`. Absent for an overlay-added entity, which is
    /// what sorts it after every canonical one.
    order: HashMap<String, usize>,
    /// Short key → the constraints its value must satisfy. Absent for an ambiguous key, so that
    /// a lookup miss and a conflict are not the same answer.
    specs: HashMap<String, EntitySpec>,
    /// Short keys claimed by two schema objects that disagree (see the module docs), mapped to
    /// the claiming object keys for the error message.
    ambiguous: BTreeMap<String, Vec<String>>,
    /// `objects.datatypes` keys.
    datatypes: BTreeSet<String>,
}

impl NameIndex {
    /// Derive the lookups from a schema `Value` — the *effective* one, overlays already merged,
    /// since an overlay is where `from`/`to`/`mode` come from.
    pub fn new(schema: &Value) -> Self {
        let entities = schema
            .get("objects")
            .and_then(|o| o.get("entities"))
            .and_then(|e| e.as_object());

        // Collect every claim on each short key first, so a conflict is visible before anything
        // is committed. `objects.entities` is a `BTreeMap` here, so `claims` is in object-key
        // order and the error message is stable.
        let mut claims: BTreeMap<String, Vec<(String, EntitySpec)>> = BTreeMap::new();
        if let Some(entities) = entities {
            for (object_key, def) in entities {
                let Some(name) = def.get("name").and_then(|v| v.as_str()) else {
                    continue;
                };
                let spec = EntitySpec {
                    format: def
                        .get("format")
                        .and_then(|f| f.as_str())
                        .map(str::to_string),
                    enum_values: def.get("enum").and_then(|e| e.as_array()).map(|vs| {
                        vs.iter()
                            .filter_map(|v| v.as_str())
                            .map(str::to_string)
                            .collect()
                    }),
                };
                claims
                    .entry(name.to_string())
                    .or_default()
                    .push((object_key.clone(), spec));
            }
        }

        let mut specs = HashMap::new();
        let mut ambiguous = BTreeMap::new();
        for (name, mut claimants) in claims {
            // Two objects agreeing on the constraints is not a conflict: nothing about rendering
            // depends on which one answers. `segmentation`/`seg` is that case.
            let distinct: Vec<&EntitySpec> = {
                let mut seen: Vec<&EntitySpec> = Vec::new();
                for (_, spec) in &claimants {
                    if !seen.contains(&spec) {
                        seen.push(spec);
                    }
                }
                seen
            };
            if distinct.len() > 1 {
                let mut objects: Vec<String> = claimants.drain(..).map(|(k, _)| k).collect();
                objects.sort();
                ambiguous.insert(name, objects);
            } else if let Some((_, spec)) = claimants.into_iter().next() {
                specs.insert(name, spec);
            }
        }

        // `rules.entities` orders long object keys; translate each to the key a filename carries.
        // An entity named there but absent from `objects.entities` cannot be rendered anyway, so
        // it simply contributes no position.
        let mut order = HashMap::new();
        if let (Some(list), Some(entities)) = (
            schema
                .get("rules")
                .and_then(|r| r.get("entities"))
                .and_then(|e| e.as_array()),
            entities,
        ) {
            for (position, object_key) in list.iter().filter_map(|v| v.as_str()).enumerate() {
                if let Some(name) = entities
                    .get(object_key)
                    .and_then(|d| d.get("name"))
                    .and_then(|v| v.as_str())
                {
                    order.entry(name.to_string()).or_insert(position);
                }
            }
        }

        Self {
            order,
            specs,
            ambiguous,
            datatypes: crate::datatypes::datatypes(schema).into_iter().collect(),
        }
    }

    /// Where `key` sorts among the canonical entities, or `None` for an overlay-added one.
    pub fn position(&self, key: &str) -> Option<usize> {
        self.order.get(key).copied()
    }

    /// Whether the effective schema declares an entity rendering as `key`, ambiguous or not.
    pub fn declares(&self, key: &str) -> bool {
        self.specs.contains_key(key) || self.ambiguous.contains_key(key)
    }

    /// Check one `key-value` pair against the schema's constraints.
    fn check(&self, key: &str, value: &str) -> Result<(), NamingError> {
        if let Some(objects) = self.ambiguous.get(key) {
            return Err(NamingError::AmbiguousKey {
                key: key.to_string(),
                objects: objects.clone(),
            });
        }
        let Some(spec) = self.specs.get(key) else {
            return Err(NamingError::UnknownEntity {
                key: key.to_string(),
            });
        };

        if let Some(allowed) = &spec.enum_values
            && !allowed.iter().any(|a| a == value)
        {
            return Err(NamingError::NotInEnum {
                key: key.to_string(),
                value: value.to_string(),
                allowed: allowed.clone(),
            });
        }

        if spec.format.as_deref() == Some("index") {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(NamingError::BadIndex {
                    key: key.to_string(),
                    value: value.to_string(),
                });
            }
        } else if !is_label(value) {
            return Err(NamingError::BadLabel {
                key: key.to_string(),
                value: value.to_string(),
            });
        }
        Ok(())
    }
}

/// A BIDS label: one or more ASCII alphanumerics, and nothing else.
///
/// The alphabet is what makes the round trip hold: `_` would split the segment, `-` would move
/// the key/value boundary, and `.` would start the extension.
fn is_label(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A BIDS filename under construction.
///
/// Entities are held under the **short** key a filename actually carries (`sub`, `desc`), never
/// the long `objects.entities` key (`subject`, `description`), because the short one is what has
/// to come back out of [`bids_core::entities::read_entities`].
///
/// Nothing is validated until [`render`](Self::render): a builder that failed on `.entity()`
/// would have to return a `Result` from every call, and the caller would then be checking the
/// same schema facts the renderer is about to check anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BidsName {
    entities: BTreeMap<String, String>,
    suffix: String,
    extension: String,
    datatype: Option<String>,
}

impl BidsName {
    /// A name with a suffix and an extension and no entities yet.
    ///
    /// `extension` includes the leading dot (`.nii.gz`), matching
    /// [`bids_core::entities::BidsFileParts::extension`] and `objects.extensions.*.value`.
    pub fn new(suffix: impl Into<String>, extension: impl Into<String>) -> Self {
        Self {
            entities: BTreeMap::new(),
            suffix: suffix.into(),
            extension: extension.into(),
            datatype: None,
        }
    }

    /// Set an entity by its short key. Setting the same key twice keeps the last value.
    #[must_use]
    pub fn entity(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.entities.insert(key.into(), value.into());
        self
    }

    /// Set the datatype directory this file sits in. Only [`render_path`](Self::render_path)
    /// reads it; a datatype is a position in the tree, not part of the filename.
    #[must_use]
    pub fn datatype(mut self, datatype: impl Into<String>) -> Self {
        self.datatype = Some(datatype.into());
        self
    }

    /// The entity value under `key`, if set.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entities.get(key).map(String::as_str)
    }

    /// Render the filename alone: `key-value` pairs in canonical order, then the suffix, then
    /// the extension.
    pub fn render(&self, index: &NameIndex) -> Result<String, NamingError> {
        if !is_label(&self.suffix) {
            return Err(NamingError::BadSuffix {
                suffix: self.suffix.clone(),
            });
        }
        if !self.extension.is_empty() && !self.extension.starts_with('.') {
            return Err(NamingError::BadExtension {
                extension: self.extension.clone(),
            });
        }

        // Canonical entities first, in `rules.entities` order; then the rest by key. `usize::MAX`
        // is safe as the "no position" sentinel because `rules.entities` has 35 elements and a
        // real position is its index.
        let mut ordered: Vec<(&String, &String)> = self.entities.iter().collect();
        ordered.sort_by_key(|(key, _)| (index.position(key).unwrap_or(usize::MAX), (*key).clone()));

        let mut out = String::new();
        for (key, value) in ordered {
            index.check(key, value)?;
            out.push_str(key);
            out.push('-');
            out.push_str(value);
            out.push('_');
        }
        out.push_str(&self.suffix);
        out.push_str(&self.extension);
        Ok(out)
    }

    /// Render the dataset-relative path: `sub-X/[ses-Y/][<datatype>/]<filename>`.
    ///
    /// With no `sub` entity the file belongs at the dataset root, which is a real BIDS shape —
    /// `atlas_description` and the derivative `descriptions` table both live there — and is why
    /// this is not simply an error.
    pub fn render_path(&self, index: &NameIndex) -> Result<String, NamingError> {
        let filename = self.render(index)?;
        if let Some(datatype) = &self.datatype
            && !index.datatypes.contains(datatype)
        {
            return Err(NamingError::UnknownDatatype {
                datatype: datatype.clone(),
            });
        }

        let Some(subject) = self.get("sub") else {
            return match &self.datatype {
                Some(datatype) => Err(NamingError::RootFileWithDatatype {
                    datatype: datatype.clone(),
                }),
                None => Ok(filename),
            };
        };

        let mut path = format!("sub-{subject}/");
        if let Some(session) = self.get("ses") {
            path.push_str(&format!("ses-{session}/"));
        }
        if let Some(datatype) = &self.datatype {
            path.push_str(datatype);
            path.push('/');
        }
        path.push_str(&filename);
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bids_core::entities::read_entities;
    use proptest::prelude::*;
    use rstest::rstest;
    use std::collections::HashMap as StdHashMap;

    fn base() -> Value {
        serde_json::from_str(crate::SCHEMA_JSON).expect("bundled schema parses")
    }

    fn index() -> NameIndex {
        NameIndex::new(&base())
    }

    /// The base schema plus the `freesurfer` overlay, which is where `hemi` stops having one
    /// answer and `seg` gains a second, agreeing claimant.
    fn with_freesurfer() -> NameIndex {
        let mut schema = base();
        let overlay = crate::overlay::bundled_overlay("freesurfer").expect("bundled overlay");
        crate::overlay::merge_into(&mut schema, &overlay).expect("overlay merges");
        NameIndex::new(&schema)
    }

    /// The base schema plus `fmriprep`, whose `from`/`to`/`mode` are the overlay-added entities
    /// with no canonical position.
    fn with_fmriprep() -> NameIndex {
        let mut schema = base();
        let overlay = crate::overlay::bundled_overlay("fmriprep").expect("bundled overlay");
        crate::overlay::merge_into(&mut schema, &overlay).expect("overlay merges");
        NameIndex::new(&schema)
    }

    proptest! {
        /// The law this module exists for: whatever `render` writes, `read_entities` reads back
        /// unchanged. The two live in different crates and neither consults the other, so this
        /// is a genuine cross-check rather than a restatement.
        #[test]
        fn render_then_read_round_trips(
            (pairs, suffix, extension) in (
                crate::strategy::schema_entity_pairs(0..=5),
                "[0-9A-Za-z]{1,10}",
                prop::sample::select(vec!["", ".nii.gz", ".json", ".tsv", ".ome.tif"]),
            )
        ) {
            let index = index();
            let mut name = BidsName::new(&suffix, extension);
            for (key, value) in &pairs {
                name = name.entity(key, value);
            }

            let rendered = name.render(&index).expect("schema-drawn entities render");

            let parts = read_entities(&rendered);
            let expected: StdHashMap<String, String> = pairs.into_iter().collect();
            prop_assert_eq!(
                (parts.entities, parts.suffix, parts.extension),
                (expected, suffix, extension.to_string())
            );
        }
    }

    #[test]
    fn entities_are_emitted_in_canonical_order() {
        let name = BidsName::new("bold", ".nii.gz")
            .entity("desc", "preproc")
            .entity("run", "01")
            .entity("sub", "01")
            .entity("task", "rest")
            .entity("ses", "V1");

        let rendered = name.render(&index()).expect("renders");

        assert_eq!(
            rendered,
            "sub-01_ses-V1_task-rest_run-01_desc-preproc_bold.nii.gz"
        );
    }

    /// `from`/`to`/`mode` are declared only by the overlay and appear nowhere in
    /// `rules.entities`, so they sort after every canonical entity — and among themselves by key,
    /// which is why `from` precedes `mode` precedes `to` rather than reading in the order fMRIPrep
    /// happens to write them.
    #[test]
    fn an_overlay_entity_sorts_after_every_canonical_one() {
        let name = BidsName::new("xfm", ".h5")
            .entity("mode", "image")
            .entity("to", "MNI152NLin2009cAsym")
            .entity("from", "T1w")
            .entity("sub", "01");

        let rendered = name.render(&with_fmriprep()).expect("renders");

        assert_eq!(
            rendered,
            "sub-01_from-T1w_mode-image_to-MNI152NLin2009cAsym_xfm.h5"
        );
    }

    #[test]
    fn a_value_outside_an_entitys_enum_is_refused() {
        let name = BidsName::new("bold", ".nii.gz").entity("hemi", "Q");

        let rendered = name.render(&index());

        assert_eq!(
            rendered.unwrap_err(),
            NamingError::NotInEnum {
                key: "hemi".to_string(),
                value: "Q".to_string(),
                allowed: vec!["L".to_string(), "R".to_string()],
            }
        );
    }

    #[test]
    fn a_non_numeric_index_entity_is_refused() {
        let name = BidsName::new("bold", ".nii.gz").entity("run", "first");

        let rendered = name.render(&index());

        assert_eq!(
            rendered.unwrap_err(),
            NamingError::BadIndex {
                key: "run".to_string(),
                value: "first".to_string(),
            }
        );
    }

    /// A label carrying a separator is the likeliest way a caller gets a name wrong — a task or
    /// space label copied from somewhere that allows `_`. Rendering it would move the key/value
    /// boundary and read back as a different entity set.
    #[rstest]
    #[case::underscore_splits_the_segment("go_nogo")]
    #[case::hyphen_moves_the_key_boundary("go-nogo")]
    #[case::dot_starts_the_extension("go.nogo")]
    #[case::empty_reads_as_the_noentity_sentinel("")]
    fn a_label_the_parser_would_re_read_is_refused(#[case] value: &str) {
        let name = BidsName::new("bold", ".nii.gz").entity("task", value);

        let rendered = name.render(&index());

        assert_eq!(
            rendered.unwrap_err(),
            NamingError::BadLabel {
                key: "task".to_string(),
                value: value.to_string(),
            }
        );
    }

    /// An extension without its dot fuses onto the suffix, so `bold` + `nii.gz` would read back
    /// with suffix `boldnii` — a name that parses, and parses wrong.
    #[test]
    fn an_extension_missing_its_dot_is_refused() {
        let name = BidsName::new("bold", "nii.gz").entity("sub", "01");

        let rendered = name.render(&index());

        assert_eq!(
            rendered.unwrap_err(),
            NamingError::BadExtension {
                extension: "nii.gz".to_string(),
            }
        );
    }

    /// Two objects rendering one key with *different* constraints has no right answer, so
    /// rendering refuses rather than picking.
    ///
    /// The overlay is built here rather than taken from the bundled set on purpose. The
    /// freesurfer overlay used to be this case — it declared a `hemi` colliding with base
    /// `hemisphere` — and does not any more, now that its term map projects the BIDS `L`/`R`
    /// labels and the overlay entity is gone. Pinning the mechanism to a document that may stop
    /// exhibiting it is how a guard quietly stops guarding.
    #[test]
    fn a_colliding_short_key_is_refused() {
        let mut schema = base();
        // A second claimant on `hemi`, disagreeing with base by carrying no `enum`.
        crate::overlay::merge_into(
            &mut schema,
            &serde_json::json!({
                "objects": { "entities": { "hemi": {
                    "name": "hemi", "display_name": "Hemi", "type": "string", "format": "label"
                } } }
            }),
        )
        .expect("overlay merges");
        let name = BidsName::new("bold", ".nii.gz").entity("hemi", "L");

        let rendered = name.render(&NameIndex::new(&schema));

        assert_eq!(
            rendered.unwrap_err(),
            NamingError::AmbiguousKey {
                key: "hemi".to_string(),
                objects: vec!["hemi".to_string(), "hemisphere".to_string()],
            }
        );
    }

    /// And the case that used to be the collision: with the freesurfer overlay merged, `hemi` is
    /// base's alone, so a hemisphere renders — and only in the labels BIDS allows, which is the
    /// point of the term map projecting `L`/`R` rather than FreeSurfer's `lh`/`rh`.
    #[test]
    fn a_hemisphere_renders_under_the_freesurfer_overlay() {
        let name = BidsName::new("bold", ".nii.gz")
            .entity("sub", "01")
            .entity("hemi", "L");

        let rendered = name.render(&with_freesurfer()).expect("renders");

        assert_eq!(rendered, "sub-01_hemi-L_bold.nii.gz");
    }

    /// The other half: base's `enum` is now the only claimant, so FreeSurfer's own spelling is
    /// refused — which is exactly what stops `lh` reaching a `hemi` column.
    #[test]
    fn the_freesurfer_hemisphere_token_is_not_a_bids_label() {
        let name = BidsName::new("bold", ".nii.gz").entity("hemi", "lh");

        let rendered = name.render(&with_freesurfer());

        assert_eq!(
            rendered.unwrap_err(),
            NamingError::NotInEnum {
                key: "hemi".to_string(),
                value: "lh".to_string(),
                allowed: vec!["L".to_string(), "R".to_string()],
            }
        );
    }

    /// `segmentation` (base) and `seg` (the overlay) also collide, and agree — same `format`,
    /// neither carrying an `enum` — so the key stays renderable. The contrast with `hemi` is the
    /// whole rule: a collision is only fatal when it changes the answer.
    #[test]
    fn a_colliding_key_whose_claimants_agree_still_renders() {
        let name = BidsName::new("dseg", ".nii.gz").entity("seg", "aseg");

        let rendered = name.render(&with_freesurfer()).expect("renders");

        assert_eq!(rendered, "seg-aseg_dseg.nii.gz");
    }

    /// An entity the effective schema has never heard of. The realistic cause is a forgotten
    /// `--overlay`, so the failure has to arrive here rather than as a missing column later.
    #[test]
    fn an_entity_the_schema_does_not_declare_is_refused() {
        let name = BidsName::new("xfm", ".h5").entity("from", "T1w");

        let rendered = name.render(&index());

        assert_eq!(
            rendered.unwrap_err(),
            NamingError::UnknownEntity {
                key: "from".to_string(),
            }
        );
    }

    /// A suffix carrying any of the three characters the parser gives meaning to would read back
    /// as something else, so all three are refused before a name is built from them.
    #[rstest]
    #[case::dot_starts_the_extension("bo.ld")]
    #[case::hyphen_reads_as_an_entity("bo-ld")]
    #[case::underscore_splits_the_segment("bo_ld")]
    #[case::empty_leaves_no_suffix("")]
    fn a_suffix_the_parser_would_re_read_is_refused(#[case] suffix: &str) {
        let name = BidsName::new(suffix, ".nii.gz");

        let rendered = name.render(&index());

        assert_eq!(
            rendered.unwrap_err(),
            NamingError::BadSuffix {
                suffix: suffix.to_string()
            }
        );
    }

    /// The compound extensions BIDS carries are single tokens to
    /// [`bids_core::entities::read_entities`], which takes everything from the first dot. Pinned
    /// as cases because each is a shape a naive `splitext` gets wrong in a different way.
    #[rstest]
    #[case::gzipped_nifti(".nii.gz")]
    #[case::gifti_surface(".surf.gii")]
    #[case::cifti_dense_series(".dtseries.nii")]
    #[case::gzipped_tsv(".tsv.gz")]
    fn a_compound_extension_survives_the_round_trip(#[case] extension: &str) {
        let name = BidsName::new("bold", extension).entity("sub", "01");

        let rendered = name.render(&index()).expect("renders");

        assert_eq!(read_entities(&rendered).extension, extension);
    }

    #[test]
    fn a_path_nests_under_subject_session_and_datatype() {
        let name = BidsName::new("bold", ".nii.gz")
            .entity("sub", "01")
            .entity("ses", "V1")
            .entity("task", "rest")
            .datatype("func");

        let path = name.render_path(&index()).expect("renders");

        assert_eq!(
            path,
            "sub-01/ses-V1/func/sub-01_ses-V1_task-rest_bold.nii.gz"
        );
    }

    /// A file with no subject belongs at the dataset root — the shape
    /// `rules.files.deriv.atlas.atlas_description` describes — so this is a supported answer and
    /// not an error.
    #[test]
    fn a_subjectless_name_renders_at_the_dataset_root() {
        let name = BidsName::new("dseg", ".tsv").entity("desc", "aseg");

        let path = name.render_path(&index()).expect("renders");

        assert_eq!(path, "desc-aseg_dseg.tsv");
    }

    #[test]
    fn a_datatype_without_a_subject_has_no_home() {
        let name = BidsName::new("bold", ".nii.gz").datatype("func");

        let path = name.render_path(&index());

        assert_eq!(
            path.unwrap_err(),
            NamingError::RootFileWithDatatype {
                datatype: "func".to_string()
            }
        );
    }

    #[test]
    fn an_undeclared_datatype_is_refused() {
        let name = BidsName::new("bold", ".nii.gz")
            .entity("sub", "01")
            .datatype("fmri");

        let path = name.render_path(&index());

        assert_eq!(
            path.unwrap_err(),
            NamingError::UnknownDatatype {
                datatype: "fmri".to_string()
            }
        );
    }
}
