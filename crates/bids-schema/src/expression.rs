//! BIDS schema expression language evaluator.
//!
//! The BIDS schema embeds a small JavaScript-like expression language in its
//! selectors and checks.  This module parses those expressions via
//! [`oxc_parser`] and walks the resulting AST to produce a [`serde_json::Value`]
//! result (truthy/falsy).
//!
//! Custom BIDS-DSL functions (`length`, `count`, `intersects`, `exists`, …)
//! are implemented as Rust functions dispatched from `eval_function`.

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    BinaryOperator, Expression, LogicalOperator, ObjectPropertyKind, Statement, UnaryOperator,
};
use oxc_parser::Parser;
use oxc_span::SourceType;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

// ============================================================================
// Value Extension Trait
// ============================================================================

/// Extension methods on [`serde_json::Value`] for JavaScript-style semantics.
pub trait ValueExt {
    /// JavaScript-style truthiness.
    fn is_truthy(&self) -> bool;
    /// Attempt to coerce a value to `f64` (numbers pass through, strings are
    /// parsed).
    ///
    /// Deliberately *not* named `as_f64`: `serde_json::Value` has an inherent `as_f64` that
    /// only accepts numbers, and Rust resolves inherent methods before trait methods. A
    /// trait method of that name would be silently shadowed at every call site, so string
    /// columns (TSV values are `Value::String`) would never coerce.
    fn coerce_f64(&self) -> Option<f64>;
    /// Return a JavaScript-compatible type name.
    fn type_name(&self) -> &'static str;
}

impl ValueExt for Value {
    fn is_truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(_) => true,
        }
    }

    fn coerce_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.parse::<f64>().ok(),
            _ => None,
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }
}

// ============================================================================
// Evaluation context
// ============================================================================

/// The environment a schema expression is evaluated against — the set of
/// bindings its bare identifiers resolve to.
///
/// BIDS selectors and checks are written as if their fields were local variables
/// (`suffix == "bold"`, `"RepetitionTime" in sidecar`, `intersects(dataset.modalities, …)`).
/// Pairing such an expression with this environment forms something like a
/// *quosure*, and evaluating it is *data-masking*: an identifier names a slot in
/// the bound data rather than a lexical variable. [`eval_ir`] performs that
/// masking, resolving each identifier against the bindings below.
///
/// The bindings come from two scopes, held by reference:
/// - `file` — the per-file bindings (`path`, `suffix`, `sidecar`, `associations`, …).
/// - `dataset` / `schema` / `subject` — shared bindings, the same for every file
///   in a dataset.
///
/// This is the Rust counterpart to the reference TS validator, which evaluates
/// `with (context)` against a live context object.
#[derive(Debug, Clone, Copy)]
pub struct EvalContext<'a> {
    /// Per-file bindings (`path`, `entities`, `suffix`, `sidecar`, `associations`, …).
    file: &'a Value,
    /// Bindings shared by every file in the dataset.
    dataset: &'a Value,
    schema: &'a Value,
    subject: &'a Value,
}

impl<'a> EvalContext<'a> {
    /// Bind the per-file value together with the shared dataset-scope values.
    pub fn new(file: &'a Value, dataset: &'a Value, schema: &'a Value, subject: &'a Value) -> Self {
        EvalContext {
            file,
            dataset,
            schema,
            subject,
        }
    }

    /// Bind only a per-file value; the shared `dataset` / `schema` / `subject`
    /// identifiers resolve to `null`. Suitable for selector sets that reference
    /// only file-level fields (e.g. association selectors).
    pub fn file_only(file: &'a Value, null: &'a Value) -> Self {
        EvalContext {
            file,
            dataset: null,
            schema: null,
            subject: null,
        }
    }

    /// Look up a top-level identifier, returning a borrow into whichever scope
    /// holds it: `dataset` / `schema` / `subject` from the shared bindings, and
    /// any other name from the per-file bindings.
    pub fn get(&self, name: &str) -> Option<&'a Value> {
        match name {
            "dataset" => Some(self.dataset),
            "schema" => Some(self.schema),
            "subject" => Some(self.subject),
            _ => self.file.get(name),
        }
    }
}

// ============================================================================
// Public API
// ============================================================================

/// Parse and evaluate a BIDS schema expression, returning its value.
///
/// The expression is parsed and lowered to an owned [`Expr`] exactly once — the result is
/// cached per source string (see [`compile_expression`]) — so evaluating the same selector
/// across thousands of files walks the cached tree without re-invoking the oxc parser.
pub fn evaluate(expr_str: &str, context: &EvalContext) -> Result<Value, String> {
    if expr_str.trim().is_empty() {
        return Ok(Value::Null);
    }
    let compiled = compile_expression(expr_str)?;
    eval_ir(&compiled, context).map(|c| c.into_owned())
}

/// Evaluate a BIDS schema expression and return whether the result is truthy.
pub fn evaluate_bool(expr_str: &str, context: &EvalContext) -> Result<bool, String> {
    evaluate(expr_str, context).map(|v| v.is_truthy())
}

/// Determine if a set of selectors matches the current context.
///
/// Selectors are expressions (e.g. `datatype == "anat"`) defined in the schema
/// that must all evaluate to true for a rule to apply.
///
/// A selector that evaluates to `null` does not apply the rule, per the schema. A selector this
/// evaluator *cannot* evaluate is indistinguishable from that here, and so silently disables its
/// rule — which is how unsupported syntax (parenthesized expressions, `**`) once turned off nine
/// rules unnoticed. `every_schema_expression_evaluates` (tests/expression_conformance.rs) asserts
/// that no expression in the bundled schema errors, so this branch is unreachable in practice.
pub fn do_selectors_select(selectors: Option<&[String]>, context: &EvalContext) -> bool {
    let mut applies = true;
    if let Some(selector) = selectors {
        for s in selector {
            match evaluate_bool(s, context) {
                Ok(res) => {
                    if !res {
                        applies = false;
                        break;
                    }
                }
                Err(_) => {
                    applies = false;
                    break;
                }
            }
        }
    }
    applies
}

