//! The **ingestion schema**: bidslake's declarative read-vs-catalog policy.
//!
//! A bidslake-specific (not BIDS) schema — BIDS has no database to read into — that decides,
//! for a file already projected onto BIDS concepts (by the BIDS filename grammar or by a
//! [term map](bids_schema::term_map)), what bidslake does with it:
//!
//! - **read** — parse its contents into a data table via a named reader;
//! - **catalog** — record it in `file_registry`, contents unread, left on disk;
//! - **ignore** — skip it entirely, registry row included (the declarative
//!   `.bidsignore`-override, and the one disposition that records nothing).
//!
//! Rules select with the BIDS selector-expression language over projected concepts, reusing
//! the same evaluator as [`Tabular::route`](super::tabular::Tabular::route). Per-table policy
//! (`concepts` to materialize, row `ordered`ing, `describes`, and whether columns the schema
//! does not declare are stored — see [`Undeclared`]) is declared for the data tables readers
//! populate. Documents are validated against `bids_schema::INGESTION_METASCHEMA_JSON`.
//!
//! Why this is a document rather than a predicate chain, and how an adapter's fragment merges
//! onto `base.json`, is ADR 0002; the storage half of the per-table policy is ADR 0004.

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
    /// BIDS selector expressions over the file's projected concepts; **all** must pass for
    /// the rule to claim the file.
    ///
    /// Required by the metaschema, but defaulted here, and
    /// [`do_selectors_select`](bids_schema::expression::do_selectors_select) treats an empty
    /// list as vacuously true — so an empty list is a catch-all. A selector that fails to
    /// *evaluate* counts as not selecting, which means a malformed expression makes the rule
    /// permanently inert rather than an error at load.
    ///
    /// Regex escapes inside `match(x, "…")` take **one** backslash: the expression compiler
    /// doubles backslashes before parsing, so a JSON string's own unescaping hands the regex
    /// what was written. Pre-doubling yields a pattern matching a literal backslash — still a
    /// valid regex, so the rule loads and then never fires. A term map's `Template` is
    /// compiled as a regex directly and is *not* doubled; the two conventions differ.
    #[serde(default)]
    pub selectors: Vec<String>,
    /// What happens to a file this rule claims. Required.
    ///
    /// Only the first matching rule decides — [`Ingestion::classify`] stops there — so rules
    /// are written narrowest-first and a later rule never reconsiders a file an earlier one
    /// took. A file no rule claims is neither read nor ignored: it still earns a registry
    /// row, which is what distinguishes "unclaimed" from [`Disposition::Ignore`].
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
    /// Do not store them. The file stays on disk and stays in `file_registry`, whose
    /// `file_path` is the record of its full column set; the names seen are collected
    /// into `tabular_undeclared_columns`. For sources whose undeclared columns dwarf
    /// the declared ones — fMRIPrep confounds are ~1,800 columns against ~13 declared,
    /// which cost 24 MB of database per file stored as per-row JSON.
    Catalog,
}

/// A declaration that a table's rows are facts about *other* files — the data files its
/// source file is associated with — resolved through `file_associations` (docs/adr/0003).
///
/// The relation itself comes from the BIDS schema's `meta.associations`; what this adds is
/// the two things that document cannot carry, because the metaschema pins its entries to
/// `additionalProperties: false`: what the row ordinal *means* on the described file, and
/// what to call the view that re-keys the rows onto it.
#[derive(Debug, Clone, Deserialize)]
pub struct Describes {
    /// The `meta.associations` key whose edges fan this table's rows out to the data files
    /// they describe.
    ///
    /// One per table, because a table holds the rows of one *kind* of file. A payload split
    /// across sibling files is split across tables too — a gradient set is a `.bval` and a
    /// `.bvec`, so it is `bvals` and `bvecs` — and joining those is a view over views rather
    /// than a case this declaration has to model. That is what keeps the generated view a
    /// plain one-to-one join with nothing to aggregate.
    pub association: String,
    /// What one step of the ordinal is a step along, on the *described* file: `volume` for a
    /// 4D image's frames.
    ///
    /// Present only where the alignment is load-bearing. The stored `row_idx` records line
    /// order; the view exposes it as `<axis>_idx`, recording what that line order *means*.
    /// Absent for a table whose rows carry no positional correspondence — `events`, whose
    /// rows are addressed by `onset`, which is exactly why it is [`ordered`] false.
    ///
    /// [`ordered`]: TablePolicy::ordered
    #[serde(default)]
    pub axis: Option<String>,
    /// Name of the view to emit, keyed by the described data file.
    ///
    /// Optional: omitting it records the relation without materializing a second name for
    /// it, which is right when the payload table and the described-file view would want the
    /// same one (`events`).
    #[serde(default)]
    pub view: Option<String>,
}

