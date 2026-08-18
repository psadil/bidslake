//! JSON sidecar bodies, from `rules.sidecars` and `objects.metadata`.
//!
//! Which keys a sidecar needs is not a list in this crate. `rules.sidecars` already says it —
//! every rule carries `selectors` and a `fields` map with a requirement level — so this
//! evaluates those selectors against the file being generated and emits what comes back
//! `required`. That is the difference between a generated tree that happens to validate today
//! and one that keeps validating when the standard adds a required field.
//!
//! Values come from `objects.metadata.<key>.type`, so a field's *type* tracks the schema too.
//! What cannot: a field whose value has to mean something. `TaskName` must match the `task`
//! entity, and `RepetitionTime` has to be a plausible TR, so those arrive as overrides from the
//! producer that knows them.

use std::collections::BTreeMap;

use bids_schema::expression::{EvalContext, do_selectors_select};
use serde_json::{Map, Value, json};

/// The file facts a sidecar rule's selectors can ask about.
pub struct SidecarContext<'a> {
    /// Dataset-relative path, leading slash.
    pub path: &'a str,
    /// The datatype directory.
    pub datatype: Option<&'a str>,
    /// The BIDS suffix of the file the sidecar describes.
    pub suffix: Option<&'a str>,
    /// The described file's extension, not the sidecar's — the rules select on the data file.
    pub extension: Option<&'a str>,
    /// The file's entities, under short keys.
    pub entities: &'a BTreeMap<String, String>,
    /// `raw` or `derivative`.
    pub dataset_type: Option<&'a str>,
}