// ============================================================================
// Macros — kept internal, used only within this module
// ============================================================================

/// Compute a numeric binary operation with null-propagation.
/// Returns `Value::Null` when either operand is null or non-numeric.
/// The closure receives two `f64`s and returns `Option<f64>` (returning `None`
/// signals an error like division by zero, which maps to `Null`).
macro_rules! numeric_binop {
    ($left:expr_2021, $right:expr_2021, |$a:ident, $b:ident| $calc:expr_2021) => {{
        if $left.is_null() || $right.is_null() {
            Ok(Value::Null)
        } else {
            match ($left.coerce_f64(), $right.coerce_f64()) {
                (Some($a), Some($b)) => match $calc {
                    Some(res) => Ok(to_json_number(res)),
                    None => Ok(Value::Null),
                },
                _ => Ok(Value::Null),
            }
        }
    }};
}

/// Compute an ordering comparison (`<`, `>`, `<=`, `>=`).
///
/// Ordering comparisons neither propagate null nor blanket-return `false`: they coerce null to
/// `0`, exactly as JavaScript's relational operators do (`ToNumber(null) == 0`). So `null > 1`
/// is `false` but `null >= -60` is `true`.
///
/// This is deliberately *not* what bids-specification's `src/schema/README.md` says — its table
/// reads "`null == 1` ⇒ `false` … Also `<`, `>`, `<=` and `>=`", which would make
/// `null >= -60` false. That is wrong: it would fire `SUSPICIOUS_NEGATIVE_EVENT_ONSET` on every
/// events.tsv with no `onset` column, which the reference validator does not do. The schema's
/// own `meta.expression_tests` covers `null == 1` but no ordering case, so nothing catches the
/// discrepancy. See bids-specification#2149, which reports exactly this prose-vs-tests conflict.
///
/// A non-numeric operand (e.g. `"n/a"`) still yields `false`, matching NaN comparison in JS.
macro_rules! cmp_binop {
    ($left:expr_2021, $right:expr_2021, |$a:ident, $b:ident| $calc:expr_2021) => {{
        // `ToNumber(null) == 0`; everything else coerces normally.
        let lv = if $left.is_null() {
            Some(0.0)
        } else {
            $left.coerce_f64()
        };
        let rv = if $right.is_null() {
            Some(0.0)
        } else {
            $right.coerce_f64()
        };
        match (lv, rv) {
            (Some($a), Some($b)) => Ok(Value::Bool($calc)),
            _ => Ok(Value::Bool(false)),
        }
    }};
}

/// Validate argument count (exact).
macro_rules! require_args {
    ($args:expr_2021, $count:expr_2021, $name:expr_2021) => {
        if $args.len() != $count {
            return Err(format!("{}() takes exactly {} arguments", $name, $count));
        }
    };
    ($args:expr_2021, $min:expr_2021, $max:expr_2021, $name:expr_2021) => {
        if $args.len() < $min || $args.len() > $max {
            return Err(format!(
                "{}() takes between {} and {} arguments",
                $name, $min, $max
            ));
        }
    };
}

/// Return `Ok(Value::Null)` early if any argument is null.
macro_rules! propagate_null {
    ($args:expr_2021) => {
        if $args.iter().any(|a| a.is_null()) {
            return Ok(Value::Null);
        }
    };
}

// ============================================================================
// Helpers
// ============================================================================

/// Convert an `f64` to a JSON number, preferring integer representation when
/// the value is whole and within `i64` range.
fn to_json_number(f: f64) -> Value {
    if f == f.floor() && f.abs() < i64::MAX as f64 {
        serde_json::json!(f as i64)
    } else {
        serde_json::json!(f)
    }
}

