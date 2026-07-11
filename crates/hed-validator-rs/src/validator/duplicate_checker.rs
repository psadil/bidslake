use crate::errors::{HedError, codes};
use crate::models::HedNode;
use std::collections::BTreeSet;

/// A structural identity for a tag/group used for order-independent duplicate detection:
/// two groups are "the same" if their child-identity sets match, regardless of child order
/// (mirroring hed-python's `hash(frozenset(child_hashes))` approach) and regardless of
/// whether a child is itself repeated within that group (multiplicity isn't tracked, just
/// like a Python frozenset).
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
enum NodeId {
    Tag(String),
    Group(BTreeSet<NodeId>),
}

fn identity(node: &HedNode) -> NodeId {
    match node {
        HedNode::Tag(t) => NodeId::Tag(t.canonical()),
        HedNode::Group(g) => NodeId::Group(g.children.iter().map(identity).collect()),
    }
}

/// Recursively flags sibling tags/groups that are structurally identical to another sibling
/// in the same group (including the implicit top-level list).
pub fn check_duplicates(nodes: &[HedNode], errors: &mut Vec<HedError>) {
    let mut seen = BTreeSet::new();
    for node in nodes {
        if !seen.insert(identity(node)) {
            errors.push(HedError::error(
                codes::TAG_EXPRESSION_REPEATED,
                "tag or group is repeated within the same group",
                None,
            ));
        }
    }

    for node in nodes {
        if let HedNode::Group(g) = node {
            check_duplicates(&g.children, errors);
        }
    }
}
