//! **Layouts**: the concepts-to-path direction, for naming a file that does not exist yet.
//!
//! A [term map](crate::term_map) answers *what does this path denote?* — enough to index a
//! tree someone else wrote. A pipeline that *writes* such a tree needs the other direction:
//! given a role (`the highres-to-standard affine`), where does it go? Without that, every
//! consumer hardcodes the convention, which is how a script ends up with two dozen
//! properties that are nothing but string joins.
//!
//! A separate document from the term map, not a term map run backwards: a term map is
//! non-invertible, because the PCRE collapsing that lets one mapping cover a whole class of
//! paths leaves no single filename to render back. ADR 0002 measures that on a `recon-all`
//! tree and settles it. So a term map keeps PCRE and stays read-only, while a layout declares
//! the roles that *can* be written, in a syntax that can write them.
//!
//! ## What stops the two drifting
//!
//! Every layout must carry `Examples`, and `Layout::validate_round_trip` renders **every role
//! under every example** and feeds the result back through the named term map. If
//! `classify(render(role))` does not reproduce the role's declared concepts, the layout
//! **fails to load**, so a rename on either side is an error at load time rather than a tree
//! nothing can read back.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::term_map::TermMap;

/// The hand-written JSON-Schema metaschema (draft 2020-12) for layout documents.
pub const LAYOUT_METASCHEMA_JSON: &str = include_str!("../data/layout-metaschema.json");

/// An error loading, validating, or rendering a layout.
#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    /// [`load_layout`] could not read the file. Only the on-disk entry point raises this; a
    /// bundled layout is `include_str!`d and never touches the filesystem.
    #[error("reading layout {path}")]
    Read {
        /// The path as displayed, carried separately because an [`std::io::Error`] does not
        /// name the file it failed on.
        path: String,
        /// The underlying read failure — no such file, no permission, not UTF-8.
        #[source]
        source: std::io::Error,
    },
    /// The text is not JSON, or — having already passed the metaschema — does not deserialize
    /// into [`LayoutFile`]. The second case is a disagreement between
    /// [`LAYOUT_METASCHEMA_JSON`] and these structs rather than anything the author did, since
    /// the metaschema types every key it names, and the untyped `x_` extensions it also admits
    /// (against the empty schema `{}`, so they may hold any JSON at all) are ones serde drops.
    #[error("parsing layout {path} as JSON")]
    Parse {
        /// The file path, or whatever label [`load_layout_str`] was handed when the document
        /// did not come from disk: a bundled layout's name, `"<test>"`.
        path: String,
        /// The serde failure, whose own message carries the line and column or the offending
        /// key.
        #[source]
        source: serde_json::Error,
    },
    /// The document is JSON but violates [`LAYOUT_METASCHEMA_JSON`] — a role with no
    /// `Template`, an absent or empty `Examples`, a role name outside `^[a-zA-Z0-9_]+$`, a
    /// stray key that is not `x_`-prefixed.
    #[error("layout does not conform to the layout metaschema:\n{}", .violations.join("\n"))]
    Invalid {
        /// Every violation [`validate_layout`] found, each rendered ``  at `<instance path>`:
        /// <message>`` — the pointer is itself backticked, which is what a grep or a snapshot
        /// review sees. Sorted lexicographically on the whole formatted string, then
        /// deduplicated, so the list is stable across runs and is not in document order.
        /// Lexicographic is pointer-major but not numeric: `` at `/Examples/10` `` lands before
        /// `` at `/Examples/2` ``. Never empty when this variant is constructed.
        violations: Vec<String>,
    },
    /// A role's template — or, at render time, the interpolated path — is one `check_template`
    /// refuses: empty, absolute, or carrying a `..` segment. Raised while loading.
    /// [`Layout::render`] applies the same check to the interpolated path but drops the error
    /// for a `None`, so a caller only ever receives this variant from a load.
    #[error("layout role {role:?} has an unsafe template {template:?}: {reason}")]
    UnsafeTemplate {
        /// The offending role's name — the only part of this variant that is a key in the
        /// document, since `template` may be an interpolated string that appears nowhere in it.
        role: String,
        /// The rejected string — the document's `Template` at load time, but the *interpolated*
        /// result when the check runs inside [`Layout::render`], where a binding rather than
        /// the document supplied the `..`.
        template: String,
        /// Which rule was broken, drawn from a closed set of three phrases (`is empty`, `is
        /// absolute`, and the `..` traversal one), which is why it is a `&'static str` and not
        /// a `String`. The tests assert on it exactly: the bundled term map recognizes none of
        /// the unsafe renders either, so `is_err()` alone stays true with the traversal guard
        /// deleted, and naming the reason is what ties the test to the guard.
        reason: &'static str,
    },
    /// The document's `TermMap` is not in [`crate::term_map::BUNDLED_TERM_MAP_NAMES`]. A term
    /// map is resolved by name from the bundled set only — there is no way to point a layout
    /// at a term-map file on disk, even when the layout itself was loaded from one.
    #[error("layout names term map {0:?}, which is not bundled")]
    UnknownTermMap(String),
    /// The two directions disagree — the whole point of `Examples`.
    #[error(
        "layout role {role:?} renders to {path:?}, which term map {term_map:?} {detail}. \
         The layout and the term map describe the same tree and must agree; fix whichever \
         is wrong."
    )]
    RoundTrip {
        /// The role whose declaration the projection contradicts. Whichever of the two
        /// documents is wrong, this is the name to search for in both.
        role: String,
        /// The full path that was classified — an [`Example::root`] with the role's rendered
        /// [`Role::template`] joined under it, not the template itself, since the template
        /// alone is not something the term map could be asked about.
        path: String,
        /// [`LayoutFile::term_map`] verbatim — the name the document declared, already resolved
        /// to a bundled term map by the time the round trip can fail, so it is the same in every
        /// `RoundTrip` error one load produces. It is here so the message names both documents
        /// that have to agree, not to distinguish this failure from its siblings.
        term_map: String,
        /// How the projection differed, phrased as a fragment completing the message's
        /// "…which term map `feat` __": `does not recognize at all` when nothing matched, or
        /// `reads back with suffix Some("mask"), not "bold"` / `reads back with desc=None, not
        /// "clean"` naming the first declaration that disagreed. Only that first one appears —
        /// `datatype`, then `suffix`, then entities in key order, and the load stops there.
        detail: String,
    },
}

