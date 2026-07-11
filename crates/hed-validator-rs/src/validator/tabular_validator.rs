use crate::data::{HedColumnDef, TabularInput};
use crate::errors::HedError;
use crate::models::{HedGroup, HedNode, HedString, HedTag};
use crate::parser::parse_hed_string;
use crate::schema::SchemaCollection;
use crate::validator::onset_validator::OnsetValidator;
use crate::validator::{
    DefinitionMap, DefinitionSite, PlaceholderMode, ValidationContext, Validator,
    gather_definitions,
};
use std::collections::{HashMap, HashSet};

fn is_whole_brace_token(s: &str) -> bool {
    s.len() >= 2
        && s.starts_with('{')
        && s.ends_with('}')
        && !s[1..s.len() - 1].contains(['{', '}'])
}

/// Substitutes every literal `#` within each tag's own text (post-parse, so a comma or other
/// delimiter-like character in `value` is never re-tokenized as HED structure — it's just
/// part of that one tag's text) — mirrors `def_validator::substitute_placeholder`.
fn substitute_hash(nodes: Vec<HedNode>, value: &str) -> Vec<HedNode> {
    nodes
        .into_iter()
        .map(|n| match n {
            HedNode::Tag(t) => HedNode::Tag(HedTag::new(t.tag.replace('#', value))),
            HedNode::Group(g) => HedNode::Group(HedGroup::new(substitute_hash(g.children, value))),
        })
        .collect()
}

/// Every column name referenced via `{col}` anywhere in `s` (a raw, unparsed template or
/// categorical-value string).
fn extract_refs(s: &str, out: &mut HashSet<String>) {
    let mut idx = 0;
    while let Some(open) = s[idx..].find('{') {
        let open = idx + open;
        let Some(close) = s[open + 1..].find('}') else {
            break;
        };
        out.insert(s[open + 1..open + 1 + close].to_string());
        idx = open + 1 + close + 1;
    }
}

/// Column names referenced via `{col}` *anywhere in the sidecar* (statically, across every
/// column's every value — not just this row). Such a column is purely a splice source and
/// never independently contributes its own top-level row content, even in rows where nothing
/// happens to reference it this time — otherwise its content would sometimes be validated
/// standalone under a value/character class meant only for its role as a spliced-in
/// fragment (e.g. a Value column's cell text may contain characters, like a bare comma,
/// that are fine once embedded via splice but invalid as a freestanding tag value).
pub fn statically_referenced_columns(columns: &HashMap<String, HedColumnDef>) -> HashSet<String> {
    let mut refs = HashSet::new();
    for def in columns.values() {
        match def {
            HedColumnDef::Value(s) => extract_refs(s, &mut refs),
            HedColumnDef::Categorical(map) => {
                for s in map.values() {
                    extract_refs(s, &mut refs);
                }
            }
        }
    }
    refs
}

