//! Proptest strategies for the JSON this crate merges and the selector source it evaluates.
//!
//! See `bids_core::strategy` for the two rules these follow (no `prop_filter`; the strategy
//! renders rather than re-derives).

use std::collections::HashSet;

use proptest::prelude::*;
use proptest::sample::SizeRange;
use serde_json::{Map, Value};

/// An arbitrary [`Value`], depth ≤ 3.
///
/// **No float leaves, deliberately.** `serde_json::Number` cannot hold NaN or an infinity —
/// `Value::from(f64::NAN)` is `Value::Null` — so a float leaf would silently collapse some
/// generated values to null and turn an equality law into a law about nulls. Integers are
/// exact and shrink toward zero, which is what the merge laws want.
pub fn json_value() -> impl Strategy<Value = Value> {
    json_value_with(3, 32, 4)
}

/// [`json_value`] with the recursion budget spelled out: `depth` levels, roughly `desired_size`
/// nodes, `branch` children per container.
pub fn json_value_with(depth: u32, desired_size: u32, branch: u32) -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i32>().prop_map(|n| Value::Number(n.into())),
        "[a-z]{0,6}".prop_map(Value::String),
    ];
    leaf.prop_recursive(depth, desired_size, branch, move |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..branch as usize).prop_map(Value::Array),
            prop::collection::btree_map("[a-z]{1,4}", inner, 0..branch as usize)
                .prop_map(|entries| Value::Object(entries.into_iter().collect::<Map<_, _>>())),
        ]
    })
}

/// An arbitrary JSON *object*, which is the only thing [`crate::overlay::merge_into`] is handed
/// at the top level — `load_overlay` refuses anything else. Generating a bare scalar root would
/// exercise a shape the loader forbids and prove nothing about overlays.
pub fn json_object() -> impl Strategy<Value = Value> {
    prop::collection::btree_map("[a-z]{1,4}", json_value(), 0..4)
        .prop_map(|entries| Value::Object(entries.into_iter().collect()))
}

/// Every renderable entity of the bundled schema, as `(short key, format, enum)`.
///
/// Drawn from `objects.entities` rather than hardcoded, so an entity the standard adds is
/// generated the day it lands. Keys claimed by two objects are dropped: the bundled schema alone
/// has no such collision, but this is what keeps the generator honest if one ever appears there,
/// since [`crate::naming::NameIndex`] refuses to render one and the round-trip law is about names
/// that *can* be rendered.
fn renderable_entities() -> Vec<(String, Option<String>, Option<Vec<String>>)> {
    let schema: Value = serde_json::from_str(crate::SCHEMA_JSON).expect("bundled schema parses");
    let Some(entities) = schema
        .get("objects")
        .and_then(|o| o.get("entities"))
        .and_then(|e| e.as_object())
    else {
        return Vec::new();
    };

    let mut by_name: Vec<(String, Option<String>, Option<Vec<String>>)> = Vec::new();
    let mut collided: HashSet<String> = HashSet::new();
    for def in entities.values() {
        let Some(name) = def.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if by_name.iter().any(|(n, _, _)| n == name) {
            collided.insert(name.to_string());
            continue;
        }
        by_name.push((
            name.to_string(),
            def.get("format")
                .and_then(|f| f.as_str())
                .map(str::to_string),
            def.get("enum").and_then(|e| e.as_array()).map(|vs| {
                vs.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect()
            }),
        ));
    }
    by_name.retain(|(name, _, _)| !collided.contains(name));
    by_name
}

/// A value satisfying one entity's declared constraints.
fn entity_value(format: Option<String>, enum_values: Option<Vec<String>>) -> BoxedStrategy<String> {
    match (enum_values, format.as_deref()) {
        (Some(allowed), _) => prop::sample::select(allowed).boxed(),
        (None, Some("index")) => "[0-9]{1,4}".boxed(),
        (None, _) => "[0-9A-Za-z]{1,8}".boxed(),
    }
}