// ---------------------------------------------------------------------------
// On-disk format.
// ---------------------------------------------------------------------------

/// A parsed layout document: the `data/layouts/<name>.json` artifact an author writes, one
/// key per struct field.
///
/// [`LAYOUT_METASCHEMA_JSON`] is the authority on the format and this is only its Rust
/// shadow — [`load_layout_str`] validates against the metaschema *before* deserializing, so
/// every field below has already been type-checked and every required key is present by the
/// time serde runs. All four keys are required. A key not listed here is a validation error
/// unless it is `x_`-prefixed, which the metaschema admits on the document, on a role and on an
/// example — but not inside `Concepts` or `Entities`, whose key rules leave no room for it
/// (`additionalProperties: false` with no `^x_` pattern, and `^[a-zA-Z0-9]+$` respectively) —
/// and which this struct discards wherever it is legal.
#[derive(Debug, Clone, Deserialize)]
pub struct LayoutFile {
    /// Version of the *layout document format*, not of the tool whose tree it describes
    /// (`feat.json` says `0.1.0`; FSL's own version appears nowhere in it). Required, but
    /// nothing in this crate reads it: a document declaring a version this build has never
    /// heard of loads exactly like one that does not, and there is no migration path.
    #[serde(rename = "LayoutVersion")]
    pub layout_version: String,
    /// The bundled term map that reads back what this layout writes. Resolved by name through
    /// [`crate::term_map::bundled_term_map`] alone, so a producer with no term map cannot have
    /// a layout — the correct order, since a tree nothing can read back is a tree bidslake
    /// cannot index (ADR 0002).
    #[serde(rename = "TermMap")]
    pub term_map: String,
    /// Role name -> where that artifact sits, relative to a unit's output root. A *role* is
    /// the stable name for a slot in the tree ("the highres-to-standard affine"), independent
    /// of the filename convention that expresses it; role names are the consumer API
    /// (`out["highres2standard_mat"]`), so renaming one breaks callers while changing its
    /// [`Role::template`] does not (ADR 0002). At least one is required, and names are
    /// confined to `^[a-zA-Z0-9_]+$` — a hyphen or a dot in a role name fails validation.
    /// `BTreeMap`, so [`Layout::roles`] and the round trip walk them in lexicographic order,
    /// never the document's.
    #[serde(rename = "Roles")]
    pub roles: BTreeMap<String, Role>,
    /// Unit output roots to check every role against, required and non-empty. This is the only
    /// thing binding the write direction to the read one, so a layout cannot opt out of the
    /// check by declaring none (ADR 0002). Each is rendered under in turn and the first
    /// disagreement aborts the load, so a document with two broken roles reports one.
    #[serde(rename = "Examples")]
    pub examples: Vec<Example>,
}

