use crate::errors::{HedError, codes};
use crate::models::{HedGroup, HedNode, HedTag};
use crate::schema::{SchemaCollection, SchemaNode, TagResolution};
use crate::validator::{char_validator, unit_validator};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct DefinitionEntry {
    pub name: String,
    pub takes_value: bool,
    pub contents: Option<HedGroup>,
    pub value_classes: Vec<String>,
    pub unit_classes: Vec<String>,
    /// Namespace prefix of the tag holding the definition's placeholder ("" normally) —
    /// unit/value classes must be looked up in that schema.
    pub class_namespace: String,
}

pub type DefinitionMap = HashMap<String, DefinitionEntry>;

/// Which validator's definition rules apply: `Definition/` groups are only legal at all
/// within a sidecar (`SidecarColumn`); a plain event-string context (`PlainString`) never
/// licenses them, regardless of how well-formed they'd otherwise be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefinitionSite {
    PlainString,
    SidecarColumn,
}

fn root_name(tag: &HedTag) -> String {
    tag.without_namespace()
        .split('/')
        .next()
        .unwrap_or("")
        .to_lowercase()
}

fn is_definition_tag(tag: &HedTag) -> bool {
    root_name(tag) == "definition"
}

fn is_def_tag(tag: &HedTag) -> bool {
    root_name(tag) == "def"
}

fn is_def_expand_tag(tag: &HedTag) -> bool {
    root_name(tag) == "def-expand"
}

fn resolved_node<'a>(schemas: &'a SchemaCollection, tag: &'a HedTag) -> Option<&'a SchemaNode> {
    match schemas.resolve_tag(&tag.tag) {
        TagResolution::Full(n) => Some(n),
        TagResolution::Value { node, .. } => Some(node),
        TagResolution::Extension { node, .. } => Some(node),
        TagResolution::InvalidExtension { node, .. } => Some(node),
        TagResolution::NotFound | TagResolution::UnknownNamespace => None,
    }
}

fn is_placeholder_position_valid(schemas: &SchemaCollection, tag: &HedTag) -> bool {
    match schemas.resolve_tag(&tag.tag) {
        TagResolution::Value { value, .. } => unit_validator::split_value_and_unit(value).0 == "#",
        _ => false,
    }
}

/// Whether this node (as an item in the top-level list of a string) is "definition-related":
/// a bare Definition tag, or a group containing one. Used to detect illegal mixing of
/// Definition groups with ordinary content at the top level of a single string.
fn is_definition_related(node: &HedNode) -> bool {
    match node {
        HedNode::Tag(t) => is_definition_tag(t),
        HedNode::Group(g) => group_is_definition(g),
    }
}

/// Whether `group` contains a Definition tag among its direct children. A definition's
/// contents (including any placeholder) are fully validated by this module's own
/// structural checks — the generic tag-level walk skips such groups entirely to avoid
/// double-reporting (or misreporting, since the outer validation context's placeholder
/// licensing doesn't apply inside a definition's own declared contents).
pub(crate) fn group_is_definition(group: &HedGroup) -> bool {
    group
        .children
        .iter()
        .any(|c| matches!(c, HedNode::Tag(t) if is_definition_tag(t)))
}

/// Scans `nodes` (the top-level list of one HED string) for `Definition/` groups, validates
/// their structure, and registers well-formed ones into `map`. Any errors found (bad
/// placement, bad shape, bad placeholders, disallowed content, duplicate names, or —
/// for `DefinitionSite::PlainString` — the mere presence of a Definition at all) are pushed
/// to `errors`.
pub fn gather_definitions(
    schemas: &SchemaCollection,
    nodes: &[HedNode],
    site: DefinitionSite,
    map: &mut DefinitionMap,
    errors: &mut Vec<HedError>,
) {
    let has_def = nodes.iter().any(is_definition_related);
    let has_other = nodes.iter().any(|n| !is_definition_related(n));
    if has_def && has_other {
        errors.push(HedError::error(
            codes::DEFINITION_INVALID,
            "Definition groups cannot be mixed with other content at the top level of a string",
            None,
        ));
    }

    walk_gather(schemas, nodes, 0, site, map, errors);
}

