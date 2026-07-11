//! The loaded HED vocabulary and everything that produces it.
//!
//! [`Schema`] is a single loaded schema (tag tree plus unit/value-class sections);
//! [`SchemaCollection`] groups schemas by namespace prefix and is what the validators
//! resolve tags against. Schemas are produced by three loaders:
//!
//! - the embedded 8.4.0 hedjson ([`Schema::load_standard`]),
//! - a mediawiki schema source ([`load_wiki_string`]), which also underpins schema-source
//!   compliance checking via [`check_compliance`], and
//! - [`load_schema_version`], the multi-schema/library merge-group loader (parses version
//!   specs, resolves `withStandard` partners, and builds a [`SchemaCollection`]).

mod collection;
mod compliance;
mod json_parser;
mod loader;
mod model;
mod wiki_parser;

pub use collection::SchemaCollection;
pub use compliance::check_compliance;
pub use loader::load_schema_version;
pub use model::{
    DuplicateName, Schema, SchemaEntry, SchemaError, SchemaNode, TagResolution, UnitClass,
    UnitEntry, UnitModifier, ValueClass, sections,
};
pub use wiki_parser::{SchemaLoadError, load_wiki_string};
