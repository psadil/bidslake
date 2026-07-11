//! A schema-driven model of BIDS **tabular files**.
//!
//! Everything here is derived from the vendored BIDS schema — `rules.tabular_data`
//! (which tables exist and what columns they hold) and `objects.columns` (the true
//! TSV header and type of each column). Nothing is hardcoded.
//!
//! ## What the BIDS schema says
//!
//! `rules.tabular_data` is a map of *groups* (`modality_agnostic`, `eeg`, `pet`, …),
//! each holding one or more **rules**. A rule has:
//! - `selectors` — a list of predicates (all ANDed) deciding which files it applies
//!   to. The tabular subset uses only equality on `path`/`suffix`/`extension`/
//!   `datatype`/`sidecar.*`/`dataset.dataset_description.DatasetType`, plus the
//!   `intersects([suffix], [...])` set test. See [`Selector`].
//! - `columns` — a map of *column key* → requirement level. The key is a
//!   schema-internal identifier (`acq_time__scans`, `name__channels`); the real TSV
//!   header is `objects.columns[key].name` (`acq_time`, `name`). **Never** strip the
//!   `__` suffix to guess the header — `source__optodes`'s name is `source_type`.
//! - `index_columns` / `initial_columns` — column *ordering* hints (always a subset
//!   of `columns`; not used as primary keys, see the crate docs on row identity).
//!
//! The lone structural wrinkle: the `derivatives` group nests one level deeper
//! (`derivatives.common_derivatives.{SegmentationLookup,Descriptions}`). [`Tabular`]
//! finds rules recursively (any node carrying both `selectors` and `columns`), so
//! that nesting needs no special case.
//!
//! ## Rules compose; tables are per-rule
//!
//! Selectors are additive: a `*_blood.tsv` matches `pet.Blood` and up to four
//! sidecar-conditional siblings (`BloodPlasma`, …); a `*_physio.tsv.gz` matches
//! `PhysioColumns` and, when the sidecar says so, `PhysioEyeTracking`. Those
//! conditional rules are **overlays** — they share their base rule's table and just
//! add columns. [`Tabular::load`] groups rules whose *identity* selectors (suffix /
//! datatype / extension / path, i.e. everything except the `sidecar.*` and
//! `DatasetType` conditions) are equal, so each group yields exactly one
//! [`TableSpec`] whose columns are the union across the group.

use super::dynamic::{json_type_to_sql, to_snake_case};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

/// One column of a tabular table, resolved from `objects.columns`.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnSpec {
    /// The schema key (`acq_time__scans`). Kept for provenance/debugging.
    pub key: String,
    /// The real TSV header and DuckDB column name (`acq_time`).
    pub name: String,
    /// The DuckDB column type (`TEXT`, `DOUBLE`, `BIGINT`, `BOOLEAN`, …).
    pub sql_type: String,
}

/// A single `selectors` predicate. Only the forms that actually occur under
/// `rules.tabular_data` are modelled; anything else parses to [`Selector::Unsupported`]
/// and never matches (so a future schema that adds a new selector form fails safe —
/// affected files become unclassified rather than mis-routed).
#[derive(Clone, Debug, PartialEq)]
pub enum Selector {
    /// `path == "/participants.tsv"` — dataset-relative path, leading slash.
    Path(String),
    /// `suffix == "events"`.
    Suffix(String),
    /// `intersects([suffix], ["dseg", "probseg"])` — suffix in a set.
    SuffixIn(Vec<String>),
    /// `extension == ".tsv"`.
    Extension(String),
    /// `datatype == "eeg"` (or single-quoted `'phenotype'`).
    Datatype(String),
    /// `sidecar.PhysioType == "eyetrack"`, `sidecar.PlasmaAvail == true`.
    Sidecar { key: String, value: Value },
    /// `dataset.dataset_description.DatasetType == "derivative"`.
    DatasetType(String),
    /// Any selector string this module does not understand.
    Unsupported(String),
}

