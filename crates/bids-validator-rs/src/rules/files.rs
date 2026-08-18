use serde::Deserialize;
use std::collections::HashMap;

use crate::context::{BidsContext, DatasetContext};
use crate::expression::{EvalContext, do_selectors_select};
use crate::issues::{BidsIssue, DatasetIssues, Severity};
use crate::schema::BidsSchema;

#[derive(Debug, Deserialize, Clone)]
pub struct FilesRules {
    pub common: CommonFileRules,
    pub deriv: HashMap<String, HashMap<String, SuffixRule>>,
    pub raw: HashMap<String, HashMap<String, SuffixRule>>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CommonFileRules {
    pub core: HashMap<String, PathOrStemRule>,
    #[serde(rename = "tables")]
    pub table: HashMap<String, StemOrSuffixRule>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum PathOrStemRule {
    Path(PathRule),
    Stem(StemRule),
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum StemOrSuffixRule {
    Stem(StemRule),
    Suffix(SuffixRule),
}

#[derive(Debug, Deserialize, Clone)]
pub struct PathRule {
    pub selectors: Option<Vec<String>>,
    pub level: Option<String>,
    pub path: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StemRule {
    pub selectors: Option<Vec<String>>,
    pub level: Option<String>,
    pub datatypes: Option<Vec<String>>,
    pub stem: String,
    pub extensions: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SuffixRule {
    pub selectors: Option<Vec<String>>,
    pub level: Option<String>,
    pub datatypes: Option<Vec<String>>,
    pub suffixes: Vec<String>,
    pub extensions: Vec<String>,
    pub entities: Option<HashMap<String, EntityRequirement>>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum EntityRequirement {
    String(String),
    Object {
        level: String,
        #[serde(rename = "enum")]
        enum_values: Option<Vec<String>>,
    },
}

impl EntityRequirement {
    pub fn requirement_level(&self) -> String {
        match self {
            Self::String(s) => s.clone(),
            Self::Object { level, .. } => level.clone(),
        }
    }
}

pub fn check_file_rules(
    context: &mut BidsContext,
    ctx_value: &EvalContext,
    dataset_ctx: &DatasetContext,
    schema: &BidsSchema,
    term_maps: &[bids_schema::term_map::TermMap],
    issues: &mut DatasetIssues,
) {
    let mut matches = Vec::new();

    // Check common rules
    check_rule_group(
        &schema.file_rules.common.core,
        "rules.files.common.core",
        context,
        ctx_value,
        &mut matches,
    );
    check_rule_group(
        &schema.file_rules.common.table,
        "rules.files.common.table",
        context,
        ctx_value,
        &mut matches,
    );

    // Check deriv rules for derivative datasets
    if dataset_ctx.dataset_type == "derivative" || context.path.starts_with("/derivatives/") {
        for (group_name, group) in &schema.file_rules.deriv {
            check_rule_group(
                group,
                &format!("rules.files.deriv.{}", group_name),
                context,
                ctx_value,
                &mut matches,
            );
        }
    }

    // Check raw rules
    if dataset_ctx.dataset_type == "raw" {
        for (group_name, group) in &schema.file_rules.raw {
            check_rule_group(
                group,
                &format!("rules.files.raw.{}", group_name),
                context,
                ctx_value,
                &mut matches,
            );
        }
    }

    // Narrowing, mirroring the reference validator's `hasMatch`. Identification is deliberately
    // loose — a suffix rule is claimed by its suffix alone — so several rules routinely co-apply
    // and something has to reduce them before their requirements are read as this file's. Two
    // passes, in this order: the datatype the file sits under, then its entities and extension.
    //
    // Each pass is *discarded if it would eliminate everything*, which is the part that matters
    // and the part a scoring scheme gets wrong. The compiled schema renders an unspecialized
    // parent rule with `datatypes: []` — matching nothing — beside the specializations carrying
    // the real lists: `electrodes` has six rules, two empty parents plus `[eeg, ieeg]`, `[meg]`
    // and `[emg]`. The guard is what keeps a file whose datatype matches no rule at all from
    // losing every candidate and being judged against nothing.
    if matches.len() > 1 {
        let by_datatype: Vec<String> = matches
            .iter()
            .filter(|rule_path| {
                schema
                    .resolve_path(rule_path)
                    .get("datatypes")
                    .and_then(|d| d.as_array())
                    .is_some_and(|permitted| {
                        context.datatype.as_deref().is_some_and(|datatype| {
                            permitted.iter().any(|d| d.as_str() == Some(datatype))
                        })
                    })
            })
            .cloned()
            .collect();
        if !by_datatype.is_empty() {
            matches = by_datatype;
        }
    }
    if matches.len() > 1 {
        let by_entities_extension: Vec<String> = matches
            .iter()
            .filter(|rule_path| entities_extension_in_rule(schema, context, rule_path))
            .cloned()
            .collect();
        if !by_entities_extension.is_empty() {
            matches = by_entities_extension;
        }
    }

    context.filename_rules = matches.clone();

    if matches.is_empty() {
        // Not matched by any BIDS rule. If a configured layout-adapter term map recognizes
        // it (a standardized non-BIDS layout, e.g. FreeSurfer `recon-all`), it is expected,
        // not an error — the same projection bidslake uses to ingest it.
        if bids_schema::term_map::any_recognizes_under(term_maps, &context.path) {
            // Recorded, not just returned on: `errors::system::NotIncluded` decides the same
            // thing again from `filename_rules.is_empty()`, and would re-raise every issue this
            // early return suppresses.
            context.term_map_recognized = true;
            return;
        }
        // No rule matched — file is not recognized by the schema
        if let Some(issue_def) = schema.get_issue("NotIncluded") {
            issues.add_issue(
                &issue_def.code,
                &issue_def.message,
                issue_def.level.unwrap_or(crate::issues::Severity::Error),
                &context.path,
                None,
                None,
            );
        }
        return;
    }

    let key_to_name = &schema.entity_key_to_name;
    // Narrowing usually leaves one rule, and where it leaves several the quietest is reported.
    //
    // The reference validator instead checks *every* survivor and reports all of them, on the
    // stated principle that a wrongly-named file should get as much feedback as possible. That is
    // right for a wrong name and wrong for a right one: `sub-01_acq-crosstalk_meg.fif` is claimed
    // by both `raw.meg.meg` and `raw.meg.crosstalk`, neither of which entities-and-extensions can
    // separate, and the first requires a `task` a crosstalk file does not have. Reporting both
    // fails `ds000248` — a canonical, correct dataset. This crate's integration tests hold every
    // vendored example to zero errors, which is a stronger claim than upstream makes, and it is
    // worth more than matching upstream's noise.
    //
    // So: upstream's narrowing, then a last pick among what it could not separate.
    let mut best_rule_issues = Vec::new();
    let mut best_error_count = usize::MAX;

    for rule_path in matches {
        let rule_val = schema.resolve_path(&rule_path);
        let mut temp_issues = Vec::new();
        let rule_name = rule_path
            .split('.')
            .next_back()
            .unwrap_or(&rule_path)
            .to_string();

        if let Some(entities) = rule_val.get("entities").and_then(|e| e.as_object()) {
            for (entity_key, requirement) in entities {
                let req_str = if let Some(s) = requirement.as_str() {
                    s.to_string()
                } else if let Some(obj) = requirement.as_object() {
                    obj.get("level")
                        .and_then(|l| l.as_str())
                        .unwrap_or("optional")
                        .to_string()
                } else {
                    "optional".to_string()
                };

                if req_str == "required" && !context.entities.contains_key(entity_key) {
                    // Metadata files (.json, .tsv, .bvec, .bval) can exist at any
                    // level of the directory hierarchy as inherited sidecars, so
                    // missing entities are not errors — they simply apply to all
                    // matching files below via the inheritance principle.
                    let is_metadata = context.extension == ".json"
                        || context.extension == ".tsv"
                        || context.extension == ".bvec"
                        || context.extension == ".bval";

                    if is_metadata {
                        continue;
                    }

                    let entity_name = key_to_name
                        .get(entity_key)
                        .map(|s| s.as_str())
                        .unwrap_or(entity_key);
                    temp_issues.push(BidsIssue {
                        code: rule_name.clone(),
                        sub_code: Some(entity_key.clone()),
                        message: format!(
                            "Required entity '{}' ({}) is missing",
                            entity_name, entity_key
                        ),
                        severity: Severity::Error,
                        location: context.path.clone(),
                        rule: Some(rule_path.clone()),
                        sub_message: None,
                    });
                }
            }
        }

        // Mirrors the TS validator's `extensionMismatch`, which runs per matched rule in its
        // `ruleChecks` array. Identification is by suffix alone (see `SuffixRule::match_context`),
        // so this is the downstream check that narrows it — and without it the pick below is not
        // a pick at all. Rules that share a suffix list differ *only* by extension: BEP-011 gives
        // `thickness`/`curv`/`sulc` a `.shape.gii` rule requiring `hemi` and a `.dscalar.nii` rule
        // that does not, so with every candidate scoring zero a GIFTI file would be judged against
        // whichever the walk reached first, and the requirement would never bite.
        //
        // Two schema spellings this has to honour, and upstream's plain `includes` honours
        // neither. `.*` is `objects.extensions.Any` — "Any extension is allowed" — so it matches
        // anything; the two MEG headshape rules (`['.*', '.pos']`) are where that shows. And a
        // *pseudo-file* extension is written with a trailing slash because it names a directory
        // (`.ds/`, `.mefd/`, `.ome.zarr/`), while the parsed extension of the matching path has
        // none — so `.ome.zarr/` must accept `.ome.zarr`, or every CTF, MEF3 and OME-Zarr
        // recording in the corpus reads as a mismatched extension.
        if let Some(datatypes) = rule_val.get("datatypes").and_then(|d| d.as_array()) {
            let permitted: Vec<&str> = datatypes.iter().filter_map(|d| d.as_str()).collect();
            if let Some(datatype) = context.datatype.as_deref()
                && !permitted.contains(&datatype)
            {
                temp_issues.push(BidsIssue {
                    code: "DATATYPE_MISMATCH".to_string(),
                    sub_code: None,
                    message: format!(
                        "Datatype {datatype:?} is not allowed by this rule; it permits {}",
                        if permitted.is_empty() {
                            "none".to_string()
                        } else {
                            permitted.join(", ")
                        }
                    ),
                    severity: Severity::Error,
                    location: context.path.clone(),
                    rule: Some(rule_path.clone()),
                    sub_message: None,
                });
            }
        }

        if let Some(extensions) = rule_val.get("extensions").and_then(|e| e.as_array()) {
            let permitted: Vec<&str> = extensions.iter().filter_map(|e| e.as_str()).collect();
            if !permitted
                .iter()
                .any(|e| *e == ".*" || e.trim_end_matches('/') == context.extension)
            {
                temp_issues.push(BidsIssue {
                    code: "EXTENSION_MISMATCH".to_string(),
                    sub_code: None,
                    message: format!(
                        "Extension {:?} is not allowed by this rule; it permits {}",
                        context.extension,
                        permitted.join(", ")
                    ),
                    severity: Severity::Error,
                    location: context.path.clone(),
                    rule: Some(rule_path.clone()),
                    sub_message: None,
                });
            }
        }

        let error_count = temp_issues
            .iter()
            .filter(|i| matches!(i.severity, Severity::Error))
            .count();
        if error_count < best_error_count {
            best_error_count = error_count;
            best_rule_issues = temp_issues;
        }
    }

    for issue in best_rule_issues {
        issues.add(issue);
    }
}

/// The reference validator's `entitiesExtensionsInRule`, the second narrowing pass.
///
/// A rule fits when the file's extension is one it permits *and* every entity the filename carries
/// is one it declares. Both halves are vacuously true for a rule that declares neither, which is
/// what lets a rule constrain one axis without accidentally constraining the other.
///
/// The entity half is a *subset* test in one direction only: the file may omit entities the rule
/// allows (that is what `optional` means), but an entity the rule has never heard of means the file
/// was written against some other rule. Note this is narrowing, not an error — an unknown entity is
/// `UNKNOWN_ENTITY`'s business, decided against the whole schema rather than one rule.
fn entities_extension_in_rule(schema: &BidsSchema, context: &BidsContext, rule_path: &str) -> bool {
    let rule = schema.resolve_path(rule_path);
    let extension_fits = match rule.get("extensions").and_then(|e| e.as_array()) {
        None => true,
        Some(permitted) => permitted
            .iter()
            .filter_map(|e| e.as_str())
            .any(|e| e == ".*" || e.trim_end_matches('/') == context.extension),
    };
    let entities_fit = match rule.get("entities").and_then(|e| e.as_object()) {
        None => true,
        Some(declared) => context
            .entities
            .keys()
            .all(|key| declared.contains_key(key)),
    };
    extension_fits && entities_fit
}

trait MatchableRule {
    fn selectors(&self) -> Option<&Vec<String>>;
    fn match_context(&self, context: &BidsContext) -> bool;
}

impl MatchableRule for PathRule {
    fn selectors(&self) -> Option<&Vec<String>> {
        self.selectors.as_ref()
    }
    fn match_context(&self, context: &BidsContext) -> bool {
        let expected_path = if self.path.starts_with('/') {
            self.path.clone()
        } else {
            format!("/{}", self.path)
        };
        context.path == expected_path
    }
}

impl MatchableRule for StemRule {
    fn selectors(&self) -> Option<&Vec<String>> {
        self.selectors.as_ref()
    }
    // Mirrors the TS validator's `matchStemRule`: the stem (as a glob) must match, and if the
    // rule names datatypes the file's datatype must be one of them. Extension is not checked
    // during identification.
    fn match_context(&self, context: &BidsContext) -> bool {
        if !glob_match(&self.stem, &context.stem) {
            return false;
        }
        if let Some(datatypes) = &self.datatypes {
            return context
                .datatype
                .as_ref()
                .is_some_and(|dt| datatypes.contains(dt));
        }
        true
    }
}

impl MatchableRule for SuffixRule {
    fn selectors(&self) -> Option<&Vec<String>> {
        self.selectors.as_ref()
    }
    // Mirrors the TS validator's `_findRuleMatches`: a suffix rule is identified by the suffix
    // alone. Datatype/extension are not gated here (a file at e.g. session level with a valid
    // suffix like `headshape` is still recognized). Requirement checks happen downstream.
    fn match_context(&self, context: &BidsContext) -> bool {
        !context.suffix.is_empty() && self.suffixes.iter().any(|s| s == &context.suffix)
    }
}

/// Match a schema glob (only `*` is meaningful) against `text`. Exact string equality when the
/// pattern has no `*`.
fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == text;
    }
    let mut re = String::from("^");
    for ch in pattern.chars() {
        if ch == '*' {
            re.push_str(".*");
        } else {
            re.push_str(&regex::escape(&ch.to_string()));
        }
    }
    re.push('$');
    regex::Regex::new(&re)
        .map(|r| r.is_match(text))
        .unwrap_or(false)
}

impl MatchableRule for PathOrStemRule {
    fn selectors(&self) -> Option<&Vec<String>> {
        match self {
            Self::Path(r) => r.selectors(),
            Self::Stem(r) => r.selectors(),
        }
    }
    fn match_context(&self, context: &BidsContext) -> bool {
        match self {
            Self::Path(r) => r.match_context(context),
            Self::Stem(r) => r.match_context(context),
        }
    }
}

impl MatchableRule for StemOrSuffixRule {
    fn selectors(&self) -> Option<&Vec<String>> {
        match self {
            Self::Stem(r) => r.selectors(),
            Self::Suffix(r) => r.selectors(),
        }
    }
    fn match_context(&self, context: &BidsContext) -> bool {
        match self {
            Self::Stem(r) => r.match_context(context),
            Self::Suffix(r) => r.match_context(context),
        }
    }
}

fn check_rule_group<T: MatchableRule>(
    group: &HashMap<String, T>,
    path_prefix: &str,
    context: &BidsContext,
    ctx_val: &EvalContext,
    matches: &mut Vec<String>,
) {
    for (key, rule) in group {
        let rule_path = format!("{}.{}", path_prefix, key);
        if !do_selectors_select(rule.selectors().map(Vec::as_slice), ctx_val) {
            continue;
        }
        if rule.match_context(context) {
            matches.push(rule_path);
        }
    }
}