/// Every metadata key `rules.sidecars` makes **required** for this file, given what the sidecar
/// already carries.
///
/// `known` matters more than it looks. Several rules are *conditional on the sidecar itself*:
/// `MRIFuncVolumeTiming` requires `VolumeTiming` only when `RepetitionTime` is absent, and BIDS
/// then makes the two mutually exclusive. Evaluating against an empty sidecar therefore asks
/// both rules and emits both fields, which is a dataset the validator rejects
/// (`VOLUME_TIMING_AND_REPETITION_TIME_MUTUALLY_EXCLUSIVE`). Seeding the context with the values
/// the caller is *going* to write is what makes the answer the one that will be true of the file.
///
/// Only `required`, not `recommended`: a recommended field that is absent is a warning, and a
/// generator that emitted all of them would produce sidecars far denser than any real tool
/// writes — which would make a benchmark's JSON parse cost fiction.
pub fn required_fields(
    schema: &Value,
    ctx: &SidecarContext<'_>,
    known: &BTreeMap<String, Value>,
) -> Vec<String> {
    let entities: Map<String, Value> = ctx
        .entities
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    let sidecar: Map<String, Value> = known.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let file = json!({
        "path": ctx.path,
        "suffix": ctx.suffix,
        "extension": ctx.extension,
        "datatype": ctx.datatype,
        "entities": Value::Object(entities),
        "sidecar": Value::Object(sidecar),
    });
    let dataset = json!({ "dataset_description": { "DatasetType": ctx.dataset_type } });
    let null = Value::Null;
    let eval = EvalContext::new(&file, &dataset, &null, &null);

    let mut out = Vec::new();
    let Some(groups) = schema
        .get("rules")
        .and_then(|r| r.get("sidecars"))
        .and_then(|s| s.as_object())
    else {
        return out;
    };
    for group in groups.values() {
        let Some(rules) = group.as_object() else {
            continue;
        };
        for rule in rules.values() {
            let selectors: Option<Vec<String>> =
                rule.get("selectors").and_then(|s| s.as_array()).map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(String::from)
                        .collect()
                });
            let Some(selectors) = selectors else { continue };
            if !do_selectors_select(Some(selectors.as_slice()), &eval) {
                continue;
            }
            let Some(fields) = rule.get("fields").and_then(|f| f.as_object()) else {
                continue;
            };
            for (key, requirement) in fields {
                if level_of(requirement) == "required" {
                    out.push(key.clone());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn level_of(requirement: &Value) -> &str {
    requirement
        .as_str()
        .or_else(|| requirement.get("level").and_then(|l| l.as_str()))
        .unwrap_or("optional")
}

/// A sidecar body: every required field, typed from `objects.metadata`, with `overrides` winning.
///
/// The real JSON field name is `objects.metadata.<key>.name`, not the key: `AtlasName`'s field is
/// `Name`. Reading it is the same trap `objects.columns` has, one level over.
pub fn body(
    schema: &Value,
    ctx: &SidecarContext<'_>,
    overrides: &BTreeMap<String, Value>,
) -> String {
    let mut object = Map::new();
    // The overrides are what the file will carry, so they are also what the conditional rules
    // must see — see [`required_fields`].
    for key in required_fields(schema, ctx, overrides) {
        let definition = schema
            .get("objects")
            .and_then(|o| o.get("metadata"))
            .and_then(|m| m.get(&key));
        let name = definition
            .and_then(|d| d.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or(&key)
            .to_string();
        object.insert(name, definition.map(placeholder).unwrap_or(json!("x")));
    }
    for (key, value) in overrides {
        object.insert(key.clone(), value.clone());
    }
    serde_json::to_string(&Value::Object(object)).expect("a JSON object serializes")
}

/// A value of the declared type. Deliberately dull — the point is that it parses and types, not
/// that it is realistic.
fn placeholder(definition: &Value) -> Value {
    if let Some(allowed) = definition.get("enum").and_then(|e| e.as_array())
        && let Some(first) = allowed.first()
    {
        return first.clone();
    }
    match definition.get("type").and_then(|t| t.as_str()) {
        Some("number") => json!(1.0),
        Some("integer") => json!(1),
        Some("boolean") => json!(false),
        Some("array") => json!([]),
        Some("object") => json!({}),
        _ => json!("x"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        serde_json::from_str(bids_schema::SCHEMA_JSON).expect("bundled schema parses")
    }

    fn bold_context(entities: &BTreeMap<String, String>) -> SidecarContext<'_> {
        SidecarContext {
            path: "/sub-01/func/sub-01_task-rest_bold.nii.gz",
            datatype: Some("func"),
            suffix: Some("bold"),
            extension: Some(".nii.gz"),
            entities,
            dataset_type: Some("raw"),
        }
    }

    /// `rules.sidecars.func.MRIFuncRequired` makes `TaskName` required for a functional run, and
    /// nothing in this crate says so — which is the property. If BIDS adds a required field, a
    /// generated sidecar gains it with no edit here.
    #[test]
    fn a_functional_run_requires_taskname_because_the_schema_says_so() {
        let entities = BTreeMap::from([
            ("sub".to_string(), "01".to_string()),
            ("task".to_string(), "rest".to_string()),
        ]);

        let fields = required_fields(&schema(), &bold_context(&entities), &BTreeMap::new());

        assert!(
            fields.contains(&"TaskName".to_string()),
            "required fields were {fields:?}"
        );
    }

    /// `VolumeTiming` is required only when `RepetitionTime` is absent, and BIDS then forbids
    /// having both. Declaring the `RepetitionTime` the file will carry is what keeps the second
    /// rule from firing — the difference between a sidecar that validates and one that does not.
    #[test]
    fn a_declared_repetition_time_suppresses_the_volume_timing_requirement() {
        let entities = BTreeMap::from([("task".to_string(), "rest".to_string())]);
        let known = BTreeMap::from([("RepetitionTime".to_string(), json!(2.0))]);

        let fields = required_fields(&schema(), &bold_context(&entities), &known);

        assert!(
            !fields.contains(&"VolumeTiming".to_string()),
            "required fields were {fields:?}"
        );
    }

    /// An override wins over the schema-typed placeholder, because `TaskName` has to equal the
    /// `task` entity and no type can say that.
    #[test]
    fn an_override_replaces_the_typed_placeholder() {
        let entities = BTreeMap::from([("task".to_string(), "rest".to_string())]);
        let overrides = BTreeMap::from([("TaskName".to_string(), json!("rest"))]);

        let json_body = body(&schema(), &bold_context(&entities), &overrides);

        let parsed: Value = serde_json::from_str(&json_body).expect("parses");
        assert_eq!(parsed.get("TaskName"), Some(&json!("rest")));
    }
}
