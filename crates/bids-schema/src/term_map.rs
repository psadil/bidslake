//! BIDS **term maps**: declarative projections that map a standardized-but-non-BIDS file
//! path onto BIDS concepts, following BIDS Extension Proposal 043 ("BIDS Term Mapping").
//!
//! A term map is a list of rules; each rule has a PCRE `Template` matched against a
//! dataset-relative path, whose named capture groups bind BIDS entities, plus literal
//! `Entities`/`Concepts`/`Metadata`. Where an [`overlay`](crate::overlay) extends the BIDS
//! schema *vocabulary*, a term map recognizes files that have no BIDS name at all
//! (FreeSurfer's `sub-01/stats/aseg.stats`) and projects each onto a [`FileFacts`] tuple.
//!
//! This module is pure and I/O-free (`&str -> Option<FileFacts>`) so both `bidslake` and the
//! `bids-validator-rs` validator can consume it. It does **not** read file bodies or decide
//! what to do with a matched file — that is the job of bidslake's ingestion schema and
//! content readers. A term-map document is validated against a hand-written JSON-Schema
//! metaschema ([`TERM_MAPPING_METASCHEMA_JSON`]).
//!
//! PCRE is one of the two `Template` syntaxes BEP-043 floats; we pin it (versioned by the
//! document's `BIDSMapVersion`) and support the subset the `regex` crate provides (named
//! groups, optional groups, character classes — no look-around/back-references), which is
//! sufficient to collapse, e.g., FreeSurfer's `sub-01_ses-1` / `sub-01` / `01` subject-dir
//! forms into one rule. That collapsing is also why a term map only *reads*: there is no
//! single filename to render an optional group back into, so this module has no `render`.
//! Naming a file a pipeline is about to write is [`layout`](crate::layout)'s job — a
//! separate document whose mandatory `Examples` are rendered and fed back through the term
//! map it names, so the two directions are checked against each other (ADR 0002 §12).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value};

/// The hand-written JSON-Schema metaschema (draft 2020-12) for term-map documents.
pub const TERM_MAPPING_METASCHEMA_JSON: &str = include_str!("../data/term-mapping-metaschema.json");

/// Capture-group / entity-name aliases: BEP-043 uses the long forms, BIDS keys are short.
const ENTITY_ALIASES: &[(&str, &str)] = &[("subject", "sub"), ("session", "ses")];

fn alias_entity(name: &str) -> &str {
    ENTITY_ALIASES
        .iter()
        .find(|(from, _)| *from == name)
        .map(|(_, to)| *to)
        .unwrap_or(name)
}

/// An error loading or compiling a term map. Typed (not `anyhow`) as this is a library
/// boundary; still composes into `anyhow` via `?`.
#[derive(Debug, thiserror::Error)]
pub enum TermMapError {
    #[error("reading term map {path}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing term map {path} as JSON")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("term map does not conform to the term-mapping metaschema:\n{}", .violations.join("\n"))]
    Invalid { violations: Vec<String> },
    #[error("term-map template `{template}` is not a valid regular expression: {source}")]
    BadTemplate {
        template: String,
        #[source]
        source: regex::Error,
    },
}

// ---------------------------------------------------------------------------
// On-disk format (BEP-043 Term Mapping).
// ---------------------------------------------------------------------------

/// A parsed term-map document.
#[derive(Debug, Clone, Deserialize)]
pub struct TermMapFile {
    #[serde(rename = "BIDSVersion", default)]
    pub bids_version: Option<String>,
    #[serde(rename = "BIDSMapVersion", default)]
    pub bids_map_version: Option<String>,
    #[serde(rename = "Mappings", default)]
    pub mappings: Vec<Mapping>,
}

/// One BEP-043 mapping rule.
#[derive(Debug, Clone, Deserialize)]
pub struct Mapping {
    /// A PCRE matched against a dataset-relative path; named groups bind BIDS entities.
    #[serde(rename = "Template")]
    pub template: String,
    /// Literal BIDS entity -> value pairs (constants not captured by the template).
    #[serde(rename = "Entities", default)]
    pub entities: BTreeMap<String, String>,
    #[serde(rename = "Concepts", default)]
    pub concepts: Concepts,
    #[serde(rename = "Metadata", default)]
    pub metadata: Map<String, Value>,
}

