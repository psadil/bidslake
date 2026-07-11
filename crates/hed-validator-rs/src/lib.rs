//! A Rust implementation of the [HED](https://www.hedtags.org/) (Hierarchical Event
//! Descriptors) validator, targeting behavioral parity with the reference
//! [`hed-python`](https://github.com/hed-standard/hed-python) implementation on the areas
//! covered by the shared [`hed-tests`](https://github.com/hed-standard/hed-tests)
//! conformance suite. When a validation rule is ambiguous, `hed-python`'s behavior is the
//! source of truth.
//!
//! # Architecture
//!
//! The crate is organized around three layers — the parsed **models**, the **schema**
//! (the vocabulary being validated against), and the **validators** that check the former
//! against the latter.
//!
//! ## Models
//!
//! - [`models`] — the parsed HED-string tree ([`HedTag`](models::HedTag),
//!   [`HedGroup`](models::HedGroup), [`HedNode`](models::HedNode),
//!   [`HedString`](models::HedString)) plus namespace-prefix helpers.
//! - [`parser`] — a hand-written recursive-descent parser producing a
//!   [`HedString`](models::HedString); the authoritative source of the structural error
//!   codes `COMMA_MISSING` and `PARENTHESES_MISMATCH`.
//! - [`errors`] — the [`HedError`](errors::HedError) type and the
//!   [`codes`](errors::codes) module of error-code string constants (kept as strings to
//!   match the JSON conformance fixtures directly).
//! - [`data`] — sidecar/tabular input types ([`Sidecar`](data::Sidecar),
//!   [`TabularInput`](data::TabularInput)) and their JSON parsing.
//! - [`reserved`] — the reserved-tag rule table (Onset/Offset/Inset/Duration/Delay/
//!   Event-context), loaded from an embedded resource.
//!
//! ## Schema
//!
//! The [`schema`] module owns the loaded vocabulary. [`Schema`](schema::Schema) holds the
//! tag tree and the unit/value-class sections; [`SchemaCollection`](schema::SchemaCollection)
//! groups several schemas by namespace prefix (`""`, `"sc:"`, …) and dispatches tag
//! resolution by prefix. Schemas load from the embedded 8.4.0 JSON, from a mediawiki
//! source (also used for schema-source compliance checking), or via
//! [`load_schema_version`](schema::load_schema_version) for multi-schema/library merge
//! groups.
//!
//! ## Validators
//!
//! [`Validator`](validator::Validator) is the entry point: it runs the whole-string checks
//! (reserved tags, uniqueness, duplicates, definitions) and per-tag checks, threading a
//! [`ValidationContext`](validator::ValidationContext) through each. See the [`validator`]
//! module for the individual focused checkers (tags, characters, units, groups,
//! definitions, temporal ordering, sidecars, tabular assembly).
//!
//! # Example
//!
//! ```
//! use hed_validator_rs::parser::parse_hed_string;
//! use hed_validator_rs::schema::{Schema, SchemaCollection};
//! use hed_validator_rs::validator::{
//!     DefinitionMap, DefinitionSite, PlaceholderMode, ValidationContext, Validator,
//! };
//!
//! let schemas = SchemaCollection::single(Schema::load_standard("8.4.0").unwrap());
//! let validator = Validator::new(&schemas);
//!
//! let parsed = parse_hed_string("Event, Action/Think").unwrap();
//! let defs = DefinitionMap::new();
//! let ctx = ValidationContext::new(
//!     PlaceholderMode::Forbidden,
//!     DefinitionSite::PlainString,
//!     &defs,
//! );
//! assert!(validator.validate(&parsed, &ctx).is_empty());
//! ```

pub mod data;
pub mod errors;
pub mod models;
pub mod parser;
pub mod reserved;
pub mod schema;
pub mod validator;
