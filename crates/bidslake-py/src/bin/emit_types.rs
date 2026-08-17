//! Render `python/bidslake/schema/_generated.py` from the bidslake schema model.
//!
//! Run with `cargo run -p bidslake-py --bin emit-types [OUT_PATH]`. This reuses
//! the *exact* DDL path (`Schema::load` + `BidsDb::create_tables`) and reads the
//! resulting column set back out of `information_schema`, so the generated
//! Python column tables can never drift from what a real database has. The
//! enumerable value sets for entities/datatypes/modalities come from the shared
//! `bids_schema` accessors — the same derivations bidslake's DDL uses, so the
//! Python `Literal`s can't drift from the columns they describe — while suffixes
//! and the `Levels` columns are read from the schema JSON here.
//!
//! No Python-side port of any naming/type logic lives here — this file only
//! serializes the Rust model into inline-typed Python.

use std::collections::BTreeMap;

use anyhow::Result;
use bidslake::db::BidsDb;
use bidslake::schema::Schema;
use serde_json::Value;

/// A Python string literal with the necessary escapes. `serde_json::to_string`
/// emits a valid double-quoted, fully-escaped string (control characters and all)
/// that is also a valid Python `str` literal — unlike a hand-rolled `\\`/`"` swap.
fn pystr(s: &str) -> String {
    serde_json::to_string(s).expect("a &str always serializes")
}

/// `type NAME = Literal["a", "b", …]` (or `= str` when the set is empty).
fn literal_alias(name: &str, values: &[String]) -> String {
    if values.is_empty() {
        return format!("type {name} = str");
    }
    let items = values
        .iter()
        .map(|v| pystr(v))
        .collect::<Vec<_>>()
        .join(", ");
    format!("type {name} = Literal[{items}]")
}