/// Walks a column's own parsed nodes, replacing any tag that is a whole `{name}` splice
/// token with that reference's resolved nodes spliced in place (0..N nodes). A group that
/// becomes empty purely as a result of its one splice reference resolving to nothing is
/// dropped entirely, rather than left behind as an (invalid) empty group.
///
/// Three cases for a `{name}` token: (1) `name` is a real column present in *this* tabular
/// file (or the literal `HED` sink, if this file has a `HED` column) — splice its resolved
/// nodes in, possibly zero if this row's cell is empty/n/a; (2) `name` is a column the
/// sidecar defines but this particular tabular file doesn't have — SIDECAR_KEY_MISSING
/// (the annotation refers to data that isn't there); (3) `name` isn't a recognized column at
/// all — left as a literal tag so the normal validator flags it (CHARACTER_INVALID) instead
/// of silently vanishing.
#[allow(clippy::too_many_arguments)]
fn splice_references(
    nodes: Vec<HedNode>,
    resolved: &HashMap<String, Vec<HedNode>>,
    hed_cell: Option<&[HedNode]>,
    known_columns: &HashSet<String>,
    columns: &HashMap<String, HedColumnDef>,
    errors: &mut Vec<HedError>,
) -> Vec<HedNode> {
    let mut out = Vec::new();
    for node in nodes {
        match node {
            HedNode::Tag(t) if is_whole_brace_token(&t.tag) => {
                let name = &t.tag[1..t.tag.len() - 1];
                if name == "HED" && known_columns.contains("HED") {
                    out.extend(hed_cell.map(|n| n.to_vec()).unwrap_or_default());
                } else if name != "HED" && known_columns.contains(name) {
                    out.extend(resolved.get(name).cloned().unwrap_or_default());
                } else if name == "HED" || columns.contains_key(name) {
                    errors.push(HedError::warning(
                        crate::errors::codes::SIDECAR_KEY_MISSING,
                        &format!(
                            "'{name}' is referenced via {{{name}}} but has no column in this data"
                        ),
                        Some(t.tag.clone()),
                    ));
                } else {
                    out.push(HedNode::Tag(t));
                }
            }
            HedNode::Tag(t) => out.push(HedNode::Tag(t)),
            HedNode::Group(g) => {
                let had_children = !g.children.is_empty();
                let spliced = splice_references(
                    g.children,
                    resolved,
                    hed_cell,
                    known_columns,
                    columns,
                    errors,
                );
                if spliced.is_empty() && had_children {
                    continue; // dropped: existed only to hold a reference that resolved to nothing
                }
                out.push(HedNode::Group(HedGroup::new(spliced)));
            }
        }
    }
    out
}

/// Assembles one row's HED-bearing columns (substituting sidecar Value-column templates,
/// resolving Categorical-column keys, and splicing `{col}` references) into a single
/// combined list of top-level nodes. `onset`/`duration` columns are pure numeric timing
/// metadata, never HED content, and are skipped. Parse errors and SIDECAR_KEY_MISSING
/// warnings encountered along the way are pushed to `errors`.
#[allow(clippy::too_many_arguments)]
fn assemble_row(
    tabular: &TabularInput,
    row: &[String],
    columns: &HashMap<String, HedColumnDef>,
    known_columns: &HashSet<String>,
    referenced: &HashSet<String>,
    errors: &mut Vec<HedError>,
) -> Vec<HedNode> {
    // Pass 1: resolve each content-bearing column's own (parsed) nodes for this row —
    // sidecar Value-column "#" substitution (post-parse, so embedded commas in the cell
    // value can't be mistaken for HED structure), Categorical-column key lookup, or the
    // raw cell for a plain/"HED" column — but don't yet resolve `{col}` splice references,
    // since a referenced column's own nodes are exactly this pre-splice form.
    let mut resolved: HashMap<String, Vec<HedNode>> = HashMap::new();
    let mut hed_cell: Option<Vec<HedNode>> = None;
    for (col_idx, header) in tabular.headers.iter().enumerate() {
        let Some(cell) = row.get(col_idx) else {
            continue;
        };
        let is_timing_metadata =
            header.eq_ignore_ascii_case("onset") || header.eq_ignore_ascii_case("duration");
        // Plain onset/duration columns are pure numeric timing metadata, not HED content —
        // but a sidecar may still (unusually) give one of them its own HED template, in
        // which case it's only ever meant to be spliced in via `{onset}`/`{duration}`
        // elsewhere (see the exclusion in Pass 2 below), never treated as row timing.
        if is_timing_metadata && !columns.contains_key(header) {
            continue;
        }
        if cell.is_empty() || cell == "n/a" {
            continue;
        }

        let nodes = match columns.get(header) {
            Some(HedColumnDef::Value(template)) => parse_hed_string(template)
                .ok()
                .map(|s| substitute_hash(s.nodes, cell)),
            Some(HedColumnDef::Categorical(map)) => match map.get(cell) {
                Some(s) => match parse_hed_string(s) {
                    Ok(parsed) => Some(parsed.nodes),
                    Err(e) => {
                        errors.push(e);
                        None
                    }
                },
                None => {
                    errors.push(HedError::warning(
                        crate::errors::codes::SIDECAR_KEY_MISSING,
                        &format!("'{cell}' is not a recognized category for column '{header}'"),
                        Some(cell.clone()),
                    ));
                    None
                }
            },
            None if header.eq_ignore_ascii_case("HED") => match parse_hed_string(cell) {
                Ok(parsed) => Some(parsed.nodes),
                Err(e) => {
                    errors.push(e);
                    None
                }
            },
            None => None,
        };

        if header.eq_ignore_ascii_case("HED") {
            hed_cell = nodes.clone();
        }
        if let Some(nodes) = nodes {
            resolved.insert(header.clone(), nodes);
        }
    }

    // Pass 2: splice `{col}` references (at the tree level, so an empty reference cleanly
    // disappears instead of leaving stray empty groups behind), collecting every
    // non-splice-source column's contribution as top-level elements. A column (including
    // "HED") statically referenced elsewhere in the sidecar is excluded here — it's already
    // being spliced in wherever it's referenced.
    let mut row_nodes: Vec<HedNode> = Vec::new();
    for (header, nodes) in &resolved {
        let is_timing_metadata =
            header.eq_ignore_ascii_case("onset") || header.eq_ignore_ascii_case("duration");
        if referenced.contains(header) || is_timing_metadata {
            continue;
        }
        row_nodes.extend(splice_references(
            nodes.clone(),
            &resolved,
            hed_cell.as_deref(),
            known_columns,
            columns,
            errors,
        ));
    }
    row_nodes
}

