use super::model::*;
use directories::ProjectDirs;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;

/// Convert a JSON attributes object into the HashMap<String, Vec<String>> format
/// expected by the validator. Boolean `true` becomes a present-with-no-values attribute;
/// `false` is treated as absent.
fn parse_attributes(attrs_val: &Value) -> HashMap<String, Vec<String>> {
    let mut attributes = HashMap::new();
    if let Some(obj) = attrs_val.as_object() {
        for (key, val) in obj {
            match val {
                Value::Bool(true) => {
                    attributes.insert(key.clone(), vec![]);
                }
                Value::Bool(false) => {}
                Value::String(s) => {
                    attributes.insert(key.clone(), vec![s.clone()]);
                }
                Value::Array(arr) => {
                    let values: Vec<String> = arr
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                    attributes.insert(key.clone(), values);
                }
                Value::Number(n) => {
                    attributes.insert(key.clone(), vec![n.to_string()]);
                }
                _ => {}
            }
        }
    }
    attributes
}

/// Translate named characters from the JSON value_classes format to their
/// actual character representations used by the validator's character checker.
pub(crate) fn translate_allowed_character(name: &str) -> String {
    match name {
        "letters" => "letters".to_string(),
        "digits" => "digits".to_string(),
        "blank" => "blank".to_string(),
        "text" => "text".to_string(),
        "hyphen" => "-".to_string(),
        "underscore" => "_".to_string(),
        "period" => ".".to_string(),
        "comma" => ",".to_string(),
        "colon" => ":".to_string(),
        "semicolon" => ";".to_string(),
        "slash" => "/".to_string(),
        "plus" => "+".to_string(),
        "dollar" => "$".to_string(),
        "caret" => "^".to_string(),
        "at" => "@".to_string(),
        "number_sign" => "#".to_string(),
        "equals" => "=".to_string(),
        "left-paren" => "(".to_string(),
        "right-paren" => ")".to_string(),
        "exclamation" => "!".to_string(),
        "question-mark" => "?".to_string(),
        "single-quote" => "'".to_string(),
        "double-quote" => "\"".to_string(),
        "ampersand" => "&".to_string(),
        "percent-sign" => "%".to_string(),
        "asterisk" => "*".to_string(),
        other => other.to_string(),
    }
}

/// Recursively build a SchemaNode tree from the flat JSON tags dictionary.
fn build_node_from_json(
    tag_name: &str,
    tags: &serde_json::Map<String, Value>,
) -> Option<SchemaNode> {
    let entry = tags.get(tag_name)?;
    let obj = entry.as_object()?;

    let name = obj
        .get("short_form")
        .and_then(|v| v.as_str())
        .unwrap_or(tag_name)
        .to_string();

    let description = obj
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let attributes = obj
        .get("attributes")
        .map(parse_attributes)
        .unwrap_or_default();

    // Build children recursively
    let mut children = HashMap::new();
    if let Some(Value::Array(child_names)) = obj.get("children") {
        for child_name_val in child_names {
            if let Some(child_name) = child_name_val.as_str()
                && let Some(child_node) = build_node_from_json(child_name, tags)
            {
                children.insert(child_name.to_lowercase(), child_node);
            }
        }
    }

    // If this tag has a placeholder, create a "#" child node
    if let Some(placeholder) = obj.get("placeholder") {
        let mut placeholder_attrs = parse_attributes(placeholder);
        placeholder_attrs.remove("description");
        let placeholder_desc = placeholder
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        children.insert(
            "#".to_string(),
            SchemaNode {
                name: "#".to_string(),
                description: placeholder_desc,
                attributes: placeholder_attrs,
                children: HashMap::new(),
            },
        );
    }

    Some(SchemaNode {
        name,
        description,
        attributes,
        children,
    })
}

fn entry_from_json(name: &str, val: &Value) -> SchemaEntry {
    let description = val
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // In the hedjson format an entry's attributes are its remaining keys (description
    // excluded); flags are booleans, values are strings/arrays.
    let mut attrs = parse_attributes(val);
    attrs.remove("description");
    SchemaEntry {
        name: name.to_string(),
        description,
        attributes: attrs,
    }
}

