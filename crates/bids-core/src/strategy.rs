//! Proptest strategies for BIDS filename shapes.
//!
//! Two rules everything here follows, both consequences of `CONTRIBUTING.md`:
//!
//! - **No `prop_filter`.** Every strategy is total. Entity-key uniqueness is enforced by
//!   deduplicating inside `prop_map`, not by rejecting a generated value, so the case budget
//!   is never quietly spent on rejections.
//! - **The strategy returns the expected answer.** [`bids_filename`] yields the name *and* the
//!   [`BidsFileParts`] `read_entities` owes for it. It gets there by *rendering* a structure it
//!   already holds, never by re-deriving what the parser would do — a strategy that models the
//!   parsing rules would be the implementation again, and would agree with a broken parser.
//!
//! Every strategy here is exercised by this crate's own tests. Under `cargo test -p bids-core`
//! this module compiles into a test binary, where a `pub` item nothing calls is `dead_code`, so
//! an unused generator fails the build rather than rotting.

use std::collections::{HashMap, HashSet};

use proptest::collection::SizeRange;
use proptest::prelude::*;

use crate::entities::BidsFileParts;

/// Extensions the splitter has to keep whole.
///
/// `""` is one of them deliberately: a name with no dot at all (`participants`) takes a
/// different branch of `split_extension`, and generating it is cheaper than remembering to
/// write the case.
const EXTENSIONS: &[&str] = &[
    "", ".nii", ".nii.gz", ".json", ".tsv", ".tsv.gz", ".bval", ".bvec", ".edf", ".ome.tif",
];

/// A BIDS label: `[0-9A-Za-z]+`, never empty.
///
/// Non-empty on purpose. An empty label (`sub-_T1w.nii.gz`) parses to the `NOENTITY` sentinel,
/// which is a different rule with its own property — folding it in here would make the
/// round-trip law false rather than better tested.
pub fn label() -> impl Strategy<Value = String> {
    "[0-9A-Za-z]{1,8}"
}

/// An entity key: `[0-9A-Za-z]+`, never empty.
///
/// Not restricted to the schema's entity list. `read_entities` is schema-agnostic by design, so
/// a key it has never heard of must round-trip exactly like `sub`; pinning the generator to the
/// known list would hide the day that stops being true.
pub fn entity_key() -> impl Strategy<Value = String> {
    "[0-9A-Za-z]{1,6}"
}

/// A suffix: `[0-9A-Za-z]+`, never empty.
///
/// The alphabet is load-bearing twice. No `.`, or the suffix would be eaten by
/// `split_extension`, which takes everything from the *first* dot. No `-`, or the parser would
/// read the last segment as a trailing entity instead of a suffix — a real shape, but a
/// different one, and [`suffixless_filename`] is where it lives.
pub fn suffix() -> impl Strategy<Value = String> {
    "[0-9A-Za-z]{1,10}"
}

/// One of [`EXTENSIONS`].
pub fn extension() -> impl Strategy<Value = String> {
    prop::sample::select(EXTENSIONS).prop_map(str::to_owned)
}

/// `count` entity pairs with distinct keys, in generation order.
///
/// Distinct because `BidsFileParts::entities` is a `HashMap`: a repeated key makes the last
/// occurrence win while `entity_keys` keeps both. That is real behaviour with its own property;
/// mixing it in here would mean the expected value could no longer be built by a plain
/// `collect()`, which is exactly the re-derivation this module avoids.
///
/// The deduplication is an ordinary iterator `filter` *inside* `prop_map`, not a `prop_filter`:
/// nothing generated is thrown away, so no case is spent on a rejection.
pub fn entity_pairs(count: impl Into<SizeRange>) -> impl Strategy<Value = Vec<(String, String)>> {
    prop::collection::vec((entity_key(), label()), count).prop_map(|pairs| {
        let mut seen = HashSet::new();
        pairs
            .into_iter()
            .filter(|(key, _)| seen.insert(key.clone()))
            .collect()
    })
}

/// A BIDS filename with a suffix, and the [`BidsFileParts`] `read_entities` owes for it.
///
/// `sub-01_ses-pre_task-rest_bold.nii.gz` shaped, but with arbitrary keys, labels, suffix,
/// entity count and entity order.
pub fn bids_filename() -> impl Strategy<Value = (String, BidsFileParts)> {
    (entity_pairs(0..=4), suffix(), extension())
        .prop_map(|(pairs, suffix, extension)| assemble(pairs, Some(suffix), extension))
}

/// A filename whose last `_` segment is an entity, so it has no suffix (`sub-01_ses-pre.json`),
/// and the parts owed for it.
///
/// At least one entity: with neither entities nor a suffix the stem is empty and the name is a
/// bare extension, which asserts nothing about either rule.
pub fn suffixless_filename() -> impl Strategy<Value = (String, BidsFileParts)> {
    (entity_pairs(1..=4), extension())
        .prop_map(|(pairs, extension)| assemble(pairs, None, extension))
}

/// Build the name and the parse owed for it from the pieces.
///
/// One function for both strategies so the two cannot drift on what the parser owes.
fn assemble(
    pairs: Vec<(String, String)>,
    suffix: Option<String>,
    extension: String,
) -> (String, BidsFileParts) {
    let mut segments: Vec<String> = pairs
        .iter()
        .map(|(key, label)| format!("{key}-{label}"))
        .collect();
    if let Some(suffix) = &suffix {
        segments.push(suffix.clone());
    }
    let stem = segments.join("_");
    let name = format!("{stem}{extension}");

    let entity_keys: Vec<String> = pairs.iter().map(|(key, _)| key.clone()).collect();
    let entities: HashMap<String, String> = pairs.into_iter().collect();

    (
        name,
        BidsFileParts {
            stem,
            suffix: suffix.unwrap_or_default(),
            extension,
            entities,
            entity_keys,
        },
    )
}

/// A path of `count` non-empty segments, joined by runs of 1–3 `/`, optionally with a leading
/// and/or trailing separator — and the segments it was built from.
///
/// The separator runs are the point: `parent_dir` filters empty segments, so `a//b///c` must
/// answer exactly as `a/b/c`, and the eight hand-written rows only ever doubled a slash at one
/// position.
pub fn slashed_path(count: impl Into<SizeRange>) -> impl Strategy<Value = (String, Vec<String>)> {
    (
        prop::collection::vec(label(), count),
        prop::collection::vec(1usize..=3, 0..8),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(segments, runs, leading, trailing)| {
            let mut path = String::new();
            if leading {
                path.push('/');
            }
            for (i, segment) in segments.iter().enumerate() {
                if i > 0 {
                    // Cycle the generated run lengths so a short `runs` still varies the joins.
                    let n = runs.get((i - 1) % runs.len().max(1)).copied().unwrap_or(1);
                    for _ in 0..n {
                        path.push('/');
                    }
                }
                path.push_str(segment);
            }
            if trailing {
                path.push('/');
            }
            (path, segments)
        })
}
