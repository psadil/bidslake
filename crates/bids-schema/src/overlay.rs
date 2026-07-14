//! Schema *overlays*: additive fragments deep-merged onto the base BIDS schema.
//!
//! An overlay is a partial BIDS schema (same `objects.*` / `rules.*` shape) describing
//! "bidsish" outputs the formal standard hasn't caught up with — e.g. fMRIPrep's
//! `desc-confounds_timeseries.tsv` tables or its `from`/`to`/`mode` transform
//! entities. Every downstream generator (the DuckDB DDL in `bidslake`, the validator's
//! `BidsSchema::from_value`) reads the schema as a [`serde_json::Value`], so merging a
//! fragment into that `Value` before generation lights up new columns, tables, and
//! rules through the existing code paths.
//!
//! Merging is **additive-only** ([`merge_into`]): an overlay may add keys and extend
//! arrays but never rewrite or delete a value the base defines — a conflict is an
//! error, not a silent override. Rationale and the wider design live in
//! `docs/adr/0001-schema-augmentation-overlays.md`.

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

use crate::METASCHEMA_JSON;

/// An error produced while loading or merging a schema overlay. Typed (rather than
/// `anyhow`) because this is a library boundary; it still composes into an `anyhow`
/// caller via `?`, since it is `Error + Send + Sync + 'static`.
#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    #[error("reading overlay {path}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing overlay {path} as JSON")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("overlay {path} must be a JSON object (a partial BIDS schema)")]
    NotObject { path: String },
    #[error(
        "overlay conflict at `{pointer}`: base has `{base}`, overlay has `{overlay}`. \
         Overlays are additive-only and may not change a value the base already defines."
    )]
    Conflict {
        pointer: String,
        base: String,
        overlay: String,
    },
    #[error("overlay makes the schema violate the BIDS metaschema:\n{}", .violations.join("\n"))]
    Invalid { violations: Vec<String> },
}

/// Read and parse an overlay file. Validates only that the top level is a JSON
/// object (a partial schema); structural/metaschema conformance is checked later,
/// against the *merged* result (see the crate's validation entry point).
pub fn load_overlay(path: &Path) -> Result<Value, OverlayError> {
    let display = path.display().to_string();
    let content = std::fs::read_to_string(path).map_err(|source| OverlayError::Read {
        path: display.clone(),
        source,
    })?;
    let value: Value = serde_json::from_str(&content).map_err(|source| OverlayError::Parse {
        path: display.clone(),
        source,
    })?;
    if !value.is_object() {
        return Err(OverlayError::NotObject { path: display });
    }
    Ok(value)
}

/// Overlays bidslake ships for common derivative pipelines, addressable by name on
/// the `--overlay` flag (e.g. `--overlay fmriprep`). Kept alongside [`bundled_overlay`]
/// so the two never drift.
pub const BUNDLED_OVERLAY_NAMES: &[&str] = &["fmriprep", "mriqc", "qsiprep", "freesurfer"];

/// The parsed bundled overlay for a pipeline `name`, or `None` if `name` is not a
/// bundled pipeline (callers then treat the argument as a filesystem path). The JSON
/// is embedded at compile time, so this needs no I/O.
pub fn bundled_overlay(name: &str) -> Option<Value> {
    let raw = match name {
        "fmriprep" => include_str!("../data/overlays/fmriprep.json"),
        "mriqc" => include_str!("../data/overlays/mriqc.json"),
        "qsiprep" => include_str!("../data/overlays/qsiprep.json"),
        "freesurfer" => include_str!("../data/overlays/freesurfer.json"),
        _ => return None,
    };
    Some(serde_json::from_str(raw).expect("bundled overlay must be valid JSON"))
}

/// Deep-merge `overlay` into `base`, additively.
///
/// - **object ⊕ object**: recurse key-by-key; a key present only in `overlay` is
///   inserted.
/// - **array ⊕ array**: append every `overlay` element not already present (dedup by
///   structural equality), preserving base order then overlay order. This is how an
///   overlay extends a rule's `suffixes`/`extensions` or appends to the
///   `rules.entities` global ordering.
/// - **anything else**: equal values are a no-op (so re-applying an overlay is
///   idempotent); a differing value — including an object-vs-scalar kind mismatch —
///   is an [`OverlayError::Conflict`] naming the RFC 6901 JSON pointer.
pub fn merge_into(base: &mut Value, overlay: &Value) -> Result<(), OverlayError> {
    merge_at(base, overlay, &mut Vec::new())
}

