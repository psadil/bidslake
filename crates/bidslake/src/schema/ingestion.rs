//! The **ingestion schema**: bidslake's declarative read-vs-catalog policy.
//!
//! A bidslake-specific (not BIDS) schema — BIDS has no database to read into — that decides,
//! for a file already projected onto BIDS concepts (by the BIDS filename grammar or by a
//! [term map](bids_schema::term_map)), what bidslake does with it:
//!
//! - **read** — parse its contents into a data table via a named reader;
//! - **catalog** — record it in the file registry (`scans`), contents unread, left on disk;
//! - **ignore** — skip it (the declarative `.bidsignore`-override).
//!
//! Rules select with the BIDS selector-expression language over projected concepts, reusing
//! the same evaluator as [`Tabular::route`](super::tabular::Tabular::route). Per-table policy
//! (`concepts` to materialize, row `ordered`ing, and whether columns the schema does not
//! declare are stored — see [`Undeclared`]) is declared for the data tables readers populate.
//! Documents are validated against [`INGESTION_METASCHEMA_JSON`]. This model subsumes
//! bidslake's previously-hardcoded `.tsv` gate, `.bval`/`.bvec` handling, and
//! recording/ordering rules.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use super::tabular::FileContext;

/// What bidslake does with a matched file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Disposition {
    /// Parse the file's contents into a data table via a reader.
    Read,
    /// Record the file in the `scans` registry (contents unread, left on disk).
    Catalog,
    /// Recognize but skip the file entirely.
    Ignore,
}

/// One ordered file-disposition rule.
#[derive(Debug, Clone, Deserialize)]
pub struct IngestionRule {
    #[serde(default)]
    pub selectors: Vec<String>,
    pub disposition: Disposition,
    /// Reader name (present when `disposition == Read`).
    #[serde(default)]
    pub reader: Option<String>,
}

/// What a table does with source columns it does not declare — the per-column
/// analogue of [`Disposition`], and the storage dial described in ADR 0004.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Undeclared {
    /// Preserve them verbatim in the table's `other_data` JSON column. The default,
    /// and what every BIDS table does.
    #[default]
    Store,
    /// Do not store them. The file stays on disk and stays in `tabular_files`, whose
    /// `file_path` is the record of its full column set; the names seen are collected
    /// into `tabular_undeclared_columns`. For sources whose undeclared columns dwarf
    /// the declared ones — fMRIPrep confounds are ~1,800 columns against ~13 declared,
    /// which cost 24 MB of database per file stored as per-row JSON.
    Catalog,
}

/// Per-table row/column policy for the data tables readers populate.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TablePolicy {
    /// BIDS concept names materialized as physical columns (else the table uses the
    /// virtual regex-over-path columns). Presence marks the table materialized.
    #[serde(default)]
    pub concepts: Vec<String>,
    /// Whether source row order is load-bearing (see bids-2-devel#98).
    #[serde(default)]
    pub ordered: Option<bool>,
    /// Table-wide undeclared-column policy. Static, so it can drive DDL: a table
    /// declared `catalog` gets no `other_data` column at all.
    #[serde(default)]
    pub undeclared: Option<Undeclared>,
}

impl TablePolicy {
    /// Merge `other` (from a later fragment) over `self`, field by field: a set field
    /// wins, an unset one leaves the earlier value alone. Wholesale replacement would
    /// mean a fragment touching one field of a shared table silently drops what an
    /// earlier fragment declared about the others.
    fn merge_from(&mut self, other: TablePolicy) {
        if !other.concepts.is_empty() {
            self.concepts = other.concepts;
        }
        if other.ordered.is_some() {
            self.ordered = other.ordered;
        }
        if other.undeclared.is_some() {
            self.undeclared = other.undeclared;
        }
    }
}

#[derive(Debug, Deserialize)]
struct IngestionFile {
    #[serde(default)]
    rules: Vec<IngestionRule>,
    #[serde(default)]
    tables: BTreeMap<String, TablePolicy>,
}

/// The compiled ingestion policy — merged from a base plus any adapter fragments.
#[derive(Debug, Clone, Default)]
pub struct Ingestion {
    rules: Vec<IngestionRule>,
    tables: BTreeMap<String, TablePolicy>,
}

