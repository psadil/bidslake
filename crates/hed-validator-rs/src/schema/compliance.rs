//! Post-load compliance checking for a parsed HED schema, ported from hed-python's
//! `schema_validation/compliance.py` + `attribute_validators.py`. These are semantic checks
//! on an already-successfully-parsed schema (character sets, attribute domains/ranges,
//! deprecation consistency, duplicates, extras completeness) — parse-time structural errors
//! live in the wiki parser instead.

use super::model::*;
use crate::errors::{HedError, codes};
use crate::validator::unit_validator;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Released versions per library line, used by the deprecatedFrom check ("must name a real
/// released version, or the schema's own current version") and the hedId previous-version
/// comparison. hed-python derives this from its schema cache; a static copy of the
/// hed-standard/hed-schemas release history is deterministic and offline-friendly.
static RELEASED_VERSIONS: LazyLock<HashMap<&'static str, Vec<&'static str>>> =
    LazyLock::new(|| {
        HashMap::from([
            ("", vec!["8.0.0", "8.1.0", "8.2.0", "8.3.0", "8.4.0"]),
            ("score", vec!["1.0.0", "1.1.0", "1.2.0", "2.0.0", "2.1.0"]),
            ("lang", vec!["1.0.0", "1.1.0"]),
            ("testlib", vec!["1.0.2", "2.0.0", "2.1.0", "2.2.0", "3.0.0"]),
        ])
    });

/// hedId numeric ranges per library, from hed-schemas' library_data.json.
static LIBRARY_ID_RANGES: LazyLock<HashMap<String, (i64, i64)>> = LazyLock::new(|| {
    let raw: serde_json::Value = serde_json::from_str(include_str!("../data/library_data.json"))
        .expect("embedded library_data.json must parse");
    raw.as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| {
                    let range = v.get("id_range")?.as_array()?;
                    Some((
                        k.clone(),
                        (range.first()?.as_i64()?, range.get(1)?.as_i64()?),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
});

/// The released 8.4.0 schema, used as the reference for the "hedId changed" comparison.
static RELEASED_STANDARD: LazyLock<Option<Schema>> =
    LazyLock::new(|| Schema::load_standard("8.4.0").ok());

fn is_text_char(c: char) -> bool {
    let cp = c as u32;
    ((0x20..0x7F).contains(&cp) && !",[]{}".contains(c)) || cp > 127
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '.' || c == '_' || (c as u32) > 127
}

/// Whether `c` belongs to a named character class (or equals a literal single-character
/// class token).
fn char_in_class(c: char, class: &str) -> bool {
    match class {
        "letters" => c.is_ascii_alphabetic(),
        "lowercase" => c.is_ascii_lowercase(),
        "uppercase" => c.is_ascii_uppercase(),
        "digits" => c.is_ascii_digit(),
        "alphanumeric" => c.is_ascii_alphanumeric(),
        "blank" => c == ' ',
        "tab" => c == '\t',
        "newline" => c == '\n',
        "text" => is_text_char(c),
        "name" => is_name_char(c),
        "nonascii" => (c as u32) > 127,
        "printable" => (0x20..0x7F).contains(&(c as u32)),
        "ascii" => (c as u32) < 127,
        single => {
            let mut chars = single.chars();
            chars.next() == Some(c) && chars.next().is_none()
        }
    }
}

/// Named classes recognized by the `allowedCharacter` attribute (hed_schema_constants'
/// `character_types` keys).
const CHARACTER_TYPE_NAMES: [&str; 38] = [
    "ascii",
    "nonascii",
    "printable",
    "lowercase",
    "uppercase",
    "digits",
    "tab",
    "newline",
    "blank",
    "exclamation",
    "double-quote",
    "number-sign",
    "dollar",
    "percent-sign",
    "ampersand",
    "single-quote",
    "left-paren",
    "right-paren",
    "asterisk",
    "plus",
    "comma",
    "hyphen",
    "period",
    "slash",
    "colon",
    "semicolon",
    "less-than",
    "equals",
    "greater-than",
    "question-mark",
    "at-sign",
    "backslash",
    "caret",
    "underscore",
    "vertical-bar",
    "tilde",
    "letters",
    "alphanumeric",
];

/// One entry (any section) presented uniformly to the attribute checks.
struct EntryRef<'a> {
    section: &'static str,
    /// Short name for tags; plain name for everything else.
    name: &'a str,
    description: &'a str,
    attributes: &'a HashMap<String, Vec<String>>,
    /// For tag entries only: the node itself (placeholder/sibling/children checks).
    node: Option<&'a SchemaNode>,
    /// For tag entries only: names of this node's siblings (excluding itself).
    sibling_count: usize,
    /// For unit entries: the owning unit class.
    unit_class: Option<&'a str>,
}

fn collect_entries<'a>(schema: &'a Schema) -> Vec<EntryRef<'a>> {
    let mut out = Vec::new();

    fn walk<'a>(node: &'a SchemaNode, siblings: usize, out: &mut Vec<EntryRef<'a>>) {
        out.push(EntryRef {
            section: sections::TAGS,
            name: &node.name,
            description: &node.description,
            attributes: &node.attributes,
            node: Some(node),
            sibling_count: siblings,
            unit_class: None,
        });
        let child_count = node.children.len();
        for child in node.children.values() {
            walk(child, child_count - 1, out);
        }
    }
    let root_count = schema.root_nodes.len();
    for node in schema.root_nodes.values() {
        walk(node, root_count - 1, &mut out);
    }

    for uc in schema.unit_classes.values() {
        out.push(EntryRef {
            section: sections::UNIT_CLASSES,
            name: &uc.name,
            description: &uc.entry.description,
            attributes: &uc.entry.attributes,
            node: None,
            sibling_count: 0,
            unit_class: None,
        });
    }
    for u in schema.units.values() {
        out.push(EntryRef {
            section: sections::UNITS,
            name: &u.name,
            description: &u.entry.description,
            attributes: &u.entry.attributes,
            node: None,
            sibling_count: 0,
            unit_class: Some(&u.unit_class),
        });
    }
    for m in schema.unit_modifiers.values() {
        out.push(EntryRef {
            section: sections::UNIT_MODIFIERS,
            name: &m.name,
            description: &m.entry.description,
            attributes: &m.entry.attributes,
            node: None,
            sibling_count: 0,
            unit_class: None,
        });
    }
    for vc in schema.value_classes.values() {
        out.push(EntryRef {
            section: sections::VALUE_CLASSES,
            name: &vc.name,
            description: &vc.entry.description,
            attributes: &vc.entry.attributes,
            node: None,
            sibling_count: 0,
            unit_class: None,
        });
    }
    for a in schema.schema_attributes.values() {
        out.push(EntryRef {
            section: sections::ATTRIBUTES,
            name: &a.name,
            description: &a.description,
            attributes: &a.attributes,
            node: None,
            sibling_count: 0,
            unit_class: None,
        });
    }
    for p in schema.properties.values() {
        out.push(EntryRef {
            section: sections::PROPERTIES,
            name: &p.name,
            description: &p.description,
            attributes: &p.attributes,
            node: None,
            sibling_count: 0,
            unit_class: None,
        });
    }

    out
}