fn walk_gather(
    schemas: &SchemaCollection,
    nodes: &[HedNode],
    depth: usize,
    site: DefinitionSite,
    map: &mut DefinitionMap,
    errors: &mut Vec<HedError>,
) {
    for node in nodes {
        match node {
            HedNode::Tag(t) if is_definition_tag(t) => {
                errors.push(HedError::error(
                    codes::DEFINITION_INVALID,
                    "Definition tag must be inside its own top-level group",
                    Some(t.tag.clone()),
                ));
            }
            HedNode::Tag(_) => {}
            HedNode::Group(g) => {
                let def_tag = g.children.iter().find_map(|c| match c {
                    HedNode::Tag(t) if is_definition_tag(t) => Some(t),
                    _ => None,
                });
                match def_tag {
                    Some(def_tag) if depth != 0 => {
                        errors.push(HedError::error(
                            codes::DEFINITION_INVALID,
                            "Definition group must be at the top level of the string",
                            Some(def_tag.tag.clone()),
                        ));
                    }
                    Some(def_tag) => {
                        register_definition_group(schemas, g, def_tag, site, map, errors);
                    }
                    None => {
                        walk_gather(schemas, &g.children, depth + 1, site, map, errors);
                    }
                }
            }
        }
    }
}

fn register_definition_group(
    schemas: &SchemaCollection,
    group: &HedGroup,
    def_tag: &HedTag,
    site: DefinitionSite,
    map: &mut DefinitionMap,
    errors: &mut Vec<HedError>,
) {
    let def_tag_count = group
        .children
        .iter()
        .filter(|c| matches!(c, HedNode::Tag(t) if is_definition_tag(t)))
        .count();
    if def_tag_count > 1 {
        errors.push(HedError::error(
            codes::DEFINITION_INVALID,
            "multiple Definition tags in the same group",
            None,
        ));
        return;
    }
    if group.children.len() > 2 {
        errors.push(HedError::error(
            codes::DEFINITION_INVALID,
            "too many elements in a Definition group",
            None,
        ));
        return;
    }

    let contents: Option<&HedGroup> = group.children.iter().find_map(|c| match c {
        HedNode::Group(g) => Some(g),
        _ => None,
    });
    if group.children.len() == 2 && contents.is_none() {
        errors.push(HedError::error(
            codes::DEFINITION_INVALID,
            "Definition contents must be a tag group",
            None,
        ));
        return;
    }

    let segs = def_tag.segments();
    if segs.len() < 2 || segs[1].is_empty() {
        errors.push(HedError::error(
            codes::DEFINITION_INVALID,
            "Definition has no name",
            None,
        ));
        return;
    }
    let takes_value = segs.len() == 3 && segs[2] == "#";
    if segs.len() > 3 || (segs.len() == 3 && segs[2] != "#") {
        errors.push(HedError::error(
            codes::DEFINITION_INVALID,
            "Definition name must be a single flat label, optionally followed by /#",
            Some(def_tag.tag.clone()),
        ));
        return;
    }
    let raw_name = segs[1];

    if let Some(c) = contents {
        check_contents_restrictions(schemas, c, errors);
    }

    let placeholder_count = contents
        .map(|c| scan_placeholders(schemas, c, errors))
        .unwrap_or(0);
    let expected = if takes_value { 1 } else { 0 };
    if placeholder_count != expected {
        errors.push(HedError::error(
            codes::DEFINITION_INVALID,
            "wrong number of placeholders in Definition contents",
            Some(def_tag.tag.clone()),
        ));
    }

    let key = raw_name.to_lowercase();
    match map.entry(key) {
        std::collections::hash_map::Entry::Occupied(_) => {
            errors.push(HedError::error(
                codes::DEFINITION_INVALID,
                "duplicate Definition name",
                Some(def_tag.tag.clone()),
            ));
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            let (value_classes, unit_classes, class_namespace) = if takes_value {
                find_placeholder_classes(schemas, contents)
            } else {
                (vec![], vec![], String::new())
            };
            entry.insert(DefinitionEntry {
                name: raw_name.to_string(),
                takes_value,
                contents: contents.cloned(),
                value_classes,
                unit_classes,
                class_namespace,
            });
        }
    }

    if site == DefinitionSite::PlainString {
        errors.push(HedError::error(
            codes::DEFINITION_INVALID,
            "Definition tags are not allowed outside a sidecar",
            Some(def_tag.tag.clone()),
        ));
    }
}