/// Render a value the way JavaScript's `+` would when concatenating (a bare string, not JSON —
/// so `"a" + "b"` is `ab`, never `"a""b"`).
fn js_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Coerce a [`Value`] to a sort-friendly string (avoids quoting for
/// `Value::String`, falls back to `Display` for other types).
fn value_to_sort_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ============================================================================
// AST Evaluation
// ============================================================================

// ── Owned expression IR ─────────────────────────────────────────────────────
//
// The oxc AST borrows its arena, so it cannot outlive a parse. To evaluate a selector
// across many files without re-parsing, the supported oxc `Expression` subset is lowered
// into this arena-free tree once (cached by `compile_expression`) and walked per file by
// `eval_ir`. Operator enums are reused from oxc (fieldless, `Copy`, `'static`), so their
// semantics stay identical to the parser's.
#[derive(Debug)]
enum Expr {
    Bool(bool),
    Num(f64),
    Str(String),
    Null,
    Ident(String),
    StaticMember {
        object: Box<Expr>,
        property: String,
    },
    ComputedMember {
        object: Box<Expr>,
        index: Box<Expr>,
    },
    Binary {
        op: BinaryOperator,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Logical {
        op: LogicalOperator,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Unary {
        op: UnaryOperator,
        argument: Box<Expr>,
    },
    Call {
        name: String,
        args: Vec<Expr>,
    },
    Array(Vec<Expr>),
    Object(Vec<(String, Expr)>),
}

/// Parse and lower a selector to an owned [`Expr`], caching the result per source string.
///
/// The oxc parse + lowering happens once per unique expression; every later call is a map
/// lookup and an `Arc` clone. Lowering *errors* (parse failures, unsupported syntax) are
/// cached too, so a malformed selector is diagnosed once rather than re-parsed per file. The
/// cache is process-global and read-mostly (a schema's selector set is fixed), so it uses an
/// `RwLock` to keep concurrent evaluation — ingest and validation both run files in parallel
/// — off a single mutex.
fn compile_expression(expr_str: &str) -> Result<Arc<Expr>, String> {
    /// Source string → its compiled IR (or the cached lowering error).
    type ExprCache = RwLock<HashMap<String, Result<Arc<Expr>, String>>>;
    static CACHE: OnceLock<ExprCache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| RwLock::new(HashMap::new()));

    if let Some(hit) = cache.read().unwrap().get(expr_str) {
        return hit.clone();
    }
    let compiled = compile_uncached(expr_str).map(Arc::new);
    cache
        .write()
        .unwrap()
        .insert(expr_str.to_string(), compiled.clone());
    compiled
}

/// Parse one expression string with oxc and lower it to an owned [`Expr`] (no caching).
fn compile_uncached(expr_str: &str) -> Result<Expr, String> {
    // Double backslashes so that JS string-literal unescaping in the parser preserves regex
    // escapes such as `\S` and `\.` (otherwise `'\S'` would decode to the literal `S`). This
    // mirrors the TS validator, which compiles `expr.replace(/\\/g, '\\\\')`.
    let escaped = expr_str.replace('\\', "\\\\");
    let allocator = Allocator::default();
    let parser = Parser::new(&allocator, &escaped, SourceType::default());
    let ret = parser.parse();
    if !ret.diagnostics.is_empty() {
        return Err(format!(
            "Parse error for '{}': {:?}",
            expr_str, ret.diagnostics[0]
        ));
    }

    if ret.program.body.is_empty() {
        if let Some(dir) = ret.program.directives.first() {
            return Ok(Expr::Str(dir.directive.to_string()));
        }
        return Ok(Expr::Null);
    }

    if let Statement::ExpressionStatement(expr_stmt) = &ret.program.body[0] {
        lower(&expr_stmt.expression)
    } else {
        Err("Expected an expression statement".to_string())
    }
}

/// Lower a supported oxc [`Expression`] node to the owned [`Expr`] IR. Structural errors
/// (spread, non-identifier callee, computed object keys, unsupported node kinds) surface
/// here; operand/operator-specific errors stay in [`eval_ir`] — both still reach the caller
/// as an `Err` from [`evaluate`], matching the previous single-pass evaluator.
fn lower(expr: &Expression) -> Result<Expr, String> {
    match expr {
        // OXC keeps parentheses as their own AST node; unwrap to the inner expression.
        Expression::ParenthesizedExpression(paren) => lower(&paren.expression),

        // ── Literals ────────────────────────────────────────────────────
        Expression::BooleanLiteral(lit) => Ok(Expr::Bool(lit.value)),
        Expression::NumericLiteral(lit) => Ok(Expr::Num(lit.value)),
        Expression::StringLiteral(lit) => Ok(Expr::Str(lit.value.to_string())),
        Expression::NullLiteral(_) => Ok(Expr::Null),

        // ── Identifiers & member access ─────────────────────────────────
        Expression::Identifier(id) => Ok(Expr::Ident(id.name.to_string())),
        Expression::StaticMemberExpression(member) => Ok(Expr::StaticMember {
            object: Box::new(lower(&member.object)?),
            property: member.property.name.to_string(),
        }),
        Expression::ComputedMemberExpression(member) => Ok(Expr::ComputedMember {
            object: Box::new(lower(&member.object)?),
            index: Box::new(lower(&member.expression)?),
        }),

        // ── Operators (evaluated in `eval_ir`) ──────────────────────────
        Expression::BinaryExpression(bin) => Ok(Expr::Binary {
            op: bin.operator,
            left: Box::new(lower(&bin.left)?),
            right: Box::new(lower(&bin.right)?),
        }),
        Expression::LogicalExpression(log) => Ok(Expr::Logical {
            op: log.operator,
            left: Box::new(lower(&log.left)?),
            right: Box::new(lower(&log.right)?),
        }),
        Expression::UnaryExpression(unary) => Ok(Expr::Unary {
            op: unary.operator,
            argument: Box::new(lower(&unary.argument)?),
        }),

        // ── Function calls ──────────────────────────────────────────────
        Expression::CallExpression(call) => {
            let name = match &call.callee {
                Expression::Identifier(id) => id.name.to_string(),
                _ => return Err("Expected identifier for function call".to_string()),
            };
            let args = call
                .arguments
                .iter()
                .map(|arg| {
                    arg.as_expression()
                        .ok_or_else(|| "Spread arguments not supported".to_string())
                        .and_then(lower)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Expr::Call { name, args })
        }

        // ── Array literals ──────────────────────────────────────────────
        Expression::ArrayExpression(arr) => {
            let elems = arr
                .elements
                .iter()
                .map(|elem| {
                    elem.as_expression()
                        .ok_or_else(|| "Spread elements not supported".to_string())
                        .and_then(lower)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Expr::Array(elems))
        }

        Expression::ObjectExpression(obj) => {
            let mut props = Vec::with_capacity(obj.properties.len());
            for prop in &obj.properties {
                let ObjectPropertyKind::ObjectProperty(p) = prop else {
                    return Err("Spread properties not supported".to_string());
                };
                let key = p
                    .key
                    .static_name()
                    .ok_or_else(|| "Computed object keys not supported".to_string())?;
                props.push((key.to_string(), lower(&p.value)?));
            }
            Ok(Expr::Object(props))
        }

        other => Err(format!(
            "Unsupported expression type: {}",
            std::any::type_name_of_val(other)
        )),
    }
}

/// Walk the owned [`Expr`] IR, resolving its identifiers against `ctx`
/// (the data-masking step; see [`EvalContext`]).
///
/// The result is a [`Cow`]: an identifier or `obj.field` access yields a borrow
/// into the bound context, and a computed result (arithmetic, a function call, an
/// array/object literal) yields an owned value.
fn eval_ir<'a>(expr: &Expr, ctx: &EvalContext<'a>) -> Result<Cow<'a, Value>, String> {
    match expr {
        // ── Literals ────────────────────────────────────────────────────
        Expr::Bool(b) => Ok(Cow::Owned(Value::Bool(*b))),
        Expr::Num(n) => Ok(Cow::Owned(to_json_number(*n))),
        Expr::Str(s) => Ok(Cow::Owned(Value::String(s.clone()))),
        Expr::Null => Ok(Cow::Owned(Value::Null)),

        // ── Identifiers & member access ─────────────────────────────────
        Expr::Ident(name) => resolve_ident(name, ctx),
        Expr::StaticMember { object, property } => {
            let obj = eval_ir(object, ctx)?;
            resolve_field(obj, property)
        }
        Expr::ComputedMember { object, index } => {
            let obj = eval_ir(object, ctx)?;
            let idx = eval_ir(index, ctx)?;
            resolve_index(obj, idx)
        }

        // ── Binary operators (inlined — no string indirection) ──────────
        Expr::Binary { op, left, right } => {
            let left = eval_ir(left, ctx)?;
            let right = eval_ir(right, ctx)?;
            match op {
                // Equality (loose and strict treated identically for JSON values)
                BinaryOperator::Equality | BinaryOperator::StrictEquality => {
                    Ok(Cow::Owned(Value::Bool(*left == *right)))
                }
                BinaryOperator::Inequality | BinaryOperator::StrictInequality => {
                    Ok(Cow::Owned(Value::Bool(*left != *right)))
                }

                // Numeric comparisons
                BinaryOperator::LessThan => cmp_binop!(left, right, |a, b| a < b).map(Cow::Owned),
                BinaryOperator::GreaterThan => {
                    cmp_binop!(left, right, |a, b| a > b).map(Cow::Owned)
                }
                BinaryOperator::LessEqualThan => {
                    cmp_binop!(left, right, |a, b| a <= b).map(Cow::Owned)
                }
                BinaryOperator::GreaterEqualThan => {
                    cmp_binop!(left, right, |a, b| a >= b).map(Cow::Owned)
                }

                // `"key" in object` — `"x" in null` is `null`, not `false`.
                BinaryOperator::In => match (&*left, &*right) {
                    (_, Value::Null) => Ok(Cow::Owned(Value::Null)),
                    (Value::String(key), Value::Object(map)) => {
                        Ok(Cow::Owned(Value::Bool(map.contains_key(key))))
                    }
                    _ => Ok(Cow::Owned(Value::Bool(false))),
                },

                // Arithmetic. `+` is overloaded: string concatenation when either side is a
                // string, numeric addition otherwise (`null + 1` stays `null`).
                BinaryOperator::Addition => match (&*left, &*right) {
                    (Value::Null, _) | (_, Value::Null) => Ok(Cow::Owned(Value::Null)),
                    (Value::String(_), _) | (_, Value::String(_)) => Ok(Cow::Owned(Value::String(
                        format!("{}{}", js_to_string(&left), js_to_string(&right)),
                    ))),
                    _ => numeric_binop!(left, right, |a, b| Some(a + b)).map(Cow::Owned),
                },
                BinaryOperator::Subtraction => {
                    numeric_binop!(left, right, |a, b| Some(a - b)).map(Cow::Owned)
                }
                BinaryOperator::Multiplication => {
                    numeric_binop!(left, right, |a, b| Some(a * b)).map(Cow::Owned)
                }
                BinaryOperator::Division => numeric_binop!(left, right, |a, b| if b == 0.0 {
                    None
                } else {
                    Some(a / b)
                })
                .map(Cow::Owned),
                BinaryOperator::Remainder => numeric_binop!(left, right, |a, b| if b == 0.0 {
                    None
                } else {
                    Some(a % b)
                })
                .map(Cow::Owned),
                // `**` is undocumented in the schema's operator table but used by
                // `rules.checks.micr.PixelSizeInconsistent` and `rules.checks.func.RepetitionTimeMismatch`.
                BinaryOperator::Exponential => {
                    numeric_binop!(left, right, |a, b| Some(a.powf(b))).map(Cow::Owned)
                }

                _ => Err(format!("Unsupported binary operator {op:?}")),
            }
        }

        // ── Logical operators ───────────────────────────────────────────
        // JavaScript semantics: short-circuit, and the *operand* is the result, not a boolean.
        // This is what makes `false && null` be `false` while `null && true` is `null`, and it
        // is how `null` propagates through `&&`/`||` without a special case.
        Expr::Logical { op, left, right } => {
            let left = eval_ir(left, ctx)?;
            match op {
                LogicalOperator::And => {
                    if left.is_truthy() {
                        eval_ir(right, ctx)
                    } else {
                        Ok(left)
                    }
                }
                LogicalOperator::Or => {
                    if left.is_truthy() {
                        Ok(left)
                    } else {
                        eval_ir(right, ctx)
                    }
                }
                _ => Err(format!("Unsupported logical operator {op:?}")),
            }
        }

        // ── Unary operators ─────────────────────────────────────────────
        Expr::Unary { op, argument } => match op {
            // `!null` is `true`, not `null` — negation always yields a boolean.
            UnaryOperator::LogicalNot => {
                let val = eval_ir(argument, ctx)?;
                Ok(Cow::Owned(Value::Bool(!val.is_truthy())))
            }
            UnaryOperator::UnaryNegation | UnaryOperator::UnaryPlus => {
                let val = eval_ir(argument, ctx)?;
                if val.is_null() {
                    return Ok(Cow::Owned(Value::Null));
                }
                let n = val
                    .coerce_f64()
                    .ok_or_else(|| format!("Cannot apply unary {op:?} to {}", *val))?;
                let n = if *op == UnaryOperator::UnaryNegation {
                    -n
                } else {
                    n
                };
                Ok(Cow::Owned(to_json_number(n)))
            }
            _ => Err(format!("Unsupported unary operator {op:?}")),
        },

        // ── Function calls ──────────────────────────────────────────────
        Expr::Call { name, args } => {
            // DSL functions take owned values, so collect the arguments. These are small (a
            // single field, a literal array) — large `dataset`/`schema` subtrees are reached
            // by identifier resolution, which borrows them.
            let args: Result<Vec<Value>, String> = args
                .iter()
                .map(|e| eval_ir(e, ctx).map(|c| c.into_owned()))
                .collect();
            eval_function(name, &args?, ctx).map(Cow::Owned)
        }

        // ── Array literals ──────────────────────────────────────────────
        Expr::Array(elems) => {
            let vals: Result<Vec<Value>, String> = elems
                .iter()
                .map(|e| eval_ir(e, ctx).map(|c| c.into_owned()))
                .collect();
            Ok(Cow::Owned(Value::Array(vals?)))
        }

        Expr::Object(props) => {
            let mut map = serde_json::Map::new();
            for (key, value) in props {
                map.insert(key.clone(), eval_ir(value, ctx)?.into_owned());
            }
            Ok(Cow::Owned(Value::Object(map)))
        }
    }
}

/// Resolve a bare identifier against the context, borrowing when present.
fn resolve_ident<'a>(name: &str, ctx: &EvalContext<'a>) -> Result<Cow<'a, Value>, String> {
    Ok(ctx
        .get(name)
        .map(Cow::Borrowed)
        .unwrap_or(Cow::Owned(Value::Null)))
}