/// The set of attribute names (lowercase) valid for entries of `section`, derived from the
/// schema's own Schema attributes + Properties sections (`_get_attributes_for_section`).
fn valid_attributes_for(schema: &Schema, section: &str) -> HashSet<String> {
    let uses_83 = schema.uses_83_props();
    let element_key = if uses_83 {
        "elementDomain"
    } else {
        "elementProperty"
    };

    let has_prop = |entry: &SchemaEntry, key: &str| entry.has_attribute(key);

    if section == sections::ATTRIBUTES || section == sections::PROPERTIES {
        let mut out: HashSet<String> = HashSet::new();
        if section == sections::ATTRIBUTES {
            out.extend(schema.properties.keys().cloned());
        }
        out.extend(
            schema
                .schema_attributes
                .iter()
                .filter(|(_, e)| has_prop(e, element_key))
                .map(|(k, _)| k.clone()),
        );
        return out;
    }

    if !uses_83 && section == sections::TAGS {
        // Old-style schemas: tags accept every attribute not marked for another section.
        return schema
            .schema_attributes
            .iter()
            .filter(|(_, e)| {
                !has_prop(e, "unitClassProperty")
                    && !has_prop(e, "unitProperty")
                    && !has_prop(e, "unitModifierProperty")
                    && !has_prop(e, "valueClassProperty")
            })
            .map(|(k, _)| k.clone())
            .collect();
    }

    let domain_key = if uses_83 {
        match section {
            sections::TAGS => "tagDomain",
            sections::UNIT_CLASSES => "unitClassDomain",
            sections::UNITS => "unitDomain",
            sections::UNIT_MODIFIERS => "unitModifierDomain",
            sections::VALUE_CLASSES => "valueClassDomain",
            _ => return HashSet::new(),
        }
    } else {
        match section {
            sections::UNIT_CLASSES => "unitClassProperty",
            sections::UNITS => "unitProperty",
            sections::UNIT_MODIFIERS => "unitModifierProperty",
            sections::VALUE_CLASSES => "valueClassProperty",
            _ => return HashSet::new(),
        }
    };

    schema
        .schema_attributes
        .iter()
        .filter(|(_, e)| has_prop(e, domain_key) || has_prop(e, element_key))
        .map(|(k, _)| k.clone())
        .collect()
}

