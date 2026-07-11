use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("Failed to load schema: {0}")]
    Load(String),
    #[error("Failed to parse schema: {0}")]
    Parse(String),
}

/// A generic named entry in one of a schema's flat sections (value classes, units, unit
/// classes, unit modifiers, schema attributes, properties). Tags use `SchemaNode` instead
/// (they're a tree), but carry the same per-entry fields.
#[derive(Debug, Clone, Default)]
pub struct SchemaEntry {
    pub name: String,
    pub description: String,
    pub attributes: HashMap<String, Vec<String>>,
}

impl SchemaEntry {
    pub fn has_attribute(&self, name: &str) -> bool {
        self.attributes.contains_key(name)
    }

    pub fn attribute_value(&self, name: &str) -> Option<&str> {
        self.attributes
            .get(name)
            .and_then(|v| v.first())
            .map(|s| s.as_str())
    }

    pub fn attribute_values(&self, name: &str) -> &[String] {
        self.attributes
            .get(name)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}

#[derive(Debug, Clone)]
pub struct SchemaNode {
    pub name: String,
    pub description: String,
    pub attributes: HashMap<String, Vec<String>>,
    pub children: HashMap<String, SchemaNode>,
}

impl SchemaNode {
    pub fn has_attribute(&self, name: &str) -> bool {
        self.attributes.contains_key(name)
    }

    pub fn attribute_value(&self, name: &str) -> Option<&str> {
        self.attributes
            .get(name)
            .and_then(|v| v.first())
            .map(|s| s.as_str())
    }