/// Resolve a static member expression (`obj.field`), borrowing the field when the
/// object is itself borrowed.
///
/// Reading a field of `null` yields `null` rather than an error: the schema specifies
/// `null.anything ⇒ null` (bids-specification, `src/schema/README.md`, "The special value
/// `null`"), and `meta.expression_tests` asserts it. Selectors such as
/// `dataset.dataset_description.DatasetType == "raw"` rely on this when the dataset has no
/// `dataset_description`.
fn resolve_field<'a>(obj: Cow<'a, Value>, field: &str) -> Result<Cow<'a, Value>, String> {
    match obj {
        Cow::Borrowed(v) => match v {
            Value::Object(map) => Ok(map
                .get(field)
                .map(Cow::Borrowed)
                .unwrap_or(Cow::Owned(Value::Null))),
            Value::Null => Ok(Cow::Owned(Value::Null)),
            _ => Err(format!("Cannot read property '{}' of non-object", field)),
        },
        Cow::Owned(v) => match v {
            Value::Object(mut map) => Ok(Cow::Owned(map.remove(field).unwrap_or(Value::Null))),
            Value::Null => Ok(Cow::Owned(Value::Null)),
            _ => Err(format!("Cannot read property '{}' of non-object", field)),
        },
    }
}

/// Resolve a computed member expression (`obj[idx]`) — array element, object
/// value by key, or a character of a string. The result is owned; an indexed
/// element is typically a scalar, so cloning it costs little.
fn resolve_index<'a>(obj: Cow<'a, Value>, idx: Cow<'a, Value>) -> Result<Cow<'a, Value>, String> {
    // `null[0]` is `null`, mirroring `null.anything`.
    if obj.is_null() {
        return Ok(Cow::Owned(Value::Null));
    }
    if idx.is_null() {
        return Ok(Cow::Owned(Value::Null));
    }
    match (&*obj, &*idx) {
        (Value::Array(arr), Value::Number(i)) if i.is_i64() => {
            let num = i.as_i64().unwrap();
            let pos = if num < 0 {
                (arr.len() as i64 + num) as usize
            } else {
                num as usize
            };
            Ok(Cow::Owned(arr.get(pos).cloned().unwrap_or(Value::Null)))
        }
        (Value::Object(map), Value::String(key)) => {
            Ok(Cow::Owned(map.get(key).cloned().unwrap_or(Value::Null)))
        }
        (Value::String(s), Value::Number(i)) if i.is_i64() => Ok(Cow::Owned(
            s.chars()
                .nth(i.as_i64().unwrap() as usize)
                .map(|c| Value::String(c.to_string()))
                .unwrap_or(Value::Null),
        )),
        _ => Err("Invalid index access".to_string()),
    }
}