/// BEP-043 `Concepts`: the BIDS `datatype`/`suffix` a mapped file represents.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Concepts {
    #[serde(default)]
    pub datatype: Option<String>,
    #[serde(default)]
    pub suffix: Option<String>,
}

// ---------------------------------------------------------------------------
// Classification output.
// ---------------------------------------------------------------------------

/// The BIDS concepts a term map projects onto a path. `datatype`/`suffix`/`extension` feed
/// the ingestion selectors; `entities` populate materialized concept columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFacts {
    pub entities: BTreeMap<String, String>,
    pub datatype: Option<String>,
    pub suffix: Option<String>,
    pub extension: Option<String>,
    pub metadata: Map<String, Value>,
}

impl FileFacts {
    /// Look up an entity value by (aliased) BIDS key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entities.get(key).map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// Compiled term map.
// ---------------------------------------------------------------------------

struct CompiledMapping {
    regex: regex::Regex,
    spec: Mapping,
}

/// A compiled, ready-to-classify term map.
pub struct TermMap {
    mappings: Vec<CompiledMapping>,
    set: regex::RegexSet,
}

impl TermMap {
    /// Compile a parsed document. Each `Template` is anchored and compiled as a regex.
    pub fn from_file(file: TermMapFile) -> Result<Self, TermMapError> {
        let mut mappings = Vec::with_capacity(file.mappings.len());
        let mut patterns = Vec::with_capacity(file.mappings.len());
        for m in file.mappings {
            let anchored = format!("^(?:{})$", m.template);
            let regex =
                regex::Regex::new(&anchored).map_err(|source| TermMapError::BadTemplate {
                    template: m.template.clone(),
                    source,
                })?;
            patterns.push(anchored);
            mappings.push(CompiledMapping { regex, spec: m });
        }
        let set = regex::RegexSet::new(&patterns).map_err(|source| TermMapError::BadTemplate {
            template: "<set>".to_string(),
            source,
        })?;
        Ok(TermMap { mappings, set })
    }

    /// Every BIDS concept this term map is capable of projecting onto some path:
    /// the literal `Entities` keys, the named capture groups of each `Template`
    /// (aliased to BIDS short keys), and whichever of `datatype`/`suffix` its
    /// `Concepts` set.
    ///
    /// This is the *static* upper bound over all mappings, not what any one path
    /// yields, and it is deliberately derived rather than declared: a term map
    /// already states what it projects, so asking an author to repeat that in a
    /// second artifact would be a second source of truth. bidslake uses it to
    /// decide which generated concept columns must consult the stored projection
    /// (see `bidslake::schema::dynamic`) — a set worth keeping tight, because every
    /// column in it pays a `COALESCE` on read.
    ///
    /// `extension` is never included: it is read off the filename, which is
    /// authoritative even for a projected path.
    pub fn projectable_concepts(&self) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        for m in &self.mappings {
            out.extend(m.spec.entities.keys().map(|k| alias_entity(k).to_string()));
            out.extend(
                m.regex
                    .capture_names()
                    .flatten()
                    .map(|n| alias_entity(n).to_string()),
            );
            if m.spec.concepts.datatype.is_some() {
                out.insert("datatype".to_string());
            }
            if m.spec.concepts.suffix.is_some() {
                out.insert("suffix".to_string());
            }
        }
        out
    }

    /// Project a dataset-relative path onto BIDS concepts, or `None` if no rule matches.
    pub fn classify(&self, rel_path: &str) -> Option<FileFacts> {
        let idx = self.set.matches(rel_path).into_iter().next()?;
        let mapping = &self.mappings[idx];
        let caps = mapping.regex.captures(rel_path)?;

        // Named capture groups -> entities (aliased to BIDS short keys).
        let mut entities: BTreeMap<String, String> = BTreeMap::new();
        for name in mapping.regex.capture_names().flatten() {
            if let Some(m) = caps.name(name) {
                entities.insert(alias_entity(name).to_string(), m.as_str().to_string());
            }
        }
        // Literal Entities override/augment.
        for (k, v) in &mapping.spec.entities {
            entities.insert(k.clone(), v.clone());
        }

        Some(FileFacts {
            entities,
            datatype: mapping.spec.concepts.datatype.clone(),
            suffix: mapping.spec.concepts.suffix.clone(),
            extension: filename_extension(rel_path),
            metadata: mapping.spec.metadata.clone(),
        })
    }
}

