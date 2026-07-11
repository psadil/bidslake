use crate::errors::{HedError, codes};
use crate::models::HedTag;
use crate::schema::{SchemaCollection, TagResolution};
use crate::validator::char_validator;
use crate::validator::unit_validator;
use crate::validator::{PlaceholderMode, ValidationContext};

fn is_whole_brace_token(s: &str) -> bool {
    s.len() >= 2
        && s.starts_with('{')
        && s.ends_with('}')
        && !s[1..s.len() - 1].contains(['{', '}'])
}

fn placeholder_licensed(ctx: &ValidationContext) -> bool {
    matches!(ctx.placeholder_mode, PlaceholderMode::ValueColumn)
}

/// Validates a single tag's text: raw characters, splice-brace licensing, schema resolution,
/// and (for takesValue tags) the value/unit-class grammar. `Def`/`Def-expand`/`Definition`
/// tags are skipped here entirely — they're validated as a whole-tree pass by
/// `def_validator`, since that needs sibling/group context this function doesn't have.
pub fn validate_tag(
    schemas: &SchemaCollection,
    tag: &HedTag,
    ctx: &ValidationContext,
    errors: &mut Vec<HedError>,
) {
    let text = tag.tag.as_str();

    if text.trim().is_empty() {
        errors.push(HedError::error(
            codes::TAG_EMPTY,
            "empty tag",
            Some(text.to_string()),
        ));
        return;
    }

    if is_whole_brace_token(text) {
        if ctx.definition_site == crate::validator::DefinitionSite::SidecarColumn {
            return;
        }
        errors.push(HedError::error(
            codes::CHARACTER_INVALID,
            "curly-brace column references are only valid within a sidecar",
            Some(text.to_string()),
        ));
        return;
    }
    if text.contains('{') || text.contains('}') {
        errors.push(HedError::error(
            codes::CHARACTER_INVALID,
            "curly braces may only appear as a whole standalone column reference",
            Some(text.to_string()),
        ));
        return;
    }

    if char_validator::first_invalid_raw_char(text).is_some() {
        errors.push(HedError::error(
            codes::CHARACTER_INVALID,
            "tag contains a disallowed control character",
            Some(text.to_string()),
        ));
        return;
    }

    let stripped = tag.without_namespace();
    let segments: Vec<&str> = stripped.split('/').collect();
    if segments.iter().any(|s| s.is_empty()) {
        errors.push(HedError::error(
            codes::TAG_INVALID,
            "tag has a leading, trailing, or consecutive slash",
            Some(text.to_string()),
        ));
        return;
    }

    let root_name = segments[0].to_lowercase();
    if root_name == "def" || root_name == "def-expand" || root_name == "definition" {
        // Handled by def_validator's whole-tree pass.
        return;
    }

    let Some((schema, rest)) = schemas.schema_for_tag(text) else {
        errors.push(HedError::error(
            codes::TAG_NAMESPACE_PREFIX_INVALID,
            &format!(
                "tag namespace prefix '{}' does not match any loaded schema (known: {:?})",
                tag.namespace(),
                schemas.valid_prefixes()
            ),
            Some(text.to_string()),
        ));
        return;
    };

    match schema.resolve_tag(rest) {
        TagResolution::NotFound => {
            errors.push(HedError::error(
                codes::TAG_INVALID,
                "tag not found in schema",
                Some(text.to_string()),
            ));
        }
        TagResolution::UnknownNamespace => {
            unreachable!("single-schema resolution never inspects namespaces")
        }
        TagResolution::Full(node) => {
            if node.has_attribute("deprecatedFrom") {
                errors.push(HedError::warning(
                    codes::ELEMENT_DEPRECATED,
                    "tag is deprecated",
                    Some(text.to_string()),
                ));
            }
            if node.has_attribute("requireChild") {
                errors.push(HedError::error(
                    codes::TAG_REQUIRES_CHILD,
                    "tag requires a child value",
                    Some(text.to_string()),
                ));
            }
        }
        TagResolution::Value { node, value } => {
            let (val_text, unit_text) = unit_validator::split_value_and_unit(value);

            if val_text == "#" {
                // The whole value slot is a bare placeholder (e.g. a sidecar Value column's
                // own template, "Label/#") — valid syntax wherever placeholders are
                // licensed at all; nothing further to check since it'll be substituted.
                if !placeholder_licensed(ctx) {
                    errors.push(HedError::error(
                        codes::PLACEHOLDER_INVALID,
                        "placeholder '#' is not licensed here",
                        Some(text.to_string()),
                    ));
                }
                return;
            }
            if value.contains('#') {
                errors.push(HedError::error(
                    codes::PLACEHOLDER_INVALID,
                    "'#' must be the entire value, not mixed with other text",
                    Some(text.to_string()),
                ));
                return;
            }

            let hash_child = node.children.get("#");
            let value_classes = hash_child
                .map(|n| n.attribute_values("valueClass"))
                .unwrap_or(&[]);
            let unit_classes = hash_child
                .map(|n| n.attribute_values("unitClass"))
                .unwrap_or(&[]);
            let has_unit_class = unit_classes
                .first()
                .is_some_and(|c| schema.unit_classes.contains_key(c));

            // Only split off a unit substring when this tag actually has a unit class —
            // otherwise the whole value (spaces and all) is checked as one value-class
            // literal (e.g. Label's nameClass forbids the embedded space in "30db kg").
            let value_to_check = if has_unit_class { val_text } else { value };

            if has_unit_class
                && let Some(unit_text) = unit_text
                && let Some(class_name) = unit_classes.first()
                && let Some(unit_class) = schema.unit_classes.get(class_name)
                && !unit_validator::validate_unit(schema, unit_class, unit_text)
            {
                errors.push(HedError::error(
                    codes::UNITS_INVALID,
                    &format!("'{}' is not a valid unit for {}", unit_text, class_name),
                    Some(text.to_string()),
                ));
                return;
            }

            for class_name in value_classes {
                if !char_validator::is_valid_for_value_class(schema, class_name, value_to_check) {
                    let code = if class_name.eq_ignore_ascii_case("numericClass") {
                        codes::VALUE_INVALID
                    } else {
                        codes::CHARACTER_INVALID
                    };
                    errors.push(HedError::error(
                        code,
                        &format!("'{}' is not a valid {} value", value_to_check, class_name),
                        Some(text.to_string()),
                    ));
                    return;
                }
            }
        }
        TagResolution::Extension { node: _, extension } => {
            if extension.contains('#') && !placeholder_licensed(ctx) {
                errors.push(HedError::error(
                    codes::PLACEHOLDER_INVALID,
                    "placeholder '#' is not licensed here",
                    Some(text.to_string()),
                ));
                return;
            }

            let bad_chars = extension
                .split('/')
                .any(|seg| !char_validator::is_valid_for_value_class(schema, "nameClass", seg));
            if bad_chars {
                errors.push(HedError::error(
                    codes::CHARACTER_INVALID,
                    "extension text contains invalid characters",
                    Some(text.to_string()),
                ));
                return;
            }

            let collides = extension.split('/').any(|seg| schema.has_tag_named(seg));
            if collides {
                errors.push(HedError::error(
                    codes::TAG_EXTENSION_INVALID,
                    "extension text duplicates an existing schema tag name",
                    Some(text.to_string()),
                ));
                return;
            }

            errors.push(HedError::warning(
                codes::TAG_EXTENDED,
                "tag uses an unrecognized extension",
                Some(text.to_string()),
            ));
        }
        TagResolution::InvalidExtension { node: _, remainder } => {
            if remainder.contains('#') {
                // A stray '#' attached to a tag that fundamentally can't take a value is
                // always a placeholder-syntax error, regardless of what the leftover text
                // otherwise looks like.
                errors.push(HedError::error(
                    codes::PLACEHOLDER_INVALID,
                    "tag does not take a value or placeholder",
                    Some(text.to_string()),
                ));
            } else {
                // Matching only part of the tag doesn't count as a valid (extensible) match
                // here — the base tag takes neither a value nor an extension, so the whole
                // thing is simply not a recognized tag.
                errors.push(HedError::error(
                    codes::TAG_INVALID,
                    "tag not found in schema",
                    Some(text.to_string()),
                ));
            }
        }
    }
}