/// Where one named artifact sits, relative to a unit's output root.
#[derive(Debug, Clone, Deserialize)]
pub struct Role {
    /// A POSIX path relative to the unit's output root, with `{name}` placeholders filled from
    /// the bindings supplied at render time. The only required key of a role. Most FEAT-style
    /// slots are wholly literal (`reg/highres.nii.gz`) — placeholders are for what genuinely
    /// varies, such as FIX's training set and threshold in
    /// `fix4melview_{training}_thr{threshold}.txt`. Must be relative and free of any `..`
    /// segment, checked at load time and again on the interpolated result, because bindings
    /// are substituted verbatim (`check_template`).
    #[serde(rename = "Template")]
    pub template: String,
    /// The `datatype`/`suffix` the named term map must project onto this role's rendered path.
    /// Optional, and each half is optional in turn: whatever is unset is simply not compared,
    /// so a role that declares no `Concepts` at all must still be *recognized* by the term map
    /// but need agree about nothing. Declaring them is what turns a rename on either side into
    /// a load-time error instead of files a pipeline writes and the index silently drops.
    #[serde(rename = "Concepts", default)]
    pub concepts: RoleConcepts,
    /// BIDS entities the term map must project onto this role on top of [`Role::concepts`] —
    /// `from`/`to`/`mode` on a registration transform, `desc` on a derivative. Empty by
    /// default, which compares nothing. Keys are looked up through
    /// [`crate::term_map::FileFacts::get`], which sees entities under BIDS *short* names, so
    /// write `sub`/`ses` and not BEP-043's `subject`/`session`; the metaschema further confines
    /// them to `^[a-zA-Z0-9]+$`, with no underscore — unlike role and binding names.
    ///
    /// The comparison is one-directional: entities the term map projects but this role omits
    /// pass, while an entity declared here that the projection does not reproduce fails the
    /// load. That asymmetry is deliberate, and it is what stops the block being repurposed as
    /// a query for the file that will *fill* the role — a role names a destination, not a
    /// source (ADR 0002).
    #[serde(rename = "Entities", default)]
    pub entities: BTreeMap<String, String>,
    /// What this slot is, for a human reading the layout rather than the tree; `None` when the
    /// author wrote none. Nothing validates or renders it — it reaches Python through
    /// `PyLayout::description` and is surfaced verbatim as `bidslake.Layout.describe(role)`,
    /// which is the only reason it is carried past the parse.
    #[serde(rename = "Description", default)]
    pub description: Option<String>,
}

/// The datatype/suffix a role's rendered path must project onto.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RoleConcepts {
    /// The BIDS datatype the round trip must reproduce — `func` for FEAT's
    /// `filtered_func_data.nii.gz`, `anat` for `reg/highres.nii.gz`. `None` leaves the rendered
    /// path's datatype unconstrained; it does not assert the term map projects none.
    #[serde(default)]
    pub datatype: Option<String>,
    /// The BIDS suffix the round trip must reproduce — `bold`, `mask`, `xfm`, `components`.
    /// `None` leaves it unconstrained on the same terms as [`RoleConcepts::datatype`]: there is
    /// no way to declare that a role's path must project *no* suffix.
    #[serde(default)]
    pub suffix: Option<String>,
}

