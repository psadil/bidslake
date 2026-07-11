//! The parsed HED-string tree ([`HedTag`], [`HedGroup`], [`HedNode`], [`HedString`]) and
//! the namespace-prefix helpers ([`split_namespace`], [`HedTag::without_namespace`]) that
//! the validators walk.

#[derive(Debug, PartialEq, Clone)]
pub struct HedTag {
    pub tag: String,
}

/// Splits raw tag text into its schema-namespace prefix and the remainder. The prefix is
/// everything up to and including the first `:` — but only when that colon appears before
/// the first `/` (a colon later in the text is part of a value, e.g. a clock time). No such
/// leading colon means the empty namespace.
pub fn split_namespace(text: &str) -> (&str, &str) {
    match (text.find(':'), text.find('/')) {
        (Some(colon), slash) if slash.is_none_or(|s| colon < s) => {
            (&text[..colon + 1], &text[colon + 1..])
        }
        _ => ("", text),
    }
}

impl HedTag {
    pub fn new(tag: String) -> Self {
        Self { tag }
    }

    pub fn segments(&self) -> Vec<&str> {
        self.tag.split('/').collect()
    }

    /// Canonical form used for order-independent duplicate-tag comparison. Keeps any
    /// namespace prefix ("ts:Red" is not the same tag as "Red").
    pub fn canonical(&self) -> String {
        self.tag.to_lowercase()
    }

    /// The schema-namespace prefix of this tag ("" or e.g. "sc:").
    pub fn namespace(&self) -> &str {
        split_namespace(&self.tag).0
    }

    /// The tag text with any schema-namespace prefix removed.
    pub fn without_namespace(&self) -> &str {
        split_namespace(&self.tag).1
    }
}

impl std::fmt::Display for HedTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.tag)
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct HedGroup {
    pub children: Vec<HedNode>,
}

impl HedGroup {
    pub fn new(children: Vec<HedNode>) -> Self {
        Self { children }
    }
}

impl std::fmt::Display for HedGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "(")?;
        for (i, child) in self.children.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}", child)?;
        }
        write!(f, ")")
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum HedNode {
    Tag(HedTag),
    Group(HedGroup),
}

impl std::fmt::Display for HedNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HedNode::Tag(t) => write!(f, "{}", t),
            HedNode::Group(g) => write!(f, "{}", g),
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct HedString {
    pub nodes: Vec<HedNode>,
}

impl HedString {
    pub fn new(nodes: Vec<HedNode>) -> Self {
        Self { nodes }
    }
}

impl std::fmt::Display for HedString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, child) in self.nodes.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}", child)?;
        }
        Ok(())
    }
}