/// Timeline-anchor tags: unlike `Duration` (which just states a span and stands fine on its
/// own), these inherently reference a moving point in time and are meaningless without an
/// `onset` column establishing a timeline to place them on.
const TIMELINE_KEYS: [&str; 4] = ["onset", "offset", "inset", "delay"];

/// Flags any timeline-anchor tag (Onset/Offset/Inset/Delay) found anywhere in `nodes` — used
/// for a row with no usable `onset` value (no such column at all, or this row's value didn't
/// parse), where there's no timeline for such a tag to relate to.
fn check_banned_without_timeline(nodes: &[HedNode], errors: &mut Vec<HedError>) {
    for node in nodes {
        match node {
            HedNode::Tag(t) => {
                let short = t.segments()[0].to_lowercase();
                if TIMELINE_KEYS.contains(&short.as_str()) {
                    errors.push(HedError::error(
                        crate::errors::codes::TEMPORAL_TAG_ERROR,
                        "temporal tag requires an 'onset' column to establish a timeline",
                        Some(t.tag.clone()),
                    ));
                }
            }
            HedNode::Group(g) => check_banned_without_timeline(&g.children, errors),
        }
    }
}

/// BIDS convention: multiple rows sharing the exact same `onset` value describe the same
/// event and are merged into one combined HED string before validation (so e.g. duplicate
/// tags *across* same-onset rows are caught the same way duplicates within one string are).
const ONSET_TOLERANCE: f64 = 1e-7;

