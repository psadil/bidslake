//! Which BIDS suffixes are **headerless continuous recordings** — derived from the
//! schema, not listed.
//!
//! A recording (`*_physio.tsv.gz`, `*_stim.tsv.gz`, `*_motion.tsv`) has no header
//! row: its first line is already data. Its column *names* therefore live somewhere
//! else, and the BIDS schema says where in exactly two ways.
//!
//! - **`rules.sidecars` requires the `Columns` field.** Requiring `Columns` *is* the
//!   schema saying the names are in the sidecar rather than in the file. The rules
//!   that do so sit in a group called `continuous`, and select `physio`, `stim` and
//!   `physioevents`.
//! - **`meta.associations.channels` selects the suffix, and no `rules.tabular_data`
//!   rule declares columns for it.** The association says a `_channels.tsv` describes
//!   this file, and the absent column rule says the schema declares no columns of its
//!   own — so the channels file is where the names are. That is `motion`. The second
//!   half is what separates it from `optodes`, which the same association selects but
//!   which has its own `nirs.nirsOptodes` columns and a header like any other table.
//!
//! Both halves say the same thing: *the schema locates this file's column names
//! outside the file*. Nothing here enumerates a suffix.
//!
//! Both are then filtered to the suffixes BIDS declares with a tabular extension
//! (`rules.files`). A recording is a TSV, and the `channels` association is not: it
//! also names `eeg`/`emg`/`ieeg`/`meg`/`nirs`, whose own files are binary and which
//! likewise declare no columns of their own. Without the filter each would be handed
//! a bare table it could never hold a row of.
//!
//! Selectors are parsed with [`Selector`], which models only the forms that occur and
//! yields [`Selector::Unsupported`] for anything else. A schema that states these
//! conditions some new way therefore drops the suffix from the set rather than
//! guessing at it — the same fail-safe [`super::tabular`] takes.

use super::tabular::{Selector, Tabular};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

/// Where a recording's column *names* come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnNames {
    /// The merged sidecar's `Columns` array.
    Sidecar,
    /// The `name` column of the associated `_channels.tsv`, in `row_idx` order.
    Channels,
}

/// One headerless continuous recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recording {
    /// The BIDS suffix (`physio`, `physioevents`, `stim`, `motion`).
    pub suffix: String,
    /// The DuckDB table its rows land in: the `rules.tabular_data` table when the
    /// schema declares columns for the suffix (`physioevents` → `physio_events`),
    /// else the suffix itself (`stim`, `motion`).
    pub table: String,
    /// Where this recording's column names come from.
    pub names: ColumnNames,
}

/// The recordings a schema declares, by suffix.
#[derive(Clone, Debug, Default)]
pub struct Recordings {
    by_suffix: BTreeMap<String, Recording>,
}

impl Recordings {
    /// Derive the set from a schema and its tabular model. Both must be
    /// post-overlay: an overlay may declare columns for a recording, which changes
    /// the table it routes to and stops it being generated bare.
    pub fn load(schema: &Value, tabular: &Tabular) -> Self {
        let mut by_suffix: BTreeMap<String, Recording> = BTreeMap::new();
        let tabular_suffixes = tabular_suffixes(schema);

        // Names in the sidecar: every `rules.sidecars` rule that declares `Columns`.
        for suffix in sidecar_columns_suffixes(schema) {
            if !tabular_suffixes.contains(&suffix) {
                continue;
            }
            let table = declared_table(tabular, &suffix).unwrap_or_else(|| suffix.clone());
            by_suffix.entry(suffix.clone()).or_insert(Recording {
                suffix,
                table,
                names: ColumnNames::Sidecar,
            });
        }

        // Names in the associated `_channels.tsv`: selected by the association, its own
        // file tabular, and declaring no columns of its own — which is what separates
        // `motion` from `optodes` (header-bearing) and from `eeg`/`meg`/… (binary).
        for suffix in channels_association_suffixes(schema) {
            if !tabular_suffixes.contains(&suffix) || declared_table(tabular, &suffix).is_some() {
                continue;
            }
            by_suffix.entry(suffix.clone()).or_insert(Recording {
                table: suffix.clone(),
                suffix,
                names: ColumnNames::Channels,
            });
        }

        Recordings { by_suffix }
    }

    /// The recording for a suffix, or `None` if it is not one.
    pub fn get(&self, suffix: &str) -> Option<&Recording> {
        self.by_suffix.get(suffix)
    }

    /// Whether a suffix names a headerless continuous recording.
    pub fn contains(&self, suffix: &str) -> bool {
        self.by_suffix.contains_key(suffix)
    }

    /// Every recording, in suffix order.
    pub fn iter(&self) -> impl Iterator<Item = &Recording> {
        self.by_suffix.values()
    }
}

/// The table whose `rules.tabular_data` rules declare columns for `suffix`. `None`
/// when the schema declares none, which is exactly when the table is generated bare.
fn declared_table(tabular: &Tabular, suffix: &str) -> Option<String> {
    tabular.table_for_suffix(suffix).map(|t| t.table.clone())
}

