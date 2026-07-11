//! The reserved-tag rule table (Onset/Offset/Inset/Duration/Delay/Event-context), loaded
//! from the embedded `data/reserved_tags.json` and consulted by the group validator.

use serde::Deserialize;
use std::collections::HashMap;

/// Co-occurrence/shape rules for a HED "reserved" tag (Onset, Offset, Inset, Duration, Delay,
/// Event-context), ported from hed-python's `data/reservedTags.json` /
/// `reserved_checker.py`. `Definition` is intentionally not modeled here — its structural
/// rules are handled entirely by the definition validator.
#[derive(Debug, Clone, Deserialize)]
pub struct ReservedTagInfo {
    #[serde(rename = "maxNonDefSubgroups")]
    pub max_non_def_subgroups: Option<i64>,
    #[serde(rename = "minNonDefSubgroups")]
    pub min_non_def_subgroups: i64,
    #[serde(rename = "requiresDef")]
    pub requires_def: bool,
    #[serde(rename = "otherAllowedNonDefTags")]
    pub other_allowed_non_def_tags: Vec<String>,
}

pub struct ReservedTags(HashMap<String, ReservedTagInfo>);

impl ReservedTags {
    pub fn load_embedded() -> Self {
        let json = include_str!("data/reserved_tags.json");
        let raw: HashMap<String, ReservedTagInfo> =
            serde_json::from_str(json).expect("embedded reserved_tags.json must parse");
        let lower = raw
            .into_iter()
            .map(|(k, v)| (k.to_lowercase(), v))
            .collect();
        ReservedTags(lower)
    }

    pub fn get(&self, short_base_tag: &str) -> Option<&ReservedTagInfo> {
        self.0.get(&short_base_tag.to_lowercase())
    }
}