impl Ingestion {
    /// Merge ingestion fragments (each a JSON document string) into one policy, validating
    /// each against the ingestion metaschema first. Rules are concatenated in order; table
    /// policies are merged **field-wise** (see [`TablePolicy::merge_from`]) rather than
    /// replaced wholesale, so a later fragment setting one field of a shared table cannot
    /// silently drop the `concepts` or `ordered` an earlier one declared.
    pub fn from_sources(sources: &[&str]) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        let mut ingestion = Ingestion::default();
        for src in sources {
            let document: Value =
                serde_json::from_str(src).context("parsing ingestion schema as JSON")?;
            let violations = bids_schema::validate_ingestion(&document);
            if !violations.is_empty() {
                anyhow::bail!(
                    "ingestion schema violates its metaschema:\n{}",
                    violations.join("\n")
                );
            }
            let file: IngestionFile =
                serde_json::from_value(document).context("reading ingestion schema")?;
            ingestion.rules.extend(file.rules);
            for (table, policy) in file.tables {
                ingestion
                    .tables
                    .entry(table)
                    .or_default()
                    .merge_from(policy);
            }
        }
        Ok(ingestion)
    }

    /// The base ingestion policy bidslake applies to every ingest (BIDS defaults), even
    /// without an adapter — e.g. `events` rows are order-insensitive.
    pub fn base() -> Self {
        Self::from_sources(&[
            bids_schema::bundled_ingestion_source("base").expect("bundled base ingestion")
        ])
        .expect("base ingestion is build-tested")
    }

    /// Whether a table's source row order is load-bearing (default `true` — order matters and
    /// rows are read sequentially). `events` is the one BIDS table declared order-insensitive
    /// (rows carry `onset`); see bids-standard/bids-2-devel#98.
    pub fn ordered(&self, table: &str) -> bool {
        self.tables
            .get(table)
            .and_then(|p| p.ordered)
            .unwrap_or(true)
    }

    /// The first rule whose selectors all pass for `ctx`, or `None`.
    pub fn classify(&self, ctx: &FileContext) -> Option<&IngestionRule> {
        let (file, dataset) = ctx.eval_bindings();
        let null = Value::Null;
        let eval = bids_schema::expression::EvalContext::new(&file, &dataset, &null, &null);
        self.rules
            .iter()
            .find(|r| bids_schema::expression::do_selectors_select(Some(&r.selectors), &eval))
    }

    /// The materialized-concept column names for a table (empty if not materialized).
    pub fn materialized_concepts(&self, table: &str) -> &[String] {
        self.tables
            .get(table)
            .map(|p| p.concepts.as_slice())
            .unwrap_or(&[])
    }

    /// A table's *static* undeclared-column policy (default [`Undeclared::Store`]).
    ///
    /// This is the DDL-time question — whether the table gets an `other_data` column at
    /// all — so it deliberately ignores `undeclaredWhen`: a table that scopes its policy
    /// per file still needs the column, since some of its files will fill it.
    pub fn undeclared(&self, table: &str) -> Undeclared {
        self.tables
            .get(table)
            .and_then(|p| p.undeclared)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs() -> Ingestion {
        Ingestion::from_sources(&[bids_schema::bundled_ingestion_source("freesurfer").unwrap()])
            .expect("freesurfer ingestion loads")
    }

    /// Every bundled fragment, not just one — a new fragment that violates the
    /// metaschema should fail here rather than at a user's first ingest.
    #[test]
    fn bundled_ingestion_is_metaschema_valid() {
        for name in std::iter::once(&"base").chain(bids_schema::BUNDLED_INGESTION_NAMES) {
            let src = bids_schema::bundled_ingestion_source(name)
                .unwrap_or_else(|| panic!("bundled ingestion {name:?} is registered but missing"));
            let doc: Value = serde_json::from_str(src)
                .unwrap_or_else(|e| panic!("bundled ingestion {name:?} is not JSON: {e}"));
            let violations = bids_schema::validate_ingestion(&doc);
            assert!(
                violations.is_empty(),
                "bundled ingestion {name:?} violates its metaschema:\n{}",
                violations.join("\n")
            );
            // And it must actually compile, not merely validate.
            Ingestion::from_sources(&[src])
                .unwrap_or_else(|e| panic!("bundled ingestion {name:?} does not load: {e}"));
        }
    }

    #[test]
    fn stats_files_are_read_by_fs_stats() {
        let ing = fs();
        let ctx = FileContext {
            path: "/sub-01/stats/aseg.stats",
            datatype: Some("anat"),
            suffix: Some("segstats"),
            extension: Some(".stats"),
            sidecar: &Value::Null,
            dataset_type: None,
        };
        let rule = ing.classify(&ctx).expect("matches");
        assert_eq!(rule.disposition, Disposition::Read);
        assert_eq!(rule.reader.as_deref(), Some("fs_stats"));
    }

    #[test]
    fn anat_non_stats_is_cataloged() {
        let ing = fs();
        let ctx = FileContext {
            path: "/sub-01/surf/lh.thickness",
            datatype: Some("anat"),
            suffix: None,
            extension: Some(".thickness"),
            sidecar: &Value::Null,
            dataset_type: None,
        };
        assert_eq!(
            ing.classify(&ctx).unwrap().disposition,
            Disposition::Catalog
        );
    }

    #[test]
    fn table_policy_carries_materialized_concepts() {
        let ing = fs();
        assert_eq!(
            ing.materialized_concepts("freesurfer_aparc"),
            ["sub", "ses", "hemi", "parc"]
        );
        assert!(ing.materialized_concepts("scans").is_empty());
    }

    /// Storing undeclared columns is the default, so plain BIDS is unaffected by the
    /// policy existing — including for tables that carry a policy for other reasons.
    #[test]
    fn undeclared_defaults_to_store() {
        let ing = fs();
        assert_eq!(ing.undeclared("events"), Undeclared::Store);
        assert_eq!(ing.undeclared("sidecars"), Undeclared::Store);
        assert_eq!(ing.undeclared("freesurfer_aparc"), Undeclared::Store);
        assert_eq!(Ingestion::base().undeclared("events"), Undeclared::Store);
    }

    /// `tablePolicy` is `additionalProperties: false`, so the new fields only work if
    /// they were actually added to the metaschema — and a typo in a policy value must
    /// be an error, not a silent fall back to the default.
    #[test]
    fn metaschema_rejects_bad_undeclared_policy() {
        for bad in [
            r#"{"IngestionSchemaVersion": "0.1.0",
                "tables": { "t": { "undeclared": "cataolg" } } }"#,
            r#"{"IngestionSchemaVersion": "0.1.0",
                "tables": { "t": { "undecalred": "catalog" } } }"#,
        ] {
            assert!(
                Ingestion::from_sources(&[bad]).is_err(),
                "metaschema should reject: {bad}"
            );
        }
    }

    #[test]
    fn fragment_declares_undeclared_catalog() {
        let ing = Ingestion::from_sources(&[r#"{
            "IngestionSchemaVersion": "0.1.0",
            "tables": { "confounds": { "undeclared": "catalog" } }
        }"#])
        .expect("fragment loads");
        assert_eq!(ing.undeclared("confounds"), Undeclared::Catalog);
        assert_eq!(ing.undeclared("events"), Undeclared::Store);
    }

    /// A later fragment touching one field of a shared table must not drop the others.
    /// `tables.extend` used to replace the whole `TablePolicy`, silently losing an
    /// earlier fragment's `concepts`/`ordered`.
    #[test]
    fn table_policies_merge_field_wise() {
        let ing = Ingestion::from_sources(&[
            r#"{
                "IngestionSchemaVersion": "0.1.0",
                "tables": { "t": { "concepts": ["sub"], "ordered": false } }
            }"#,
            r#"{
                "IngestionSchemaVersion": "0.1.0",
                "tables": { "t": { "undeclared": "catalog" } }
            }"#,
        ])
        .expect("fragments load");

        assert_eq!(ing.materialized_concepts("t"), ["sub"], "concepts survive");
        assert!(!ing.ordered("t"), "ordered survives");
        assert_eq!(ing.undeclared("t"), Undeclared::Catalog, "later field wins");
    }
}
