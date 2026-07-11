//! Hand-written recursive-descent parser turning raw HED text into a [`HedString`]. It is
//! the authoritative source of the structural error codes `COMMA_MISSING` and
//! `PARENTHESES_MISMATCH`.

use crate::errors::{HedError, codes};
use crate::models::{HedGroup, HedNode, HedString, HedTag};

/// A structural parse failure, with the byte offset it was detected at.
#[derive(Debug)]
enum ParseIssue {
    /// A ')' was found with no corresponding open '(' (or a top-level ')' with no group open
    /// at all).
    UnmatchedCloseParen(usize),
    /// A '(' was never closed before the string (or the enclosing group) ended.
    UnmatchedOpenParen(usize),
    /// Two adjacent tags/groups were found with no ',' between them.
    MissingComma(usize),
}

impl ParseIssue {
    fn into_hed_error(self, input: &str) -> HedError {
        let (code, pos) = match self {
            ParseIssue::UnmatchedCloseParen(p) => (codes::PARENTHESES_MISMATCH, p),
            ParseIssue::UnmatchedOpenParen(p) => (codes::PARENTHESES_MISMATCH, p),
            ParseIssue::MissingComma(p) => (codes::COMMA_MISSING, p),
        };
        let snippet_start = input
            .char_indices()
            .rev()
            .find(|(i, _)| *i <= pos.saturating_sub(10))
            .map(|(i, _)| i)
            .unwrap_or(0);
        let snippet_end = (pos + 10).min(input.len());
        let snippet = &input[snippet_start..snippet_end];
        HedError::new(
            "ERROR",
            code,
            "",
            &format!("{} at byte {}: near '{}'", code, pos, snippet),
            Some(snippet.to_string()),
        )
    }
}

fn is_delimiter(c: char) -> bool {
    matches!(c, ',' | '(' | ')')
}

fn skip_whitespace(input: &str, pos: &mut usize) {
    while let Some(c) = input[*pos..].chars().next() {
        if c.is_whitespace() {
            *pos += c.len_utf8();
        } else {
            break;
        }
    }
}

fn take_tag_text<'a>(input: &'a str, pos: &mut usize) -> &'a str {
    let start = *pos;
    while let Some(c) = input[*pos..].chars().next() {
        if is_delimiter(c) {
            break;
        }
        *pos += c.len_utf8();
    }
    &input[start..*pos]
}

/// Parses a comma-separated list of tags/groups until a ')' (not consumed) or end of input.
// `pending_trailing_empty`'s resets are read on a later loop iteration (or not at all, if a
// group closes without ever looping back) depending on control flow the linter can't see
// through in one pass; every reset is load-bearing (see the trailing-comma handling below).
#[allow(unused_assignments)]
fn parse_list(input: &str, pos: &mut usize) -> Result<Vec<HedNode>, ParseIssue> {
    let mut nodes = Vec::new();
    let mut pending_trailing_empty = false;

    loop {
        skip_whitespace(input, pos);
        match input[*pos..].chars().next() {
            None => {
                if pending_trailing_empty {
                    nodes.push(HedNode::Tag(HedTag::new(String::new())));
                }
                break;
            }
            Some(')') => {
                if pending_trailing_empty {
                    nodes.push(HedNode::Tag(HedTag::new(String::new())));
                }
                break;
            }
            Some('(') => {
                pending_trailing_empty = false;
                let open_pos = *pos;
                *pos += 1; // consume '('
                let inner = parse_list(input, pos)?;
                skip_whitespace(input, pos);
                match input[*pos..].chars().next() {
                    Some(')') => *pos += 1,
                    _ => return Err(ParseIssue::UnmatchedOpenParen(open_pos)),
                }
                nodes.push(HedNode::Group(HedGroup::new(inner)));
            }
            Some(_) => {
                pending_trailing_empty = false;
                let text = take_tag_text(input, pos).trim().to_string();
                nodes.push(HedNode::Tag(HedTag::new(text)));
            }
        }

        skip_whitespace(input, pos);
        match input[*pos..].chars().next() {
            Some(',') => {
                *pos += 1;
                pending_trailing_empty = true;
                continue;
            }
            Some(')') | None => break,
            Some(_) => return Err(ParseIssue::MissingComma(*pos)),
        }
    }

    Ok(nodes)
}

pub fn parse_hed_string(input: &str) -> Result<HedString, HedError> {
    let mut pos = 0usize;
    let nodes = parse_list(input, &mut pos).map_err(|e| e.into_hed_error(input))?;

    // Anything left over at the top level can only be an unmatched ')'.
    if pos < input.len() {
        return Err(ParseIssue::UnmatchedCloseParen(pos).into_hed_error(input));
    }

    Ok(HedString::new(nodes))
}
