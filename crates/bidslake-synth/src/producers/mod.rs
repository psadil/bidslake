//! The per-producer path recipes.
//!
//! [`layout`] is the general one and needs no recipe at all: a layout document already declares
//! every path in its tree, so the driver renders each role under each example and is done. The
//! other two are here because their trees are named by BIDS entities, and a layout cannot carry
//! those — ADR 0002 §10 ties a layout to a term map, and a BIDS-named producer has none.
//!
//! [`raw`] is the one that genuinely enumerates the schema: `rules.files.raw` says which
//! suffix, extension and datatype go together, and `rules.tabular_data` says what goes in each
//! table. [`fmriprep`] cannot, because its vocabulary — `timeseries`, `xfm`, `boldref` — appears
//! in no `rules.files` group at all, in the base schema or in any overlay. Its recipe is the
//! honest consequence, and `every_overlay_suffix_is_emitted_by_some_producer` is what keeps it
//! from falling behind the overlays.

pub mod fmriprep;
pub mod layout;
pub mod raw;

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// `dataset_description.json` for a raw dataset.
///
/// `Authors` is recommended rather than required, and is here because omitting it is two
/// warnings (`NO_AUTHORS`, `TOO_FEW_AUTHORS`) on every generated tree — noise that would drown
/// the warnings a reader actually wants to see.
pub(crate) fn raw_description(name: &str) -> String {
    serde_json::to_string_pretty(&json!({
        "Name": name,
        "BIDSVersion": "1.11.1",
        "Authors": ["bidslake-synth", "nobody at all"],
    }))
    .expect("a JSON object serializes")
}

/// `dataset_description.json` for a derivative, carrying the `GeneratedBy` that
/// `rules.dataset_metadata.derivative_description` makes required once `DatasetType` says
/// `derivative`, and the DOI-shaped `SourceDatasets` that gives `dataset_relations` something to
/// resolve (ADR 0003).
pub(crate) fn derivative_description(name: &str, tool: &str, version: &str) -> String {
    serde_json::to_string_pretty(&json!({
        "Name": name,
        "BIDSVersion": "1.11.1",
        "DatasetType": "derivative",
        "GeneratedBy": [{ "Name": tool, "Version": version }],
        "SourceDatasets": [{ "DOI": "10.0000/synthetic.bidslake" }],
    }))
    .expect("a JSON object serializes")
}

/// A README, which `rules.files.common.core` recommends and every real dataset carries.
pub(crate) fn readme(name: &str) -> String {
    format!(
        "# {name}\n\nA synthetic tree written by `bidslake-synth`. Every imaging file is empty:\n\
         indexing never opens one, which is what lets a hundred-thousand-file tree cost seconds\n\
         and almost no disk.\n"
    )
}

/// JSON override map helper.
pub(crate) fn overrides<const N: usize>(pairs: [(&str, Value); N]) -> BTreeMap<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}