impl Selector {
    /// Parse one selector string from the schema.
    pub fn parse(s: &str) -> Selector {
        let s = s.trim();

        // intersects([suffix], ["a", "b"])
        if let Some(rest) = s.strip_prefix("intersects(") {
            let inner = rest.trim_end_matches(')');
            // Expect two array args: the field list and the value list.
            if let Some((lhs, rhs)) = split_top_comma(inner)
                && lhs.trim() == "[suffix]"
            {
                return Selector::SuffixIn(parse_str_array(rhs.trim()));
            }
            return Selector::Unsupported(s.to_string());
        }

        // <lhs> == <rhs>
        let Some((lhs, rhs)) = s.split_once("==") else {
            return Selector::Unsupported(s.to_string());
        };
        let lhs = lhs.trim();
        let rhs_val = parse_literal(rhs.trim());
        let rhs_str = rhs_val.as_str().map(|s| s.to_string());

        match lhs {
            "path" => opt(rhs_str, Selector::Path, s),
            "suffix" => opt(rhs_str, Selector::Suffix, s),
            "extension" => opt(rhs_str, Selector::Extension, s),
            "datatype" => opt(rhs_str, Selector::Datatype, s),
            "dataset.dataset_description.DatasetType" => opt(rhs_str, Selector::DatasetType, s),
            _ => {
                if let Some(key) = lhs.strip_prefix("sidecar.") {
                    Selector::Sidecar {
                        key: key.to_string(),
                        value: rhs_val,
                    }
                } else {
                    Selector::Unsupported(s.to_string())
                }
            }
        }
    }

    /// Is this an *identity* selector (decides which table) rather than a
    /// *condition* (a per-file gate that overlays a rule onto its base table)?
    fn is_identity(&self) -> bool {
        matches!(
            self,
            Selector::Path(_)
                | Selector::Suffix(_)
                | Selector::SuffixIn(_)
                | Selector::Extension(_)
                | Selector::Datatype(_)
        )
    }
}

/// One leaf rule of `rules.tabular_data`.
#[derive(Clone, Debug)]
pub struct TabularRule {
    /// Dotted id, e.g. `pet.Blood`, `derivatives.common_derivatives.Descriptions`.
    pub id: String,
    /// Parsed selectors — used **only** for structural grouping into DDL tables
    /// (`is_identity`/`identity_key`/`datatype`), not for matching.
    pub selectors: Vec<Selector>,
    /// The raw selector strings, evaluated by the shared BIDS expression evaluator for *matching*.
    pub selectors_raw: Vec<String>,
    /// Columns in a stable order: `initial_columns` first, then the rest by key.
    pub columns: Vec<ColumnSpec>,
}

impl TabularRule {
    /// A rule matches a file iff all its selectors evaluate true — delegated to the shared BIDS
    /// expression evaluator (`bids_schema::expression`) over the raw selector strings, so the
    /// full schema selector language is honoured (not bidslake's partial parser).
    pub fn matches(&self, ctx: &FileContext) -> bool {
        let (file, dataset) = ctx.eval_bindings();
        let null = Value::Null;
        let eval = bids_schema::expression::EvalContext::new(&file, &dataset, &null, &null);
        bids_schema::expression::do_selectors_select(&Some(self.selectors_raw.clone()), &eval)
    }

    /// The `datatype ==` selector's value, if the rule has one (used to
    /// disambiguate `channels`/`electrodes` tables across modalities).
    fn datatype(&self) -> Option<&str> {
        self.selectors.iter().find_map(|s| match s {
            Selector::Datatype(d) => Some(d.as_str()),
            _ => None,
        })
    }

    /// A canonical string of just the identity selectors, so rules that differ
    /// only by a `sidecar.*`/`DatasetType` condition group to the same table.
    fn identity_key(&self) -> String {
        let mut parts: Vec<String> = self
            .selectors
            .iter()
            .filter(|s| s.is_identity())
            .map(|s| format!("{s:?}"))
            .collect();
        parts.sort();
        parts.join("&")
    }
}