fn attribute_definition<'a>(
    schema: &'a Schema,
    section: &str,
    attr: &str,
) -> Option<&'a SchemaEntry> {
    let key = attr.to_lowercase();
    // Attributes used on Attributes-section entries are defined in Properties (or as
    // elementDomain attributes); everything else looks in Schema attributes.
    if section == sections::ATTRIBUTES
        && let Some(p) = schema.properties.get(&key)
    {
        return Some(p);
    }
    schema.schema_attributes.get(&key)
}

fn find_value_class<'a>(schema: &'a Schema, name: &str) -> Option<&'a ValueClass> {
    schema.value_classes.get(name).or_else(|| {
        schema
            .value_classes
            .values()
            .find(|vc| vc.name.eq_ignore_ascii_case(name))
    })
}

fn find_unit_class<'a>(schema: &'a Schema, name: &str) -> Option<&'a UnitClass> {
    schema.unit_classes.get(name).or_else(|| {
        schema
            .unit_classes
            .values()
            .find(|uc| uc.name.eq_ignore_ascii_case(name))
    })
}

fn tag_deprecated(schema: &Schema, name: &str) -> Option<bool> {
    let node = if name.contains('/') {
        schema.find_by_full_path(name)
    } else {
        schema.tag_entry_by_short_name(name)
    }?;
    Some(node.has_attribute("deprecatedFrom"))
}

fn parse_float(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok()
}

fn library_version_for(schema: &Schema, library: &str) -> Option<String> {
    let names: Vec<&str> = schema.library.split(',').collect();
    let versions: Vec<&str> = schema.version.split(',').collect();
    for (name, version) in names.iter().zip(versions.iter()) {
        if *name == library {
            return Some(version.to_string());
        }
    }
    if library.is_empty() && !schema.with_standard.is_empty() {
        return Some(schema.with_standard.clone());
    }
    None
}

fn version_lt(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> (u64, u64, u64) {
        let mut it = v.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
        (
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
        )
    };
    parse(a) < parse(b)
}

/// Runs every compliance check and returns the accumulated issues (warnings + errors; the
/// schema-test harness treats any issue as reportable, matching hed-python).
pub fn check_compliance(schema: &Schema) -> Vec<HedError> {
    let mut issues = Vec::new();

    check_prologue_epilogue(schema, &mut issues);
    check_invalid_characters(schema, &mut issues);
    check_attributes(schema, &mut issues);
    check_duplicate_names(schema, &mut issues);
    check_extras_columns(schema, &mut issues);

    issues
}

fn check_prologue_epilogue(schema: &Schema, issues: &mut Vec<HedError>) {
    for (label, text) in [
        ("prologue", &schema.prologue),
        ("epilogue", &schema.epilogue),
    ] {
        for c in text.chars() {
            if !is_text_char(c) && c != '\n' && c != ',' {
                issues.push(HedError::warning(
                    codes::SCHEMA_CHARACTER_INVALID,
                    &format!("invalid character {:?} in {}", c, label),
                    None,
                ));
            }
        }
    }
}