/// A unit output root to validate every role against.
#[derive(Debug, Clone, Deserialize)]
pub struct Example {
    /// A unit output-root directory name, relative to the dataset root — for FEAT, the BIDS
    /// stem of the run the tree was built from
    /// (`sub-01_ses-V1_task-rest_run-01_desc-preproc_bold`). Required. Every rendered role is
    /// joined under it, one `/` with trailing slashes trimmed, and the join handed to the term
    /// map, so this has to be a string that term map can parse: it carries the entities — `sub`,
    /// `task`, `run` — that the roles' own filenames do not. A plausible-looking but
    /// unparseable root such as `out/` makes every role fail to classify and the whole layout
    /// fail to load.
    #[serde(rename = "Root")]
    pub root: String,
    /// Values for the `{name}` placeholders this layout's roles use.
    ///
    /// Their job is validation: a pipeline writing a real tree supplies its own at
    /// [`Layout::render`], and nothing here reaches *that* path. They are readable via
    /// [`Layout::examples`] all the same, because a caller materializing the tree a layout
    /// merely *describes* — a synthetic dataset, a probe for a bundle under development — needs
    /// a binding set the round trip has already vouched for.
    ///
    /// A role whose placeholders are not all bound here is **skipped by this example**, and
    /// skipped silently: a placeholder role is checked only if some example binds it, and
    /// nothing — not the metaschema, not the load — detects a role that no example covers.
    /// `feat.json` binds `training`, `threshold` and `rater` in both of its examples for
    /// precisely that reason.
    #[serde(rename = "Bindings", default)]
    pub bindings: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// Compiled layout.
// ---------------------------------------------------------------------------

/// A validated layout: every role renders, and every rendered role reads back.
#[derive(Debug, Clone)]
pub struct Layout {
    file: LayoutFile,
}

impl Layout {
    /// Validate and compile a parsed document, including the round trip through
    /// `term_map` (see the module docs).
    pub fn from_file(file: LayoutFile, term_map: &TermMap) -> Result<Self, LayoutError> {
        for (role, spec) in &file.roles {
            check_template(role, &spec.template)?;
        }
        let layout = Layout { file };
        layout.validate_round_trip(term_map)?;
        Ok(layout)
    }

    /// The document's role names, sorted.
    pub fn roles(&self) -> Vec<&str> {
        self.file.roles.keys().map(String::as_str).collect()
    }

    /// The named term map this layout is checked against.
    pub fn term_map_name(&self) -> &str {
        &self.file.term_map
    }

    /// The document's `Examples`, in document order.
    ///
    /// Exposed because they are the only *worked* instance of this layout anybody wrote down: a
    /// root that the term map can parse, and a binding for every placeholder the roles use.
    /// A caller that wants to materialize the tree a layout describes — a synthetic dataset, a
    /// probe for an adapter bundle under development — would otherwise have to invent both, and
    /// an invented root that the term map cannot parse produces a tree where nothing classifies,
    /// which looks exactly like a broken term map.
    ///
    /// These are still validation data first: the round trip every load runs is what makes them
    /// trustworthy, and a role no example binds is checked by nothing.
    pub fn examples(&self) -> &[Example] {
        &self.file.examples
    }

    /// The declaration for one role.
    pub fn role(&self, name: &str) -> Option<&Role> {
        self.file.roles.get(name)
    }

    /// Render `role`'s path relative to a unit's output root, interpolating `bindings`
    /// into any `{name}` placeholders.
    ///
    /// `None` if the role is unknown or a placeholder is unbound — an unbound placeholder
    /// must not silently render as empty, since that produces a plausible-looking path
    /// pointing at the wrong file.
    /// `None` also when the bindings would render a path that leaves the root — see
    /// `check_template`, which runs here against the interpolated result as well as at load
    /// time against the template.
    pub fn render(&self, role: &str, bindings: &BTreeMap<String, String>) -> Option<String> {
        let spec = self.file.roles.get(role)?;
        let rendered = interpolate(&spec.template, bindings)?;
        // `check_template` ran at load time against the *template*, which is the half of the
        // guarantee the document controls. Binding values are substituted verbatim, so a
        // caller's `training = "../.."` reconstitutes exactly what the template was checked
        // for. Re-checking the rendered path is what makes the guarantee whole.
        check_template(role, &rendered).ok()?;
        Some(rendered)
    }