fn check_contents_restrictions(
    schemas: &SchemaCollection,
    group: &HedGroup,
    errors: &mut Vec<HedError>,
) {
    for child in &group.children {
        match child {
            HedNode::Tag(t) => {
                let root = root_name(t);
                if root == "definition" || root == "def" || root == "def-expand" {
                    errors.push(HedError::error(
                        codes::DEFINITION_INVALID,
                        "Definition contents cannot reference other definitions",
                        Some(t.tag.clone()),
                    ));
                    continue;
                }
                if let Some(node) = resolved_node(schemas, t)
                    && (node.has_attribute("unique")
                        || node.has_attribute("required")
                        || node.has_attribute("topLevelTagGroup"))
                {
                    errors.push(HedError::error(
                        codes::DEFINITION_INVALID,
                        "Definition contents cannot contain this tag",
                        Some(t.tag.clone()),
                    ));
                }
            }
            HedNode::Group(g) => check_contents_restrictions(schemas, g, errors),
        }
    }
}

fn scan_placeholders(
    schemas: &SchemaCollection,
    group: &HedGroup,
    errors: &mut Vec<HedError>,
) -> usize {
    group
        .children
        .iter()
        .map(|c| scan_placeholders_node(schemas, c, errors))
        .sum()
}

fn scan_placeholders_node(
    schemas: &SchemaCollection,
    node: &HedNode,
    errors: &mut Vec<HedError>,
) -> usize {
    match node {
        HedNode::Tag(t) => {
            let n = t.tag.matches('#').count();
            if n > 0 && !is_placeholder_position_valid(schemas, t) {
                errors.push(HedError::error(
                    codes::DEFINITION_INVALID,
                    "misplaced placeholder in Definition contents",
                    Some(t.tag.clone()),
                ));
            }
            n
        }
        HedNode::Group(g) => scan_placeholders(schemas, g, errors),
    }
}

fn find_placeholder_classes(
    schemas: &SchemaCollection,
    contents: Option<&HedGroup>,
) -> (Vec<String>, Vec<String>, String) {
    contents
        .map(|c| find_in_group(schemas, c))
        .unwrap_or_default()
}

fn find_in_group(
    schemas: &SchemaCollection,
    group: &HedGroup,
) -> (Vec<String>, Vec<String>, String) {
    for child in &group.children {
        match child {
            HedNode::Tag(t) if is_placeholder_position_valid(schemas, t) => {
                if let TagResolution::Value { node, .. } = schemas.resolve_tag(&t.tag) {
                    let hash_child = node.children.get("#");
                    let vc = hash_child
                        .map(|n| n.attribute_values("valueClass").to_vec())
                        .unwrap_or_default();
                    let uc = hash_child
                        .map(|n| n.attribute_values("unitClass").to_vec())
                        .unwrap_or_default();
                    return (vc, uc, t.namespace().to_string());
                }
            }
            HedNode::Group(g) => {
                let found = find_in_group(schemas, g);
                if !found.0.is_empty() || !found.1.is_empty() {
                    return found;
                }
            }
            _ => {}
        }
    }
    (vec![], vec![], String::new())
}

fn substitute_placeholder(group: &HedGroup, value: &str) -> HedGroup {
    HedGroup::new(
        group
            .children
            .iter()
            .map(|c| match c {
                HedNode::Tag(t) => HedNode::Tag(HedTag::new(t.tag.replace('#', value))),
                HedNode::Group(g) => HedNode::Group(substitute_placeholder(g, value)),
            })
            .collect(),
    )
}