fn check_invalid_characters(schema: &Schema, issues: &mut Vec<HedError>) {
    for entry in collect_entries(schema) {
        if entry.attributes.contains_key("deprecatedFrom") {
            continue;
        }

        // Capitalization (tags only): first character must be a digit or uppercase.
        if entry.section == sections::TAGS
            && entry.name != "#"
            && let Some(first) = entry.name.chars().next()
            && !(first.is_ascii_digit() || first.is_uppercase())
        {
            issues.push(HedError::warning(
                "SCHEMA_INVALID_CAPITALIZATION",
                &format!(
                    "tag '{}' should start with an uppercase letter or digit",
                    entry.name
                ),
                Some(entry.name.to_string()),
            ));
        }

        // Term characters: name class plus the entry's own allowedCharacter grants.
        let extra: Vec<String> = entry
            .attributes
            .get("allowedCharacter")
            .map(|vals| {
                vals.iter()
                    .flat_map(|v| v.split(','))
                    .map(super::json_parser::translate_allowed_character)
                    .collect()
            })
            .unwrap_or_default();
        if entry.name != "#" {
            for c in entry.name.chars() {
                let ok = is_name_char(c) || extra.iter().any(|class| char_in_class(c, class));
                if !ok {
                    issues.push(HedError::warning(
                        codes::SCHEMA_CHARACTER_INVALID,
                        &format!("invalid character {:?} in name '{}'", c, entry.name),
                        Some(entry.name.to_string()),
                    ));
                }
            }
        }

        // Description characters: text + comma.
        for c in entry.description.chars() {
            if !is_text_char(c) && c != ',' {
                issues.push(HedError::warning(
                    codes::SCHEMA_CHARACTER_INVALID,
                    &format!(
                        "invalid character {:?} in description of '{}'",
                        c, entry.name
                    ),
                    Some(entry.name.to_string()),
                ));
            }
        }
    }
}

fn check_attributes(schema: &Schema, issues: &mut Vec<HedError>) {
    let mut valid_by_section: HashMap<&'static str, HashSet<String>> = HashMap::new();
    for section in [
        sections::TAGS,
        sections::UNIT_CLASSES,
        sections::UNITS,
        sections::UNIT_MODIFIERS,
        sections::VALUE_CLASSES,
        sections::ATTRIBUTES,
        sections::PROPERTIES,
    ] {
        valid_by_section.insert(section, valid_attributes_for(schema, section));
    }

    let mut hed_ids_seen: HashMap<String, String> = HashMap::new();

    for entry in collect_entries(schema) {
        let valid = &valid_by_section[entry.section];
        for (attr_name, attr_values) in entry.attributes {
            let attr_key = attr_name.to_lowercase();

            // Unknown attribute for this section's domain.
            if !valid.contains(&attr_key) {
                issues.push(HedError::error(
                    codes::SCHEMA_ATTRIBUTE_INVALID,
                    &format!(
                        "attribute '{}' is not valid for '{}' ({})",
                        attr_name, entry.name, entry.section
                    ),
                    Some(entry.name.to_string()),
                ));
            }

            let definition = attribute_definition(schema, entry.section, attr_name);

            // Using a deprecated attribute on a non-deprecated entry.
            if let Some(def) = definition
                && def.has_attribute("deprecatedFrom")
                && !entry.attributes.contains_key("deprecatedFrom")
            {
                issues.push(HedError::error(
                    codes::SCHEMA_DEPRECATION_ERROR,
                    &format!("'{}' uses deprecated attribute '{}'", entry.name, attr_name),
                    Some(entry.name.to_string()),
                ));
            }

            // Range checks, driven by the attribute definition's *Range properties.
            if let Some(def) = definition {
                check_range(schema, &entry, attr_name, attr_values, def, issues);
            }

            // Semantic per-attribute checks.
            match attr_key.as_str() {
                "takesvalue" | "unitclass" | "valueclass" => {
                    check_placeholder_placement(&entry, attr_name, issues);
                }
                "deprecatedfrom" => check_deprecated_from(schema, &entry, attr_values, issues),
                "conversionfactor" => {
                    let cf = attr_values.first().map(|s| s.replace('^', "e"));
                    let value = cf.as_deref().and_then(parse_float);
                    if value.is_none_or(|v| v <= 0.0) {
                        issues.push(HedError::error(
                            codes::SCHEMA_ATTRIBUTE_VALUE_INVALID,
                            &format!(
                                "conversionFactor on '{}' is not a positive number",
                                entry.name
                            ),
                            Some(entry.name.to_string()),
                        ));
                    }
                }
                "allowedcharacter" => {
                    for token in attr_values.iter().flat_map(|v| v.split(',')) {
                        if !CHARACTER_TYPE_NAMES.contains(&token) && token.chars().count() != 1 {
                            issues.push(HedError::error(
                                codes::SCHEMA_ATTRIBUTE_VALUE_INVALID,
                                &format!(
                                    "allowedCharacter token '{}' on '{}' is invalid",
                                    token, entry.name
                                ),
                                Some(entry.name.to_string()),
                            ));
                        }
                    }
                }
                "inlibrary" => {
                    let value = attr_values.first().map(|s| s.as_str()).unwrap_or("");
                    if !schema.library.split(',').any(|l| l == value) {
                        issues.push(HedError::error(
                            codes::SCHEMA_ATTRIBUTE_VALUE_INVALID,
                            &format!(
                                "inLibrary value '{}' on '{}' is not a library of this schema",
                                value, entry.name
                            ),
                            Some(entry.name.to_string()),
                        ));
                    }
                }
                "hedid" => check_hed_id(schema, &entry, attr_values, &mut hed_ids_seen, issues),
                _ => {}
            }
        }
    }
}