fn parse_unit_classes(root: &Value) -> HashMap<String, UnitClass> {
    let mut unit_classes = HashMap::new();
    if let Some(obj) = root.get("unit_classes").and_then(|v| v.as_object()) {
        for (name, val) in obj {
            let units = val
                .get("units")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let default_unit = val
                .get("default_units")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let mut entry = entry_from_json(name, val);
            entry.attributes.remove("units");
            entry.attributes.remove("default_units");
            unit_classes.insert(
                name.clone(),
                UnitClass {
                    name: name.clone(),
                    units,
                    default_unit,
                    entry,
                },
            );
        }
    }
    unit_classes
}

fn parse_units(
    root: &Value,
    unit_classes: &HashMap<String, UnitClass>,
) -> HashMap<String, UnitEntry> {
    let owning_class: HashMap<&str, &str> = unit_classes
        .values()
        .flat_map(|uc| uc.units.iter().map(move |u| (u.as_str(), uc.name.as_str())))
        .collect();

    let mut units = HashMap::new();
    if let Some(obj) = root.get("units").and_then(|v| v.as_object()) {
        for (name, val) in obj {
            let si_unit = val.get("SIUnit").and_then(|v| v.as_bool()).unwrap_or(false);
            let unit_symbol = val
                .get("unitSymbol")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            units.insert(
                name.clone(),
                UnitEntry {
                    name: name.clone(),
                    si_unit,
                    unit_symbol,
                    unit_class: owning_class.get(name.as_str()).unwrap_or(&"").to_string(),
                    entry: entry_from_json(name, val),
                },
            );
        }
    }
    units
}

fn parse_unit_modifiers(root: &Value) -> HashMap<String, UnitModifier> {
    let mut modifiers = HashMap::new();
    if let Some(obj) = root.get("unit_modifiers").and_then(|v| v.as_object()) {
        for (name, val) in obj {
            let is_symbol_modifier = val
                .get("SIUnitSymbolModifier")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            modifiers.insert(
                name.clone(),
                UnitModifier {
                    name: name.clone(),
                    is_symbol_modifier,
                    entry: entry_from_json(name, val),
                },
            );
        }
    }
    modifiers
}

fn parse_entry_section(root: &Value, key: &str) -> HashMap<String, SchemaEntry> {
    let mut out = HashMap::new();
    if let Some(obj) = root.get(key).and_then(|v| v.as_object()) {
        for (name, val) in obj {
            // Keyed by lowercase name (entries keep their original casing in `.name`),
            // matching the wiki parser so attribute-domain lookups are case-insensitive.
            out.insert(name.to_lowercase(), entry_from_json(name, val));
        }
    }
    out
}

fn parse_extras(root: &Value) -> HashMap<String, Vec<HashMap<String, String>>> {
    let mut extras = HashMap::new();
    // The hedjson format calls each row's leading column "name"; the canonical column names
    // (used by the wiki format and the required-column compliance check) differ per section.
    for (key, name_column) in [
        ("sources", "source"),
        ("prefixes", "prefix"),
        ("external_annotations", "prefix"),
    ] {
        if let Some(arr) = root.get(key).and_then(|v| v.as_array()) {
            let rows: Vec<HashMap<String, String>> = arr
                .iter()
                .filter_map(|row| row.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| {
                            let k = if k == "name" { name_column } else { k.as_str() };
                            v.as_str().map(|s| (k.to_string(), s.to_string()))
                        })
                        .collect()
                })
                .collect();
            extras.insert(key.to_string(), rows);
        }
    }
    extras
}