/// How a table's rows are keyed. Governs the DDL (PK, `file_path`/`row_idx`) and
/// the idempotency strategy at ingest.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::enum_variant_names)] // Per{File,Entity,Row} reads clearly as a set
pub enum RowIdentity {
    /// One row per imaging file; `scans` — PK `(dataset_id, file_path)`, the
    /// registry every `sidecars` row references.
    PerFile,
    /// Entity-keyed; `participants`/`sessions` — no `file_path`/`row_idx`.
    PerEntity,
    /// One row per source line — the default for data tables. No PK; carries a
    /// `row_idx` ordinal.
    PerRow,
}

/// A materialized DuckDB table derived from one or more rules.
#[derive(Clone, Debug)]
pub struct TableSpec {
    pub table: String,
    pub columns: Vec<ColumnSpec>,
    pub identity: RowIdentity,
    /// Whether the table has a `file_path` column (and thus gets the generated
    /// virtual BIDS-concept columns). True for everything except entity tables.
    pub file_based: bool,
    /// The rule ids merged into this table, for provenance/debugging.
    #[allow(dead_code)] // read by tests and forthcoming coverage tooling
    pub rule_ids: Vec<String>,
}

/// The parsed tabular model: every rule, and the tables they roll up into.
#[derive(Clone, Debug)]
pub struct Tabular {
    rules: Vec<TabularRule>,
    tables: Vec<TableSpec>,
    /// rule id → index into `tables`, for [`Tabular::route`].
    rule_table: HashMap<String, usize>,
}

impl Tabular {
    /// Parse the tabular model out of a BIDS `schema.json` value.
    pub fn load(schema: &Value) -> Self {
        let columns_obj = &schema["objects"]["columns"];
        let mut rules = Vec::new();
        if let Some(td) = schema["rules"]["tabular_data"].as_object() {
            for (group, gval) in td {
                collect_rules(group, gval, columns_obj, &mut rules);
            }
        }
        // Deterministic order so table generation is stable across runs.
        rules.sort_by(|a, b| a.id.cmp(&b.id));

        let (tables, rule_table) = build_tables(&rules);
        Tabular {
            rules,
            tables,
            rule_table,
        }
    }

    #[allow(dead_code)] // used by tests; ingest routes via `route`
    pub fn rules(&self) -> &[TabularRule] {
        &self.rules
    }

    pub fn tables(&self) -> &[TableSpec] {
        &self.tables
    }

    /// Every rule whose selectors all pass for `ctx` (additive — a file can match
    /// a base rule and its sidecar-conditional overlays). Used by the headerless
    /// recording ingest, which needs the union of matching rules' columns.
    #[allow(dead_code)] // consumed by the headerless-recording ingest (next change)
    pub fn matching_rules(&self, ctx: &FileContext) -> Vec<&TabularRule> {
        self.rules.iter().filter(|r| r.matches(ctx)).collect()
    }

    /// The table a file routes to, or `None` if no rule matches it. All rules that
    /// match a given file share one table (they differ only by conditions), so the
    /// first match's table is the answer.
    pub fn route(&self, ctx: &FileContext) -> Option<&TableSpec> {
        for r in &self.rules {
            if r.matches(ctx)
                && let Some(&idx) = self.rule_table.get(&r.id)
            {
                return Some(&self.tables[idx]);
            }
        }
        None
    }
}

/// The subset of the BIDS `meta.context` needed to evaluate tabular selectors.
/// Built by the ingest pipeline from a file's path, parsed entities, and merged
/// sidecar. `path` is dataset-relative **with a leading slash** (to match
/// `path == "/participants.tsv"` selectors).
pub struct FileContext<'a> {
    pub path: &'a str,
    pub datatype: Option<&'a str>,
    pub suffix: Option<&'a str>,
    pub extension: Option<&'a str>,
    /// The merged sidecar (inheritance already applied), or `Value::Null`.
    pub sidecar: &'a Value,
    pub dataset_type: Option<&'a str>,
}

impl Default for FileContext<'_> {
    fn default() -> Self {
        FileContext {
            path: "",
            datatype: None,
            suffix: None,
            extension: None,
            sidecar: &Value::Null,
            dataset_type: None,
        }
    }
}

