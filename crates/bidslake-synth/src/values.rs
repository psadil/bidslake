//! Column values, chosen from what the schema says a column holds.
//!
//! Everything a generated table puts in a cell comes from `objects.columns.<key>`: the declared
//! `type`, or the `definition.Format` a handful of columns use instead, and any `enum` or
//! `definition.Levels` that closes the value set. Nothing here knows what a confound or a
//! handedness *is* — which is the point, since a column the standard adds tomorrow is filled
//! correctly today.
//!
//! Values are a deterministic function of `(column, row)`. A benchmark tree has to be
//! byte-identical across runs or its numbers cannot be compared, so there is no randomness here
//! at all, seeded or otherwise.

use serde_json::Value;

/// What kind of value a column holds, once `type` and the `definition.Format` fallback are
/// reconciled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A closed value set, from `enum` or the keys of `definition.Levels`.
    OneOf(Vec<String>),
    /// `type: integer`.
    Integer,
    /// `type: number`.
    Number,
    /// `type: boolean`.
    Boolean,
    /// A value the generator must not invent, written `n/a` instead.
    ///
    /// Two families land here. A column whose `format` points at *another file* —
    /// `participant_relative`, `stimuli_relative` — is checked by the validator with
    /// `exists(…)`, so a made-up value is a hard error (`STIMULUS_FILE_MISSING`) rather than
    /// filler. And `HED` annotations are checked against a HED schema the dataset would have to
    /// declare, so inventing one is `HED_VERSION_NOT_DEFINED`.
    ///
    /// `n/a` is the BIDS spelling of "no value", legal in any column, so this is a real value
    /// and not a hole. A caller that *does* know the right paths passes them as an override.
    ///
    /// A column of these is dropped from a generated table unless something overrides it (see
    /// [`crate::tabular::tsv`]): a column that can only say "no value" carries nothing, and an
    /// empty `HED` column still asks the dataset for a `HEDVersion` it does not declare.
    NotApplicable,
    /// `format: datetime` — an ISO-8601 timestamp, which `acq_time` is.
    DateTime,
    /// Anything else, including an absent type.
    Text,
}

/// Read the declared kind of one `objects.columns` entry.
///
/// About ten columns — `age`, `handedness`, `sex`, the physio traces — carry no top-level `type`
/// at all and declare their shape under `definition.Format`/`definition.Levels` instead. Reading
/// both is not tidiness: `sex` and `handedness` are the two columns in the standard whose values
/// are *enumerated*, and a generator that filled them with `x0` would emit a `participants.tsv`
/// the validator rejects.
///
/// A `Levels` set comes back **sorted**, not in document order: `serde_json::Map` is a `BTreeMap`
/// in this workspace (nothing enables `preserve_order`), so its keys arrive alphabetically. That
/// is the property worth having anyway — [`cell`] cycles through the set by row index, and a
/// generated tree has to be byte-identical across runs.
pub fn kind_of(column: &Value) -> Kind {
    // Checked before the value sets, because a column that must not be invented must not be
    // invented from an `enum` either.
    let name = column.get("name").and_then(Value::as_str).unwrap_or("");
    let format = column.get("format").and_then(Value::as_str).unwrap_or("");
    if name == "HED" || matches!(format, "participant_relative" | "stimuli_relative") {
        return Kind::NotApplicable;
    }
    if format == "datetime" {
        return Kind::DateTime;
    }

    if let Some(values) = column.get("enum").and_then(Value::as_array) {
        let allowed: Vec<String> = values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if !allowed.is_empty() {
            return Kind::OneOf(allowed);
        }
    }
    if let Some(levels) = column
        .get("definition")
        .and_then(|d| d.get("Levels"))
        .and_then(Value::as_object)
    {
        let allowed: Vec<String> = levels.keys().cloned().collect();
        if !allowed.is_empty() {
            return Kind::OneOf(allowed);
        }
    }

    let declared = column
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| {
            column
                .get("definition")
                .and_then(|d| d.get("Format"))
                .and_then(Value::as_str)
        })
        .unwrap_or("string");

    match declared {
        "integer" => Kind::Integer,
        "number" | "float" => Kind::Number,
        "boolean" => Kind::Boolean,
        _ => Kind::Text,
    }
}