fn check_range(
    schema: &Schema,
    entry: &EntryRef,
    attr_name: &str,
    attr_values: &[String],
    definition: &SchemaEntry,
    issues: &mut Vec<HedError>,
) {
    let entry_deprecated = entry.attributes.contains_key("deprecatedFrom");
    let values = || {
        attr_values
            .iter()
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };

    fn report_missing(
        issues: &mut Vec<HedError>,
        entry_name: &str,
        attr_name: &str,
        value: &str,
        kind: &str,
    ) {
        issues.push(HedError::error(
            codes::SCHEMA_ATTRIBUTE_VALUE_INVALID,
            &format!(
                "{}='{}' on '{}' does not name an existing {}",
                attr_name, value, entry_name, kind
            ),
            Some(entry_name.to_string()),
        ));
    }

    if definition.has_attribute("tagRange") {
        for value in values() {
            match tag_deprecated(schema, value) {
                None => report_missing(issues, entry.name, attr_name, value, "tag"),
                Some(true) if !entry_deprecated => {
                    issues.push(HedError::error(
                        codes::SCHEMA_DEPRECATION_ERROR,
                        &format!(
                            "{}='{}' on '{}' references a deprecated tag",
                            attr_name, value, entry.name
                        ),
                        Some(entry.name.to_string()),
                    ));
                }
                _ => {}
            }
        }
    }
    if definition.has_attribute("unitClassRange") {
        for value in values() {
            match find_unit_class(schema, value) {
                None => report_missing(issues, entry.name, attr_name, value, "unit class"),
                Some(uc) if uc.entry.has_attribute("deprecatedFrom") && !entry_deprecated => {
                    issues.push(HedError::error(
                        codes::SCHEMA_DEPRECATION_ERROR,
                        &format!(
                            "{}='{}' on '{}' references a deprecated unit class",
                            attr_name, value, entry.name
                        ),
                        Some(entry.name.to_string()),
                    ));
                }
                _ => {}
            }
        }
    }
    if definition.has_attribute("valueClassRange") {
        for value in values() {
            match find_value_class(schema, value) {
                None => report_missing(issues, entry.name, attr_name, value, "value class"),
                Some(vc) if vc.entry.has_attribute("deprecatedFrom") && !entry_deprecated => {
                    issues.push(HedError::error(
                        codes::SCHEMA_DEPRECATION_ERROR,
                        &format!(
                            "{}='{}' on '{}' references a deprecated value class",
                            attr_name, value, entry.name
                        ),
                        Some(entry.name.to_string()),
                    ));
                }
                _ => {}
            }
        }
    }
    if definition.has_attribute("unitRange") {
        // The value (e.g. defaultUnits) must resolve to a unit of THIS entry's unit class.
        let class = match entry.section {
            sections::UNIT_CLASSES => find_unit_class(schema, entry.name),
            _ => entry.unit_class.and_then(|c| find_unit_class(schema, c)),
        };
        for value in values() {
            let Some(class) = class else { continue };
            let direct = class
                .units
                .iter()
                .find(|u| u.eq_ignore_ascii_case(value) || u.as_str() == value)
                .and_then(|u| schema.units.get(u));
            match direct {
                Some(unit) => {
                    if unit.entry.has_attribute("deprecatedFrom") && !entry_deprecated {
                        issues.push(HedError::error(
                            codes::SCHEMA_DEPRECATION_ERROR,
                            &format!(
                                "{}='{}' on '{}' references a deprecated unit",
                                attr_name, value, entry.name
                            ),
                            Some(entry.name.to_string()),
                        ));
                    }
                }
                None => {
                    // Derivative spellings (plurals/SI prefixes) also count as resolving.
                    if !unit_validator::validate_unit(schema, class, value) {
                        report_missing(issues, entry.name, attr_name, value, "unit");
                    }
                }
            }
        }
    }
    if definition.has_attribute("numericRange") {
        for value in values() {
            if parse_float(value).is_none() {
                issues.push(HedError::error(
                    codes::SCHEMA_ATTRIBUTE_VALUE_INVALID,
                    &format!(
                        "{}='{}' on '{}' is not numeric",
                        attr_name, value, entry.name
                    ),
                    Some(entry.name.to_string()),
                ));
            }
        }
    }
}

