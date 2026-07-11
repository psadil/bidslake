use crate::errors::{HedError, codes};
use crate::models::{HedGroup, HedNode, HedTag};
use crate::reserved::ReservedTags;
use std::collections::{HashMap, HashSet};

fn short_base(tag: &HedTag) -> String {
    tag.without_namespace()
        .split('/')
        .next()
        .unwrap_or("")
        .to_lowercase()
}

fn is_def_tag(tag: &HedTag) -> bool {
    short_base(tag) == "def"
}

fn is_def_expand_group(group: &HedGroup) -> bool {
    matches!(group.children.first(), Some(HedNode::Tag(t)) if short_base(t) == "def-expand")
}

/// The def name (and any attached value) a Def/Def-expand tag references — e.g. "MyColor"
/// for `Def/MyColor`, "Acc/4.5" for `Def/Acc/4.5`.
fn def_extension(tag: &HedTag) -> String {
    let stripped = tag.without_namespace();
    let segments: Vec<&str> = stripped.split('/').collect();
    segments[1..].join("/")
}

fn find_def_extension(group: &HedGroup) -> Option<String> {
    group.children.iter().find_map(|c| match c {
        HedNode::Tag(t) if is_def_tag(t) => Some(def_extension(t)),
        HedNode::Group(g) if is_def_expand_group(g) => {
            if let Some(HedNode::Tag(t)) = g.children.first() {
                Some(def_extension(t))
            } else {
                None
            }
        }
        _ => None,
    })
}

/// Validates Onset/Offset/Inset pairing *across rows* of a tabular file: an Onset opens a
/// timeline for its def name, an Offset closes it, an Inset requires one already open. One
/// instance's state persists across successive calls to `validate_temporal_relations` for
/// each row (in row order), mirroring hed-python's `OnsetValidator`.
pub struct OnsetValidator<'a> {
    reserved: &'a ReservedTags,
    open_onsets: HashMap<String, String>,
}

impl<'a> OnsetValidator<'a> {
    pub fn new(reserved: &'a ReservedTags) -> Self {
        Self {
            reserved,
            open_onsets: HashMap::new(),
        }
    }

    /// Checks the temporal anchors (Onset/Offset/Inset) in a single row's HED string against
    /// the timeline state accumulated from prior rows.
    pub fn validate_temporal_relations(&mut self, nodes: &[HedNode]) -> Vec<HedError> {
        let mut errors = Vec::new();
        let mut used_this_row: HashSet<String> = HashSet::new();

        for node in nodes {
            let HedNode::Group(group) = node else {
                continue;
            };

            let temporal_tag = group.children.iter().find_map(|c| match c {
                HedNode::Tag(t)
                    if self
                        .reserved
                        .get(&short_base(t))
                        .is_some_and(|i| i.requires_def) =>
                {
                    Some(t)
                }
                _ => None,
            });
            let Some(temporal_tag) = temporal_tag else {
                continue;
            };

            let Some(def_name) = find_def_extension(group) else {
                continue;
            };
            let key = def_name.to_lowercase();

            if !used_this_row.insert(key.clone()) {
                errors.push(HedError::error(
                    codes::TEMPORAL_TAG_ERROR,
                    "the same definition is used by more than one temporal tag in this row",
                    Some(def_name.clone()),
                ));
                continue;
            }

            let is_onset = short_base(temporal_tag) == "onset";
            let is_offset = short_base(temporal_tag) == "offset";

            if is_onset {
                self.open_onsets.insert(key, def_name);
            } else if !self.open_onsets.contains_key(&key) {
                let which = if is_offset { "Offset" } else { "Inset" };
                errors.push(HedError::error(
                    codes::TEMPORAL_TAG_ERROR,
                    &format!(
                        "{} for '{}' appears with no matching Onset",
                        which, def_name
                    ),
                    Some(def_name.clone()),
                ));
            } else if is_offset {
                self.open_onsets.remove(&key);
            }
        }

        errors
    }
}