impl FileContext<'_> {
    /// The `(file, dataset)` selector bindings for the shared expression evaluator. Only the
    /// fields BIDS tabular selectors reference are populated; `schema`/`subject` are left null.
    fn eval_bindings(&self) -> (Value, Value) {
        let file = json!({
            "path": self.path,
            "suffix": self.suffix,
            "extension": self.extension,
            "datatype": self.datatype,
            "sidecar": self.sidecar,
        });
        let dataset = json!({
            "dataset_description": { "DatasetType": self.dataset_type },
        });
        (file, dataset)
    }
}

// --- parsing helpers -------------------------------------------------------

/// Recursively collect every rule (a node with both `selectors` and `columns`),
/// so `derivatives.common_derivatives.*` needs no special case.
fn collect_rules(prefix: &str, node: &Value, columns_obj: &Value, out: &mut Vec<TabularRule>) {
    let Some(obj) = node.as_object() else { return };
    if obj.contains_key("selectors") && obj.contains_key("columns") {
        out.push(parse_rule(prefix, obj, columns_obj));
        return;
    }
    for (k, v) in obj {
        collect_rules(&format!("{prefix}.{k}"), v, columns_obj, out);
    }
}

fn parse_rule(
    id: &str,
    rule: &serde_json::Map<String, Value>,
    columns_obj: &Value,
) -> TabularRule {
    let selectors = rule
        .get("selectors")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(Selector::parse)
                .collect()
        })
        .unwrap_or_default();

    // The raw selector strings, kept verbatim for the shared expression evaluator (matching).
    let selectors_raw: Vec<String> = rule
        .get("selectors")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    // Column keys in a stable order: initial_columns first, then the rest sorted.
    let colmap = rule.get("columns").and_then(|v| v.as_object());
    let mut ordered: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    if let Some(inits) = rule.get("initial_columns").and_then(|v| v.as_array()) {
        for k in inits.iter().filter_map(|v| v.as_str()) {
            if colmap.is_some_and(|m| m.contains_key(k)) && seen.insert(k) {
                ordered.push(k.to_string());
            }
        }
    }
    if let Some(m) = colmap {
        let mut keys: Vec<&str> = m.keys().map(|s| s.as_str()).collect();
        keys.sort();
        for k in keys {
            if seen.insert(k) {
                ordered.push(k.to_string());
            }
        }
    }

    let columns = ordered
        .into_iter()
        .map(|key| {
            let def = columns_obj.get(&key);
            let name = def
                .and_then(|d| d.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or(&key)
                .to_string();
            let sql_type = def.map(json_type_to_sql).unwrap_or_else(|| "TEXT".to_string());
            ColumnSpec { key, name, sql_type }
        })
        .collect();

    TabularRule {
        id: id.to_string(),
        selectors,
        selectors_raw,
        columns,
    }
}

/// Group rules into tables by identity, unioning columns across each group.
fn build_tables(rules: &[TabularRule]) -> (Vec<TableSpec>, HashMap<String, usize>) {
    // Preserve first-seen order of identity keys for stable output.
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, r) in rules.iter().enumerate() {
        let key = r.identity_key();
        groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            Vec::new()
        });
        groups.get_mut(&key).unwrap().push(i);
    }

    let mut tables = Vec::new();
    let mut rule_table = HashMap::new();
    for key in &order {
        let members = &groups[key];
        // Base rule = fewest selectors (i.e. no conditions); it names the table
        // and its columns come first.
        let base = *members
            .iter()
            .min_by_key(|&&i| rules[i].selectors.len())
            .unwrap();
        let table = table_name(&rules[base].id, rules[base].datatype());
        let identity = row_identity(&table);
        let file_based = identity != RowIdentity::PerEntity;

        // Union columns: base first, then overlays, deduped by resolved name.
        let mut columns: Vec<ColumnSpec> = Vec::new();
        let mut by_name: HashMap<String, usize> = HashMap::new();
        let ordered_members = std::iter::once(base).chain(members.iter().copied().filter(|&i| i != base));
        let mut rule_ids = Vec::new();
        for i in ordered_members {
            rule_ids.push(rules[i].id.clone());
            rule_table.insert(rules[i].id.clone(), tables.len());
            for c in &rules[i].columns {
                if let Some(&pos) = by_name.get(&c.name) {
                    // Same header from two rules with different types → widen to
                    // TEXT. (Does not occur in the current schema; defensive.)
                    if columns[pos].sql_type != c.sql_type {
                        columns[pos].sql_type = "TEXT".to_string();
                    }
                } else {
                    by_name.insert(c.name.clone(), columns.len());
                    columns.push(c.clone());
                }
            }
        }

        tables.push(TableSpec {
            table,
            columns,
            identity,
            file_based,
            rule_ids,
        });
    }
    (tables, rule_table)
}

