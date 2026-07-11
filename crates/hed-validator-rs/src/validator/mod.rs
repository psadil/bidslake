//! The validation engine. [`Validator`] is the entry point; each submodule is one focused
//! family of checks, and [`Validator::validate`] orchestrates them over a parsed string:
//!
//! - [`tag_validator`] — per-tag schema resolution and character/value/unit-class rules.
//! - [`char_validator`] — character-class and numeric/date-time value grammars.
//! - [`unit_validator`] — unit parsing with SI-prefix and pluralization rules.
//! - [`group_validator`] — reserved-tag group rules, tag uniqueness, empty groups.
//! - [`duplicate_checker`] — order-independent repeated-tag/group detection.
//! - [`def_validator`] — gathering and validating `Definition`/`Def`/`Def-expand`.
//! - [`onset_validator`] — cross-row `Onset`/`Offset`/`Inset` pairing state.
//! - [`sidecar_validator`] — sidecar JSON shape and `{column}` splice references.
//! - [`tabular_validator`] — assembling per-row HED strings from a tabular file plus a
//!   sidecar, then running the full and cross-row checks over them.

use crate::errors::HedError;
use crate::models::HedString;
use crate::reserved::ReservedTags;
use crate::schema::SchemaCollection;

pub mod char_validator;
pub mod def_validator;
pub mod duplicate_checker;
pub mod group_validator;
pub mod onset_validator;
pub mod sidecar_validator;
pub mod tabular_validator;
pub mod tag_validator;
pub mod unit_validator;

pub use def_validator::{DefinitionEntry, DefinitionMap, DefinitionSite, gather_definitions};

/// Whether (and how) a bare `#` placeholder is licensed in the text currently being
/// validated. Sidecar value-column templates license it (subject to a separate
/// exactly-one-`#` sidecar-level check); plain strings and categorical sidecar values never
/// do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderMode {
    Forbidden,
    ForbiddenStrict,
    ValueColumn,
}

pub struct ValidationContext<'a> {
    pub placeholder_mode: PlaceholderMode,
    pub definition_site: DefinitionSite,
    pub definitions: &'a DefinitionMap,
}

impl<'a> ValidationContext<'a> {
    pub fn new(
        placeholder_mode: PlaceholderMode,
        definition_site: DefinitionSite,
        definitions: &'a DefinitionMap,
    ) -> Self {
        Self {
            placeholder_mode,
            definition_site,
            definitions,
        }
    }
}

pub struct Validator<'a> {
    pub schemas: &'a SchemaCollection,
    pub reserved: ReservedTags,
}

impl<'a> Validator<'a> {
    pub fn new(schemas: &'a SchemaCollection) -> Self {
        Self {
            schemas,
            reserved: ReservedTags::load_embedded(),
        }
    }

    pub fn validate(&self, hed_string: &HedString, ctx: &ValidationContext) -> Vec<HedError> {
        let mut errors = Vec::new();
        let nodes = &hed_string.nodes;

        group_validator::check_reserved_tags(&self.reserved, nodes, &mut errors);
        group_validator::check_tag_group_attribute(self.schemas, nodes, &mut errors);
        group_validator::check_unique_tags(self.schemas, nodes, &mut errors);
        group_validator::check_empty_groups(nodes, &mut errors);
        duplicate_checker::check_duplicates(nodes, &mut errors);
        let allow_placeholder = matches!(ctx.placeholder_mode, PlaceholderMode::ValueColumn);
        def_validator::validate_def_usage(
            self.schemas,
            nodes,
            ctx.definitions,
            allow_placeholder,
            &mut errors,
        );
        self.validate_tags(nodes, ctx, &mut errors);

        errors
    }

    fn validate_tags(
        &self,
        nodes: &[crate::models::HedNode],
        ctx: &ValidationContext,
        errors: &mut Vec<HedError>,
    ) {
        for node in nodes {
            match node {
                crate::models::HedNode::Tag(t) => {
                    tag_validator::validate_tag(self.schemas, t, ctx, errors);
                }
                crate::models::HedNode::Group(g) => {
                    if def_validator::group_is_definition(g) {
                        // Contents (incl. any placeholder) are fully handled by
                        // def_validator's own structural checks.
                        continue;
                    }
                    self.validate_tags(&g.children, ctx, errors);
                }
            }
        }
    }
}