fn merge_at(base: &mut Value, overlay: &Value, path: &mut Vec<String>) -> Result<(), OverlayError> {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, overlay_val) in overlay_map {
                path.push(escape_pointer_token(key));
                match base_map.get_mut(key) {
                    Some(base_val) => merge_at(base_val, overlay_val, path)?,
                    None => {
                        base_map.insert(key.clone(), overlay_val.clone());
                    }
                }
                path.pop();
            }
            Ok(())
        }
        (Value::Array(base_arr), Value::Array(overlay_arr)) => {
            for item in overlay_arr {
                if !base_arr.iter().any(|existing| existing == item) {
                    base_arr.push(item.clone());
                }
            }
            Ok(())
        }
        (base_leaf, overlay_leaf) => {
            if *base_leaf == *overlay_leaf {
                Ok(()) // idempotent: overlay restates a value the base already has
            } else {
                Err(OverlayError::Conflict {
                    pointer: format!("/{}", path.join("/")),
                    base: truncate(base_leaf),
                    overlay: truncate(overlay_leaf),
                })
            }
        }
    }
}

/// Check that merging the overlay did not make the schema violate the BIDS
/// metaschema.
///
/// The vendored base schema itself carries a small, known set of metaschema
/// deviations — the bundled metaschema lags the schema version slightly (e.g. it
/// predates `rules.dataset_metadata`). Validating the merged schema outright would
/// therefore reject even a no-op overlay. So this checks the **delta**: it reports
/// only violations the overlay *introduces* — error signatures present when
/// validating `effective` but absent when validating `pre_overlay`. Pre-existing base
/// deviations are tolerated; anything new the overlay causes is an error.
pub fn validate_effective(pre_overlay: &Value, effective: &Value) -> Result<(), OverlayError> {
    let metaschema: Value =
        serde_json::from_str(METASCHEMA_JSON).expect("embedded metaschema.json must parse");
    let validator = jsonschema::validator_for(&metaschema)
        .expect("embedded BIDS metaschema must compile as a JSON Schema");

    // Signature = where the error is (instance pointer) + what it says. Two
    // additional-property errors under the same parent but naming different
    // properties get distinct signatures, so an overlay-added bad key is flagged
    // even though a base-added bad key at the same parent is tolerated.
    let signature = |e: &jsonschema::ValidationError<'_>| format!("{}\u{1}{e}", e.instance_path());

    let base_errors: HashSet<String> = validator
        .iter_errors(pre_overlay)
        .map(|e| signature(&e))
        .collect();

    let mut introduced: Vec<String> = validator
        .iter_errors(effective)
        .filter(|e| !base_errors.contains(&signature(e)))
        .map(|e| format!("  at `{}`: {e}", e.instance_path()))
        .collect();
    introduced.sort();
    introduced.dedup();

    if introduced.is_empty() {
        Ok(())
    } else {
        Err(OverlayError::Invalid {
            violations: introduced,
        })
    }
}

