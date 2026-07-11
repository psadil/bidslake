//! Conformance tests for the BIDS schema expression language.
//!
//! The schema ships a normative test suite at `meta.expression_tests` — a list of
//! `{expression, result}` pairs that any schema interpreter must satisfy. `build.rs` turns each
//! case into one `expr_test!` invocation in `OUT_DIR/expression_tests.rs`, which is `include!`d
//! below, so the suite tracks whichever schema version the build vendors.
//!
//! The cases pin the semantics that are easy to get subtly wrong — above all the treatment of
//! `null`, which propagates through arithmetic and logical operators but collapses to `false`
//! under comparison. See the "The special value `null`" section of `src/schema/README.md` in
//! bids-specification.
//!
//! Note: the schema's prose and these cases disagree in a few places (for example the prose says
//! `!null` is `null`, while the test says `true`). That is a known, open upstream question —
//! bids-specification#2149. We follow the machine-readable cases, since they are what the
//! reference validator is tested against.

use bids_schema::expression::{EvalContext, ValueExt, evaluate};
use serde_json::Value;

/// The bindings the conformance cases evaluate against.
///
/// Only `sidecar` is referenced by name (`sidecar.MissingValue`), and it must be an object so
/// that reading an absent key yields `null` rather than an error.
fn eval(expr: &str) -> Result<Value, String> {
    let file = serde_json::json!({ "sidecar": {} });
    let null = Value::Null;
    let ctx = EvalContext::new(&file, &null, &null, &null);
    evaluate(expr, &ctx)
}

/// Compare a result against the expected value.
///
/// Numeric comparison is by value, not by JSON representation: the schema writes `3` where our
/// arithmetic yields `3.0`, and both denote the same number. Everything else compares structurally.
fn equivalent(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Number(_), Value::Number(_)) => {
            match (actual.coerce_f64(), expected.coerce_f64()) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| equivalent(x, y))
        }
        _ => actual == expected,
    }
}

macro_rules! expr_test {
    ($name:ident, $expr:expr, $expected:expr) => {
        #[test]
        fn $name() {
            let expected: Value = serde_json::from_str($expected)
                .unwrap_or_else(|e| panic!("bad expected JSON {:?}: {e}", $expected));
            let actual =
                eval($expr).unwrap_or_else(|e| panic!("evaluating `{}` failed: {e}", $expr));
            assert!(
                equivalent(&actual, &expected),
                "`{}`\n  expected: {}\n  actual:   {}",
                $expr,
                expected,
                actual
            );
        }
    };
}

include!(concat!(env!("OUT_DIR"), "/expression_tests.rs"));

/// Every `selectors`/`checks` expression in the bundled schema must *evaluate*, not merely parse.
///
/// Evaluated against a context in which every identifier resolves to `null`, so the result is
/// uninteresting — what matters is that the evaluator never returns `Err`. An expression it
/// cannot handle would otherwise be swallowed (`do_selectors_select` treats `Err` as "does not
/// apply"; `check_expression_rules` skips the check), silently disabling the rule.
///
/// This is the regression guard for exactly that class of bug: `ParenthesizedExpression` was
/// unsupported, which quietly disabled nine rules — among them `B0_FIELD_IDENTIFIER_RECOMMENDED`,
/// `EXCESSIVE_ELECTRODE_SPECIFICITY` and `DEPRECATED_ACQUISITION_DURATION`.
#[test]
fn every_schema_expression_evaluates() {
    let schema: Value =
        serde_json::from_str(include_str!(concat!(env!("OUT_DIR"), "/schema.json"))).unwrap();

    // All identifiers resolve to null; `resolve_ident` yields null for absent keys.
    let file = Value::Object(Default::default());
    let null = Value::Null;
    let ctx = EvalContext::new(&file, &null, &null, &null);

    // Known-bad expressions in the schema itself, which no conforming interpreter can evaluate.
    // Deliberately *not* worked around in the evaluator — the schema should be fixed upstream.
    //
    // `len()` is not part of the BIDS expression language (the function is `length()`; the
    // reference TS validator defines only the 13 documented functions, `len` among none of them).
    // `rules.checks.anat.PDT2Echos` therefore can never emit
    // `PDT2_ECHOS_SHOULD_MATCH_NIFTI_LENGTH` in any interpreter. Present as of schema 1.2.4;
    // reported upstream as bids-standard/bids-schema#13.
    const KNOWN_BAD: &[&str] = &["len(sidecar.EchoTime) == nifti_header.dim[4]"];

    let mut failures: Vec<String> = Vec::new();
    fn walk(v: &Value, path: &str, ctx: &EvalContext, failures: &mut Vec<String>, known: &[&str]) {
        let Value::Object(map) = v else { return };
        for key in ["selectors", "checks"] {
            if let Some(Value::Array(exprs)) = map.get(key) {
                for e in exprs.iter().filter_map(|e| e.as_str()) {
                    if known.contains(&e) {
                        continue;
                    }
                    if let Err(err) = evaluate(e, ctx) {
                        failures.push(format!("{path}.{key}: `{e}`\n      {err}"));
                    }
                }
            }
        }
        for (k, child) in map {
            walk(child, &format!("{path}.{k}"), ctx, failures, known);
        }
    }
    for root in ["rules", "meta"] {
        if let Some(v) = schema.get(root) {
            walk(v, root, &ctx, &mut failures, KNOWN_BAD);
        }
    }

    assert!(
        failures.is_empty(),
        "{} schema expression(s) could not be evaluated:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}