/// takesValue/unitClass/valueClass may only appear on a `#` placeholder node, which in turn
/// must have no siblings and no children.
fn check_placeholder_placement(entry: &EntryRef, attr_name: &str, issues: &mut Vec<HedError>) {
    if entry.section != sections::TAGS {
        return;
    }
    if entry.name != "#" {
        issues.push(HedError::error(
            codes::SCHEMA_ATTRIBUTE_VALUE_INVALID,
            &format!(
                "'{}' has attribute '{}' but is not a '#' placeholder",
                entry.name, attr_name
            ),
            Some(entry.name.to_string()),
        ));
        return;
    }
    if entry.sibling_count > 0 {
        issues.push(HedError::error(
            codes::SCHEMA_ATTRIBUTE_INVALID,
            "a '#' placeholder node cannot have sibling tags",
            Some(entry.name.to_string()),
        ));
    }
    if entry.node.is_some_and(|n| !n.children.is_empty()) {
        issues.push(HedError::error(
            codes::SCHEMA_ATTRIBUTE_INVALID,
            "a '#' placeholder node cannot have child tags",
            Some(entry.name.to_string()),
        ));
    }
}

fn check_deprecated_from(
    schema: &Schema,
    entry: &EntryRef,
    attr_values: &[String],
    issues: &mut Vec<HedError>,
) {
    let deprecated_version = attr_values.first().map(|s| s.as_str()).unwrap_or("");
    let library = entry
        .attributes
        .get("inLibrary")
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or_else(|| {
            if schema.with_standard.is_empty() {
                schema.library.split(',').next().unwrap_or("").to_string()
            } else {
                String::new()
            }
        });

    if !deprecated_version.is_empty() {
        let released = RELEASED_VERSIONS
            .get(library.as_str())
            .cloned()
            .unwrap_or_default();
        let current = library_version_for(schema, &library);
        let mut allowed: HashSet<&str> = released.into_iter().collect();
        if let Some(current) = current.as_deref() {
            allowed.insert(current);
        }
        let future = current
            .as_deref()
            .is_some_and(|current| version_lt(current, deprecated_version));
        if !allowed.contains(deprecated_version) || future {
            issues.push(HedError::error(
                codes::SCHEMA_DEPRECATION_ERROR,
                &format!(
                    "deprecatedFrom='{}' on '{}' is not a known version",
                    deprecated_version, entry.name
                ),
                Some(entry.name.to_string()),
            ));
        }
    }

    // Every child of a deprecated tag must carry deprecatedFrom directly.
    if let Some(node) = entry.node {
        for child in node.children.values() {
            if !child.attributes.contains_key("deprecatedFrom") {
                issues.push(HedError::error(
                    codes::SCHEMA_DEPRECATION_ERROR,
                    &format!(
                        "'{}' is deprecated but child '{}' is not",
                        entry.name, child.name
                    ),
                    Some(child.name.to_string()),
                ));
            }
        }
    }

    // Same rule for a deprecated unit class: its units must be deprecated too.
    if entry.section == sections::UNIT_CLASSES
        && let Some(class) = find_unit_class(schema, entry.name)
    {
        for unit_name in &class.units {
            if schema
                .units
                .get(unit_name)
                .is_some_and(|u| !u.entry.attributes.contains_key("deprecatedFrom"))
            {
                issues.push(HedError::error(
                    codes::SCHEMA_DEPRECATION_ERROR,
                    &format!(
                        "unit class '{}' is deprecated but unit '{}' is not",
                        entry.name, unit_name
                    ),
                    Some(unit_name.to_string()),
                ));
            }
        }
    }
}

