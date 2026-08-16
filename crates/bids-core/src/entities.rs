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
    use crate::strategy;
    use proptest::prelude::*;
    use rstest::rstest;

    /// The one filename shape worth naming: a real, extremely common file whose parse is
    /// genuinely surprising.
    ///
    /// `dataset_description` has no `-` in either `_`-segment, so the last segment becomes the
    /// suffix and the first is silently dropped — the file is not "no entities and no suffix",
    /// it is `suffix == "description"`. The other seven rows this table carried were points of
    /// the laws below; this one stays as documentation, and the properties do not generate it
    /// because their alphabets exclude the dash-free segment that causes it.
    #[rstest]
    #[case::dataset_description("dataset_description.json", "description", ".json")]
    fn read_entities_reads_a_dash_free_stem_as_a_bare_suffix(
        #[case] name: &str,
        #[case] suffix: &str,
        #[case] extension: &str,
    ) {
        let parts = read_entities(name);

        assert_eq!(
            (
                parts.suffix.as_str(),
                parts.extension.as_str(),
                parts.entities.len()
            ),
            (suffix, extension, 0)
        );
    }

    proptest! {
        /// Any filename assembled from entities, a suffix and an extension parses back into
        /// exactly those pieces.
        ///
        /// There-and-back-again over arbitrary keys, labels, entity counts, entity orders and
        /// extensions — including keys the schema has never heard of, which the parser is
        /// documented not to consult. Replaces `basic`, `multiple_entities`, `json_sidecar`,
        /// `tabular` and `compressed`, which were five points of this one law.
        ///
        /// The expected value comes from the strategy, which *renders* a structure it already
        /// holds; nothing here re-derives what the parser should do, so the test cannot agree
        /// with a broken parser. One `prop_assert_eq!` over the whole struct, for the reason
        /// the old table compared a whole triple: a subset comparison catches a stray entity
        /// only in the cases that happened to count.
        #[test]
        fn read_entities_round_trips_a_filename_built_from_its_parts(
            (name, expected) in strategy::bids_filename()
        ) {
            let parts = read_entities(&name);

            prop_assert_eq!(parts, expected);
        }

        /// A name whose last `_` segment is an entity has no suffix, and every segment is an
        /// entity.
        ///
        /// Separate from the round-trip rather than an `Option` inside it: these are two rules
        /// with two expected shapes, and folding them into one strategy would give one test
        /// whose failure could mean either.
        #[test]
        fn read_entities_reports_no_suffix_when_the_last_segment_is_an_entity(
            (name, expected) in strategy::suffixless_filename()
        ) {
            let parts = read_entities(&name);

            prop_assert_eq!(parts, expected);
        }

        /// An entity written with a key but no label reads as the `NOENTITY` sentinel.
        ///
        /// Was `entity_without_label`, pinned at `sub-` alone. The key is generated because the
        /// sentinel is not a fact about `sub`.
        #[test]
        fn an_entity_with_no_label_reads_as_the_noentity_sentinel(
            key in strategy::entity_key(),
            suffix in strategy::suffix(),
        ) {
            let parts = read_entities(&format!("{key}-_{suffix}.nii.gz"));

            prop_assert_eq!(
                parts.entities.get(key.as_str()).map(String::as_str),
                Some("NOENTITY")
            );
        }

        /// A `_`-segment carrying no `-` is not an entity, and is dropped unless it is the
        /// suffix.
        ///
        /// The rule behind `dataset_description.json`, stated directly. It was previously only
        /// implied by rows whose stems happened to contain such a segment, so nothing asserted
        /// the segment was *dropped* rather than mis-parsed into an entity.
        #[test]
        fn a_segment_without_a_dash_is_not_an_entity(
            bare in strategy::label(),
            key in strategy::entity_key(),
            value in strategy::label(),
            suffix in strategy::suffix(),
        ) {
            let parts = read_entities(&format!("{bare}_{key}-{value}_{suffix}.tsv"));

            prop_assert_eq!(parts.entity_keys, vec![key]);
        }
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