/// Order-independent structural equality (mirrors duplicate_checker's identity comparison,
/// kept separate here to avoid coupling the two modules together).
fn nodes_equal(a: &HedNode, b: &HedNode) -> bool {
    match (a, b) {
        (HedNode::Tag(t1), HedNode::Tag(t2)) => t1.canonical() == t2.canonical(),
        (HedNode::Group(g1), HedNode::Group(g2)) => groups_equal(g1, g2),
        _ => false,
    }
}

fn groups_equal(a: &HedGroup, b: &HedGroup) -> bool {
    if a.children.len() != b.children.len() {
        return false;
    }
    let mut remaining: Vec<&HedNode> = b.children.iter().collect();
    for child in &a.children {
        match remaining.iter().position(|other| nodes_equal(child, other)) {
            Some(pos) => {
                remaining.remove(pos);
            }
            None => return false,
        }
    }
    true
}

/// Validates every `Def`/`Def-expand` usage found anywhere in `nodes` against `defs`.
/// `allow_placeholder` licenses a bare `#` as a definition's value (e.g. a sidecar Value
/// column's own template, `"Def/Acc/#"`), mirroring `tag_validator`'s placeholder licensing.
pub fn validate_def_usage(
    schemas: &SchemaCollection,
    nodes: &[HedNode],
    defs: &DefinitionMap,
    allow_placeholder: bool,
    errors: &mut Vec<HedError>,
) {
    for node in nodes {
        match node {
            HedNode::Tag(t) if is_def_tag(t) => {
                validate_def_tag(schemas, t, defs, allow_placeholder, errors);
            }
            HedNode::Tag(_) => {}
            HedNode::Group(g) => {
                if let Some(HedNode::Tag(t)) = g.children.first()
                    && is_def_expand_tag(t)
                {
                    validate_def_expand_group(schemas, g, t, defs, allow_placeholder, errors);
                }
                validate_def_usage(schemas, &g.children, defs, allow_placeholder, errors);
            }
        }
    }
}

/// Looks up `name` in `defs` and checks that a value is supplied iff the definition takes
/// one, validating the value/unit against the definition's placeholder tag's classes.
/// Returns `true` if the reference resolved cleanly (no errors pushed).
#[allow(clippy::too_many_arguments)]
fn check_def_reference(
    schemas: &SchemaCollection,
    whole_tag_text: &str,
    name: &str,
    value: Option<&str>,
    defs: &DefinitionMap,
    code: &str,
    allow_placeholder: bool,
    errors: &mut Vec<HedError>,
) -> bool {
    let Some(entry) = defs.get(&name.to_lowercase()) else {
        errors.push(HedError::error(
            code,
            "definition not found",
            Some(whole_tag_text.to_string()),
        ));
        return false;
    };
    if entry.takes_value != value.is_some() {
        errors.push(HedError::error(
            code,
            "definition value presence does not match its declaration",
            Some(whole_tag_text.to_string()),
        ));
        return false;
    }

    if let Some(val) = value {
        let (val_text, unit_text) = unit_validator::split_value_and_unit(val);

        if val_text == "#" {
            if !allow_placeholder {
                errors.push(HedError::error(
                    codes::PLACEHOLDER_INVALID,
                    "placeholder '#' is not licensed here",
                    Some(whole_tag_text.to_string()),
                ));
                return false;
            }
            return true;
        }
        // Unit/value classes are looked up in the schema the definition's placeholder tag
        // belongs to (matters when a whole definition is namespaced, e.g. "ts:...").
        let Some(schema) = schemas.schema_for(&entry.class_namespace) else {
            return true;
        };
        let has_unit_class = entry
            .unit_classes
            .first()
            .is_some_and(|c| schema.unit_classes.contains_key(c));
        let value_to_check = if has_unit_class { val_text } else { val };

        if has_unit_class
            && let Some(unit_text) = unit_text
            && let Some(class_name) = entry.unit_classes.first()
            && let Some(unit_class) = schema.unit_classes.get(class_name)
            && !unit_validator::validate_unit(schema, unit_class, unit_text)
        {
            errors.push(HedError::error(
                codes::UNITS_INVALID,
                "invalid unit for definition value",
                Some(whole_tag_text.to_string()),
            ));
            return false;
        }

        for class_name in &entry.value_classes {
            if !char_validator::is_valid_for_value_class(schema, class_name, value_to_check) {
                let vcode = if class_name.eq_ignore_ascii_case("numericClass") {
                    codes::VALUE_INVALID
                } else {
                    codes::CHARACTER_INVALID
                };
                errors.push(HedError::error(
                    vcode,
                    "invalid value for definition placeholder",
                    Some(whole_tag_text.to_string()),
                ));
                return false;
            }
        }
    }

    true
}

