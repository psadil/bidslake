//! Proptest strategies for the JSON this crate merges and the selector source it evaluates.
//!
//! See `bids_core::strategy` for the two rules these follow (no `prop_filter`; the strategy
//! renders rather than re-derives).

use proptest::prelude::*;
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