/// An [`Undeclared`] policy that applies only to the files a selector matches.
#[derive(Debug, Clone, Deserialize)]
pub struct ScopedUndeclared {
    /// BIDS selector expressions over the file's projected concepts; all must pass.
    #[serde(default)]
    pub selectors: Vec<String>,
    /// The policy for the files those selectors match. Required — unlike
    /// [`TablePolicy::undeclared`] a scoped entry has nothing to fall back *to*, so an entry
    /// that declared no policy would match a file and then say nothing about it.
    ///
    /// Scoping never changes the table's shape: the DDL reads the static
    /// [`TablePolicy::undeclared`], so a table with `undeclaredWhen` entries still gets its
    /// `other_data` column, and `catalog` here means the column is left NULL for the matched
    /// files rather than absent for all of them.
    pub undeclared: Undeclared,
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
    /// Per-file undeclared-column policy, for tables holding more than one class of
    /// file. `sidecars` is the motivating case — one table for *every* file in the
    /// catalog, so a table-wide flag would strip custom metadata from all raw BIDS to
    /// reach one derivative's sidecars. First match wins; falls back to
    /// [`TablePolicy::undeclared`], then to [`Undeclared::Store`].
    #[serde(default, rename = "undeclaredWhen")]
    pub undeclared_when: Vec<ScopedUndeclared>,
    /// Source keys this table never stores, by name.
    ///
    /// The key-level counterpart of [`Disposition::Ignore`], completing the vocabulary:
    /// a file is read/cataloged/ignored, the columns a table does not declare are
    /// stored or cataloged, and a *named key* can be dropped outright. It exists
    /// because the other two dials are the wrong shape for a converter that attaches
    /// something enormous and non-BIDS to otherwise ordinary metadata — dcmstack's
    /// DcmMeta writes per-slice DICOM dumps under `global` and `time`, megabytes per
    /// sidecar — where `undeclared: catalog` would discard every other custom field
    /// along with it.
    ///
    /// Dropped before anything reads the metadata, so an ignored key costs nothing to
    /// merge, store, or hold in memory. Applies whether or not the key has a declared
    /// column.
    #[serde(default, rename = "ignoreKeys")]
    pub ignore_keys: Vec<String>,
    /// That this table's rows describe *other* files, and how to reach them (docs/adr/0003).
    #[serde(default)]
    pub describes: Option<Describes>,
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
        if !other.ignore_keys.is_empty() {
            self.ignore_keys = other.ignore_keys;
        }
        if other.ordered.is_some() {
            self.ordered = other.ordered;
        }
        if other.undeclared.is_some() {
            self.undeclared = other.undeclared;
        }
        if other.describes.is_some() {
            self.describes = other.describes;
        }
        // Appends rather than replaces, so several adapters can each scope a policy
        // onto a shared table (`sidecars` above all) without clobbering one another.
        self.undeclared_when.extend(other.undeclared_when);
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
    /// policies are merged **field-wise** (see `TablePolicy::merge_from`) rather than
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
        ingestion.validate_describes()?;
        Ok(ingestion)
    }

    /// Refuse a `describes` declaration the view generator could not honour.
    ///
    /// Checked after the merge rather than per fragment, because both failures are properties
    /// of the *effective* policy: an adapter can declare `describes` on a table whose `ordered`
    /// another fragment set, and two independently-authored adapters can each name a view.
    fn validate_describes(&self) -> anyhow::Result<()> {
        let mut views: BTreeMap<&str, &str> = BTreeMap::new();
        for (table, policy) in &self.tables {
            let Some(describes) = &policy.describes else {
                continue;
            };
            // The ordinal a view would expose as `<axis>_idx` is `row_idx`, which an unordered
            // table does not have — `schema::dynamic`'s `PerRow` arm omits the column entirely.
            // Naming an axis there asks for an alignment the storage cannot carry.
            if describes.axis.is_some() && !self.ordered(table) {
                anyhow::bail!(
                    "ingestion schema: table `{table}` declares `describes.axis` but is \
                     `ordered: false`, so it has no `row_idx` to align on. Either drop the \
                     axis (the view is then a plain re-key) or make the table ordered."
                );
            }
            let Some(view) = describes.view.as_deref() else {
                continue;
            };
            // `CREATE OR REPLACE VIEW` would otherwise let one fragment silently shadow
            // another's view, or shadow a table outright.
            if self.tables.contains_key(view) {
                anyhow::bail!(
                    "ingestion schema: table `{table}` declares view `{view}`, which is \
                     already the name of a table. Pick a name for the described-file view \
                     that the payload table does not already use."
                );
            }
            if let Some(other) = views.insert(view, table) {
                anyhow::bail!(
                    "ingestion schema: tables `{other}` and `{table}` both declare the view \
                     `{view}`. Views are emitted `CREATE OR REPLACE`, so one would silently \
                     shadow the other."
                );
            }
        }
        Ok(())
    }

    /// A table's [`Describes`] declaration, if it has one.
    pub fn describes(&self, table: &str) -> Option<&Describes> {
        self.tables.get(table).and_then(|p| p.describes.as_ref())
    }

    /// Every `(table, declaration)` pair that asks for a view, in table order.
    ///
    /// The generator's input. A declaration with no `view` records the relation only and is
    /// skipped here.
    pub fn described_views(&self) -> impl Iterator<Item = (&str, &Describes)> {
        self.tables.iter().filter_map(|(table, policy)| {
            let describes = policy.describes.as_ref()?;
            describes.view.as_ref()?;
            Some((table.as_str(), describes))
        })
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
    ///
    /// This is what decides whether the table **has** a `row_idx` column at all
    /// (`schema::dynamic`'s `PerRow` arm). Declaring a table unordered buys a concurrent read
    /// of its files, which fixes no row order — so there is nothing to record, and recording an
    /// arbitrary label anyway would only invite `ORDER BY row_idx`. Only declare a table
    /// unordered when its rows are addressed by their own content rather than by position.
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

    /// The source keys `table` never stores (see [`TablePolicy::ignore_keys`]). Empty for
    /// every table by default, so the check is one `is_empty` on the hot path.
    ///
    /// Consulted on the JSON parse path only, which means `sidecars` in practice — the
    /// metaschema description says so, `ignore_keys_is_honoured_on_sidecars_only` below pins the
    /// two together, and `tests/test_ignore_keys.rs` asserts the inertness end to end.
    pub fn ignore_keys(&self, table: &str) -> &[String] {
        self.tables
            .get(table)
            .map(|p| p.ignore_keys.as_slice())
            .unwrap_or_default()
    }

    /// Whether [`Self::undeclared_for`] will actually consult its `FileContext` for `table`.
    ///
    /// It only does so when the table declares `undeclaredWhen`, which no bundled fragment but
    /// fMRIPrep's does. Callers on a per-row path can check this first and skip building a
    /// context — parsing the filename, locating the datatype directory and allocating the
    /// path — that the answer does not depend on.
    pub fn undeclared_needs_context(&self, table: &str) -> bool {
        self.tables
            .get(table)
            .is_some_and(|p| !p.undeclared_when.is_empty())
    }

    /// A table's undeclared-column policy for one specific file: the first matching
    /// `undeclaredWhen` entry, else the static [`Self::undeclared`].
    ///
    /// Returns early for a table with no policy at all — the overwhelmingly common case — so
    /// a plain BIDS ingest evaluates no selectors, and an unrecognized table name answers
    /// [`Undeclared::Store`] rather than erroring.
    ///
    /// This is the *per-row* question, and it can disagree with the DDL: the table's shape
    /// was fixed by [`Self::undeclared`], so `Catalog` here means the row leaves `other_data`
    /// NULL, not that the column is missing.
    pub fn undeclared_for(&self, table: &str, ctx: &FileContext) -> Undeclared {
        let Some(policy) = self.tables.get(table) else {
            return Undeclared::Store;
        };
        if policy.undeclared_when.is_empty() {
            return policy.undeclared.unwrap_or_default();
        }
        let (file, dataset) = ctx.eval_bindings();
        let null = Value::Null;
        let eval = bids_schema::expression::EvalContext::new(&file, &dataset, &null, &null);
        policy
            .undeclared_when
            .iter()
            .find(|scoped| {
                bids_schema::expression::do_selectors_select(Some(&scoped.selectors), &eval)
            })
            .map(|scoped| scoped.undeclared)
            .unwrap_or_else(|| policy.undeclared.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

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

    /// Every `reader` a bundled fragment names has somewhere to go.
    ///
    /// The metaschema says `reader` is "validated against the compiled-in reader registry", and
    /// nothing does it — a name is dispatched at ingest, where an unknown one is a warning on
    /// stderr and a table that reads back empty. A typo in a bundled fragment should fail here
    /// instead, and the check has to span both halves of the dispatch: an engine handled by name
    /// in `bids.rs` never appears in `default_readers()`, and a `ContentReader` never appears in
    /// `ENGINE_READERS`.
    #[test]
    fn every_bundled_reader_name_resolves() {
        let readers = crate::readers::default_readers();
        for name in std::iter::once(&"base").chain(bids_schema::BUNDLED_INGESTION_NAMES) {
            let src = bids_schema::bundled_ingestion_source(name).unwrap();
            let ing = Ingestion::from_sources(&[src]).unwrap();
            for rule in &ing.rules {
                let Some(reader) = rule.reader.as_deref() else {
                    continue;
                };
                assert!(
                    crate::readers::ENGINE_READERS.contains(&reader)
                        || readers.contains_key(reader),
                    "bundled ingestion {name:?} names reader {reader:?}, which is neither an \
                     engine nor a registered content reader"
                );
            }
        }
    }

    /// A `read` rule with no reader has nothing to dispatch, and the metaschema cannot say so —
    /// it has no way to make one property required by another's value.
    #[test]
    fn every_bundled_read_rule_names_a_reader() {
        for name in std::iter::once(&"base").chain(bids_schema::BUNDLED_INGESTION_NAMES) {
            let src = bids_schema::bundled_ingestion_source(name).unwrap();
            let ing = Ingestion::from_sources(&[src]).unwrap();
            for rule in &ing.rules {
                assert!(
                    rule.disposition != Disposition::Read || rule.reader.is_some(),
                    "bundled ingestion {name:?} has a `read` rule with no reader: {:?}",
                    rule.selectors
                );
            }
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
    ///
    /// Each case names the violation it expects, because `from_sources` has three failure
    /// modes on this path — JSON parse, metaschema violation, and deserialization — and a
    /// case whose fixture was mistyped into a parse error would satisfy a bare `is_err()`
    /// without the metaschema ever having an opinion.
    #[rstest]
    // A typo in the policy *value*.
    #[case::misspelled_value(
        r#"{"IngestionSchemaVersion": "0.1.0",
            "tables": { "t": { "undeclared": "cataolg" } } }"#,
        "at `/tables/t/undeclared`"
    )]
    // A typo in the policy *key*, which `additionalProperties: false` is what catches.
    #[case::misspelled_key(
        r#"{"IngestionSchemaVersion": "0.1.0",
            "tables": { "t": { "undecalred": "catalog" } } }"#,
        "Additional properties are not allowed ('undecalred' was unexpected)"
    )]
    // `undeclaredWhen` entries need both fields.
    #[case::incomplete_scoped_entry(
        r#"{"IngestionSchemaVersion": "0.1.0",
            "tables": { "t": { "undeclaredWhen": [{ "undeclared": "catalog" }] } } }"#,
        "\"selectors\" is a required property"
    )]
    fn metaschema_rejects_bad_undeclared_policy(#[case] bad: &str, #[case] violation: &str) {
        let err = Ingestion::from_sources(&[bad]).expect_err("metaschema should reject");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("violates its metaschema"),
            "should fail metaschema validation, not JSON parsing: {msg}"
        );
        assert!(msg.contains(violation), "expected {violation:?} in: {msg}");
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

    /// A fragment cataloging undeclared sidecar keys for one suffix only.
    const SCOPED_SIDECARS: &str = r#"{
        "IngestionSchemaVersion": "0.1.0",
        "tables": { "sidecars": { "undeclaredWhen": [
            { "selectors": ["suffix == \"timeseries\""], "undeclared": "catalog" }
        ] } }
    }"#;

    /// `undeclaredWhen` scopes the policy to matching files only — the property that
    /// lets one derivative's sidecars be cataloged while raw BIDS sidecars in the same
    /// database keep their custom metadata.
    #[rstest]
    #[case::selected("timeseries", Undeclared::Catalog)]
    #[case::not_selected("bold", Undeclared::Store)]
    fn undeclared_when_scopes_to_matching_files(#[case] suffix: &str, #[case] want: Undeclared) {
        let ing = Ingestion::from_sources(&[SCOPED_SIDECARS]).expect("fragment loads");
        let ctx = FileContext {
            suffix: Some(suffix),
            ..Default::default()
        };

        let got = ing.undeclared_for("sidecars", &ctx);

        assert_eq!(got, want, "suffix {suffix}");
    }

    /// The static policy stays `Store`, so the table still gets its `other_data` column —
    /// the scoped form must not change the table's shape.
    #[test]
    fn a_scoped_policy_leaves_the_static_one_alone() {
        let ing = Ingestion::from_sources(&[SCOPED_SIDECARS]).expect("fragment loads");

        assert_eq!(ing.undeclared("sidecars"), Undeclared::Store);
    }

    /// Two fragments naming the same table, each setting fields the other does not.
    fn merged_policies() -> Ingestion {
        Ingestion::from_sources(&[
            r#"{
                "IngestionSchemaVersion": "0.1.0",
                "tables": { "t": { "concepts": ["sub"], "ordered": false,
                    "undeclaredWhen": [{ "selectors": ["suffix == \"a\""], "undeclared": "catalog" }] } }
            }"#,
            r#"{
                "IngestionSchemaVersion": "0.1.0",
                "tables": { "t": { "undeclared": "catalog",
                    "undeclaredWhen": [{ "selectors": ["suffix == \"b\""], "undeclared": "store" }] } }
            }"#,
        ])
        .expect("fragments load")
    }

    /// A later fragment touching one field of a shared table must not drop the others.
    /// `tables.extend` used to replace the whole `TablePolicy`, silently losing an
    /// earlier fragment's `concepts`/`ordered`.
    #[test]
    fn table_policies_merge_field_wise() {
        let ing = merged_policies();

        assert_eq!(ing.materialized_concepts("t"), ["sub"], "concepts survive");
        assert!(!ing.ordered("t"), "ordered survives");
        assert_eq!(ing.undeclared("t"), Undeclared::Catalog, "later field wins");
    }

    /// ...and both fragments' scoped entries are live, in declaration order.
    #[rstest]
    #[case::from_the_first_fragment("a", Undeclared::Catalog)]
    #[case::from_the_second_fragment("b", Undeclared::Store)]
    fn merged_scoped_entries_are_all_live(#[case] suffix: &str, #[case] want: Undeclared) {
        let ing = merged_policies();
        let ctx = FileContext {
            suffix: Some(suffix),
            ..Default::default()
        };

        let got = ing.undeclared_for("t", &ctx);

        assert_eq!(got, want, "suffix {suffix}");
    }

    /// `ignoreKeys` sits on the shared table policy, so a fragment may name it on any table —
    /// but only the JSON parse path consults it, and `sidecars` is the table fed from parsed
    /// JSON. That asymmetry is stated in the metaschema description; this pins the two together
    /// so the description cannot quietly become false.
    #[test]
    fn ignore_keys_is_honoured_on_sidecars_only() {
        let fragment = r#"{
          "IngestionSchemaVersion": "0.1.0",
          "tables": {
            "sidecars": { "ignoreKeys": ["global", "time"] },
            "events":   { "ignoreKeys": ["onset"] }
          }
        }"#;
        let ing = Ingestion::from_sources(&[fragment]).expect("valid fragment");

        // Both parse and are readable — the policy is generic, as the schema allows. That a
        // fragment naming a tabular table is *inert* is asserted end to end in
        // `tests/test_ignore_keys.rs`; here we only pin that the metaschema says so.
        assert_eq!(ing.ignore_keys("sidecars"), ["global", "time"]);
        assert_eq!(ing.ignore_keys("events"), ["onset"]);

        // The metaschema must document the restriction.
        let meta: serde_json::Value =
            serde_json::from_str(bids_schema::INGESTION_METASCHEMA_JSON).unwrap();
        let desc = meta["$defs"]["tablePolicy"]["properties"]["ignoreKeys"]["description"]
            .as_str()
            .expect("ignoreKeys description");
        assert!(
            desc.contains("`sidecars` table only"),
            "metaschema must state where ignoreKeys applies: {desc}"
        );
    }

    /// An `axis` names what the *stored ordinal* means, and an unordered table has no
    /// ordinal — `schema::dynamic`'s `PerRow` arm omits `row_idx` entirely. Refuse at load
    /// rather than emit a view selecting a column that does not exist.
    #[test]
    fn an_axis_on_an_unordered_table_is_refused() {
        let err = Ingestion::from_sources(&[r#"{
            "IngestionSchemaVersion": "0.1.0",
            "tables": {
                "events": {
                    "ordered": false,
                    "describes": { "association": "events", "axis": "volume", "view": "ev" }
                }
            }
        }"#])
        .expect_err("an axis on an unordered table must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("row_idx"), "the error should say why: {msg}");
    }

    /// Two fragments naming the same view is the failure mode `CREATE OR REPLACE VIEW`
    /// cannot report: the second would silently shadow the first. fMRIPrep and QSIPrep both
    /// declare a confounds view, so this is a live risk, not a hypothetical.
    #[test]
    fn two_tables_claiming_one_view_name_are_refused() {
        let err = Ingestion::from_sources(&[r#"{
            "IngestionSchemaVersion": "0.1.0",
            "tables": {
                "fmriprep_confounds": {
                    "describes": { "association": "a", "axis": "volume", "view": "timeseries" }
                },
                "qsiprep_confounds": {
                    "describes": { "association": "b", "axis": "volume", "view": "timeseries" }
                }
            }
        }"#])
        .expect_err("a duplicate view name must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("timeseries"), "name the collision: {msg}");
    }

    /// The bundled fragments must not collide with each other, which is the whole point of
    /// the check above — `--adapter fmriprep --adapter qsiprep` is a supported combination.
    #[test]
    fn every_bundled_ingestion_fragment_can_be_applied_together() {
        let mut sources = vec![bids_schema::bundled_ingestion_source("base").unwrap()];
        for name in bids_schema::BUNDLED_INGESTION_NAMES {
            sources.push(bids_schema::bundled_ingestion_source(name).unwrap());
        }
        let ingestion = Ingestion::from_sources(&sources).expect("bundled fragments co-apply");
        // And the two confounds tables keep distinct views.
        assert_eq!(
            ingestion
                .describes("fmriprep_confounds")
                .and_then(|d| d.view.as_deref()),
            Some("timeseries")
        );
        assert_eq!(
            ingestion
                .describes("qsiprep_confounds")
                .and_then(|d| d.view.as_deref()),
            Some("dwi_timeseries")
        );
    }

    /// `events` declares the relation without an axis or a view: its rows are addressed by
    /// `onset`, and both sides of the relation would want the name `events`. Pinned because
    /// the two optional fields are what let a relation be *recorded* rather than materialized.
    #[test]
    fn events_declares_the_relation_without_a_view() {
        let base = Ingestion::base();
        let events = base.describes("events").expect("events declares describes");
        assert_eq!(events.association, "events");
        assert!(
            events.axis.is_none(),
            "an event row is a time, not a position"
        );
        assert!(events.view.is_none());
        assert!(!base.described_views().any(|(t, _)| t == "events"));

        // Whereas the gradient tables do ask for one.
        let bvals = base.describes("bvals").expect("bvals declares describes");
        assert_eq!(bvals.axis.as_deref(), Some("volume"));
        assert_eq!(bvals.view.as_deref(), Some("bval_volumes"));
    }
}