fn split_name_value(tag: &HedTag) -> (String, Option<String>) {
    let segs = tag.segments();
    let name = segs.get(1).copied().unwrap_or("").to_string();
    let value = if segs.len() > 2 {
        Some(segs[2..].join("/"))
    } else {
        None
    };
    (name, value)
}

fn validate_def_tag(
    schemas: &SchemaCollection,
    tag: &HedTag,
    defs: &DefinitionMap,
    allow_placeholder: bool,
    errors: &mut Vec<HedError>,
) {
    let (name, value) = split_name_value(tag);
    if name.is_empty() {
        errors.push(HedError::error(
            codes::TAG_REQUIRES_CHILD,
            "Def tag requires a name",
            Some(tag.tag.clone()),
        ));
        return;
    }
    check_def_reference(
        schemas,
        &tag.tag,
        &name,
        value.as_deref(),
        defs,
        codes::DEF_INVALID,
        allow_placeholder,
        errors,
    );
}

fn validate_def_expand_group(
    schemas: &SchemaCollection,
    group: &HedGroup,
    def_tag: &HedTag,
    defs: &DefinitionMap,
    allow_placeholder: bool,
    errors: &mut Vec<HedError>,
) {
    let (name, value) = split_name_value(def_tag);
    if name.is_empty() {
        errors.push(HedError::error(
            codes::TAG_REQUIRES_CHILD,
            "Def-expand tag requires a name",
            Some(def_tag.tag.clone()),
        ));
        return;
    }
    let ok = check_def_reference(
        schemas,
        &def_tag.tag,
        &name,
        value.as_deref(),
        defs,
        codes::DEF_EXPAND_INVALID,
        allow_placeholder,
        errors,
    );
    if !ok {
        return;
    }
    let entry = defs
        .get(&name.to_lowercase())
        .expect("checked present by check_def_reference");

    if group.children.len() > 2 {
        errors.push(HedError::error(
            codes::DEF_EXPAND_INVALID,
            "Def-expand group has extra content",
            None,
        ));
        return;
    }

    let contents_child = group.children.get(1);
    match (&entry.contents, contents_child) {
        (None, None) => {}
        (None, Some(_)) => {
            errors.push(HedError::error(
                codes::DEF_EXPAND_INVALID,
                "definition has no contents but Def-expand supplied one",
                None,
            ));
        }
        (Some(_), None) => {
            errors.push(HedError::error(
                codes::DEF_EXPAND_INVALID,
                "missing definition contents group",
                None,
            ));
        }
        (Some(expected), Some(HedNode::Group(actual))) => {
            let expected_group = match &value {
                Some(v) => substitute_placeholder(expected, v),
                None => expected.clone(),
            };
            if !groups_equal(&expected_group, actual) {
                errors.push(HedError::error(
                    codes::DEF_EXPAND_INVALID,
                    "Def-expand contents do not match the definition",
                    None,
                ));
            }
        }
        (Some(_), Some(HedNode::Tag(_))) => {
            errors.push(HedError::error(
                codes::DEF_EXPAND_INVALID,
                "definition contents must be a tag group",
                None,
            ));
        }
    }
}