/// The extension of the final path component, from its first `.` (BIDS filename semantics).
fn filename_extension(path: &str) -> Option<String> {
    let fname = path.rsplit('/').next().unwrap_or(path);
    fname.find('.').map(|i| fname[i..].to_string())
}

// ---------------------------------------------------------------------------
// Validation + registry.
// ---------------------------------------------------------------------------

/// Validate a term-map document against [`TERM_MAPPING_METASCHEMA_JSON`]. Returns the list of
/// violations (empty on success).
pub fn validate_term_map(document: &Value) -> Vec<String> {
    let metaschema: Value = serde_json::from_str(TERM_MAPPING_METASCHEMA_JSON)
        .expect("embedded term-mapping metaschema must parse");
    let validator = jsonschema::validator_for(&metaschema)
        .expect("term-mapping metaschema must compile as a JSON Schema");
    let mut violations: Vec<String> = validator
        .iter_errors(document)
        .map(|e| format!("  at `{}`: {e}", e.instance_path()))
        .collect();
    violations.sort();
    violations.dedup();
    violations
}

/// Term maps bidslake ships, addressable by name.
pub const BUNDLED_TERM_MAP_NAMES: &[&str] = &["freesurfer", "feat"];

/// The raw JSON source of a bundled term map, or `None` if `name` is not bundled.
pub fn bundled_term_map_source(name: &str) -> Option<&'static str> {
    Some(match name {
        "freesurfer" => include_str!("../data/term-maps/freesurfer.json"),
        "feat" => include_str!("../data/term-maps/feat.json"),
        _ => return None,
    })
}

/// The parsed+compiled bundled term map for `name` (build-tested, hence `expect`).
pub fn bundled_term_map(name: &str) -> Option<TermMap> {
    let raw = bundled_term_map_source(name)?;
    let file: TermMapFile = serde_json::from_str(raw).expect("bundled term map must be valid JSON");
    Some(TermMap::from_file(file).expect("bundled term map must compile"))
}