/// Suffixes BIDS declares with a tabular extension, from `rules.files`. A file rule
/// is any node carrying both `suffixes` and `extensions`; the nesting above that
/// (`raw`/`deriv`/`common` → group → rule) needs no special case.
fn tabular_suffixes(schema: &Value) -> HashSet<String> {
    fn walk(node: &Value, out: &mut HashSet<String>) {
        let Some(obj) = node.as_object() else { return };
        match (
            obj.get("suffixes").and_then(Value::as_array),
            obj.get("extensions").and_then(Value::as_array),
        ) {
            (Some(suffixes), Some(extensions)) => {
                if extensions
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|e| e.starts_with(".tsv"))
                {
                    out.extend(
                        suffixes
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string),
                    );
                }
            }
            _ => obj.values().for_each(|v| walk(v, out)),
        }
    }
    let mut out = HashSet::new();
    walk(&schema["rules"]["files"], &mut out);
    out
}

/// Suffixes whose sidecar rules declare a `Columns` field — the schema's way of
/// saying the column names are in the sidecar, not in a header row.
fn sidecar_columns_suffixes(schema: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(groups) = schema["rules"]["sidecars"].as_object() else {
        return out;
    };
    for rules in groups.values() {
        let Some(rules) = rules.as_object() else {
            continue;
        };
        for rule in rules.values() {
            if rule["fields"].get("Columns").is_some() {
                out.extend(suffixes_from_selectors(&rule["selectors"]));
            }
        }
    }
    out
}

/// Suffixes the `channels` association applies to — the files a `_channels.tsv`
/// describes.
fn channels_association_suffixes(schema: &Value) -> Vec<String> {
    suffixes_from_selectors(&schema["meta"]["associations"]["channels"]["selectors"])
}

/// The suffixes a selector list names, via `suffix == "x"` or
/// `intersects([suffix], [...])`. Any other selector contributes nothing.
fn suffixes_from_selectors(selectors: &Value) -> Vec<String> {
    let Some(arr) = selectors.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for s in arr.iter().filter_map(Value::as_str) {
        match Selector::parse(s) {
            Selector::Suffix(x) => out.push(x),
            Selector::SuffixIn(xs) => out.extend(xs),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        serde_json::from_str(bids_schema::SCHEMA_JSON).unwrap()
    }

    fn recordings() -> Recordings {
        let s = schema();
        let t = Tabular::load(&s);
        Recordings::load(&s, &t)
    }

    /// Pins what the derivation currently yields for the vendored schema. The set is
    /// derived, so this is what makes a schema bump that changes it *visible* rather
    /// than silent — the same "derive in code, pin in test" shape as
    /// `tabular::tests::table_names_are_as_expected`.
    #[test]
    fn the_derived_set_is_the_four_bids_recordings() {
        let r = recordings();
        let got: Vec<(&str, &str, ColumnNames)> = r
            .iter()
            .map(|rec| (rec.suffix.as_str(), rec.table.as_str(), rec.names))
            .collect();
        assert_eq!(
            got,
            vec![
                ("motion", "motion", ColumnNames::Channels),
                ("physio", "physio", ColumnNames::Sidecar),
                ("physioevents", "physio_events", ColumnNames::Sidecar),
                ("stim", "stim", ColumnNames::Sidecar),
            ]
        );
    }

    /// `physioevents` is the one suffix whose table is not its own name; it comes from
    /// the `PhysioEventsColumns` rule, not from a hardcoded mapping.
    #[test]
    fn the_table_name_comes_from_the_column_rule_when_there_is_one() {
        let r = recordings();
        assert_eq!(r.get("physioevents").unwrap().table, "physio_events");
        assert_eq!(r.get("stim").unwrap().table, "stim");
    }

    /// `optodes` is selected by the same `channels` association as `motion`, but has
    /// its own columns and a header — the condition that separates them.
    #[test]
    fn a_channels_associated_table_with_its_own_columns_is_not_a_recording() {
        let r = recordings();
        assert!(!r.contains("optodes"), "optodes has its own column rule");
        assert!(r.contains("motion"));
    }

    /// An overlay that declares columns for a recording changes where its rows land,
    /// and stops it being bare — the derivation must see the merged schema.
    #[test]
    fn an_overlay_declaring_columns_moves_the_recording_off_the_bare_path() {
        let mut s = schema();
        bids_schema::overlay::merge_into(
            &mut s,
            &serde_json::json!({
                "objects": { "columns": {
                    "stim_signal__test": { "name": "stim_signal", "type": "number" }
                }},
                "rules": { "tabular_data": { "stimtest": { "stim": {
                    "selectors": ["suffix == \"stim\""],
                    "columns": { "stim_signal__test": "optional" }
                }}}}
            }),
        )
        .unwrap();
        let t = Tabular::load(&s);
        let r = Recordings::load(&s, &t);
        // Still a recording — it is still headerless — but no longer bare.
        let stim = r.get("stim").expect("stim is still a recording");
        assert_eq!(stim.table, "stim");
        assert!(t.tables().iter().any(|spec| spec.table == "stim"));
    }
}