    pub fn attribute_values(&self, name: &str) -> &[String] {
        self.attributes
            .get(name)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}

#[derive(Debug, Clone, Default)]
pub struct ValueClass {
    pub name: String,
    pub allowed_characters: Vec<String>,
    pub entry: SchemaEntry,
}

/// A single named unit (e.g. "m-per-s^2", "metre", "kg").
#[derive(Debug, Clone, Default)]
pub struct UnitEntry {
    pub name: String,
    /// Whether this unit participates in SI-prefix combination at all.
    pub si_unit: bool,
    /// True for symbol-style units (case-sensitive, no pluralization, e.g. "m", "kg", "$").
    /// False for word-style units (case-insensitive, mechanical "+s" pluralization, e.g. "meter").
    pub unit_symbol: bool,
    /// Name of the unit class this unit belongs to.
    pub unit_class: String,
    pub entry: SchemaEntry,
}

/// An SI prefix modifier (e.g. "kilo"/"k", "milli"/"m").
#[derive(Debug, Clone, Default)]
pub struct UnitModifier {
    pub name: String,
    /// True for symbol-style modifiers (combine only with symbol units, case-sensitive).
    /// False for word-style modifiers (combine only with word units, case-insensitive).
    pub is_symbol_modifier: bool,
    pub entry: SchemaEntry,
}

#[derive(Debug, Clone, Default)]
pub struct UnitClass {
    pub name: String,
    /// Exact spellings of the units belonging to this class; look these up in `Schema::units`.
    pub units: Vec<String>,
    pub default_unit: Option<String>,
    pub entry: SchemaEntry,
}

/// One recorded duplicate: the colliding name plus whether the copies span a library
/// boundary (one from the base standard schema, one merged in from a library) — that
/// distinction changes the reported error code (SCHEMA_LIBRARY_INVALID vs
/// SCHEMA_DUPLICATE_NODE).
#[derive(Debug, Clone)]
pub struct DuplicateName {
    pub name: String,
    pub spans_library: bool,
}

/// Names of the flat sections, used as keys in `Schema::duplicate_names` and by the
/// compliance checker to talk about where a problem lives.
pub mod sections {
    pub const TAGS: &str = "tags";
    pub const UNIT_CLASSES: &str = "unit_classes";
    pub const UNITS: &str = "units";
    pub const UNIT_MODIFIERS: &str = "unit_modifiers";
    pub const VALUE_CLASSES: &str = "value_classes";
    pub const ATTRIBUTES: &str = "schema_attributes";
    pub const PROPERTIES: &str = "properties";
}

#[derive(Debug, Clone, Default)]
pub struct Schema {
    pub version: String,
    /// Library name(s), comma-joined after a merge ("" for a standard schema).
    pub library: String,
    /// The standard-schema version this library partners with ("" if unpartnered/standard).
    pub with_standard: String,
    /// Whether this schema's source file already contains the merged standard tree
    /// (i.e. the header did NOT say `unmerged="True"`).
    pub merged: bool,
    /// The namespace prefix this schema answers to in a collection ("" or e.g. "sc:").
    pub namespace: String,
    /// Raw header attributes from the source (version/library/withStandard/unmerged/...).
    pub header_attributes: HashMap<String, String>,
    pub prologue: String,
    pub epilogue: String,
    pub root_nodes: HashMap<String, SchemaNode>,
    pub value_classes: HashMap<String, ValueClass>,
    pub unit_classes: HashMap<String, UnitClass>,
    pub units: HashMap<String, UnitEntry>,
    pub unit_modifiers: HashMap<String, UnitModifier>,
    pub schema_attributes: HashMap<String, SchemaEntry>,
    pub properties: HashMap<String, SchemaEntry>,
    /// Extras tables (8.3+): section name ("sources" | "prefixes" | "external_annotations")
    /// -> rows of column -> value.
    pub extras: HashMap<String, Vec<HashMap<String, String>>>,
    /// Section name (see `sections`) -> names that appeared more than once (per that
    /// section's case rules). Populated at load/merge time, reported by compliance.
    pub duplicate_names: HashMap<String, Vec<DuplicateName>>,
    /// Lowercase short tag name -> full "/"-joined path(s) (original case) from the tree
    /// root to that node, for every node in the schema (excludes the "#" placeholder
    /// pseudo-child). HED tags are conventionally written using just the short name (or any
    /// trailing suffix of the full path), not the full root-to-leaf path, so this is the
    /// primary index `resolve_tag` matches against. A short name can map to more than one
    /// path if it's reused at different positions in the tree (a compliance error, but the
    /// model tolerates it so the duplicate can be reported).
    pub(crate) tag_index: HashMap<String, Vec<String>>,
}

/// Result of resolving a user-supplied tag string against the schema tree.
#[derive(Debug)]
pub enum TagResolution<'a> {
    /// The tag matched a schema node exactly, with no remaining text.
    Full(&'a SchemaNode),
    /// The matched node takes a value (has a "#" child); `value` is the text after the
    /// matched prefix (the part that would replace "#").
    Value {
        node: &'a SchemaNode,
        value: &'a str,
    },
    /// The matched node allows extensions; `extension` is the free text after the matched
    /// prefix.
    Extension {
        node: &'a SchemaNode,
        extension: &'a str,
    },
    /// A real prefix of the tag matched a schema node, but that node takes neither a value
    /// nor an extension, so the leftover `remainder` text doesn't belong anywhere (e.g.
    /// "Sensory-event/#" — Sensory-event is a plain leaf tag).
    InvalidExtension {
        node: &'a SchemaNode,
        remainder: &'a str,
    },
    /// No prefix of the tag matched anything in the schema at all.
    NotFound,
    /// The tag carried a namespace prefix (e.g. "xy:Tag") that no schema in the collection
    /// answers to. Only produced by `SchemaCollection::resolve_tag`.
    UnknownNamespace,
}

impl Schema {
    /// Whether this is an 8.3+ schema for attribute-domain purposes: version (or partner
    /// version) >= 8.3.0, or the Properties section defines `elementDomain`.
    pub fn uses_83_props(&self) -> bool {
        if self.properties.contains_key("elementdomain") {
            return true;
        }
        let effective = if !self.with_standard.is_empty() {
            self.with_standard.as_str()
        } else if self.library.is_empty() {
            self.version.as_str()
        } else {
            // Unpartnered library: its own version numbers aren't on the standard timeline.
            return false;
        };
        version_at_least(effective, (8, 3, 0))
    }

    /// (Re)builds `tag_index` from `root_nodes` and records duplicate short tag names.
    /// Call after any structural change to the tag tree.
    pub fn finalize(&mut self) {
        self.tag_index.clear();
        let mut index: HashMap<String, Vec<String>> = HashMap::new();
        for root_node in self.root_nodes.values() {
            Self::build_tag_index(root_node, "", &mut index);
        }
        let dups: Vec<DuplicateName> = index
            .iter()
            .filter(|(_, paths)| paths.len() > 1)
            .map(|(name, paths)| {
                let origins: std::collections::HashSet<bool> = paths
                    .iter()
                    .filter_map(|p| self.lookup_full_path(&index, p))
                    .map(|n| n.attributes.contains_key("inLibrary"))
                    .collect();
                DuplicateName {
                    name: name.clone(),
                    spans_library: origins.len() > 1,
                }
            })
            .collect();
        if !dups.is_empty() {
            self.duplicate_names
                .entry(sections::TAGS.to_string())
                .or_default()
                .extend(dups);
        }
        self.tag_index = index;
    }

    /// Like `find_by_full_path` but usable while `tag_index` is being rebuilt.
    fn lookup_full_path<'a>(
        &'a self,
        _index: &HashMap<String, Vec<String>>,
        full_path: &str,
    ) -> Option<&'a SchemaNode> {
        let parts: Vec<&str> = full_path.split('/').collect();
        let mut node = self.root_nodes.get(&parts.first()?.to_lowercase())?;
        for part in &parts[1..] {
            node = node.children.get(&part.to_lowercase())?;
        }
        Some(node)
    }