/// Read, validate, parse, and compile a term map from disk.
pub fn load_term_map(path: &Path) -> Result<TermMap, TermMapError> {
    let display = path.display().to_string();
    let content = std::fs::read_to_string(path).map_err(|source| TermMapError::Read {
        path: display.clone(),
        source,
    })?;
    let document: Value = serde_json::from_str(&content).map_err(|source| TermMapError::Parse {
        path: display.clone(),
        source,
    })?;
    let violations = validate_term_map(&document);
    if !violations.is_empty() {
        return Err(TermMapError::Invalid { violations });
    }
    let file: TermMapFile =
        serde_json::from_value(document).map_err(|source| TermMapError::Parse {
            path: display,
            source,
        })?;
    TermMap::from_file(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn fs() -> TermMap {
        bundled_term_map("freesurfer").expect("bundled")
    }

    /// Every bundled term map, not just one — a new map that violates the metaschema or
    /// fails to compile should fail here rather than at a user's first ingest. Mirrors
    /// `bundled_ingestion_is_metaschema_valid` in `bidslake::schema::ingestion`.
    #[test]
    fn bundled_term_maps_are_metaschema_valid() {
        for name in BUNDLED_TERM_MAP_NAMES {
            let raw = bundled_term_map_source(name)
                .unwrap_or_else(|| panic!("term map {name:?} is registered but missing"));
            let doc: Value = serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("bundled term map {name:?} is not JSON: {e}"));
            let violations = validate_term_map(&doc);
            assert!(
                violations.is_empty(),
                "bundled term map {name:?} invalid: {violations:?}"
            );
            // And it must actually compile, not merely validate.
            bundled_term_map(name)
                .unwrap_or_else(|| panic!("bundled term map {name:?} does not compile"));
        }
    }

    #[test]
    fn malformed_term_map_is_rejected() {
        // A mapping missing the required `Template`.
        let doc = serde_json::json!({
            "BIDSVersion": "1.11.1", "BIDSMapVersion": "0.1.0",
            "Mappings": [ { "Concepts": { "datatype": "anat" } } ]
        });
        assert!(!validate_term_map(&doc).is_empty());
    }

    /// The three ways a FreeSurfer subject directory can be named, each yielding the same
    /// `aseg.stats` classification.
    #[rstest]
    #[case::bids_subject_and_session("sub-01_ses-1/stats/aseg.stats", "01", Some("1"))]
    #[case::bids_subject_only("sub-02/stats/aseg.stats", "02", None)]
    // A bare label, which is what `recon-all` writes when nobody imposes BIDS naming.
    #[case::bare_label("03/stats/aseg.stats", "03", None)]
    fn pcre_collapses_all_subject_dir_forms(
        #[case] path: &str,
        #[case] sub: &str,
        #[case] ses: Option<&str>,
    ) {
        let f = fs()
            .classify(path)
            .unwrap_or_else(|| panic!("no match: {path}"));

        assert_eq!(
            (
                f.get("sub"),
                f.get("ses"),
                f.get("seg"),
                f.suffix.as_deref()
            ),
            (Some(sub), ses, Some("aseg"), Some("segstats"))
        );
    }

    #[test]
    fn aparc_captures_hemi_and_parc_variants() {
        let f = fs()
            .classify("sub-01_ses-1/stats/lh.aparc.a2009s.stats")
            .expect("match");
        assert_eq!(f.get("hemi"), Some("lh"));
        assert_eq!(f.get("parc"), Some("aparc.a2009s"));
        assert_eq!(f.suffix.as_deref(), Some("parcstats"));
    }

    #[test]
    fn surface_and_volume_project_to_anat() {
        let s = fs().classify("bert/surf/lh.thickness").expect("surf");
        assert_eq!(s.datatype.as_deref(), Some("anat"));
        assert_eq!(s.get("hemi"), Some("lh"));
        let v = fs().classify("bert/mri/aparc+aseg.mgz").expect("mri");
        assert_eq!(v.datatype.as_deref(), Some("anat"));
    }

    /// The volumetric segmentations are the ones downstream code actually reaches for
    /// (`wmparc.mgz` above all), so they carry `seg` rather than being swallowed by the
    /// `mri/*.mgz` catch-all — which binds no entity and left consumers string-matching
    /// the path. Order matters: `RegexSet` yields the lowest matching index, so the
    /// specific mapping must precede the catch-all.
    #[rstest]
    #[case("sub-10113_ses-V1/mri/wmparc.mgz", "wmparc")]
    #[case("bert/mri/aseg.mgz", "aseg")]
    #[case("bert/mri/aparc+aseg.mgz", "aparc+aseg")]
    #[case("bert/mri/aparc.a2009s+aseg.mgz", "aparc.a2009s+aseg")]
    #[case("bert/mri/aparc.DKTatlas+aseg.mgz", "aparc.DKTatlas+aseg")]
    fn volumetric_segmentations_carry_seg(#[case] path: &str, #[case] seg: &str) {
        let f = fs()
            .classify(path)
            .unwrap_or_else(|| panic!("no match: {path}"));

        assert_eq!(
            (f.get("seg"), f.suffix.as_deref(), f.datatype.as_deref()),
            (Some(seg), Some("dseg"), Some("anat")),
            "{path}"
        );
    }

    /// ...and the catch-all still claims every other volume, so adding the specific
    /// mapping above it cannot make a file stop being recognized.
    #[rstest]
    #[case("bert/mri/T1.mgz")]
    #[case("bert/mri/brainmask.mgz")]
    #[case("bert/mri/orig.mgz")]
    fn other_volumes_still_match_the_catch_all(#[case] path: &str) {
        let f = fs()
            .classify(path)
            .unwrap_or_else(|| panic!("no match: {path}"));

        assert_eq!(
            (f.datatype.as_deref(), f.get("seg")),
            (Some("anat"), None),
            "{path}"
        );
    }

    #[test]
    fn unrelated_path_is_none() {
        assert!(
            fs().classify("sub-01/func/sub-01_task-rest_bold.nii.gz")
                .is_none()
        );
    }

    /// A census over a real `recon-all` subject established that the specific mappings above
    /// reach about a third of the tree; the rest is whole subtrees, not edge cases. Anything
    /// this map leaves unclassified is invisible to concept queries *and* raises `NotIncluded`
    /// in `bids-validator-rs` (`rules::files::term_map_recognizes`), so recognition — not
    /// projection — is what a subtree catch-all is for. One path per subtree that was missed,
    /// so a narrowing shows up here rather than on a user's tree.
    #[rstest]
    // label/: only *.ctab and ?h.*.annot were covered, and .label is the bulk of it.
    #[case("bert/label/lh.cortex.label")]
    #[case("bert/label/lh.BA1_exvivo.thresh.label")]
    #[case("bert/label/aparc.annot.ctab")]
    // stats/: the parc alternation named aparc*/BA_exvivo* only.
    #[case("bert/stats/sclimbic.stats")]
    #[case("bert/stats/lh.curv.stats")]
    #[case("bert/stats/synthseg.vol.csv")]
    // mri/: both mappings required a direct child ending in .mgz.
    #[case("bert/mri/orig/001.mgz")]
    #[case("bert/mri/transforms/talairach.xfm")]
    #[case("bert/mri/transforms/synthmorph.mni305/log/run.log")]
    #[case("bert/mri/samseg/samseg.fs.stats")]
    #[case("bert/mri/wm1.txt")]
    // surf/: the mapping required a ?h. prefix.
    #[case("bert/surf/autodet.gw.stats.lh.dat")]
    // Bookkeeping subtrees, recognized but projecting nothing.
    #[case("bert/scripts/recon-all.log")]
    #[case("bert/scripts/log/label-cortex.lh.log")]
    #[case("bert/touch/wmsegment.touch")]
    #[case("bert/tmp/filled.edits.txt")]
    #[case("bert/trash/anything")]
    #[case("bert/README.txt")]
    // The mirrored-hemisphere subtree, whose data half is cataloged like the top-level one
    // and whose bookkeeping half is only recognized.
    #[case("bert/xhemi/surf/lh.area")]
    #[case("bert/xhemi/mri/transforms/talairach.xfm")]
    #[case("bert/xhemi/label/lh.aparc.annot")]
    #[case("bert/xhemi/scripts/recon-all.done")]
    #[case("bert/xhemi/touch/talairach.touch")]
    fn every_recon_all_subtree_is_recognized(#[case] path: &str) {
        let f = fs()
            .classify(path)
            .unwrap_or_else(|| panic!("unrecognized: {path}"));

        // The subject must survive every mapping: it is what `scans.sub` projects from,
        // and what the participants stub is synthesized from.
        assert_eq!(f.get("sub"), Some("bert"), "{path}");
    }

    /// A catch-all states `datatype` but never `suffix`, because `suffix` is what the
    /// ingestion fragment dispatches a *reader* on (`data/ingestion/freesurfer.json` routes
    /// `segstats`/`parcstats` to `fs_stats`). Claiming `segstats` for every `stats/*.stats`
    /// would send files like `sclimbic.stats` through that reader into `freesurfer_aseg` with
    /// no `seg` to tell them apart. Recognition is the catch-all's job; reading more stats
    /// families needs a `seg` capture per family and is tracked separately.
    #[rstest]
    #[case("bert/stats/sclimbic.stats")]
    #[case("bert/stats/qa.stats")]
    #[case("bert/stats/lh.curv.stats")]
    #[case("bert/label/lh.cortex.label")]
    #[case("bert/mri/wm1.txt")]
    #[case("bert/surf/autodet.gw.stats.lh.dat")]
    fn catch_alls_claim_no_suffix_so_no_reader_is_dispatched(#[case] path: &str) {
        let f = fs().classify(path).expect(path);

        assert_eq!(
            (f.datatype.as_deref(), f.suffix.as_deref()),
            (Some("anat"), None),
            "{path}"
        );
    }

    /// A real `SUBJECTS_DIR` holds FreeSurfer's shipped template subjects beside the study's
    /// own — `fsaverage` and friends. They look exactly like a subject directory, so the
    /// subject-capturing mappings would bind `sub = "fsaverage"`, catalog the template as study
    /// data and mint a participant for it. The regex crate has no lookaround, so an earlier
    /// mapping claims them instead: recognized (no validator noise) with nothing projected.
    #[rstest]
    #[case("fsaverage/label/lh.PALS_B12_Brodmann.annot")]
    #[case("fsaverage/mri/aseg.mgz")]
    #[case("fsaverage5/surf/lh.white")]
    #[case("fsaverage_sym/stats/aseg.stats")]
    #[case("cvs_avg35_inMNI152/mri/norm.mgz")]
    #[case("lh.EC_average/mri/T1.mgz")]
    fn template_subjects_are_recognized_but_not_participants(#[case] path: &str) {
        let f = fs()
            .classify(path)
            .unwrap_or_else(|| panic!("unrecognized: {path}"));

        assert_eq!(
            (f.get("sub"), f.datatype.as_deref(), f.suffix.as_deref()),
            (None, None, None),
            "{path}"
        );
    }

    /// ...and a study subject whose label merely starts with the same letters is unaffected,
    /// which is what keeps the template mappings from being a blunt prefix match.
    #[test]
    fn a_subject_named_like_a_template_is_still_a_subject() {
        let real = fs()
            .classify("fsaverageStudy01/mri/aseg.mgz")
            .expect("subject");

        assert_eq!(real.get("sub"), Some("fsaverageStudy01"));
    }

    /// The subtree catch-alls must not claim concepts they cannot know. A log or a touch file
    /// is not anatomical data, so it is recognized with nothing projected — the distinction
    /// `docs/adr/0002` §12 draws for catch-alls.
    #[rstest]
    #[case("bert/scripts/recon-all.log")]
    #[case("bert/touch/wmsegment.touch")]
    #[case("bert/xhemi/scripts/recon-all.done")]
    #[case("bert/README.txt")]
    fn bookkeeping_subtrees_project_no_concepts(#[case] path: &str) {
        let f = fs().classify(path).expect(path);

        assert_eq!(
            (f.datatype.as_deref(), f.suffix.as_deref()),
            (None, None),
            "{path}"
        );
    }

    /// ...while `xhemi`'s data half is anat, like the top-level tree it mirrors.
    #[rstest]
    #[case("bert/xhemi/surf/lh.area")]
    #[case("bert/xhemi/mri/aparc+aseg.mgz")]
    fn the_mirrored_hemisphere_data_half_is_anat(#[case] path: &str) {
        let f = fs().classify(path).expect(path);

        assert_eq!(f.datatype.as_deref(), Some("anat"), "{path}");
    }

    /// Adding the catch-alls must not shadow the mappings that carry concepts. `RegexSet`
    /// yields the lowest matching index, so this pins the ordering the file depends on.
    #[test]
    fn specific_mappings_still_win_over_the_new_catch_alls() {
        let tm = fs();
        let seg = tm.classify("bert/stats/aseg.stats").expect("aseg");
        assert_eq!(seg.get("seg"), Some("aseg"));
        assert_eq!(seg.suffix.as_deref(), Some("segstats"));

        let parc = tm.classify("bert/stats/lh.aparc.stats").expect("aparc");
        assert_eq!(parc.get("parc"), Some("aparc"));
        assert_eq!(parc.suffix.as_deref(), Some("parcstats"));

        let dseg = tm.classify("bert/mri/wmparc.mgz").expect("wmparc");
        assert_eq!(dseg.get("seg"), Some("wmparc"));
        assert_eq!(dseg.suffix.as_deref(), Some("dseg"));

        let ctab = tm.classify("bert/label/aparc.annot.ctab").expect("ctab");
        assert_eq!(ctab.suffix.as_deref(), Some("fslabels"));

        // A hemisphere surface keeps `hemi` rather than falling to the `surf/` catch-all.
        let surf = tm.classify("bert/surf/lh.thickness").expect("surf");
        assert_eq!(surf.get("hemi"), Some("lh"));
    }

    /// The set drives DDL (which concept columns consult the projection), so it must
    /// cover literal `Entities`, named capture groups, and `Concepts` alike — and
    /// stay tight, since every member costs a `COALESCE` on read.
    #[test]
    fn projectable_concepts_span_every_source() {
        let got = fs().projectable_concepts();
        let want: std::collections::BTreeSet<String> =
            ["datatype", "hemi", "parc", "seg", "ses", "sub", "suffix"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert_eq!(got, want);
    }

    /// `extension` comes off the filename even for a projected path, so wrapping it
    /// would buy nothing and cost a COALESCE on every row.
    #[test]
    fn projectable_concepts_exclude_extension() {
        assert!(!fs().projectable_concepts().contains("extension"));
    }

    /// Capture groups use BEP-043's long forms; the DDL needs BIDS short keys.
    #[test]
    fn projectable_concepts_are_aliased() {
        let got = fs().projectable_concepts();
        assert!(got.contains("sub") && got.contains("ses"));
        assert!(!got.contains("subject") && !got.contains("session"));
    }
}
