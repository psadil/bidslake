use crate::errors::{HedError, codes};
use crate::models::{HedGroup, HedNode, HedTag};
use crate::reserved::ReservedTags;
use crate::schema::{SchemaCollection, TagResolution};
use std::collections::HashSet;

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
    matches!(
        group.children.first(),
        Some(HedNode::Tag(t)) if short_base(t) == "def-expand"
    )
}

fn count_def_like_children(group: &HedGroup) -> usize {
    group
        .children
        .iter()
        .filter(|c| match c {
            HedNode::Tag(t) => is_def_tag(t),
            HedNode::Group(g) => is_def_expand_group(g),
        })
        .count()
}

/// Reserved-tag co-occurrence/shape rules (Onset/Offset/Inset/Duration/Delay/Event-context),
/// ported from hed-python's `reserved_checker.py`. Every violation this function finds is
/// reported as `TAG_GROUP_ERROR`, except "a tag requiring a Def has none/too many", which
/// hed-python reports as `TEMPORAL_TAG_ERROR` (`ONSET_NO_DEF_TAG_FOUND`'s `actual_code`) —
/// both choices are cross-accepted via `alt_codes` in the conformance suite regardless.
pub fn check_reserved_tags(reserved: &ReservedTags, nodes: &[HedNode], errors: &mut Vec<HedError>) {
    walk(reserved, nodes, 0, errors);
}

fn walk(reserved: &ReservedTags, nodes: &[HedNode], depth: usize, errors: &mut Vec<HedError>) {
    if depth == 0 {
        for node in nodes {
            if let HedNode::Tag(t) = node
                && reserved.get(&short_base(t)).is_some()
            {
                errors.push(HedError::error(
                    codes::TAG_GROUP_ERROR,
                    "reserved tag must appear inside a tag group",
                    Some(t.tag.clone()),
                ));
            }
        }
    }

    for node in nodes {
        if let HedNode::Group(g) = node {
            let reserved_tags: Vec<&HedTag> = g
                .children
                .iter()
                .filter_map(|c| match c {
                    HedNode::Tag(t) if reserved.get(&short_base(t)).is_some() => Some(t),
                    _ => None,
                })
                .collect();

            if !reserved_tags.is_empty() {
                if depth != 0 {
                    errors.push(HedError::error(
                        codes::TAG_GROUP_ERROR,
                        "reserved tag group must be at the top level of the string",
                        None,
                    ));
                } else {
                    check_group_rules(reserved, g, &reserved_tags, errors);
                }
            }

            walk(reserved, &g.children, depth + 1, errors);
        }
    }
}

fn check_group_rules(
    reserved: &ReservedTags,
    group: &HedGroup,
    reserved_tags: &[&HedTag],
    errors: &mut Vec<HedError>,
) {
    let mut seen_names = HashSet::new();
    for t in reserved_tags {
        if !seen_names.insert(short_base(t)) {
            errors.push(HedError::error(
                codes::TAG_GROUP_ERROR,
                "reserved tag repeated in the same group",
                None,
            ));
            return;
        }
    }

    for t in reserved_tags {
        let info = reserved.get(&short_base(t)).unwrap();
        let incompatible = reserved_tags.iter().any(|other| {
            short_base(other) != short_base(t)
                && !info
                    .other_allowed_non_def_tags
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(&short_base(other)))
        });
        if incompatible {
            errors.push(HedError::error(
                codes::TAG_GROUP_ERROR,
                "tags are not allowed together in the same group",
                None,
            ));
            return;
        }
    }

    let def_count = count_def_like_children(group);
    let requires_def_tags: Vec<&&HedTag> = reserved_tags
        .iter()
        .filter(|t| reserved.get(&short_base(t)).unwrap().requires_def)
        .collect();

    if requires_def_tags.len() > 1 {
        errors.push(HedError::error(
            codes::TAG_GROUP_ERROR,
            "more than one reserved tag requires a def in this group",
            None,
        ));
        return;
    }
    if requires_def_tags.len() == 1 && def_count != 1 {
        errors.push(HedError::error(
            codes::TEMPORAL_TAG_ERROR,
            "tag requires exactly one Def or Def-expand group",
            None,
        ));
        return;
    }
    if requires_def_tags.is_empty() && def_count != 0 {
        errors.push(HedError::error(
            codes::TAG_GROUP_ERROR,
            "a Def is not allowed with these reserved tags",
            None,
        ));
        return;
    }

    for child in &group.children {
        if let HedNode::Tag(t) = child
            && reserved.get(&short_base(t)).is_none()
            && !is_def_tag(t)
        {
            errors.push(HedError::error(
                codes::TAG_GROUP_ERROR,
                "tag is not allowed alongside these reserved tags",
                Some(t.tag.clone()),
            ));
            return;
        }
    }

    let non_def_subgroups = group
        .children
        .iter()
        .filter(|c| matches!(c, HedNode::Group(g) if !is_def_expand_group(g)))
        .count() as i64;

    let mut max_allowed = i64::MAX;
    let mut min_allowed = i64::MIN;
    for t in reserved_tags {
        let info = reserved.get(&short_base(t)).unwrap();
        if info.min_non_def_subgroups > min_allowed {
            min_allowed = info.min_non_def_subgroups;
        }
        if let Some(m) = info.max_non_def_subgroups
            && m < max_allowed
        {
            max_allowed = m;
        }
    }
    if max_allowed < min_allowed && reserved_tags.len() > 1 {
        min_allowed = max_allowed;
    }

    if max_allowed != i64::MAX {
        if non_def_subgroups > max_allowed {
            errors.push(HedError::error(
                codes::TAG_GROUP_ERROR,
                "too many subgroups for the reserved tag(s) present",
                None,
            ));
            return;
        }
        if min_allowed > non_def_subgroups {
            errors.push(HedError::error(
                codes::TAG_GROUP_ERROR,
                "too few subgroups for the reserved tag(s) present",
                None,
            ));
        }
    }
}