/// Escape a key for use as an RFC 6901 JSON-pointer token (`~` → `~0`, `/` → `~1`).
fn escape_pointer_token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// A compact one-line rendering of a value for conflict messages (long values are elided).
fn truncate(value: &Value) -> String {
    let s = value.to_string();
    if s.chars().count() > 80 {
        let mut elided: String = s.chars().take(80).collect();
        elided.push('…');
        elided
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inserts_a_new_key() {
        let mut base = json!({ "a": 1 });
        merge_into(&mut base, &json!({ "b": 2 })).unwrap();
        assert_eq!(base, json!({ "a": 1, "b": 2 }));
    }

    #[test]
    fn recurses_into_nested_objects() {
        let mut base = json!({ "objects": { "entities": { "sub": { "name": "sub" } } } });
        let overlay = json!({ "objects": { "entities": { "from": { "name": "from" } } } });
        merge_into(&mut base, &overlay).unwrap();
        assert_eq!(
            base,
            json!({ "objects": { "entities": {
                "sub": { "name": "sub" },
                "from": { "name": "from" },
            } } })
        );
    }

    #[test]
    fn appends_and_dedups_arrays() {
        let mut base = json!({ "entities": ["a", "b"] });
        merge_into(&mut base, &json!({ "entities": ["b", "c"] })).unwrap();
        assert_eq!(base, json!({ "entities": ["a", "b", "c"] }));
    }

    #[test]
    fn equal_scalar_is_idempotent() {
        let mut base = json!({ "a": 1, "nested": { "x": "y" } });
        let before = base.clone();
        merge_into(&mut base, &json!({ "a": 1, "nested": { "x": "y" } })).unwrap();
        assert_eq!(base, before);
    }

    #[test]
    fn differing_scalar_conflicts_with_pointer() {
        let mut base = json!({ "objects": { "columns": { "trans_x": { "type": "number" } } } });
        let overlay = json!({ "objects": { "columns": { "trans_x": { "type": "string" } } } });
        let err = merge_into(&mut base, &overlay).unwrap_err();
        match err {
            OverlayError::Conflict { pointer, .. } => {
                assert_eq!(pointer, "/objects/columns/trans_x/type");
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
    }

    #[test]
    fn kind_mismatch_conflicts() {
        // Overlay tries to replace an object subtree with a scalar.
        let mut base = json!({ "a": { "x": 1 } });
        let err = merge_into(&mut base, &json!({ "a": 5 })).unwrap_err();
        assert!(matches!(err, OverlayError::Conflict { .. }));
    }

    #[test]
    fn multiple_overlays_are_order_independent() {
        let o1 = json!({ "objects": { "suffixes": { "timeseries": { "value": "timeseries" } } } });
        let o2 = json!({ "objects": { "entities": { "from": { "name": "from" } } } });

        let mut forward = json!({ "objects": { "entities": {}, "suffixes": {} } });
        merge_into(&mut forward, &o1).unwrap();
        merge_into(&mut forward, &o2).unwrap();

        let mut backward = json!({ "objects": { "entities": {}, "suffixes": {} } });
        merge_into(&mut backward, &o2).unwrap();
        merge_into(&mut backward, &o1).unwrap();

        assert_eq!(forward, backward);
    }

    fn base_schema() -> Value {
        serde_json::from_str(crate::SCHEMA_JSON).expect("embedded schema parses")
    }

    #[test]
    fn no_op_overlay_is_metaschema_valid() {
        // The base schema has known metaschema deviations; validating base-vs-base
        // must still pass, because the delta is empty.
        let base = base_schema();
        validate_effective(&base, &base).unwrap();
    }

    #[test]
    fn conformant_addition_passes_validation() {
        let base = base_schema();
        let mut effective = base.clone();
        let overlay = json!({
            "objects": {
                "entities": {
                    "from": {
                        "name": "from",
                        "display_name": "From",
                        "description": "Source space of a transform.",
                        "type": "string",
                        "format": "label"
                    }
                }
            }
        });
        merge_into(&mut effective, &overlay).unwrap();
        validate_effective(&base, &effective).unwrap();
    }

    #[test]
    fn malformed_addition_fails_validation() {
        let base = base_schema();
        let mut effective = base.clone();
        // Missing the metaschema-required `display_name`/`description`.
        let overlay = json!({
            "objects": { "entities": { "bogus": { "name": "bogus", "type": "string" } } }
        });
        merge_into(&mut effective, &overlay).unwrap();
        let err = validate_effective(&base, &effective).unwrap_err();
        assert!(
            matches!(err, OverlayError::Invalid { .. }),
            "expected Invalid, got {err:?}"
        );
    }

    #[test]
    fn bundled_overlays_merge_and_validate() {
        for name in BUNDLED_OVERLAY_NAMES {
            let overlay = bundled_overlay(name)
                .unwrap_or_else(|| panic!("bundled overlay {name} should resolve"));
            let base = base_schema();
            let mut effective = base.clone();
            merge_into(&mut effective, &overlay)
                .unwrap_or_else(|e| panic!("bundled overlay {name} conflicts with base: {e}"));
            validate_effective(&base, &effective)
                .unwrap_or_else(|e| panic!("bundled overlay {name} is not metaschema-valid: {e}"));
        }
    }

    #[test]
    fn bundled_overlays_are_co_applicable() {
        // Shared derivative concepts (from/to/mode, timeseries, xfm, confound columns)
        // are identical across pipelines, so applying several bundled overlays to one
        // dataset merges idempotently rather than tripping the additive conflict check.
        let mut effective = base_schema();
        for name in BUNDLED_OVERLAY_NAMES {
            let overlay = bundled_overlay(name).unwrap();
            merge_into(&mut effective, &overlay).unwrap_or_else(|e| {
                panic!("bundled overlays must be co-applicable; {name} conflicts: {e}")
            });
        }
        validate_effective(&base_schema(), &effective).unwrap();
    }

    #[test]
    fn rejects_non_object_overlay_from_disk() {
        let dir = std::env::temp_dir();
        let path = dir.join("bidslake_overlay_test_array.json");
        std::fs::write(&path, "[1, 2, 3]").unwrap();
        let err = load_overlay(&path).unwrap_err();
        assert!(matches!(err, OverlayError::NotObject { .. }));
        let _ = std::fs::remove_file(&path);
    }
}
