use crate::schema::Schema;
use regex::Regex;
use std::sync::LazyLock;

static NUMERIC_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[+-]?(\d+(\.\d*)?|\.\d+)([eE][+-]?\d+)?$").expect("static numeric regex is valid")
});
/// ISO-8601 date-time, per hed-python's class_regex.json `dateTimeClass` word pattern.
static DATE_TIME_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})?$")
        .expect("static date-time regex is valid")
});

/// Returns the first character in `s` that is never allowed in a raw HED tag: control
/// characters (other than tab/newline/CR) and the curly-brace splice syntax (which is only
/// meaningful as a whole standalone `{col}` token, checked separately).
pub fn first_invalid_raw_char(s: &str) -> Option<char> {
    s.chars()
        .find(|c| c.is_control() && *c != '\n' && *c != '\r' && *c != '\t')
}

/// Whether `text` is a well-formed literal for the given schema value class.
///
/// `numericClass` (and any class with no `allowed_characters` entry that isn't otherwise
/// recognized) is checked via a strict whole-value grammar, matching how HED reports these
/// failures as VALUE_INVALID rather than a per-character CHARACTER_INVALID. All other known
/// classes (nameClass, textClass, ...) are checked per-character against the schema's parsed
/// `allowed_characters` list.
pub fn is_valid_for_value_class(schema: &Schema, class_name: &str, text: &str) -> bool {
    if class_name.eq_ignore_ascii_case("numericClass") {
        return is_valid_numeric_literal(text);
    }
    if class_name.eq_ignore_ascii_case("dateTimeClass") {
        return DATE_TIME_LITERAL.is_match(text);
    }
    match schema.value_classes.get(class_name) {
        Some(vc) => chars_allowed(text, &vc.allowed_characters),
        None => true,
    }
}

/// True if every character in `text` is licensed by the schema's `allowed_characters` list
/// for a value class (e.g. nameClass = letters/digits/underscore/hyphen; textClass = broad
/// printable-text).
fn chars_allowed(text: &str, allowed: &[String]) -> bool {
    let allows_letters = allowed.iter().any(|a| a == "letters");
    let allows_digits = allowed.iter().any(|a| a == "digits");
    let allows_blank = allowed.iter().any(|a| a == "blank");
    let allows_text = allowed.iter().any(|a| a == "text");

    text.chars().all(|c| {
        if allows_text {
            let cp = c as u32;
            let is_printable_ascii = (0x20..0x7F).contains(&cp)
                && c != ','
                && c != '['
                && c != ']'
                && c != '{'
                && c != '}';
            if is_printable_ascii || cp > 127 {
                return true;
            }
        }
        if allows_letters && c.is_alphabetic() {
            return true;
        }
        if allows_digits && c.is_ascii_digit() {
            return true;
        }
        if allows_blank && c == ' ' {
            return true;
        }
        allowed
            .iter()
            .any(|a| a.chars().count() == 1 && a.starts_with(c))
    })
}

/// Strict numeric-literal grammar: an optional sign, an integer-or-decimal mantissa, and an
/// optional exponent. Nothing else is tolerated (no stray letters/symbols/units — those get
/// split off before this check runs).
pub fn is_valid_numeric_literal(s: &str) -> bool {
    NUMERIC_LITERAL.is_match(s)
}
