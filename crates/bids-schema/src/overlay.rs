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

/// Overlays bidslake ships for common derivative pipelines. Reached by name on the
/// `--adapter` flag (e.g. `--adapter fmriprep`), which loads this overlay together with
/// whatever term map and ingestion fragment are bundled under the same name; `--overlay`
/// takes file paths only. Kept alongside [`bundled_overlay`] so the two never drift.
pub const BUNDLED_OVERLAY_NAMES: &[&str] = &["fmriprep", "mriqc", "qsiprep", "freesurfer", "feat"];

/// The parsed bundled overlay for a pipeline `name`, or `None` if `name` is not a
/// bundled pipeline (callers then treat the argument as a filesystem path). The JSON
/// is embedded at compile time, so this needs no I/O.
pub fn bundled_overlay(name: &str) -> Option<Value> {
    let raw = match name {
        "fmriprep" => include_str!("../data/overlays/fmriprep.json"),
        "mriqc" => include_str!("../data/overlays/mriqc.json"),
        "qsiprep" => include_str!("../data/overlays/qsiprep.json"),
        "freesurfer" => include_str!("../data/overlays/freesurfer.json"),
        "feat" => include_str!("../data/overlays/feat.json"),
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
///
/// On [`OverlayError::Conflict`], `base` is left exactly as it was found.
///
/// `merge_at` walks and mutates in one pass, so a conflict discovered part-way used to leave
/// every earlier key already written — a caller told the merge failed, holding a schema with
/// half an overlay applied to it. Merging into a copy and committing only on success is the
/// whole fix; the clone costs one deep copy of the schema per overlay, paid at startup where a
/// handful of overlays are resolved, not per file.
pub fn merge_into(base: &mut Value, overlay: &Value) -> Result<(), OverlayError> {
    let mut candidate = base.clone();
    merge_at(&mut candidate, overlay, &mut Vec::new())?;
    *base = candidate;
    Ok(())
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
    use crate::strategy;
    use proptest::prelude::*;
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

    proptest! {
        // 128 rather than 256: generating a recursive `Value` dominates the cost here, not the
        // merge.
        #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

        /// Merging a document into itself changes nothing and cannot conflict.
        ///
        /// The law the additive-only rule rests on, and the one `equal_scalar_is_idempotent`
        /// above states for a single flat shape: every branch of `merge_at` has to read "the
        /// overlay restates what the base says" as a no-op — equal scalars, array elements
        /// already present, keys already there — at every depth, not just the top.
        ///
        /// Asserted over the `Result` rather than through an `unwrap()`: a merge that started
        /// erroring shows its message instead of panicking somewhere that says nothing about
        /// which branch went wrong.
        #[test]
        fn merging_a_document_into_itself_leaves_it_unchanged(value in strategy::json_object()) {
            let mut merged = value.clone();

            let outcome = merge_into(&mut merged, &value)
                .map(|()| merged)
                .map_err(|e| e.to_string());

            prop_assert_eq!(outcome, Ok(value));
        }

        /// Two overlays extending one array contribute the same *elements* in either order.
        ///
        /// Acts four times on purpose: the behaviour *is* the relationship between the two
        /// orders, which is why `multiple_overlays_are_order_independent` exists. That test
        /// uses two disjoint *objects* and no array at all, so it cannot fail on the case its
        /// name claims — this is that case.
        ///
        /// **Element sets, not element order, and that is a known gap.** ADR 0001 §2 states
        /// that additive-only merging "makes merging order-independent", and for arrays it does
        /// not: `merge_at` appends in overlay order, so base `[]` extended by `[1]` then `[0]`
        /// gives `[1, 0]` and the reverse gives `[0, 1]`. That matters because the module doc
        /// names `rules.entities` — the BIDS entity ordering — as the array overlays extend, so
        /// element order is what filenames validate against. Sorting the appended tail does not
        /// fix it, because after the first merge nothing distinguishes base elements from
        /// appended ones; the fix is to apply overlays as a *set*. Recorded in `TODO.md`.
        ///
        /// Values are drawn from a small range so the generator produces overlapping and
        /// disjoint extensions in the same run — dedup is half the rule being tested.
        #[test]
        fn two_overlays_extending_one_array_contribute_the_same_elements_in_either_order(
            base_items in prop::collection::vec(0i64..5, 0..3),
            first in prop::collection::vec(0i64..5, 0..3),
            second in prop::collection::vec(0i64..5, 0..3),
        ) {
            let doc = |items: &[i64]| json!({ "rules": { "entities": items } });
            let elements = |v: &Value| {
                let mut xs: Vec<i64> = v["rules"]["entities"]
                    .as_array()
                    .expect("the document is built with an array here")
                    .iter()
                    .map(|x| x.as_i64().expect("built from i64s"))
                    .collect();
                xs.sort_unstable();
                xs
            };

            // Array merge has no conflict branch, so neither order can error.
            let mut forward = doc(&base_items);
            merge_into(&mut forward, &doc(&first)).unwrap();
            merge_into(&mut forward, &doc(&second)).unwrap();

            let mut backward = doc(&base_items);
            merge_into(&mut backward, &doc(&second)).unwrap();
            merge_into(&mut backward, &doc(&first)).unwrap();

            prop_assert_eq!(elements(&forward), elements(&backward));
        }

        /// A merge that conflicts leaves the base as it found it.
        ///
        /// `merge_at` walks and mutates in one pass, so a conflict found late used to leave
        /// every earlier key already written — a half-applied overlay in a `Schema` whose
        /// caller was told the merge failed. The keys are ordered so the mergeable one is
        /// visited first (`serde_json::Map` is a `BTreeMap` here, `preserve_order` being off),
        /// which is what makes a partial write observable.
        ///
        /// The two leaves differ by construction rather than through a `prop_assume!`: a
        /// filter that drops a fifth of its cases is a fifth of the budget spent proving
        /// nothing, and the strategy can simply not generate them.
        #[test]
        fn a_conflicting_merge_leaves_the_base_untouched(
            addition in strategy::json_value(),
            (base_leaf, overlay_leaf) in (0i64..5, 1i64..5)
                .prop_map(|(base, delta)| (base, (base + delta) % 5)),
        ) {
            let mut base = json!({ "a": {}, "z": base_leaf });
            let before = base.clone();

            let outcome = merge_into(&mut base, &json!({ "a": addition, "z": overlay_leaf }));

            prop_assert!(outcome.is_err() && base == before, "base is {base}");
        }
    }

    #[test]
    fn rejects_non_object_overlay_from_disk() {
        // A `TempDir` rather than a fixed name under `std::env::temp_dir()`: the fixed path
        // collided with any concurrent run of this test, and was only removed on the success
        // path, so a failure leaked it and the next run read the previous one's file.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = dir.path().join("overlay_array.json");
        std::fs::write(&path, "[1, 2, 3]").unwrap();

        let err = load_overlay(&path).unwrap_err();

        assert!(matches!(err, OverlayError::NotObject { .. }));
    }
}