fn check_hed_id(
    _schema: &Schema,
    entry: &EntryRef,
    attr_values: &[String],
    seen: &mut HashMap<String, String>,
    issues: &mut Vec<HedError>,
) {
    let id_str = attr_values.first().map(|s| s.as_str()).unwrap_or("");

    // Duplicate hedId across all sections.
    if !id_str.is_empty() {
        if let Some(previous) = seen.get(id_str) {
            issues.push(HedError::error(
                codes::SCHEMA_ATTRIBUTE_VALUE_INVALID,
                &format!(
                    "hedId '{}' on '{}' already used by '{}'",
                    id_str, entry.name, previous
                ),
                Some(entry.name.to_string()),
            ));
        } else {
            seen.insert(id_str.to_string(), entry.name.to_string());
        }
    }

    let new_id = match id_str.strip_prefix("HED_").unwrap_or(id_str).parse::<i64>() {
        Ok(v) => v,
        Err(_) => {
            issues.push(HedError::error(
                codes::SCHEMA_ATTRIBUTE_VALUE_INVALID,
                &format!("hedId '{}' on '{}' is malformed", id_str, entry.name),
                Some(entry.name.to_string()),
            ));
            return;
        }
    };

    let library = entry
        .attributes
        .get("inLibrary")
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or_default();

    // "Changed" comparison against the released standard schema (by short name, tags only;
    // hed-python compares against the previous released version of the same line — the
    // embedded 8.4.0 serves as that reference for standard-schema entries).
    if library.is_empty()
        && entry.section == sections::TAGS
        && let Some(released) = RELEASED_STANDARD.as_ref()
        && let Some(released_node) = released.tag_entry_by_short_name(entry.name)
        && let Some(old_id_str) = released_node.attribute_value("hedId")
        && let Ok(old_id) = old_id_str
            .strip_prefix("HED_")
            .unwrap_or(old_id_str)
            .parse::<i64>()
        && old_id != new_id
    {
        issues.push(HedError::error(
            codes::SCHEMA_ATTRIBUTE_VALUE_INVALID,
            &format!(
                "hedId on '{}' changed from HED_{:07} in the released schema",
                entry.name, old_id
            ),
            Some(entry.name.to_string()),
        ));
    }

    if let Some((min, max)) = LIBRARY_ID_RANGES.get(library.as_str())
        && (new_id < *min || new_id > *max)
    {
        issues.push(HedError::error(
            codes::SCHEMA_ATTRIBUTE_VALUE_INVALID,
            &format!(
                "hedId {} on '{}' is outside the allowed range {}..{}",
                new_id, entry.name, min, max
            ),
            Some(entry.name.to_string()),
        ));
    }
}

fn check_duplicate_names(schema: &Schema, issues: &mut Vec<HedError>) {
    for (section, names) in &schema.duplicate_names {
        for dup in names {
            // A duplicate whose copies span the standard schema and a merged-in library is
            // a library-integration problem rather than a plain duplicate node.
            let code = if dup.spans_library {
                codes::SCHEMA_LIBRARY_INVALID
            } else {
                codes::SCHEMA_DUPLICATE_NODE
            };
            issues.push(HedError::error(
                code,
                &format!("duplicate name '{}' in section {}", dup.name, section),
                Some(dup.name.to_string()),
            ));
        }
    }
}

fn check_extras_columns(schema: &Schema, issues: &mut Vec<HedError>) {
    let required: [(&str, &[&str]); 3] = [
        ("sources", &["source", "link", "description"]),
        ("prefixes", &["prefix", "namespace", "description"]),
        (
            "external_annotations",
            &["prefix", "id", "iri", "description"],
        ),
    ];
    for (key, columns) in required {
        for row in schema.extras.get(key).into_iter().flatten() {
            for column in columns {
                if row
                    .get(*column)
                    .map(|v| v.trim().is_empty())
                    .unwrap_or(true)
                {
                    issues.push(HedError::warning(
                        codes::SCHEMA_MISSING_EXTRA_VALUE,
                        &format!(
                            "extras section '{}' row is missing required column '{}'",
                            key, column
                        ),
                        None,
                    ));
                }
            }
        }
    }
}
