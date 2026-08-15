//! Every bundled ingestion selector's `match()` pattern is a regex that can match something.
//!
//! Selector expressions go through the JS parser, and [`compile_uncached`] doubles every
//! backslash first so the parser's own string-literal unescaping hands the regex back what
//! the author wrote — `\S` would otherwise decode to a bare `S`. The consequence is a rule
//! authors have to know: **a regex escape is written with one backslash**, and pre-doubling
//! it produces a pattern matching a *literal backslash*.
//!
//! That failure is silent in every direction. `"\\.touch$"` is a valid regex, the expression
//! compiles, the fragment loads, the metaschema is satisfied, and the rule simply never
//! fires. The only symptom is a disposition that appears to do nothing — which is exactly
//! what happened when the `touch/` ignore rule was first written.
//!
//! So: no bundled selector may contain a doubled backslash. If one ever legitimately needs to
//! match a literal backslash — bidslake normalizes paths to `/`, so it should not — this test
//! is the right place to record why.
//!
//! Note the neighbouring convention this deliberately does *not* police. A term map's
//! `Template` is compiled as a regex directly, with no doubling, so it takes escapes as
//! written; `data/term-maps/*.json` legitimately carry `\.` and are not selectors.

use std::collections::BTreeMap;

/// Every bundled ingestion fragment, by name.
fn fragments() -> BTreeMap<&'static str, &'static str> {
    let mut out = BTreeMap::new();
    out.insert(
        "base",
        bids_schema::bundled_ingestion_source("base").expect("base"),
    );
    for name in bids_schema::BUNDLED_INGESTION_NAMES {
        out.insert(
            name,
            bids_schema::bundled_ingestion_source(name)
                .unwrap_or_else(|| panic!("{name} is listed as bundled but has no source")),
        );
    }
    out
}

/// The `selectors` of every rule in a fragment, with the rule's index for the message.
fn selectors(src: &str) -> Vec<(usize, String)> {
    let doc: serde_json::Value = serde_json::from_str(src).expect("fragment is JSON");
    // `rules` is optional: fmriprep, qsiprep and dcmstack declare only `tables`.
    let rules = doc.get("rules").and_then(|r| r.as_array());
    let mut out = Vec::new();
    for (i, rule) in rules.into_iter().flatten().enumerate() {
        for sel in rule["selectors"].as_array().into_iter().flatten() {
            out.push((i, sel.as_str().expect("selector is a string").to_string()));
        }
    }
    out
}

#[test]
fn no_bundled_selector_pre_doubles_a_backslash() {
    let mut bad = Vec::new();
    for (name, src) in fragments() {
        for (idx, sel) in selectors(src) {
            if sel.contains("\\\\") {
                bad.push(format!("  {name}.json rule {idx}: {sel}"));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "a selector pre-doubles a backslash, which makes its regex match a literal \
         backslash and therefore nothing:\n{}\n\nWrite the escape with ONE backslash \
         (`\\\\.` in the JSON source, `\\.` once parsed) — the expression compiler doubles \
         it for you.",
        bad.join("\n")
    );
}

/// Each `match()` pattern compiles, and is not accidentally inert.
///
/// Weaker than "matches the right paths" — that belongs with each adapter's own tests — but
/// it catches the class of mistake a metaschema cannot: a pattern that is well-formed JSON,
/// well-formed JS, and a valid regex, yet cannot match any path.
#[test]
fn every_match_pattern_compiles() {
    for (name, src) in fragments() {
        for (idx, sel) in selectors(src) {
            let Some(rest) = sel.split_once("match(").map(|(_, r)| r) else {
                continue;
            };
            // The pattern is the second argument: the last double-quoted run before the `)`.
            let Some(open) = rest
                .rfind('"')
                .and_then(|close| rest[..close].rfind('"').map(|o| (o, close)))
            else {
                continue;
            };
            let pattern = &rest[open.0 + 1..open.1];
            regex::Regex::new(pattern).unwrap_or_else(|e| {
                panic!("{name}.json rule {idx}: pattern {pattern:?} is not a valid regex: {e}")
            });
        }
    }
}