/// A value for `kind` at row `row`, as it would be written into a TSV cell.
///
/// `column` names the header only so text values are distinguishable in a dump; nothing
/// interprets it.
pub fn cell(kind: &Kind, column: &str, row: usize) -> String {
    match kind {
        // Cycling rather than fixing one value, so a column with levels exercises more than the
        // first of them — a `sex` column that only ever said `male` would let a levels bug
        // through.
        Kind::OneOf(allowed) => allowed[row % allowed.len()].clone(),
        Kind::Integer => row.to_string(),
        // Two decimals, and never an integer-looking string: DuckDB's CSV sniffer types a column
        // from what it sees, and `1` in row one would have it guess BIGINT for a DOUBLE column.
        Kind::Number => format!("{:.2}", (row as f64) * 0.25 + 0.5),
        Kind::Boolean => {
            if row.is_multiple_of(2) {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        Kind::NotApplicable => "n/a".to_string(),
        // BIDS permits a date-only or a full timestamp; the full one is what a scanner writes,
        // and the year is fixed so a generated tree is byte-identical whenever it is built.
        Kind::DateTime => format!("2026-01-01T{:02}:{:02}:00", row / 60 % 24, row % 60),
        Kind::Text => format!("{column}{row}"),
    }
}

/// The kind of the column whose *schema key* is `key`, or [`Kind::Text`] when the schema declares
/// no such column.
///
/// Falling back to text rather than refusing is deliberate: a table's `TableSpec` may carry a
/// column an overlay declared and this lookup is against the same effective schema, so a miss
/// means a column resolved by name rather than by key, and a text cell is always writable.
pub fn kind_for_key(schema: &Value, key: &str) -> Kind {
    schema
        .get("objects")
        .and_then(|o| o.get("columns"))
        .and_then(|c| c.get(key))
        .map(kind_of)
        .unwrap_or(Kind::Text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    fn schema() -> Value {
        serde_json::from_str(bids_schema::SCHEMA_JSON).expect("bundled schema parses")
    }

    /// The columns whose kind comes from somewhere other than a top-level `type`. Each is a real
    /// entry in the bundled schema, and each would be filled with nonsense by a generator that
    /// read `type` alone.
    #[rstest]
    // Alphabetical, not document order: `serde_json::Map` is a `BTreeMap` here, and the
    // determinism that buys is what a benchmark tree needs.
    #[case::levels_under_definition("sex", Kind::OneOf(vec![
        "F".into(), "FEMALE".into(), "Female".into(), "M".into(), "MALE".into(), "Male".into(),
        "O".into(), "OTHER".into(), "Other".into(), "f".into(), "female".into(), "m".into(),
        "male".into(), "o".into(), "other".into(),
    ]))]
    #[case::format_under_definition("age", Kind::Number)]
    #[case::plain_type("participant_id", Kind::Text)]
    #[case::plain_integer("index", Kind::Integer)]
    fn a_columns_kind_comes_from_type_or_the_definition_fallback(
        #[case] key: &str,
        #[case] expected: Kind,
    ) {
        let kind = kind_for_key(&schema(), key);

        assert_eq!(kind, expected, "column {key}");
    }

    /// A `number` cell never looks like an integer, because DuckDB types a CSV column from the
    /// values it sees and a leading `1` would have it guess BIGINT for a DOUBLE column.
    #[test]
    fn a_number_cell_always_carries_a_decimal_point() {
        let cells: Vec<String> = (0..8).map(|r| cell(&Kind::Number, "x", r)).collect();

        assert!(
            cells.iter().all(|c| c.contains('.')),
            "integer-looking cells: {cells:?}"
        );
    }

    /// Levels cycle rather than repeating the first value, so a table wide enough to have rows
    /// exercises more than one of them.
    #[test]
    fn a_closed_value_set_cycles_across_rows() {
        let kind = kind_of(&json!({ "enum": ["L", "R"] }));

        let cells: Vec<String> = (0..4).map(|r| cell(&kind, "hemi", r)).collect();

        assert_eq!(cells, ["L", "R", "L", "R"]);
    }
}