// ============================================================================
// DSL Functions
// ============================================================================

/// Dispatch a BIDS-DSL function call to its Rust implementation.
fn eval_function(name: &str, args: &[Value], ctx: &EvalContext) -> Result<Value, String> {
    match name {
        "length" => func_length(args),
        "count" => func_count(args),
        "index" => func_index(args),
        "intersects" => func_intersects(args),
        "match" => func_match(args),
        "type" => func_type(args),
        "min" => func_min(args),
        "max" => func_max(args),
        "substr" => func_substr(args),
        "sorted" => func_sorted(args),
        "unique" => func_unique(args),
        "allequal" => func_allequal(args),
        "exists" => func_exists(args, ctx),
        _ => Err(format!("Unknown function: {}", name)),
    }
}

/// `length(x)` — length of an array, string, or object (key count).
fn func_length(args: &[Value]) -> Result<Value, String> {
    require_args!(args, 1, "length");
    propagate_null!(args);
    match &args[0] {
        Value::Array(a) => Ok(serde_json::json!(a.len() as i64)),
        Value::String(s) => Ok(serde_json::json!(s.len() as i64)),
        Value::Object(m) => Ok(serde_json::json!(m.len() as i64)),
        _ => Ok(Value::Null),
    }
}

/// `count(array, value)` — number of elements in `array` equal to `value`.
fn func_count(args: &[Value]) -> Result<Value, String> {
    require_args!(args, 2, "count");
    propagate_null!(args);
    if let Value::Array(arr) = &args[0] {
        let count = arr.iter().filter(|v| *v == &args[1]).count();
        Ok(serde_json::json!(count as i64))
    } else {
        Ok(Value::Null)
    }
}

/// `index(array, value)` — index of the first occurrence of `value` in `array`.
fn func_index(args: &[Value]) -> Result<Value, String> {
    require_args!(args, 2, "index");
    propagate_null!(args);
    if let Value::Array(arr) = &args[0]
        && let Some(pos) = arr.iter().position(|v| v == &args[1])
    {
        return Ok(serde_json::json!(pos as i64));
    }
    Ok(Value::Null)
}

