//! **Layouts**: the concepts-to-path direction, for naming a file that does not exist yet.
//!
//! A [term map](crate::term_map) answers *what does this path denote?* — enough to index a
//! tree someone else wrote. A pipeline that *writes* such a tree needs the other direction:
//! given a role (`the highres-to-standard affine`), where does it go? Without that, every
//! consumer hardcodes the convention, which is how a script ends up with two dozen
//! properties that are nothing but string joins.
//!
//! ## Why this is a separate document from the term map
//!
//! Not tidiness — the two directions genuinely cannot share one template.
//!
//! Term-map templates are PCRE, pinned by ADR 0002 §2 because optional groups collapse
//! FreeSurfer's `sub-01_ses-1` / `sub-01` / bare `bert` subject-directory forms into one
//! mapping. That collapsing is exactly what makes them non-invertible: there is no single
//! filename to render `(?:sub-)?(?P<subject>[0-9A-Za-z]+)` back into.
//!
//! Replacing PCRE with `{var}` interpolation does not fix it, because invertibility is a
//! property of the *mapping*, not of the syntax. Measured against a real `recon-all` tree
//! (657 files): the 8 PCRE mappings recognize all of them, while a pure-`{var}` rewrite
//! needs 12 mappings, recognizes 430, and loses 227 — the catch-alls (`label/*.annot`,
//! `mri/*.mgz`) that match a whole *class* of filenames. Those cannot be enumerated, and
//! cannot be rendered either: there is no concept to render *from*. It also over-matches
//! where alternation was doing work, labelling `mri/T1.mgz` as `seg=T1`.
//!
//! So a term map keeps PCRE and stays read-only; a layout declares the roles that *can* be
//! written, in a syntax that can write them.
//!
//! ## What stops them drifting
//!
//! Two documents describing one tree is a drift risk, and co-locating them would only
//! prevent *textual* drift. Instead every layout must carry `Examples`, and
//! `Layout::validate_round_trip` renders **every role under every example** and feeds the
//! result back through the named term map. If `classify(render(role))` does not reproduce
//! the role's declared concepts, the layout **fails to load**. The two directions are
//! therefore verifiably in agreement rather than merely adjacent, and a rename on either
//! side is an error at load time rather than a tree nothing can read back.

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
    #[error("reading layout {path}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing layout {path} as JSON")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("layout does not conform to the layout metaschema:\n{}", .violations.join("\n"))]
    Invalid { violations: Vec<String> },
    #[error("layout role {role:?} has an unsafe template {template:?}: {reason}")]
    UnsafeTemplate {
        role: String,
        template: String,
        reason: &'static str,
    },
    #[error("layout names term map {0:?}, which is not bundled")]
    UnknownTermMap(String),
    /// The two directions disagree — the whole point of `Examples`.
    #[error(
        "layout role {role:?} renders to {path:?}, which term map {term_map:?} {detail}. \
         The layout and the term map describe the same tree and must agree; fix whichever \
         is wrong."
    )]
    RoundTrip {
        role: String,
        path: String,
        term_map: String,
        detail: String,
    },
}

// ---------------------------------------------------------------------------
// On-disk format.
// ---------------------------------------------------------------------------

/// A parsed layout document.
#[derive(Debug, Clone, Deserialize)]
pub struct LayoutFile {
    #[serde(rename = "LayoutVersion")]
    pub layout_version: String,
    /// The bundled term map that reads back what this layout writes.
    #[serde(rename = "TermMap")]
    pub term_map: String,
    #[serde(rename = "Roles")]
    pub roles: BTreeMap<String, Role>,
    #[serde(rename = "Examples")]
    pub examples: Vec<Example>,
}

/// Where one named artifact sits, relative to a unit's output root.
#[derive(Debug, Clone, Deserialize)]
pub struct Role {
    #[serde(rename = "Template")]
    pub template: String,
    #[serde(rename = "Concepts", default)]
    pub concepts: RoleConcepts,
    #[serde(rename = "Entities", default)]
    pub entities: BTreeMap<String, String>,
    #[serde(rename = "Description", default)]
    pub description: Option<String>,
}

/// The datatype/suffix a role's rendered path must project onto.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RoleConcepts {
    #[serde(default)]
    pub datatype: Option<String>,
    #[serde(default)]
    pub suffix: Option<String>,
}

/// A unit output root to validate every role against.
#[derive(Debug, Clone, Deserialize)]
pub struct Example {
    #[serde(rename = "Root")]
    pub root: String,
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
    /// [`check_template`], which runs here against the interpolated result as well as at load
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
pub const BUNDLED_LAYOUT_NAMES: &[&str] = &["feat"];

/// The raw JSON source of a bundled layout, or `None` if `name` is not bundled.
pub fn bundled_layout_source(name: &str) -> Option<&'static str> {
    Some(match name {
        "feat" => include_str!("../data/layouts/feat.json"),
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