/// Suffix strings (the `value` field of each `objects.suffixes` entry).
fn suffix_values(raw: &Value) -> Vec<String> {
    let mut v: Vec<String> = raw["objects"]["suffixes"]
        .as_object()
        .map(|m| {
            m.values()
                .filter_map(|s| s.get("value").and_then(|x| x.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v.dedup();
    v
}

/// The allowed `Levels` values of a column (e.g. `sex`, `handedness`), if any.
fn column_levels(raw: &Value, col: &str) -> Vec<String> {
    let c = &raw["objects"]["columns"][col];
    let levels = c
        .get("Levels")
        .or_else(|| c.get("definition").and_then(|d| d.get("Levels")));
    levels
        .and_then(|l| l.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// Every `main`-schema table's `(column, duckdb_type)` in ordinal order, read
/// from a throwaway in-memory database built via the real `create_tables`.
fn introspect_columns() -> Result<BTreeMap<String, Vec<(String, String)>>> {
    let schema = Schema::load(None)?;
    let db = BidsDb::new(":memory:")?;
    db.create_tables(&schema)?;
    let mut out: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut stmt = db.conn.prepare(
        "SELECT table_name, column_name, data_type FROM information_schema.columns \
         WHERE table_schema = 'main' ORDER BY table_name, ordinal_position",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (table, col, ty) = row?;
        out.entry(table).or_default().push((col, ty));
    }
    Ok(out)
}

/// Every table's declared primary-key columns. Views have none, so they are absent
/// here and [`model_pk`] picks a synthetic one.
fn introspect_primary_keys() -> Result<BTreeMap<String, Vec<String>>> {
    let schema = Schema::load(None)?;
    let db = BidsDb::new(":memory:")?;
    db.create_tables(&schema)?;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // `constraint_column_names` is a `VARCHAR[]`; unnest it rather than parsing the list
    // literal back out of a cast. One row per key column, in declaration order.
    let mut stmt = db.conn.prepare(
        "SELECT table_name, unnest(constraint_column_names) AS col FROM duckdb_constraints() \
         WHERE constraint_type = 'PRIMARY KEY'",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (table, col) = row?;
        out.entry(table).or_default().push(col);
    }
    Ok(out)
}

/// `all_files` -> `AllFiles`.
fn class_name(table: &str) -> String {
    table
        .split('_')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut c = p.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// The SQLAlchemy type and Python annotation for a DuckDB type.
///
/// Approximate on purpose: these models are compiled to SQL strings and executed by the
/// Rust engine, never used to emit DDL, so the type only has to be close enough to render
/// a literal correctly. `HUGEINT` has no SQLAlchemy spelling and maps to `BigInteger`.
fn sa_type(duckdb_type: &str) -> (&'static str, &'static str) {
    match duckdb_type {
        "BIGINT" | "INTEGER" | "HUGEINT" | "SMALLINT" | "TINYINT" | "UBIGINT" => {
            ("BigInteger", "int")
        }
        "DOUBLE" | "FLOAT" | "REAL" | "DECIMAL" => ("Float", "float"),
        "BOOLEAN" => ("Boolean", "bool"),
        _ => ("String", "str"),
    }
}

/// A declarative mapper needs a primary key. Use the declared one; else `file_id` for the
/// registry surfaces; else every column, which is always valid and only ever used for
/// row identity — nothing here persists.
fn model_pk<'a>(cols: &'a [(String, String)], declared: Option<&'a Vec<String>>) -> Vec<String> {
    if let Some(pk) = declared
        && !pk.is_empty()
    {
        return pk.clone();
    }
    if cols.iter().any(|(c, _)| c == "file_id") {
        return vec!["file_id".to_string()];
    }
    cols.iter().map(|(c, _)| c.clone()).collect()
}

/// The preamble of the generated constants module. A raw string so the Python it emits is
/// the Python you read here, indentation included.
const GENERATED_HEADER: &str = r#"# @generated by `cargo run -p bidslake-py --bin emit-types` — DO NOT EDIT.
# Regenerate after bumping the vendored BIDS schema; the `codegen-drift` CI job
# (.github/workflows/ci.yml) fails if this file is out of sync.
"""Inline-typed constants generated from the BIDS schema.

The vocabularies of the vendored BIDS schema, frozen into static types: one `Literal` per
enumerable value set (`Entity`, `Datatype`, `Suffix`, `Modality`, `Sex`, `Handedness`), the
`GetFilters` keyword shape for `BidsLake.get`, the `COLUMNS` map from each table's columns to
their DuckDB types, and `C`, the typed `pl.col` accessors.

Import them from `bidslake.schema`, which re-exports every public name:

    from bidslake.schema import C, GetFilters, Suffix

These describe the schema *this build ships*. A database an overlay augmented has entities,
suffixes and columns that are not in here, and `bidslake.stubgen` emits a module of the same
shape from that database's own stored schema. Querying an augmented catalog at runtime needs
neither module — `BidsLake.get` and the table accessors validate against the live database.

Note:
    Generated by `cargo run -p bidslake-py --bin emit-types`. The column maps are read back
    out of a throwaway database built through the real DDL path, and the value sets come from
    the same schema accessors that DDL uses, so neither can drift from a real catalog.
    Hand-edits do not survive: the `codegen-drift` CI job regenerates this file and fails on
    any diff.
"""

from __future__ import annotations

from collections.abc import Sequence
from typing import Literal, TypedDict

import polars as pl

"#;

/// `GetFilters` down to and including its first field; the entity fields follow.
const GET_FILTERS_HEADER: &str = r#"class GetFilters(TypedDict, total=False):
    """Typed keyword filters for `BidsLake.get`.

    One key per column `get` can filter on: `dataset_id`, every BIDS entity of this schema
    build, `datatype`, `suffix`, `extension`, `modality`, and `pseudofile` (true for an opaque
    directory that BIDS counts as one file, such as a `.ds` recording). Every key is optional,
    and `get` takes them as `**filters: Unpack[GetFilters]`, so a misspelled entity is a type
    error at the call site rather than a filter that quietly matches nothing.

    A scalar matches by equality, a sequence by `IN (...)`, and `None` by `IS NULL`:

        lake.get(task="rest", suffix="bold", ses=None)

    Entity values are free text and so are typed `str`. `datatype`, `suffix` and `modality`
    are the `Literal` aliases above, so those are checked against the schema's own vocabulary.
    """

    dataset_id: str | Sequence[str] | None
"#;

/// `C`'s class statement and docstring; the per-table nested classes follow.
const C_HEADER: &str = r#"class C:
    """Typed `pl.col` accessors, one nested class per table.

    Every column is a class attribute holding `pl.col("<name>")`, so `C.all_files.task` both
    autocompletes and *is* the expression — a mistyped column is a type error rather than a
    Polars `ColumnNotFound` once the query runs:

        df.filter(C.all_files.task == "rest").select(C.all_files.file_path)

    The nested classes are named for the table, so `C.all_files`, not `C.AllFiles`. A column
    whose name is not a valid Python identifier has no attribute here — reach it with
    `pl.col("...")`.
    """

"#;

/// The preamble of the generated models module. A raw string so the Python it emits is
/// the Python you read here, indentation included.
const MODELS_HEADER: &str = r#"# @generated by `cargo run -p bidslake-py --bin emit-types` — DO NOT EDIT.
# Regenerate after bumping the vendored BIDS schema; the `codegen-drift` CI job
# (.github/workflows/ci.yml) fails if this file is out of sync.
"""SQLAlchemy models for the catalog's tables and views.

For building queries, not for persistence. A statement is compiled to a string and
executed by the Rust engine that already owns the connection, so nothing here opens a
second one and no `Engine` or `Session` is involved:

    from sqlalchemy import select
    from bidslake.schema.models import AllFiles

    lake.sql(select(AllFiles.file_path).where(AllFiles.task == "rest"))

Declarative classes rather than Core `Table`s because only these are statically
checkable: a type checker rejects `AllFiles.tsk`, while `table.c.tsk` on a Core table
is resolved at runtime and passes silently. Column *values* are checked by neither —
SQLAlchemy types its comparison operators `Any` — which is why `BidsLake.get` and its
`GetFilters` remain the typed path for filtering by value.

A column whose name is a Python keyword (fMRIPrep's `from`) takes a trailing underscore
as its attribute, with the real column name passed to `mapped_column`.

Every model needs a primary key because a declarative mapper requires one. Where the
table declares none — every view here — one is synthesized purely to satisfy that, and
carries no meaning.

These are the tables of the schema *this build ships*. For a catalog an overlay
augmented, use the models `bidslake.stubgen` emits from that database instead; they are
the same classes built from its own stored schema.

Note:
    Generated by `cargo run -p bidslake-py --bin emit-types`, which builds a throwaway
    database through the real DDL path and reads the columns back out, so no class here
    can drift from the table it maps. Hand-edits do not survive: the `codegen-drift` CI
    job regenerates this file and fails on any diff.
"""

from __future__ import annotations

from sqlalchemy import BigInteger, Boolean, Float, String
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column


class Base(DeclarativeBase):
    """Declarative base for every generated model."""


"#;

fn render_models(
    columns: &BTreeMap<String, Vec<(String, String)>>,
    pks: &BTreeMap<String, Vec<String>>,
) -> String {
    let mut o = String::new();
    o.push_str(MODELS_HEADER);
    for (table, cols) in columns {
        let pk = model_pk(cols, pks.get(table));
        o.push_str(&format!("class {}(Base):\n", class_name(table)));
        o.push_str(&format!("    __tablename__ = {}\n", pystr(table)));
        if pks.get(table).map(Vec::is_empty).unwrap_or(true) {
            o.push_str(
                "    # A view has no declared key; this one exists only to satisfy the mapper.\n",
            );
        }
        o.push('\n');
        for (col, ty) in cols {
            let (sa, py) = sa_type(ty);
            let attr = if is_py_identifier(col) {
                col.clone()
            } else {
                format!("{col}_")
            };
            let optional = if pk.contains(col) { "" } else { " | None" };
            // Name the column explicitly whenever the attribute had to be renamed.
            let arg = if attr == *col {
                sa.to_string()
            } else {
                format!("{}, {sa}", pystr(col))
            };
            let key = if pk.contains(col) {
                ", primary_key=True"
            } else {
                ""
            };
            o.push_str(&format!(
                "    {attr}: Mapped[{py}{optional}] = mapped_column({arg}{key})\n"
            ));
        }
        o.push_str("\n\n");
    }
    // Exactly one trailing newline: the per-class separator leaves two, which every
    // formatter and most diffs would want to strip back.
    o.truncate(o.trim_end().len());
    o.push('\n');
    o
}

fn main() -> Result<()> {
    let out_path = std::env::args().nth(1).unwrap_or_else(|| {
        format!(
            "{}/python/bidslake/schema/_generated.py",
            env!("CARGO_MANIFEST_DIR")
        )
    });

    let schema = Schema::load(None)?;
    let raw = schema.raw();
    let schema_version = raw["schema_version"].as_str().unwrap_or("");

    let entities: Vec<String> = bids_schema::datatypes::entities(raw)
        .into_iter()
        .map(|e| e.name)
        .collect();
    let datatypes = bids_schema::datatypes::datatypes(raw);
    let suffixes = suffix_values(raw);
    let modalities = bids_schema::datatypes::modalities(raw);
    let sex = column_levels(raw, "sex");
    let handedness = column_levels(raw, "handedness");
    let columns = introspect_columns()?;

    let mut o = String::new();
    o.push_str(GENERATED_HEADER);

    o.push_str(&format!("SCHEMA_VERSION = {}\n\n", pystr(schema_version)));

    // Enumerable value Literals.
    o.push_str(&literal_alias("Entity", &entities));
    o.push('\n');
    o.push_str(&literal_alias("Datatype", &datatypes));
    o.push('\n');
    o.push_str(&literal_alias("Suffix", &suffixes));
    o.push('\n');
    o.push_str(&literal_alias("Modality", &modalities));
    o.push('\n');
    o.push_str(&literal_alias("Sex", &sex));
    o.push('\n');
    o.push_str(&literal_alias("Handedness", &handedness));
    o.push_str("\n\n");

    // GetFilters: typed kwargs for `BidsLake.get`.
    o.push_str(GET_FILTERS_HEADER);
    for e in &entities {
        o.push_str(&format!("    {e}: str | Sequence[str] | None\n"));
    }
    o.push_str("    datatype: Datatype | Sequence[Datatype] | None\n");
    o.push_str("    suffix: Suffix | Sequence[Suffix] | None\n");
    o.push_str("    extension: str | Sequence[str] | None\n");
    o.push_str("    modality: Modality | Sequence[Modality] | None\n");
    o.push_str("    pseudofile: bool | None\n\n\n");

    // COLUMNS: runtime column->type map per table (backs validation/introspection).
    o.push_str("COLUMNS: dict[str, dict[str, str]] = {\n");
    for (table, cols) in &columns {
        o.push_str(&format!("    {}: {{\n", pystr(table)));
        for (col, ty) in cols {
            o.push_str(&format!("        {}: {},\n", pystr(col), pystr(ty)));
        }
        o.push_str("    },\n");
    }
    o.push_str("}\n\n\n");

    // C: per-table typed `pl.col` accessors. Columns whose name is not a valid Python
    // identifier are skipped; the class docstring says what to use instead.
    o.push_str(C_HEADER);
    for (table, cols) in &columns {
        o.push_str(&format!("    class {table}:\n"));
        let mut wrote = false;
        for (col, _ty) in cols {
            if is_py_identifier(col) {
                o.push_str(&format!(
                    "        {col}: pl.Expr = pl.col({})\n",
                    pystr(col)
                ));
                wrote = true;
            }
        }
        if !wrote {
            o.push_str("        pass\n");
        }
        o.push('\n');
    }

    std::fs::write(&out_path, o)?;
    eprintln!("wrote {out_path}");

    // The SQLAlchemy models live in their own module: a different shape from the inline
    // constants above, and a different import cost — `_generated` is free, this one pulls
    // SQLAlchemy in.
    let models_path = std::path::Path::new(&out_path)
        .parent()
        .map(|d| d.join("models.py"))
        .ok_or_else(|| anyhow::anyhow!("output path {out_path} has no parent directory"))?;
    let pks = introspect_primary_keys()?;
    std::fs::write(&models_path, render_models(&columns, &pks))?;
    eprintln!("wrote {}", models_path.display());
    Ok(())
}

/// Whether `name` can be used verbatim as a Python identifier (attribute name).
fn is_py_identifier(name: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class",
        "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global",
        "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return",
        "try", "while", "with", "yield",
    ];
    let mut chars = name.chars();
    let valid = match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        _ => false,
    };
    valid && !KEYWORDS.contains(&name)
}