/// `intersects(a, b)` — return overlapping elements, or `false` if none.
fn func_intersects(args: &[Value]) -> Result<Value, String> {
    require_args!(args, 2, "intersects");
    // A null operand means "no shared values", not "unknown": `intersects([], null) == false`
    // (`meta.expression_tests`).
    if args.iter().any(|a| a.is_null()) {
        return Ok(Value::Bool(false));
    }
    // Tolerate single (non-array) values by treating them as one-element arrays, matching the
    // TS validator's `intersects`.
    let as_vec = |v: &Value| -> Vec<Value> {
        match v {
            Value::Array(a) => a.clone(),
            other => vec![other.clone()],
        }
    };
    let a = as_vec(&args[0]);
    let b = as_vec(&args[1]);
    let intersection: Vec<Value> = a.iter().filter(|v| b.contains(v)).cloned().collect();
    if intersection.is_empty() {
        Ok(Value::Bool(false))
    } else {
        Ok(Value::Array(intersection))
    }
}

fn get_or_compile_regex(pattern: &str) -> Result<regex::Regex, String> {
    static REGEX_CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, regex::Regex>>,
    > = std::sync::OnceLock::new();
    let cache = REGEX_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));

    let mut map = cache.lock().unwrap();
    if let Some(re) = map.get(pattern) {
        return Ok(re.clone());
    }

    let re =
        regex::Regex::new(pattern).map_err(|e| format!("Invalid regex '{}': {}", pattern, e))?;
    map.insert(pattern.to_string(), re.clone());
    Ok(re)
}

/// `match(string, pattern)` — test a regex pattern against a string.
fn func_match(args: &[Value]) -> Result<Value, String> {
    require_args!(args, 2, "match");
    // Asymmetric, per `meta.expression_tests`: a null subject is unknown (`match(null, p)` is
    // `null`), but a null pattern matches nothing (`match('string', null)` is `false`).
    if args[0].is_null() {
        return Ok(Value::Null);
    }
    if args[1].is_null() {
        return Ok(Value::Bool(false));
    }
    if let (Value::String(s), Value::String(pattern)) = (&args[0], &args[1]) {
        let re = get_or_compile_regex(pattern)?;
        Ok(Value::Bool(re.is_match(s)))
    } else {
        Ok(Value::Null)
    }
}

/// `type(value)` — return the JavaScript-style type name.
fn func_type(args: &[Value]) -> Result<Value, String> {
    require_args!(args, 1, "type");
    Ok(Value::String(args[0].type_name().to_string()))
}

/// Shared implementation for `min` and `max` — fold an array of numbers.
///
/// A scalar argument is treated as a one-element array (`min(42) == 42`), per the schema's
/// own conformance cases in `meta.expression_tests`. Non-numeric elements (e.g. `"n/a"`) are
/// skipped, so `min([-1, "n/a", 1]) == -1`.
fn aggregate_array<F>(args: &[Value], name: &str, agg_fn: F) -> Result<Value, String>
where
    F: Fn(f64, f64) -> f64,
{
    require_args!(args, 1, name);
    propagate_null!(args);
    let result = match &args[0] {
        Value::Array(arr) => arr.iter().filter_map(|v| v.coerce_f64()).reduce(agg_fn),
        scalar => scalar.coerce_f64(),
    };
    Ok(result.map(|n| serde_json::json!(n)).unwrap_or(Value::Null))
}

/// `min(array)` — smallest numeric element.
fn func_min(args: &[Value]) -> Result<Value, String> {
    aggregate_array(args, "min", f64::min)
}

/// `max(array)` — largest numeric element.
fn func_max(args: &[Value]) -> Result<Value, String> {
    aggregate_array(args, "max", f64::max)
}

/// `substr(string, start, end)` — substring by character indices.
fn func_substr(args: &[Value]) -> Result<Value, String> {
    require_args!(args, 3, "substr");
    propagate_null!(args);
    match (&args[0], &args[1], &args[2]) {
        (Value::String(s), Value::Number(start), Value::Number(end))
            if start.is_i64() && end.is_i64() =>
        {
            let start_idx = start.as_i64().unwrap() as usize;
            let end_idx = end.as_i64().unwrap() as usize;
            let result: String = s
                .chars()
                .skip(start_idx)
                .take(end_idx - start_idx)
                .collect();
            Ok(Value::String(result))
        }
        _ => Ok(Value::Null),
    }
}