    /// Render every role under every example and check the named term map reads each back
    /// as the role declares. This is what keeps the write and read directions honest.
    fn validate_round_trip(&self, term_map: &TermMap) -> Result<(), LayoutError> {
        for example in &self.file.examples {
            for (role, spec) in &self.file.roles {
                let Some(rel) = interpolate(&spec.template, &example.bindings) else {
                    continue; // this example does not bind the role's placeholders
                };
                let path = format!("{}/{}", example.root.trim_end_matches('/'), rel);
                let fail = |detail: String| LayoutError::RoundTrip {
                    role: role.clone(),
                    path: path.clone(),
                    term_map: self.file.term_map.clone(),
                    detail,
                };
                let Some(facts) = term_map.classify(&path) else {
                    return Err(fail("does not recognize at all".to_string()));
                };
                if let Some(want) = &spec.concepts.datatype
                    && facts.datatype.as_deref() != Some(want.as_str())
                {
                    return Err(fail(format!(
                        "reads back with datatype {:?}, not {want:?}",
                        facts.datatype
                    )));
                }
                if let Some(want) = &spec.concepts.suffix
                    && facts.suffix.as_deref() != Some(want.as_str())
                {
                    return Err(fail(format!(
                        "reads back with suffix {:?}, not {want:?}",
                        facts.suffix
                    )));
                }
                for (key, want) in &spec.entities {
                    if facts.get(key) != Some(want.as_str()) {
                        return Err(fail(format!(
                            "reads back with {key}={:?}, not {want:?}",
                            facts.get(key)
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Substitute `{name}` placeholders. `None` if any placeholder is unbound.
fn interpolate(template: &str, bindings: &BTreeMap<String, String>) -> Option<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let close = rest[open..].find('}')? + open;
        out.push_str(&rest[..open]);
        out.push_str(bindings.get(&rest[open + 1..close])?);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// A path must stay findable from the root it is joined to: it is appended to a
/// caller-supplied directory, so an absolute path or a `..` would silently address something
/// outside the tree.
///
/// "Findable from the root" rather than "textually inside it", because the two come apart. A
/// rendered `sub-01.feat/x_../../../etc/cron.d/y` still *starts with* the root and normalizes
/// outside it, so a `starts_with` check reports a safety that is not there. And the root may be
/// an `s3://` URI, where joining is pure concatenation and `..` is a literal key component — so
/// a traversal segment does not escape, it addresses a different object that will never be
/// found again. One rule avoids both: no traversal segment at all.
///
/// Applied at load time to the template, which is the half the document controls, and again in
/// [`Layout::render`] to the interpolated result, which is the half the caller controls.
fn check_template(role: &str, template: &str) -> Result<(), LayoutError> {
    let bad = |reason| LayoutError::UnsafeTemplate {
        role: role.to_string(),
        template: template.to_string(),
        reason,
    };
    if template.is_empty() {
        return Err(bad("is empty"));
    }
    if template.starts_with('/') {
        return Err(bad("is absolute"));
    }
    if template.split('/').any(|c| c == "..") {
        return Err(bad("escapes the output root via `..`"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Validation + registry.
// ---------------------------------------------------------------------------

/// Validate a layout document against [`LAYOUT_METASCHEMA_JSON`]. Returns the list of
/// violations (empty on success).
pub fn validate_layout(document: &Value) -> Vec<String> {
    let metaschema: Value = serde_json::from_str(LAYOUT_METASCHEMA_JSON)
        .expect("embedded layout metaschema must parse");
    let validator = jsonschema::validator_for(&metaschema)
        .expect("layout metaschema must compile as a JSON Schema");
    let mut violations: Vec<String> = validator
        .iter_errors(document)
        .map(|e| format!("  at `{}`: {e}", e.instance_path()))
        .collect();
    violations.sort();
    violations.dedup();
    violations
}

/// Layouts bidslake ships, addressable by name.
pub const BUNDLED_LAYOUT_NAMES: &[&str] = &["feat", "freesurfer"];

/// The raw JSON source of a bundled layout, or `None` if `name` is not bundled.
pub fn bundled_layout_source(name: &str) -> Option<&'static str> {
    Some(match name {
        "feat" => include_str!("../data/layouts/feat.json"),
        "freesurfer" => include_str!("../data/layouts/freesurfer.json"),
        _ => return None,
    })
}

/// Parse and compile a layout document, resolving its `TermMap` from the bundled set.
pub fn load_layout_str(raw: &str, path: &str) -> Result<Layout, LayoutError> {
    let document: Value = serde_json::from_str(raw).map_err(|source| LayoutError::Parse {
        path: path.to_string(),
        source,
    })?;
    let violations = validate_layout(&document);
    if !violations.is_empty() {
        return Err(LayoutError::Invalid { violations });
    }
    let file: LayoutFile =
        serde_json::from_value(document).map_err(|source| LayoutError::Parse {
            path: path.to_string(),
            source,
        })?;
    let term_map = crate::term_map::bundled_term_map(&file.term_map)
        .ok_or_else(|| LayoutError::UnknownTermMap(file.term_map.clone()))?;
    Layout::from_file(file, &term_map)
}

/// The compiled bundled layout for `name` (build-tested, hence `expect`).
pub fn bundled_layout(name: &str) -> Option<Layout> {
    let raw = bundled_layout_source(name)?;
    Some(load_layout_str(raw, name).expect("bundled layout must load"))
}

/// Read, validate, parse, and compile a layout from disk.
pub fn load_layout(path: &Path) -> Result<Layout, LayoutError> {
    let display = path.display().to_string();
    let content = std::fs::read_to_string(path).map_err(|source| LayoutError::Read {
        path: display.clone(),
        source,
    })?;
    load_layout_str(&content, &display)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;

    fn feat() -> Layout {
        bundled_layout("feat").expect("bundled")
    }

    fn bindings(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn bundled_layouts_are_metaschema_valid_and_round_trip() {
        for name in BUNDLED_LAYOUT_NAMES {
            let raw = bundled_layout_source(name)
                .unwrap_or_else(|| panic!("layout {name:?} is registered but missing"));
            let doc: Value = serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("bundled layout {name:?} is not JSON: {e}"));
            let violations = validate_layout(&doc);
            assert!(
                violations.is_empty(),
                "bundled layout {name:?} invalid: {violations:?}"
            );
            // Loading runs the round trip, so this is the drift check itself.
            bundled_layout(name).unwrap_or_else(|| panic!("bundled layout {name:?} does not load"));
        }
    }

    #[test]
    fn renders_a_literal_role() {
        assert_eq!(
            feat()
                .render("filtered_func_clean", &BTreeMap::new())
                .as_deref(),
            Some("filtered_func_data_clean.nii.gz")
        );
        assert_eq!(
            feat()
                .render("highres2standard_mat", &BTreeMap::new())
                .as_deref(),
            Some("reg/highres2standard.mat")
        );
    }

    #[test]
    fn renders_placeholders_and_refuses_unbound_ones() {
        let b = bindings(&[("training", "UKBiobank"), ("threshold", "1")]);
        assert_eq!(
            feat().render("classification", &b).as_deref(),
            Some("fix4melview_UKBiobank_thr1.txt")
        );
        // An unbound placeholder must not render as empty: that produces a
        // plausible-looking path pointing at the wrong file.
        assert_eq!(feat().render("classification", &BTreeMap::new()), None);
    }

    #[test]
    fn unknown_role_is_none() {
        assert_eq!(feat().render("no_such_role", &BTreeMap::new()), None);
    }

    /// The point of `Examples`: a role whose rendered path the term map reads back
    /// differently must not load.
    #[test]
    fn round_trip_mismatch_is_rejected() {
        let raw = r#"{
            "LayoutVersion": "0.1.0",
            "TermMap": "feat",
            "Roles": {
                "wrong": {
                    "Template": "reg/highres.nii.gz",
                    "Concepts": { "datatype": "func", "suffix": "bold" }
                }
            },
            "Examples": [{ "Root": "sub-01_ses-V1_task-rest_run-01_desc-preproc_bold" }]
        }"#;
        let err = load_layout_str(raw, "<test>").expect_err("must reject");
        let msg = err.to_string();
        assert!(msg.contains("wrong"), "{msg}");
        assert!(msg.contains("must agree"), "{msg}");
    }

    /// The entity half of the same check, and the case ADR 0002 argues the destination/source
    /// distinction from: `desc-preproc` is the entity that would *select* fMRIPrep's T1w, and
    /// declaring it on the role FEAT writes its own copy to makes the two directions disagree.
    /// The role and its mapping can gain entities together — they now declare `desc-brain` —
    /// but that moves the destination's description; it never supplies the source's.
    #[test]
    fn a_declared_entity_the_projection_contradicts_is_rejected() {
        let raw = r#"{
            "LayoutVersion": "0.1.0",
            "TermMap": "feat",
            "Roles": {
                "highres": {
                    "Template": "reg/highres.nii.gz",
                    "Concepts": { "datatype": "anat", "suffix": "T1w" },
                    "Entities": { "desc": "preproc" }
                }
            },
            "Examples": [{ "Root": "sub-01_ses-V1_task-rest_run-01_desc-preproc_bold" }]
        }"#;

        let err = load_layout_str(raw, "<test>").expect_err("must reject");

        // The concepts agree, so only the entity comparison can be what rejected this.
        assert!(
            matches!(&err, LayoutError::RoundTrip { role, detail, .. }
                     if role == "highres" && detail.contains(r#"desc=Some("brain"), not "preproc""#)),
            "{err:?}"
        );
    }

    /// A role the term map does not recognize at all is the other half of the same check:
    /// the layout would write a file nothing can read back.
    #[test]
    fn unrecognized_render_is_rejected() {
        let raw = r#"{
            "LayoutVersion": "0.1.0",
            "TermMap": "feat",
            "Roles": { "stray": { "Template": "not/a/feat/slot.txt" } },
            "Examples": [{ "Root": "sub-01_ses-V1_task-rest_run-01_desc-preproc_bold" }]
        }"#;
        let err = load_layout_str(raw, "<test>").expect_err("must reject");
        assert!(err.to_string().contains("does not recognize"), "{err}");
    }

    /// The rejection must come from `check_template`, not from the round-trip that runs after
    /// it: the feat term map recognizes none of these renders either, so `is_err()` alone stays
    /// true with the whole path-traversal guard deleted. Naming the variant and the reason is
    /// what ties the test to the guard.
    #[rstest]
    #[case::absolute("/etc/passwd", "is absolute")]
    #[case::parent_escape("../escape.txt", "escapes the output root via `..`")]
    #[case::empty("", "is empty")]
    fn unsafe_templates_are_rejected(#[case] template: &str, #[case] reason: &str) {
        let raw = format!(
            r#"{{"LayoutVersion":"0.1.0","TermMap":"feat",
                "Roles":{{"r":{{"Template":{}}}}},
                "Examples":[{{"Root":"x"}}]}}"#,
            serde_json::to_string(template).unwrap()
        );

        let err = load_layout_str(&raw, "<test>").expect_err("should reject");

        assert!(
            matches!(&err, LayoutError::UnsafeTemplate { template: t, reason: r, .. }
                     if t == template && *r == reason),
            "expected UnsafeTemplate({reason:?}) for {template:?}, got {err:?}"
        );
    }

    proptest! {
        /// A rendered path never carries a traversal segment, whatever the bindings say.
        ///
        /// `unsafe_templates_are_rejected` above covers the half of this the *document*
        /// controls. This is the half the *caller* controls, and it was open: `interpolate`
        /// substitutes binding values verbatim, so `training = "../.."` rebuilds exactly the
        /// shape `check_template` exists to refuse. On the bundled `feat` layout that rendered
        /// `<root>/fix4melview_../../../../../etc/cron.d/x_thr20.txt`, which normalizes to
        /// `/etc/cron.d/x_thr20.txt` — and `LayoutAt.mkdir` calls `mkdir(parents=True)` on
        /// whatever comes back.
        ///
        /// A `starts_with(root)` check does not catch it: that string *does* start with the
        /// root. Asserting on the segments is also what makes this true for an `s3://` root,
        /// where `..` is a literal key component rather than a parent.
        #[test]
        fn a_rendered_path_never_carries_a_traversal_segment(
            training in prop_oneof![
                "[0-9A-Za-z]{1,6}",
                Just("..".to_string()),
                Just("/etc/passwd".to_string()),
                "[0-9A-Za-z]{0,3}(/\\.\\.){1,3}/[0-9A-Za-z]{0,3}",
            ],
            threshold in "[0-9]{1,3}",
        ) {
            let bindings = BTreeMap::from([
                ("training".to_string(), training),
                ("threshold".to_string(), threshold),
            ]);

            let rendered = feat().render("classification", &bindings);

            prop_assert!(
                rendered.as_deref().is_none_or(|p| {
                    !p.starts_with('/') && !p.split('/').any(|segment| segment == "..")
                }),
                "rendered {rendered:?}"
            );
        }
    }

    #[test]
    fn metaschema_rejects_a_layout_with_no_examples() {
        let doc = serde_json::json!({
            "LayoutVersion": "0.1.0",
            "TermMap": "feat",
            "Roles": { "r": { "Template": "mask.nii.gz" } },
            "Examples": []
        });
        assert!(
            !validate_layout(&doc).is_empty(),
            "Examples is what prevents drift; an empty list must not validate"
        );
    }
}
