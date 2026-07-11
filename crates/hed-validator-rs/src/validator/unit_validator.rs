use crate::schema::{Schema, UnitClass};

/// Splits a tag's value/extension text on the first space into `(value, unit_text)`. If
/// there's no space, there's no unit portion.
pub fn split_value_and_unit(text: &str) -> (&str, Option<&str>) {
    match text.find(' ') {
        Some(idx) => (&text[..idx], Some(&text[idx + 1..])),
        None => (text, None),
    }
}

/// Whether `unit_text` is a valid (optionally SI-prefixed/pluralized) spelling of one of the
/// units in `unit_class`.
pub fn validate_unit(schema: &Schema, unit_class: &UnitClass, unit_text: &str) -> bool {
    unit_class
        .units
        .iter()
        .filter_map(|name| schema.units.get(name))
        .any(|unit| unit_matches(schema, unit, unit_text))
}

fn unit_matches(schema: &Schema, unit: &crate::schema::UnitEntry, unit_text: &str) -> bool {
    if matches_spelling(&unit.name, unit_text, unit.unit_symbol) {
        return true;
    }

    // Mechanical "+s" pluralization — word units only, symbols never pluralize.
    if !unit.unit_symbol {
        let plural = format!("{}s", unit.name);
        if matches_spelling(&plural, unit_text, false) {
            return true;
        }
    }

    // SI-prefix combination: only for SI-eligible units, and only pairing symbol-modifiers
    // with symbol-units / word-modifiers with word-units.
    if unit.si_unit {
        for modifier in schema.unit_modifiers.values() {
            if modifier.is_symbol_modifier != unit.unit_symbol {
                continue;
            }
            let prefix_matches = if modifier.is_symbol_modifier {
                unit_text.starts_with(modifier.name.as_str())
            } else {
                unit_text
                    .to_lowercase()
                    .starts_with(&modifier.name.to_lowercase())
            };
            if !prefix_matches {
                continue;
            }
            let rest = &unit_text[modifier.name.len()..];
            if matches_spelling(&unit.name, rest, unit.unit_symbol) {
                return true;
            }
            if !unit.unit_symbol {
                let plural = format!("{}s", unit.name);
                if matches_spelling(&plural, rest, false) {
                    return true;
                }
            }
        }
    }

    false
}

fn matches_spelling(candidate: &str, text: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        candidate == text
    } else {
        candidate.eq_ignore_ascii_case(text)
    }
}