/// Some tags (e.g. `Def-expand`, which isn't in the reserved-tags table but carries the
/// schema `tagGroup` attribute) must always appear inside some tag group, without
/// necessarily being top-level. A bare occurrence at the top level of the string (not
/// wrapped in any group at all) is invalid — TAG_GROUP_ERROR.
pub fn check_tag_group_attribute(
    schemas: &SchemaCollection,
    nodes: &[HedNode],
    errors: &mut Vec<HedError>,
) {
    for node in nodes {
        if let HedNode::Tag(t) = node {
            let has_tag_group_attr = match schemas.resolve_tag(&t.tag) {
                TagResolution::Full(n) => n.has_attribute("tagGroup"),
                TagResolution::Value { node, .. } => node.has_attribute("tagGroup"),
                _ => false,
            };
            if has_tag_group_attr {
                errors.push(HedError::error(
                    codes::TAG_GROUP_ERROR,
                    "tag must appear inside a tag group",
                    Some(t.tag.clone()),
                ));
            }
        }
    }
}

/// Any schema tag with the `unique` attribute may appear at most once anywhere in the whole
/// string (not just within one group) — TAG_NOT_UNIQUE.
pub fn check_unique_tags(
    schemas: &SchemaCollection,
    nodes: &[HedNode],
    errors: &mut Vec<HedError>,
) {
    let mut seen = HashSet::new();
    walk_unique(schemas, nodes, &mut seen, errors);
}

fn walk_unique(
    schemas: &SchemaCollection,
    nodes: &[HedNode],
    seen: &mut HashSet<String>,
    errors: &mut Vec<HedError>,
) {
    for node in nodes {
        match node {
            HedNode::Tag(t) => {
                let resolved_node = match schemas.resolve_tag(&t.tag) {
                    TagResolution::Full(n) => Some(n),
                    TagResolution::Value { node, .. } => Some(node),
                    TagResolution::Extension { node, .. } => Some(node),
                    TagResolution::InvalidExtension { node, .. } => Some(node),
                    TagResolution::NotFound | TagResolution::UnknownNamespace => None,
                };
                if let Some(n) = resolved_node
                    && n.has_attribute("unique")
                    && !seen.insert(t.canonical())
                {
                    errors.push(HedError::error(
                        codes::TAG_NOT_UNIQUE,
                        "tag may only appear once in the string",
                        Some(t.tag.clone()),
                    ));
                }
            }
            HedNode::Group(g) => walk_unique(schemas, &g.children, seen, errors),
        }
    }
}

/// Empty groups (`()`, or a group left empty after trimming whitespace) are TAG_EMPTY.
pub fn check_empty_groups(nodes: &[HedNode], errors: &mut Vec<HedError>) {
    for node in nodes {
        if let HedNode::Group(g) = node {
            if g.children.is_empty() {
                errors.push(HedError::error(codes::TAG_EMPTY, "group is empty", None));
            }
            check_empty_groups(&g.children, errors);
        }
    }
}
