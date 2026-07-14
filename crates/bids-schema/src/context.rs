//! The per-file selector context used to evaluate BIDS schema expressions.
//!
//! [`build_file_context`] produces the single `{ path, extension, suffix, datatype, modality,
//! entities }` object that both the validator and bidslake feed to
//! [`expression::do_selectors_select`](crate::expression::do_selectors_select) — one builder
//! instead of a copy in each consumer.

use crate::datatypes::{find_datatype, find_modality};
use bids_core::entities::{read_entities, resolve_entities};
use bids_core::filetree::BidsFile;
use serde_json::{Value, json};
use std::collections::HashMap;

/// Map each entity's abbreviation (`entity`, falling back to `name`) → its schema object-key,
/// from `schema.objects.entities`. e.g. `"sub" → "subject"`. Used to lift a filename's raw
/// entity keys into the schema's namespace, matching the reference validator. Public so the
/// validator can populate its own `entity_name_to_key` from this single derivation rather than
/// re-deriving the same mapping from its parsed entity list.
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

/// Build the file-level selector context for `file` against `schema` (the raw schema JSON).
///
/// `name_to_key` is the entity-abbreviation → schema-key map from [`entity_name_to_key`]. It
/// depends only on the schema, not the file, so callers compute it **once** and pass it in for
/// every file rather than rebuilding it per file. `entities` are resolved into the schema's key
/// namespace (e.g. `subject`, not `sub`); the shared `dataset`/`schema`/`subject` scopes are left
/// to the caller's [`crate::expression::EvalContext`].
pub fn build_file_context(
    file: &BidsFile,
    schema: &Value,
    name_to_key: &HashMap<String, String>,
) -> Value {
    let parts = read_entities(&file.name);
    let entities = resolve_entities(&parts.entities, name_to_key);
    let datatype = find_datatype(&file.path, schema);
    let modality = datatype.as_deref().and_then(|dt| find_modality(dt, schema));

    json!({
        "path": file.path,
        "extension": parts.extension,
        "suffix": parts.suffix,
        "datatype": datatype,
        "modality": modality,
        "entities": entities,
    })
}
