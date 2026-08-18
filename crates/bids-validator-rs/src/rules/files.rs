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
    let mut best_rule_issues = Vec::new();
    // Ranked by (does the file's extension fit this rule, how many errors) — lower is better.
    // Error count alone ties too often to be a choice: a `_thickness.shape.gii` missing its `hemi`
    // scores one error against the GIFTI rule (the missing entity) and one against the CIFTI rule
    // (the extension), and first-past-the-post then reports whichever the walk reached first. A
    // rule that does not permit the file's extension is not the rule the file was written
    // against, so it loses to one that does even when it would have had less to say.
    let mut best_rank = (usize::MAX, usize::MAX);

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
        let extension_fits =
            usize::from(temp_issues.iter().any(|i| i.code == "EXTENSION_MISMATCH"));
        if (extension_fits, error_count) < best_rank {
            best_rank = (extension_fits, error_count);
            best_rule_issues = temp_issues;
        }
    }

    for issue in best_rule_issues {
        issues.add(issue);
    }
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
