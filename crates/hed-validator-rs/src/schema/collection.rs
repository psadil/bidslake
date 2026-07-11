use super::model::{Schema, TagResolution};
use super::wiki_parser::SchemaLoadError;
use crate::errors::codes;
use crate::models::split_namespace;
use std::collections::HashMap;

/// A set of loaded schemas keyed by namespace prefix ("" for the default/unprefixed schema,
/// "sc:" etc. for library nicknames). Tag text carries its namespace inline
/// ("sc:Sleep-modulator"); resolution strips the prefix and dispatches to the matching
/// schema — an unmatched prefix is itself a validation error
/// (TAG_NAMESPACE_PREFIX_INVALID).
#[derive(Debug, Clone, Default)]
pub struct SchemaCollection {
    schemas: HashMap<String, Schema>,
}

impl SchemaCollection {
    /// Wraps one schema as the collection's unprefixed ("") member — the common case.
    pub fn single(mut schema: Schema) -> Self {
        schema.namespace = String::new();
        let mut schemas = HashMap::new();
        schemas.insert(String::new(), schema);
        SchemaCollection { schemas }
    }

    /// Builds a collection from schemas that already carry their namespace. Two schemas
    /// sharing a prefix cannot coexist.
    pub fn from_schemas(list: Vec<Schema>) -> Result<Self, SchemaLoadError> {
        let mut schemas = HashMap::new();
        for schema in list {
            if schemas.contains_key(&schema.namespace) {
                return Err(SchemaLoadError::single(
                    codes::SCHEMA_LOAD_FAILED,
                    "Multiple schema share the same tag name_prefix so schema cannot be loaded.",
                ));
            }
            schemas.insert(schema.namespace.clone(), schema);
        }
        Ok(SchemaCollection { schemas })
    }

    pub fn schema_for(&self, namespace: &str) -> Option<&Schema> {
        self.schemas.get(namespace)
    }

    /// Splits the tag's namespace prefix off and returns the owning schema plus the
    /// remaining (prefix-free) tag text, or `None` when no schema answers to the prefix.
    pub fn schema_for_tag<'a>(&'a self, text: &'a str) -> Option<(&'a Schema, &'a str)> {
        let (namespace, rest) = split_namespace(text);
        self.schemas.get(namespace).map(|s| (s, rest))
    }

    pub fn resolve_tag<'a>(&'a self, text: &'a str) -> TagResolution<'a> {
        match self.schema_for_tag(text) {
            Some((schema, rest)) => schema.resolve_tag(rest),
            None => TagResolution::UnknownNamespace,
        }
    }

    pub fn valid_prefixes(&self) -> Vec<&str> {
        self.schemas.keys().map(|s| s.as_str()).collect()
    }

    pub fn schemas(&self) -> impl Iterator<Item = &Schema> {
        self.schemas.values()
    }
}