/// Table name from the base rule's name, snake-cased.
///
/// A rule named `XColumns` describes the columns of table `X`, so a trailing
/// `_columns` is dropped (`PhysioColumns` → `physio`, `PhysioEventsColumns` →
/// `physio_events`). BIDS spells intracranial EEG `iEEG`, which snake-cases to the
/// split token `i_eeg`; the datatype directory carries the canonical `ieeg`, so we
/// prefer it there (`iEEGChannels` → `ieeg_channels`).
fn table_name(rule_id: &str, datatype: Option<&str>) -> String {
    let leaf = rule_id.rsplit('.').next().unwrap_or(rule_id);
    let mut name = to_snake_case(leaf);
    if let Some(stripped) = name.strip_suffix("_columns") {
        name = stripped.to_string();
    }
    if name.starts_with("i_eeg")
        && let Some(dt) = datatype
    {
        name = name.replacen("i_eeg", dt, 1);
    }
    name
}

/// Policy: which tables are entity-keyed or file-registry tables rather than the
/// per-row default. This is BIDS structural knowledge (participants/sessions are
/// entities; `scans` is the imaging-file registry) not expressible in
/// `rules.tabular_data`.
fn row_identity(table: &str) -> RowIdentity {
    match table {
        "participants" | "sessions" => RowIdentity::PerEntity,
        "scans" => RowIdentity::PerFile,
        _ => RowIdentity::PerRow,
    }
}

/// `opt(Some(x), Ctor, orig)` → `Ctor(x)`; `opt(None, …)` → `Unsupported(orig)`.
fn opt(v: Option<String>, ctor: fn(String) -> Selector, orig: &str) -> Selector {
    match v {
        Some(x) => ctor(x),
        None => Selector::Unsupported(orig.to_string()),
    }
}

/// Parse a scalar literal: quoted string, `true`/`false`, number, else raw string.
fn parse_literal(s: &str) -> Value {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        return Value::String(s[1..s.len() - 1].to_string());
    }
    match s {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    if let Ok(n) = s.parse::<i64>() {
        return Value::Number(n.into());
    }
    if let Ok(f) = s.parse::<f64>()
        && let Some(n) = serde_json::Number::from_f64(f)
    {
        return Value::Number(n);
    }
    Value::String(s.to_string())
}

/// Parse `["a", "b"]` into `["a", "b"]`.
fn parse_str_array(s: &str) -> Vec<String> {
    let inner = s.trim().trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(|x| x.trim())
        .filter(|x| !x.is_empty())
        .map(|x| match parse_literal(x) {
            Value::String(s) => s,
            other => other.to_string(),
        })
        .collect()
}