/// `sorted(array[, method])` — return a sorted copy.
///
/// `method` is one of `"numeric"`, `"lexical"`, or `"default"` (try numeric,
/// fall back to lexical).
fn func_sorted(args: &[Value]) -> Result<Value, String> {
    require_args!(args, 1, 2, "sorted");
    propagate_null!(args);
    let method = args.get(1).and_then(|v| v.as_str()).unwrap_or("default");

    if let Value::Array(arr) = &args[0] {
        let mut sorted = arr.clone();
        match method {
            "numeric" => {
                sorted.sort_by(|a, b| {
                    let af = a.coerce_f64().unwrap_or(f64::NAN);
                    let bf = b.coerce_f64().unwrap_or(f64::NAN);
                    af.partial_cmp(&bf).unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            "lexical" => {
                sorted.sort_by_key(value_to_sort_string);
            }
            _ => {
                // Default is type-determined, not parse-determined: numeric strings still sort
                // lexically (`sorted(["1","2","5","10"]) == ["1","10","2","5"]`). Only actual
                // JSON numbers sort numerically.
                sorted.sort_by(|a, b| match (a, b) {
                    (Value::Number(_), Value::Number(_)) => {
                        let (af, bf) = (a.coerce_f64(), b.coerce_f64());
                        match (af, bf) {
                            (Some(af), Some(bf)) => {
                                af.partial_cmp(&bf).unwrap_or(std::cmp::Ordering::Equal)
                            }
                            _ => std::cmp::Ordering::Equal,
                        }
                    }
                    _ => value_to_sort_string(a).cmp(&value_to_sort_string(b)),
                });
            }
        }
        Ok(Value::Array(sorted))
    } else {
        Ok(Value::Null)
    }
}

/// `unique(array)` — return deduplicated elements preserving first occurrence.
fn func_unique(args: &[Value]) -> Result<Value, String> {
    require_args!(args, 1, "unique");
    propagate_null!(args);
    if let Value::Array(arr) = &args[0] {
        let mut result = Vec::new();
        for v in arr {
            if !result.contains(v) {
                result.push(v.clone());
            }
        }
        Ok(Value::Array(result))
    } else {
        Ok(Value::Null)
    }
}

/// `allequal(a, b)` — true iff two arrays have identical elements in order.
fn func_allequal(args: &[Value]) -> Result<Value, String> {
    require_args!(args, 2, "allequal");
    // `allequal([], null) == false` (`meta.expression_tests`) — a null operand is unequal,
    // not unknown.
    match (&args[0], &args[1]) {
        (Value::Array(a), Value::Array(b)) => Ok(Value::Bool(a == b)),
        _ => Ok(Value::Bool(false)),
    }
}

/// `exists(path, rule)` — check whether a path (or array of paths) exists in
/// the dataset file tree, applying the given rule-specific path resolution.
fn func_exists(args: &[Value], ctx: &EvalContext) -> Result<Value, String> {
    require_args!(args, 2, "exists");
    // `exists` counts files, so a null operand names no files: the count is 0, not null
    // (`meta.expression_tests`).
    if args.iter().any(|a| a.is_null()) {
        return Ok(serde_json::json!(0));
    }

    let rule = match &args[1] {
        Value::String(s) => s.as_str(),
        _ => return Ok(Value::Null),
    };

    let tree = ctx
        .get("dataset")
        .and_then(|d| d.get("tree"))
        .and_then(|t| t.as_array());

    let resolve_path = |path: &str| -> Option<String> {
        let mut p = path.to_string();

        let is_bids_uri = p.starts_with("bids:");

        if rule == "bids-uri" && !is_bids_uri {
            return None;
        }
        if rule != "bids-uri" && is_bids_uri {
            return None;
        }

        // Strip bids: / bids:: prefix
        if p.starts_with("bids::") {
            p = p["bids::".len()..].to_string();
        } else if p.starts_with("bids:") {
            p = p["bids:".len()..].to_string();
        }

        match rule {
            "stimuli" => {
                if !p.starts_with("stimuli/") && !p.starts_with("/stimuli/") {
                    p = format!("/stimuli/{}", p.trim_start_matches('/'));
                } else if !p.starts_with('/') {
                    p = format!("/{}", p);
                }
            }
            "subject" => {
                if let Some(sub) = ctx
                    .get("entities")
                    .and_then(|e| e.get("subject"))
                    .and_then(|s| s.as_str())
                {
                    let prefix = format!("sub-{}/", sub);
                    if !p.starts_with(&prefix) && !p.starts_with(&format!("/{}", prefix)) {
                        p = format!("/{}{}", prefix, p.trim_start_matches('/'));
                    } else if !p.starts_with('/') {
                        p = format!("/{}", p);
                    }
                } else if !p.starts_with('/') {
                    p = format!("/{}", p);
                }
            }
            "file" => {
                let current_dir = ctx
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(|s| s.rsplit_once('/').map_or("", |(dir, _)| dir))
                    .unwrap_or("");
                if !p.starts_with('/') {
                    p = format!("{}/{}", current_dir, p);
                }
            }
            // "dataset" and any other rule: ensure leading `/`.
            _ => {
                if !p.starts_with('/') {
                    p = format!("/{}", p);
                }
            }
        }
        Some(p)
    };

    let count_matches = |paths: &[&str]| -> i64 {
        let Some(tree_files) = tree else { return 0 };
        paths
            .iter()
            .filter_map(|path| resolve_path(path))
            .filter(|full| tree_files.iter().any(|v| v.as_str() == Some(full)))
            .count() as i64
    };

    match &args[0] {
        Value::String(path) => Ok(serde_json::json!(count_matches(&[path]))),
        Value::Array(paths) => {
            let path_strs: Vec<&str> = paths.iter().filter_map(|v| v.as_str()).collect();
            Ok(serde_json::json!(count_matches(&path_strs)))
        }
        _ => Ok(Value::Null),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    static NULL: Value = Value::Null;

    fn empty_ctx() -> Value {
        serde_json::json!({})
    }

    /// Wrap a plain per-file JSON value as an [`EvalContext`] for tests (the
    /// shared `dataset`/`schema`/`subject` identifiers resolve to null).
    fn ec(v: &Value) -> EvalContext<'_> {
        EvalContext::file_only(v, &NULL)
    }

    /// Ordering comparisons coerce `null` to `0`, as JavaScript does — they neither propagate
    /// null nor uniformly return `false`.
    ///
    /// `meta.expression_tests` has no ordering-with-null case, so the generated conformance
    /// suite cannot catch a regression here; this test stands in for it. Getting this wrong in
    /// either direction is observable on real datasets:
    ///   - blanket `false` fires `SUSPICIOUS_NEGATIVE_EVENT_ONSET` on every events.tsv that has
    ///     no `onset` column (`min(columns.onset) >= -60`);
    ///   - propagating null suppresses `TOO_FEW_AUTHORS` when `Authors` is absent
    ///     (`length(json.Authors) > 1`).
    ///
    /// bids-specification's prose claims ordering behaves like `==` here. It does not; see
    /// bids-specification#2149.
    #[test]
    fn ordering_comparisons_coerce_null_to_zero() {
        let v = empty_ctx();
        let ctx = ec(&v);
        for (expr, want) in [
            ("null > 1", false),
            ("null >= -60", true),
            ("null < 1", true),
            ("null <= -60", false),
            ("length(json.Authors) > 1", false), // absent Authors ⇒ TOO_FEW_AUTHORS fires
            ("min(columns.onset) >= -60", true), // absent onset column ⇒ no warning
            ("max(columns.onset) < 2678400", true),
            // Non-numeric operands compare false, like NaN in JS.
            ("\"n/a\" > 1", false),
            ("\"n/a\" <= 1", false),
        ] {
            let got = evaluate(expr, &ctx).unwrap();
            assert_eq!(got, Value::Bool(want), "`{expr}` should be {want}");
        }
    }

    #[test]
    fn test_literal_expressions() {
        assert_eq!(
            evaluate("42", &ec(&empty_ctx())).unwrap(),
            serde_json::json!(42)
        );
        assert_eq!(
            evaluate("4.13", &ec(&empty_ctx())).unwrap(),
            serde_json::json!(4.13)
        );
        assert_eq!(
            evaluate("\"hello\"", &ec(&empty_ctx())).unwrap(),
            Value::String("hello".to_string())
        );
        assert_eq!(
            evaluate("true", &ec(&empty_ctx())).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(evaluate("null", &ec(&empty_ctx())).unwrap(), Value::Null);
    }

    #[test]
    fn test_comparison_operators() {
        let ctx = serde_json::json!({ "suffix": "T1w" });
        assert!(evaluate_bool("suffix == \"T1w\"", &ec(&ctx)).unwrap());
        assert!(!evaluate_bool("suffix == \"bold\"", &ec(&ctx)).unwrap());
        assert!(evaluate_bool("suffix != \"bold\"", &ec(&ctx)).unwrap());
    }

    #[test]
    fn test_in_operator() {
        let ctx = serde_json::json!({
            "sidecar": { "Units": "mm", "RepetitionTime": 2.0 }
        });
        assert!(evaluate_bool("\"Units\" in sidecar", &ec(&ctx)).unwrap());
        assert!(!evaluate_bool("\"Missing\" in sidecar", &ec(&ctx)).unwrap());
    }

    #[test]
    fn test_logical_operators() {
        let ctx = serde_json::json!({ "a": true, "b": false });
        assert!(!evaluate_bool("a && b", &ec(&ctx)).unwrap());
        assert!(evaluate_bool("a || b", &ec(&ctx)).unwrap());
        assert!(evaluate_bool("!b", &ec(&ctx)).unwrap());
    }

    #[test]
    fn test_null_propagation() {
        let ctx = empty_ctx();
        assert_eq!(evaluate("missing_var", &ec(&ctx)).unwrap(), Value::Null);
        assert_eq!(
            evaluate("missing_var == 5", &ec(&ctx)).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(evaluate("missing_var + 5", &ec(&ctx)).unwrap(), Value::Null);
    }

    #[test]
    fn test_dot_access() {
        let ctx = serde_json::json!({
            "sidecar": { "RepetitionTime": 2.0 },
            "nifti_header": { "dim": [4, 2, 2, 2, 2, 1, 1, 1] }
        });
        assert_eq!(
            evaluate("sidecar.RepetitionTime", &ec(&ctx)).unwrap(),
            serde_json::json!(2.0)
        );
        assert_eq!(evaluate("sidecar.Missing", &ec(&ctx)).unwrap(), Value::Null);
        assert!(evaluate_bool("nifti_header != null", &ec(&ctx)).unwrap());
        assert!(!evaluate_bool("nifti_header.dim[0] == 3", &ec(&ctx)).unwrap());
    }

    #[test]
    fn test_type_function() {
        let ctx = serde_json::json!({
            "str_var": "hello",
            "int_var": 42,
            "bool_var": true,
            "arr_var": [1, 2, 3],
            "obj_var": { "a": 1 }
        });
        assert_eq!(
            evaluate("type(str_var)", &ec(&ctx)).unwrap(),
            Value::String("string".to_string())
        );
        assert_eq!(
            evaluate("type(int_var)", &ec(&ctx)).unwrap(),
            Value::String("number".to_string())
        );
        assert_eq!(
            evaluate("type(bool_var)", &ec(&ctx)).unwrap(),
            Value::String("boolean".to_string())
        );
        assert_eq!(
            evaluate("type(arr_var)", &ec(&ctx)).unwrap(),
            Value::String("array".to_string())
        );
        assert_eq!(
            evaluate("type(obj_var)", &ec(&ctx)).unwrap(),
            Value::String("object".to_string())
        );
        assert_eq!(
            evaluate("type(missing_var)", &ec(&ctx)).unwrap(),
            Value::String("null".to_string())
        );
    }

    #[test]
    fn test_bundled_schema_expressions() {
        fn parse_only(expr_str: &str) -> Result<(), String> {
            if expr_str.trim().is_empty() {
                return Ok(());
            }
            let allocator = oxc_allocator::Allocator::default();
            let parser =
                oxc_parser::Parser::new(&allocator, expr_str, oxc_span::SourceType::default());
            let ret = parser.parse();
            if !ret.diagnostics.is_empty() {
                return Err(format!(
                    "Parse error for '{}': {:?}",
                    expr_str, ret.diagnostics[0]
                ));
            }
            Ok(())
        }

        let schema_str = include_str!(concat!(env!("OUT_DIR"), "/schema.json"));
        let schema_json: Value =
            serde_json::from_str(schema_str).expect("Failed to parse schema.json");
        let rules = schema_json
            .get("rules")
            .expect("No rules in schema")
            .as_object()
            .expect("rules is not an object");

        for (_category, rule_group) in rules {
            if let Some(rule_obj) = rule_group.as_object() {
                for (rule_name, rule_def) in rule_obj {
                    if let Some(selectors) = rule_def.get("selectors").and_then(|s| s.as_array()) {
                        for selector in selectors {
                            if let Some(s) = selector.as_str() {
                                let res = parse_only(s);
                                assert!(
                                    res.is_ok(),
                                    "Failed to parse selector in {}: {}\nError: {:?}",
                                    rule_name,
                                    s,
                                    res.err()
                                );
                            }
                        }
                    }
                    if let Some(checks) = rule_def.get("checks").and_then(|c| c.as_array()) {
                        for check in checks {
                            if let Some(c) = check.as_str() {
                                let res = parse_only(c);
                                assert!(
                                    res.is_ok(),
                                    "Failed to parse check in {}: {}\nError: {:?}",
                                    rule_name,
                                    c,
                                    res.err()
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