impl Schema {
    pub fn parse(json_str: &str) -> Result<Self, SchemaError> {
        let root: Value =
            serde_json::from_str(json_str).map_err(|e| SchemaError::Parse(e.to_string()))?;

        let get_str = |key: &str| {
            root.get(key)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };

        let version = get_str("version");
        let library = get_str("library");
        let with_standard = get_str("withStandard");
        let unmerged = get_str("unmerged");

        let mut header_attributes = HashMap::new();
        for key in ["version", "library", "withStandard", "unmerged"] {
            let v = get_str(key);
            if !v.is_empty() {
                header_attributes.insert(key.to_string(), v);
            }
        }

        let tags = root
            .get("tags")
            .and_then(|v| v.as_object())
            .ok_or_else(|| SchemaError::Parse("Missing 'tags' object in schema".to_string()))?;

        // Find root tags (those with parent: null)
        let mut root_nodes = HashMap::new();
        for (tag_name, tag_val) in tags {
            if let Some(obj) = tag_val.as_object() {
                let is_root = obj.get("parent").map(|v| v.is_null()).unwrap_or(false);
                if is_root && let Some(node) = build_node_from_json(tag_name, tags) {
                    root_nodes.insert(node.name.to_lowercase(), node);
                }
            }
        }

        // Parse value classes
        let mut value_classes = HashMap::new();
        if let Some(vc_obj) = root.get("value_classes").and_then(|v| v.as_object()) {
            for (vc_name, vc_val) in vc_obj {
                let allowed_characters = vc_val
                    .get("allowed_characters")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .map(translate_allowed_character)
                            .collect()
                    })
                    .unwrap_or_default();

                let mut entry = entry_from_json(vc_name, vc_val);
                entry.attributes.remove("allowed_characters");
                value_classes.insert(
                    vc_name.clone(),
                    ValueClass {
                        name: vc_name.clone(),
                        allowed_characters,
                        entry,
                    },
                );
            }
        }

        let unit_classes = parse_unit_classes(&root);
        let units = parse_units(&root, &unit_classes);
        let unit_modifiers = parse_unit_modifiers(&root);
        let schema_attributes = parse_entry_section(&root, "schema_attributes");
        let properties = parse_entry_section(&root, "properties");
        let extras = parse_extras(&root);

        let mut schema = Schema {
            version,
            library,
            with_standard,
            merged: unmerged.is_empty(),
            namespace: String::new(),
            header_attributes,
            prologue: get_str("prologue"),
            epilogue: get_str("epilogue"),
            root_nodes,
            value_classes,
            unit_classes,
            units,
            unit_modifiers,
            schema_attributes,
            properties,
            extras,
            duplicate_names: HashMap::new(),
            tag_index: HashMap::new(),
        };
        schema.finalize();
        Ok(schema)
    }

    pub fn load_standard(version: &str) -> Result<Self, SchemaError> {
        // For the bundled version, use the embedded JSON
        if version == "8.4.0" {
            let json_content = include_str!("../data/HED8.4.0.json");
            return Self::parse(json_content);
        }

        let cache_dir = ProjectDirs::from("", "HED", "ValidatorRs")
            .map(|proj_dirs| proj_dirs.cache_dir().to_path_buf())
            .unwrap_or_else(std::env::temp_dir);

        if !cache_dir.exists() {
            let _ = fs::create_dir_all(&cache_dir);
        }

        let file_name = format!("HED{}.json", version);
        let file_path = cache_dir.join(&file_name);

        let json_content = if file_path.exists() {
            fs::read_to_string(&file_path)
                .map_err(|e| SchemaError::Load(format!("Failed to read cached file: {}", e)))?
        } else {
            let url = format!(
                "https://raw.githubusercontent.com/hed-standard/hed-schemas/main/standard_schema/hedjson/{}",
                file_name
            );
            let resp = reqwest::blocking::get(&url)
                .map_err(|e| SchemaError::Load(format!("Failed to fetch schema: {}", e)))?;

            if !resp.status().is_success() {
                return Err(SchemaError::Load(format!(
                    "Failed to download {}: HTTP {}",
                    url,
                    resp.status()
                )));
            }

            let text = resp
                .text()
                .map_err(|e| SchemaError::Load(format!("Failed to read response: {}", e)))?;
            let _ = fs::write(&file_path, &text);
            text
        };

        Self::parse(&json_content)
    }
}