    pub fn has_duplicates(&self) -> bool {
        self.duplicate_names.values().any(|v| !v.is_empty())
    }

    /// Recursively index every schema-defined tag by its lowercase short name -> full path
    /// (excluding "#" placeholders).
    fn build_tag_index(
        node: &SchemaNode,
        current_path: &str,
        index: &mut HashMap<String, Vec<String>>,
    ) {
        let full_path = if current_path.is_empty() {
            node.name.clone()
        } else {
            format!("{}/{}", current_path, node.name)
        };
        index
            .entry(node.name.to_lowercase())
            .or_default()
            .push(full_path.clone());

        for (key, child) in &node.children {
            if key == "#" {
                continue;
            }
            Self::build_tag_index(child, &full_path, index);
        }
    }

    /// Whether any schema tag anywhere in the tree has this (lowercase-compared) short name.
    pub fn has_tag_named(&self, name: &str) -> bool {
        self.tag_index.contains_key(&name.to_lowercase())
    }

    /// The full path(s) recorded for a short tag name.
    pub fn paths_for_tag(&self, name: &str) -> &[String] {
        self.tag_index
            .get(&name.to_lowercase())
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Walks `full_path` (a "/"-joined path from the tree root, original case, as recorded
    /// in `tag_index`) down through `root_nodes`/`children` to find the node it names.
    pub(crate) fn find_by_full_path(&self, full_path: &str) -> Option<&SchemaNode> {
        let parts: Vec<&str> = full_path.split('/').collect();
        let mut node = self.root_nodes.get(&parts.first()?.to_lowercase())?;
        for part in &parts[1..] {
            node = node.children.get(&part.to_lowercase())?;
        }
        Some(node)
    }

    /// Looks up a tag entry by short name (first indexed path wins).
    pub fn tag_entry_by_short_name(&self, name: &str) -> Option<&SchemaNode> {
        self.paths_for_tag(name)
            .first()
            .and_then(|p| self.find_by_full_path(p))
    }

    /// Resolve a user-supplied tag string (e.g. "Acceleration/3 m-per-s^2") against the
    /// schema tree.
    ///
    /// HED tags are conventionally written as just a short name, or any trailing suffix of
    /// a tag's full ancestry path — not necessarily the full root-to-leaf path. This finds
    /// the *longest* trailing run of segments (starting from the end of `tag`) that matches
    /// some schema node's full path (exactly, or as a suffix of it), then treats anything
    /// left over as a value/extension. Any empty segment (leading/trailing/consecutive
    /// slash) fails the whole resolution immediately -> `NotFound`.
    pub fn resolve_tag<'a>(&'a self, tag: &'a str) -> TagResolution<'a> {
        let segments: Vec<&str> = tag.split('/').collect();
        if segments.is_empty() || segments.iter().any(|s| s.is_empty()) {
            return TagResolution::NotFound;
        }

        let mut matched_len = segments.len();
        while matched_len > 0 {
            let last_short = segments[matched_len - 1].to_lowercase();
            if let Some(candidates) = self.tag_index.get(&last_short) {
                let prefix_lower = segments[..matched_len].join("/").to_lowercase();
                let found = candidates.iter().find_map(|full_path| {
                    let full_lower = full_path.to_lowercase();
                    let is_match = full_lower == prefix_lower
                        || full_lower.ends_with(&format!("/{}", prefix_lower));
                    is_match
                        .then(|| self.find_by_full_path(full_path))
                        .flatten()
                });

                if let Some(node) = found {
                    if matched_len == segments.len() {
                        return TagResolution::Full(node);
                    }

                    let remainder_start = segments[..matched_len]
                        .iter()
                        .map(|s| s.len() + 1)
                        .sum::<usize>();
                    let remainder = &tag[remainder_start..];

                    if node.children.contains_key("#") {
                        return TagResolution::Value {
                            node,
                            value: remainder,
                        };
                    }
                    if node.has_attribute("extensionAllowed") {
                        return TagResolution::Extension {
                            node,
                            extension: remainder,
                        };
                    }
                    return TagResolution::InvalidExtension { node, remainder };
                }
            }
            matched_len -= 1;
        }

        TagResolution::NotFound
    }
}

/// Parses "X.Y.Z" (ignoring any comma-joined extras after a merge) and compares to `min`.
pub(crate) fn version_at_least(version: &str, min: (u64, u64, u64)) -> bool {
    let first = version.split(',').next().unwrap_or("");
    let mut parts = first.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let v = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    v >= min
}