/// `count` `(short key, value)` pairs drawn from the schema's own entities, with distinct keys
/// and each value satisfying that entity's declared `format` and `enum`.
///
/// The contrast with [`bids_core::strategy::entity_pairs`] is the point of having both: that one
/// generates arbitrary keys because `read_entities` is schema-agnostic and must treat an unheard-of
/// key exactly like `sub`. [`crate::naming`] is the opposite — it renders only what the schema
/// declares — so its law needs keys the schema declares, and values it admits.
///
/// Deduplication is an ordinary `filter` inside `prop_map`, not a `prop_filter`, so no case is
/// spent on a rejection.
pub fn schema_entity_pairs(
    count: impl Into<SizeRange>,
) -> impl Strategy<Value = Vec<(String, String)>> {
    let entities = renderable_entities();
    prop::collection::vec(
        prop::sample::select(entities).prop_flat_map(|(name, format, enum_values)| {
            (Just(name), entity_value(format, enum_values))
        }),
        count,
    )
    .prop_map(|pairs| {
        let mut seen = HashSet::new();
        pairs
            .into_iter()
            .filter(|(key, _)| seen.insert(key.clone()))
            .collect()
    })
}

/// Every function name [`crate::expression::evaluate`] dispatches, plus one it does not.
///
/// The unknown name is generated on purpose: `Unknown function` is an `Err`, and an `Err` from
/// a selector is silently read as "this rule does not apply" — so the path that swallows it has
/// to stay total too.
const FUNCTIONS: &[&str] = &[
    "length",
    "count",
    "index",
    "intersects",
    "match",
    "type",
    "min",
    "max",
    "substr",
    "sorted",
    "unique",
    "allequal",
    "exists",
    "nosuchfn",
];

/// Identifiers the evaluator binds, and one it does not.
const IDENTIFIERS: &[&str] = &[
    "path",
    "suffix",
    "datatype",
    "extension",
    "entities",
    "sidecar",
    "dataset",
    "schema",
    "subject",
    "associations",
    "nosuchident",
];

/// Source text for a BIDS schema selector expression.
///
/// Generates *structured* source rather than arbitrary bytes: a random string is almost always
/// a parse error, which exercises the parser's error path and never reaches the evaluator. This
/// reaches the evaluator, which is where the arithmetic lives.
///
/// String literals use a quote-free, backslash-free alphabet. `compile_uncached` doubles every
/// backslash before handing the source to oxc, so a generated backslash would be testing that
/// escaping rather than the evaluator, and it has its own test.
pub fn dsl_expression() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        // Small magnitudes, both signs, because the indexing functions take `i64` and cast to
        // `usize`. A negative index is the case that wraps.
        (-4i64..8).prop_map(|n| n.to_string()),
        "[a-z0-9_.]{0,6}".prop_map(|s| format!("\"{s}\"")),
        Just("null".to_string()),
        Just("true".to_string()),
        Just("false".to_string()),
        prop::sample::select(IDENTIFIERS).prop_map(str::to_owned),
    ];
    leaf.prop_recursive(3, 24, 3, |inner| {
        prop_oneof![
            // Calls, 1–3 args: where `substr`, `index` and the aggregates live.
            (
                prop::sample::select(FUNCTIONS),
                prop::collection::vec(inner.clone(), 1..=3)
            )
                .prop_map(|(f, args)| format!("{f}({})", args.join(", "))),
            prop::collection::vec(inner.clone(), 0..3)
                .prop_map(|xs| format!("[{}]", xs.join(", "))),
            (
                inner.clone(),
                prop::sample::select(
                    &[
                        "==", "!=", "<", "<=", ">", ">=", "+", "-", "*", "/", "&&", "||"
                    ][..]
                ),
                inner.clone()
            )
                .prop_map(|(l, op, r)| format!("({l} {op} {r})")),
            inner.clone().prop_map(|e| format!("!{e}")),
            (inner.clone(), inner.clone()).prop_map(|(o, i)| format!("{o}[{i}]")),
            (inner, "[a-z]{1,5}").prop_map(|(o, p)| format!("{o}.{p}")),
        ]
    })
}