/// Assembles every row (see `assemble_row`), merges rows that share the same `onset` value,
/// then runs full validation on each resulting group plus cross-row Onset/Offset/Inset
/// pairing (in onset order) via `OnsetValidator`.
pub fn validate_tabular(
    schemas: &SchemaCollection,
    tabular: &TabularInput,
    columns: &HashMap<String, HedColumnDef>,
    defs: &DefinitionMap,
) -> Vec<HedError> {
    let mut errors = Vec::new();
    let reserved = crate::reserved::ReservedTags::load_embedded();
    let mut onset_validator = OnsetValidator::new(&reserved);
    let validator = Validator::new(schemas);

    let known_columns: HashSet<String> = tabular
        .headers
        .iter()
        .filter(|h| h.eq_ignore_ascii_case("HED") || columns.contains_key(h.as_str()))
        .cloned()
        .collect();
    let referenced = statically_referenced_columns(columns);

    let onset_idx = tabular
        .headers
        .iter()
        .position(|h| h.eq_ignore_ascii_case("onset"));

    // Assemble every row first, pairing it with its onset value (if any).
    let mut assembled: Vec<(Option<f64>, Vec<HedNode>)> = Vec::new();
    for row in &tabular.rows {
        let onset_cell = onset_idx.and_then(|i| row.get(i));
        let onset = onset_cell.and_then(|s| s.parse::<f64>().ok());
        let nodes = assemble_row(
            tabular,
            row,
            columns,
            &known_columns,
            &referenced,
            &mut errors,
        );
        if !nodes.is_empty() {
            // No "onset" column at all, or this row's onset value doesn't parse (e.g.
            // "n/a") — either way this row has no place on a timeline, so any use of a
            // temporal/timeline tag (Onset/Offset/Inset/Duration/Delay/...) in it is banned
            // outright.
            if onset.is_none() {
                check_banned_without_timeline(&nodes, &mut errors);
            }
            assembled.push((onset, nodes));
        }
    }

    // Rows with a real onset are processed in onset order, merging exact (within-tolerance)
    // ties into one combined group; rows with no onset value are each their own group,
    // processed afterward in original order (no temporal relation to establish).
    let mut with_onset: Vec<(f64, Vec<HedNode>)> = Vec::new();
    let mut without_onset: Vec<Vec<HedNode>> = Vec::new();
    for (onset, nodes) in assembled {
        match onset {
            Some(o) => with_onset.push((o, nodes)),
            None => without_onset.push(nodes),
        }
    }
    with_onset.sort_by(|a, b| a.0.total_cmp(&b.0));

    // Temporal sequence for Onset/Offset/Inset pairing: a top-level group carrying a
    // `Delay/X` tag describes an event at onset+X, not at the row's own time (hed-python's
    // `split_delay_tags`) — split such groups out to their effective time before ordering.
    let mut temporal: Vec<(f64, Vec<HedNode>)> = Vec::new();
    for (onset, nodes) in &with_onset {
        let mut undelayed: Vec<HedNode> = Vec::new();
        for node in nodes {
            match delay_amount(node) {
                Some(delay) => temporal.push((onset + delay, vec![node.clone()])),
                None => undelayed.push(node.clone()),
            }
        }
        if !undelayed.is_empty() {
            temporal.push((*onset, undelayed));
        }
    }
    temporal.sort_by(|a, b| a.0.total_cmp(&b.0));
    let temporal_groups = merge_by_onset(temporal);
    for nodes in temporal_groups {
        errors.extend(onset_validator.validate_temporal_relations(&nodes));
    }

    // Full validation runs on the unshifted same-onset merges (group-structure rules don't
    // depend on the delay shift), plus the no-onset rows.
    let mut groups = merge_by_onset(with_onset);
    groups.extend(without_onset);

    for nodes in groups {
        if nodes.is_empty() {
            continue;
        }
        let hed_string = HedString::new(nodes);

        let mut row_defs = defs.clone();
        gather_definitions(
            schemas,
            &hed_string.nodes,
            DefinitionSite::PlainString,
            &mut row_defs,
            &mut errors,
        );

        let ctx = ValidationContext::new(
            PlaceholderMode::Forbidden,
            DefinitionSite::PlainString,
            &row_defs,
        );
        errors.extend(validator.validate(&hed_string, &ctx));
    }

    errors
}

/// Merges an onset-sorted sequence, combining entries whose onsets agree within tolerance.
fn merge_by_onset(sorted: Vec<(f64, Vec<HedNode>)>) -> Vec<Vec<HedNode>> {
    let mut groups: Vec<Vec<HedNode>> = Vec::new();
    let mut group_onsets: Vec<f64> = Vec::new();
    for (onset, nodes) in sorted {
        match group_onsets.last().copied() {
            Some(last) if (onset - last).abs() <= ONSET_TOLERANCE => {
                groups.last_mut().unwrap().extend(nodes);
            }
            _ => {
                groups.push(nodes);
                group_onsets.push(onset);
            }
        }
    }
    groups
}

/// If `node` is a top-level group carrying a `Delay/X` tag, returns X (in seconds; the
/// conformance fixtures always express delays in seconds).
fn delay_amount(node: &HedNode) -> Option<f64> {
    let HedNode::Group(group) = node else {
        return None;
    };
    group.children.iter().find_map(|child| {
        let HedNode::Tag(t) = child else { return None };
        let stripped = t.without_namespace();
        let (root, value) = stripped.split_once('/')?;
        if !root.eq_ignore_ascii_case("delay") {
            return None;
        }
        let (amount, _unit) = crate::validator::unit_validator::split_value_and_unit(value);
        amount.parse::<f64>().ok()
    })
}