/// Split `a, b` on the top-level comma (not inside brackets). Used for the two
/// args of `intersects(...)`.
fn split_top_comma(s: &str) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '[' | '(' => depth += 1,
            ']' | ')' => depth -= 1,
            ',' if depth == 0 => return Some((&s[..i], &s[i + 1..])),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        serde_json::from_str(bids_schema::SCHEMA_JSON).unwrap()
    }

    #[test]
    fn selector_parses_every_form() {
        assert_eq!(
            Selector::parse("path == \"/participants.tsv\""),
            Selector::Path("/participants.tsv".into())
        );
        assert_eq!(
            Selector::parse("suffix == \"events\""),
            Selector::Suffix("events".into())
        );
        assert_eq!(
            Selector::parse("extension == \".tsv\""),
            Selector::Extension(".tsv".into())
        );
        // single-quoted datatype
        assert_eq!(
            Selector::parse("datatype == 'phenotype'"),
            Selector::Datatype("phenotype".into())
        );
        assert_eq!(
            Selector::parse("sidecar.PlasmaAvail == true"),
            Selector::Sidecar {
                key: "PlasmaAvail".into(),
                value: Value::Bool(true)
            }
        );
        assert_eq!(
            Selector::parse("sidecar.PhysioType == \"eyetrack\""),
            Selector::Sidecar {
                key: "PhysioType".into(),
                value: Value::String("eyetrack".into())
            }
        );
        assert_eq!(
            Selector::parse("dataset.dataset_description.DatasetType == \"derivative\""),
            Selector::DatasetType("derivative".into())
        );
        assert_eq!(
            Selector::parse("intersects([suffix], [\"dseg\", \"probseg\"])"),
            Selector::SuffixIn(vec!["dseg".into(), "probseg".into()])
        );
    }

    #[test]
    fn unknown_selector_is_unsupported() {
        // Unsupported forms parse to `Unsupported` so they never count as an *identity*
        // selector (grouping); matching itself is delegated to the shared expression evaluator.
        assert!(matches!(
            Selector::parse("entities.subject == \"01\""),
            Selector::Unsupported(_)
        ));
    }

    /// Every rule name maps to the intended table name.
    #[test]
    fn table_names_are_as_expected() {
        let t = Tabular::load(&schema());
        let names: HashSet<&str> = t.tables().iter().map(|s| s.table.as_str()).collect();
        for expected in [
            "participants",
            "samples",
            "scans",
            "sessions",
            "phenotype",
            "events",
            "behavioral",
            "eeg_channels",
            "eeg_electrodes",
            "emg_channels",
            "emg_electrodes",
            "ieeg_channels",
            "ieeg_electrodes",
            "meg_channels",
            "motion_channels",
            "nirs_channels",
            "nirs_optodes",
            "asl_context",
            "blood",
            "physio",
            "physio_events",
            "descriptions",
            "segmentation_lookup",
        ] {
            assert!(names.contains(expected), "missing table {expected}; got {names:?}");
        }
        // 23 rules → 23 tables (the 5 conditional rules overlay their bases).
        assert_eq!(t.tables().len(), 23, "tables: {names:?}");
    }

    #[test]
    fn table_names_are_unique() {
        let t = Tabular::load(&schema());
        let mut seen = HashSet::new();
        for spec in t.tables() {
            assert!(seen.insert(&spec.table), "duplicate table {}", spec.table);
        }
    }

    #[test]
    fn no_table_has_duplicate_columns() {
        let t = Tabular::load(&schema());
        for spec in t.tables() {
            let mut seen = HashSet::new();
            for c in &spec.columns {
                assert!(
                    seen.insert(&c.name),
                    "table {} has duplicate column {}",
                    spec.table,
                    c.name
                );
            }
        }
    }

    /// Bug #2 fix: the `acq_time__scans` key resolves to header `acq_time`.
    #[test]
    fn scans_has_acq_time_not_the_schema_key() {
        let t = Tabular::load(&schema());
        let scans = t.tables().iter().find(|s| s.table == "scans").unwrap();
        assert!(scans.columns.iter().any(|c| c.name == "acq_time"));
        assert!(!scans.columns.iter().any(|c| c.name.contains("__")));
        assert_eq!(scans.identity, RowIdentity::PerFile);
    }

    /// Bug #1 fix: events gets its real schema columns, not just onset/duration.
    #[test]
    fn events_has_full_columns() {
        let t = Tabular::load(&schema());
        let events = t.tables().iter().find(|s| s.table == "events").unwrap();
        let names: HashSet<&str> = events.columns.iter().map(|c| c.name.as_str()).collect();
        for c in ["onset", "duration", "trial_type", "response_time", "stim_file", "HED"] {
            assert!(names.contains(c), "events missing {c}: {names:?}");
        }
    }

    /// The physio overlay: PhysioColumns ∪ PhysioEyeTracking in one table.
    #[test]
    fn physio_overlay_unions_columns() {
        let t = Tabular::load(&schema());
        let physio = t.tables().iter().find(|s| s.table == "physio").unwrap();
        let names: Vec<&str> = physio.columns.iter().map(|c| c.name.as_str()).collect();
        // base columns come first, overlay after
        assert_eq!(
            names,
            vec![
                "cardiac",
                "respiratory",
                "trigger",
                "timestamp",
                "x_coordinate",
                "y_coordinate",
                "pupil_size",
            ]
        );
        assert!(physio.rule_ids.len() >= 2, "expected overlay rules: {:?}", physio.rule_ids);
    }

    /// Blood: base + 4 conditional overlays collapse into one `blood` table.
    #[test]
    fn blood_conditional_rules_overlay_one_table() {
        let t = Tabular::load(&schema());
        let blood = t.tables().iter().find(|s| s.table == "blood").unwrap();
        assert!(blood.rule_ids.len() >= 4, "rule_ids: {:?}", blood.rule_ids);
        let names: HashSet<&str> = blood.columns.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains("time"));
        assert!(names.contains("plasma_radioactivity"));
        assert!(names.contains("whole_blood_radioactivity"));
    }

    /// Column types resolve through anyOf and definition.Format.
    #[test]
    fn column_types_resolve() {
        let t = Tabular::load(&schema());
        let events = t.tables().iter().find(|s| s.table == "events").unwrap();
        let onset = events.columns.iter().find(|c| c.name == "onset").unwrap();
        assert_eq!(onset.sql_type, "DOUBLE");
        // participants.age is a definition.Format=number column.
        let parts = t.tables().iter().find(|s| s.table == "participants").unwrap();
        let age = parts.columns.iter().find(|c| c.name == "age").unwrap();
        assert_eq!(age.sql_type, "DOUBLE");
    }

    /// End-to-end routing for representative files.
    #[test]
    fn routing() {
        let t = Tabular::load(&schema());

        let participants = FileContext {
            path: "/participants.tsv",
            suffix: Some("participants"),
            extension: Some(".tsv"),
            ..Default::default()
        };
        assert_eq!(t.route(&participants).map(|s| s.table.as_str()), Some("participants"));

        let eeg_ch = FileContext {
            path: "/sub-01/eeg/sub-01_task-x_channels.tsv",
            datatype: Some("eeg"),
            suffix: Some("channels"),
            extension: Some(".tsv"),
            ..Default::default()
        };
        assert_eq!(t.route(&eeg_ch).map(|s| s.table.as_str()), Some("eeg_channels"));

        // A physio file without the eyetrack sidecar → physio, one matching rule.
        let physio = FileContext {
            path: "/sub-01/func/sub-01_task-x_physio.tsv.gz",
            datatype: Some("func"),
            suffix: Some("physio"),
            extension: Some(".tsv.gz"),
            ..Default::default()
        };
        assert_eq!(t.route(&physio).map(|s| s.table.as_str()), Some("physio"));
        assert_eq!(t.matching_rules(&physio).len(), 1);

        // With the eyetrack sidecar → still physio, now two matching rules.
        let sidecar = serde_json::json!({ "PhysioType": "eyetrack" });
        let eye = FileContext {
            sidecar: &sidecar,
            ..physio
        };
        assert_eq!(t.route(&eye).map(|s| s.table.as_str()), Some("physio"));
        assert_eq!(t.matching_rules(&eye).len(), 2);

        // A motion.tsv has no tabular_data rule → no route (handled specially).
        let motion = FileContext {
            path: "/sub-01/motion/sub-01_task-x_tracksys-y_motion.tsv",
            datatype: Some("motion"),
            suffix: Some("motion"),
            extension: Some(".tsv"),
            ..Default::default()
        };
        assert!(t.route(&motion).is_none());
    }
}
