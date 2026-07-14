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
//! (`concepts` to materialize, row `ordered`ing) is declared for the data tables readers
//! populate. Documents are validated against [`INGESTION_METASCHEMA_JSON`]. This model
//! subsumes bidslake's previously-hardcoded `.tsv` gate, `.bval`/`.bvec` handling, and
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
    /// policies are unioned (later fragments win on a table-name clash).
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
            ingestion.tables.extend(file.tables);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs() -> Ingestion {
        Ingestion::from_sources(&[bids_schema::bundled_ingestion_source("freesurfer").unwrap()])
            .expect("freesurfer ingestion loads")
    }

    #[test]
    fn bundled_ingestion_is_metaschema_valid() {
        let src = bids_schema::bundled_ingestion_source("freesurfer").unwrap();
        let doc: Value = serde_json::from_str(src).unwrap();
        assert!(bids_schema::validate_ingestion(&doc).is_empty());
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
}
