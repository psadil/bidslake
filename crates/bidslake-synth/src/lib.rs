//! Synthetic BIDS and derivative trees, built from the schema rather than from format strings.
//!
//! ## What this is for
//!
//! Three jobs, and it is worth being clear that correctness testing is not among them:
//!
//! - **Benchmarking.** `bids-examples` tops out around 2,400 files and its widest tabular header
//!   is about eleven columns, so the costs that dominate at 100k files and 1,800 columns are
//!   invisible there.
//! - **Experimentation.** A tree of a chosen shape, on demand, with no dataset to download.
//! - **Authoring an adapter bundle.** A [layout](bids_schema::layout) plus a
//!   [term map](bids_schema::term_map) plus an [ingestion fragment](bidslake::schema::Ingestion)
//!   describe a tree that does not exist yet; [`Plan`] materializes it, and `--explain` reports
//!   what each path classifies as and how it would be dispositioned. That loop is how a
//!   hand-written PCRE's edge cases surface before any real data does.
//!
//! Correctness fixtures stay in `crates/bidslake/tests/`, hand-written, where the expected values
//! are written out by a human and each file carries the reason it exists.
//!
//! ## How much of it tracks the schema
//!
//! Honestly, not all of it, and the boundary is worth stating because it looks otherwise from
//! outside:
//!
//! | | tracks the schema? |
//! |---|---|
//! | Raw BIDS paths — `rules.files.raw`, `objects.entities`, `rules.entities` order | yes |
//! | Every tabular body — `rules.tabular_data` + `objects.columns` | yes |
//! | Sidecar bodies — `rules.sidecars` + `objects.metadata` | yes |
//! | Layout-backed tree paths — a layout's `Roles` and `Examples` | yes |
//! | Derivative paths for pipeline-invented vocabulary | **no** |
//! | The surface family — `rules.files.deriv.structural_mri` | yes, for what it declares |
//!
//! The "no" row is not an oversight. A producer's own overlay carries `objects.*` and
//! `rules.tabular_data` only, and base `rules.files.deriv` declares none of `timeseries`, `xfm`,
//! `boldref`, `mixing`, `components` or `classification`. There is nothing in any schema document
//! to enumerate, so [`producers`] carries a small path recipe per producer, and
//! `every_overlay_suffix_is_emitted_by_some_producer` turns a newly added overlay suffix into a
//! failing test rather than a silent gap.
//!
//! The surface family is the exception, and it shows what the rest is missing. The always-applied
//! `bep011` overlay declares `rules.files` as well as vocabulary, so those paths *are* described
//! by a rule group — and the guard on them is not the suffix census but
//! `a_generated_fmriprep_trees_surfaces_are_part_of_bids`, which runs the real validator over the
//! tree. Note what that means for the census: with the overlay applied to every load, its
//! suffixes are in the baseline `Schema::load(None)` returns, so they never enter that test's
//! `added` set at all. Correct rather than a hole — its premise is "nothing in any schema
//! document to enumerate", and for this family that stopped being true. It still guards the
//! vocabulary that is genuinely pipeline-invented.
//!
//! ## Two axes, and why they are kept apart
//!
//! The scale knobs are uniform by construction: every subject gets the same files, so the file
//! count is exactly linear in `--subjects` and a superlinear cost shows up as a bend in a
//! one-at-a-time sweep. [`Hazards`] — ragged coverage, symlinks, empty directories, filenames
//! that stress a parser — break that by design, so they are opt-in, named individually, and
//! recorded in the [`Manifest`]. A benchmark number quoted without saying which hazards were on
//! is not a number.

pub mod hazards;
pub mod nifti;
pub mod producers;
pub mod sidecar;
pub mod tabular;
pub mod values;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use bids_schema::layout::Layout;
use bids_schema::term_map::TermMap;
use bidslake::schema::Schema;

pub use hazards::Hazards;

/// Which tree to build.
///
/// [`Layout`](Self::Layout) is the general case and the other two are the exceptions: a layout
/// already declares every path in its tree, so `feat` and `freesurfer` need no code here at all,
/// while raw BIDS and fMRIPrep are named by entities that no layout can carry (ADR 0002 §10 — a
/// producer with no term map cannot have a layout, and a BIDS-named producer needs no term map).
pub enum Producer {
    /// Raw BIDS, from `rules.files.raw`.
    Raw,
    /// An fMRIPrep-shaped derivative tree: BIDS-named, with the confounds and transforms its
    /// overlay exists to index.
    Fmriprep,
    /// Every role of a layout, under each of its `Examples`. Bundled by name or loaded from a
    /// path — the authoring case, and the reason this variant carries the whole [`Layout`]
    /// rather than a name.
    Layout(Box<Layout>),
}

impl Producer {
    /// The producer's name, for the manifest and for the `--print-index` hints.
    pub fn name(&self) -> &str {
        match self {
            Producer::Raw => "raw",
            Producer::Fmriprep => "fmriprep",
            Producer::Layout(layout) => layout.term_map_name(),
        }
    }

    /// The adapters an ingest of this tree needs, in the order `bidslake index` wants them.
    pub fn adapters(&self) -> Vec<&str> {
        match self {
            Producer::Raw => Vec::new(),
            Producer::Fmriprep => vec!["fmriprep"],
            Producer::Layout(layout) => vec![layout.term_map_name()],
        }
    }
}

/// How much of it to build. Every axis is independent, and every subject gets the same files.
#[derive(Debug, Clone)]
pub struct Scale {
    /// How many subject directories. The axis that widens the dataset root.
    pub subjects: usize,
    /// Sessions per subject. Ignored by a layout producer, whose unit granularity its own
    /// `Examples` declare.
    pub sessions: usize,
    /// Runs per session — the axis that deepens a per-suffix sidecar bucket.
    pub runs: usize,
    /// Task labels to cycle through.
    pub tasks: Vec<String>,
    /// `space-` labels emitted per run, beyond the native-space file. Each one is another data
    /// file that a single confounds table describes, which is the many-to-many an association
    /// has to fan out.
    pub spaces: Vec<String>,
    /// Undeclared confound columns per run — the ~1,800 regressors that make the per-column
    /// storage policy of ADR 0004 matter.
    pub confound_columns: usize,
    /// Rows in each tabular body. For confounds this is the volume count.
    pub confound_rows: usize,
}

impl Default for Scale {
    fn default() -> Self {
        Self {
            subjects: 2,
            sessions: 1,
            runs: 2,
            tasks: vec!["rest".to_string()],
            spaces: vec!["MNI152NLin2009cAsym".to_string()],
            confound_columns: 16,
            confound_rows: 8,
        }
    }
}

impl Scale {
    /// The measured shape of a real fMRIPrep 25.2.5 confounds file: 1,841 columns by 450 rows.
    ///
    /// Named rather than left to the caller because those two numbers are evidence — they came
    /// off a real tree — and a benchmark quoting a different pair is measuring something else.
    pub fn a2cps_melodic(mut self) -> Self {
        self.confound_columns = 1841;
        self.confound_rows = 450;
        self
    }
}

/// What the catalog should make of a generated file — the generator's own claim, checked by
/// [`Manifest::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// A BIDS-named file: `read_entities` must read back the suffix that was rendered, and the
    /// effective schema must declare it.
    Bids,
    /// A file with no BIDS name, claimed by a term map.
    Projected {
        /// The term map expected to recognize it.
        term_map: String,
    },
    /// Recognized but projecting nothing, or deliberately ignored — `scripts/`, `touch/`, FIX
    /// scratch. Asserting anything here would be asserting the absence of a rule.
    Unclaimed,
}

/// One file the plan will write.
///
/// Bodies are `Arc`-shared because they repeat: one confounds table at the a2cps shape is ~2 MB
/// and every run in the tree has the same one, so a thousand runs would otherwise be gigabytes
/// of identical bytes held at once.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    /// Dataset-relative path, no leading slash.
    pub rel_path: String,
    /// What the catalog should make of it.
    pub claim: Claim,
    /// The bytes, or `None` for a file written empty.
    pub body: Option<Arc<[u8]>>,
}

impl PlannedFile {
    /// A file written with no content at all. Only for files nothing reads *and* nothing
    /// validates — imaging volumes get a real header from [`nifti`], because an empty `.nii.gz`
    /// is four validator errors.
    fn empty(rel_path: impl Into<String>, claim: Claim) -> Self {
        Self {
            rel_path: rel_path.into(),
            claim,
            body: None,
        }
    }

    fn text(rel_path: impl Into<String>, claim: Claim, body: impl Into<String>) -> Self {
        Self::bytes(rel_path, claim, Arc::from(body.into().into_bytes()))
    }

    fn bytes(rel_path: impl Into<String>, claim: Claim, body: Arc<[u8]>) -> Self {
        Self {
            rel_path: rel_path.into(),
            claim,
            body: Some(body),
        }
    }
}

/// A plan for one tree: what to build, how much, and how faithful.
pub struct Plan {
    producer: Producer,
    scale: Scale,
    hazards: Hazards,
    bidsignore: bool,
}

impl Plan {
    /// A plan for `producer` at the default (small) scale, with no hazards.
    pub fn producer(producer: Producer) -> Self {
        Self {
            producer,
            scale: Scale::default(),
            hazards: Hazards::NONE,
            bidsignore: false,
        }
    }

    /// Replace the scale wholesale.
    #[must_use]
    pub fn scale(mut self, scale: Scale) -> Self {
        self.scale = scale;
        self
    }

    /// Turn on the named hazards. Off by default, and never implied by anything else.
    #[must_use]
    pub fn hazards(mut self, hazards: Hazards) -> Self {
        self.hazards = hazards;
        self
    }

    /// Write a `.bidsignore` listing what fMRIPrep really hides.
    ///
    /// Off by default, and that is the opposite of faithful on purpose: the real file hides
    /// `*_xfm.*` and `*_timeseries.tsv`, which are exactly the files an fMRIPrep tree is
    /// generated to exercise. With it on, an ingest needs `--no-bidsignore` to see them at all.
    #[must_use]
    pub fn bidsignore(mut self, bidsignore: bool) -> Self {
        self.bidsignore = bidsignore;
        self
    }

    /// The scale this plan will build at.
    pub fn scale_of(&self) -> &Scale {
        &self.scale
    }

    /// The producer this plan will build.
    pub fn producer_of(&self) -> &Producer {
        &self.producer
    }

    /// Every file the plan would write, with no filesystem access at all.
    ///
    /// Separate from [`write`](Self::write) because most questions about a tree — how many files,
    /// what shape, does every path classify — are questions about the path set, and no test or
    /// `--explain` run should need a temporary directory to ask one.
    pub fn paths(&self, schema: &Schema) -> Result<Vec<PlannedFile>> {
        let mut files = match &self.producer {
            Producer::Raw => producers::raw::plan(schema, &self.scale)?,
            Producer::Fmriprep => producers::fmriprep::plan(schema, &self.scale, self.bidsignore)?,
            Producer::Layout(layout) => producers::layout::plan(schema, layout, &self.scale)?,
        };
        hazards::apply(&mut files, self.hazards);
        files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        Ok(files)
    }

    /// Materialize the tree under `root`, returning what was written.
    ///
    /// `schema` is the caller's, not one loaded here, because which overlays are in force decides
    /// what the tree's tabular files even look like — and a caller that forgets the `fmriprep`
    /// overlay gets confounds that route nowhere, which is a real and quiet way to benchmark the
    /// wrong thing.
    pub fn write(&self, root: &Path, schema: &Schema) -> Result<Manifest> {
        let files = self.paths(schema)?;
        for file in &files {
            let path = root.join(&file.rel_path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            match &file.body {
                Some(body) => std::fs::write(&path, body.as_ref()),
                None => std::fs::write(&path, b""),
            }
            .with_context(|| format!("writing {}", path.display()))?;
        }
        hazards::materialize(root, self.hazards)?;
        Ok(Manifest {
            files,
            producer: self.producer.name().to_string(),
            adapters: self
                .producer
                .adapters()
                .into_iter()
                .map(str::to_string)
                .collect(),
            hazards: self.hazards,
            bidsignore: self.bidsignore,
        })
    }
}

/// A claim [`Manifest::verify`] could not substantiate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The path whose claim failed.
    pub rel_path: String,
    /// What went wrong, in one phrase.
    pub detail: String,
}

/// What a [`Plan`] wrote, and the settings that shaped it.
///
/// The settings travel with the file list because a hazard set or a `.bidsignore` changes what a
/// benchmark measured, and a manifest that recorded only paths would let a number be quoted
/// without them.
#[derive(Debug, Clone)]
pub struct Manifest {
    /// Every file written, sorted by path.
    pub files: Vec<PlannedFile>,
    /// The producer's name.
    pub producer: String,
    /// The adapters an ingest of this tree needs.
    pub adapters: Vec<String>,
    /// Which hazards were enabled.
    pub hazards: Hazards,
    /// Whether a `.bidsignore` was written.
    pub bidsignore: bool,
}

impl Manifest {
    /// Check every file's [`Claim`] against the schema and term maps that will read the tree.
    ///
    /// This is the generator's own contract, and it is deliberately weaker than the validator:
    /// it asks only whether each file is the *kind* of thing it was planned to be. Whether the
    /// tree is valid BIDS is `bids-validator-rs`'s answer, and the acceptance tests ask it
    /// directly rather than approximating it here.
    pub fn verify(&self, schema: &Schema, term_maps: &[TermMap]) -> Result<(), Vec<Violation>> {
        let suffixes = schema
            .raw()
            .get("objects")
            .and_then(|o| o.get("suffixes"))
            .and_then(|s| s.as_object());

        let mut violations = Vec::new();
        for file in &self.files {
            match &file.claim {
                Claim::Bids => {
                    let name = file.rel_path.rsplit('/').next().unwrap_or(&file.rel_path);
                    let parts = bids_core::entities::read_entities(name);
                    if parts.suffix.is_empty() {
                        violations.push(Violation {
                            rel_path: file.rel_path.clone(),
                            detail: "planned as BIDS but reads back with no suffix".to_string(),
                        });
                        continue;
                    }
                    let declared = suffixes.is_some_and(|s| {
                        s.values().any(|d| {
                            d.get("value").and_then(|v| v.as_str()) == Some(parts.suffix.as_str())
                        })
                    });
                    if !declared {
                        violations.push(Violation {
                            rel_path: file.rel_path.clone(),
                            detail: format!(
                                "suffix {:?} is not in the effective schema's objects.suffixes",
                                parts.suffix
                            ),
                        });
                    }
                }
                Claim::Projected { term_map } => {
                    if !bids_schema::term_map::any_recognizes_under(term_maps, &file.rel_path) {
                        violations.push(Violation {
                            rel_path: file.rel_path.clone(),
                            detail: format!("term map {term_map:?} does not recognize it"),
                        });
                    }
                }
                Claim::Unclaimed => {}
            }
        }
        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }

    /// How many files carry each kind of claim, for a one-line summary.
    pub fn claim_counts(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for file in &self.files {
            let key = match file.claim {
                Claim::Bids => "bids",
                Claim::Projected { .. } => "projected",
                Claim::Unclaimed => "unclaimed",
            };
            *counts.entry(key).or_insert(0) += 1;
        }
        counts
    }
}

/// A zero-padded subject label, `0001`-based.
pub(crate) fn subject_label(index: usize) -> String {
    format!("{:04}", index + 1)
}

/// A session label. `V`-prefixed because a bare index would be a `format: label` entity that
/// looks like an index, and the a2cps trees this is modelled on spell theirs `ses-V1`.
pub(crate) fn session_label(index: usize) -> String {
    format!("V{}", index + 1)
}

/// A run index. Zero-padded to two digits, which BIDS permits but does not require — the
/// [`bids_schema::naming`] writer deliberately leaves the choice here.
pub(crate) fn run_label(index: usize) -> String {
    format!("{:02}", index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bids_schema::layout::bundled_layout;
    use bidslake::schema::AppliedOverlay;
    use rstest::rstest;

    fn schema_with(adapters: &[&str]) -> Schema {
        let overlays: Vec<AppliedOverlay> = adapters
            .iter()
            .filter_map(|name| {
                bids_schema::overlay::bundled_overlay(name).map(|content| AppliedOverlay {
                    source: (*name).to_string(),
                    content,
                })
            })
            .collect();
        Schema::load_with_overlays(None, &overlays).expect("schema loads")
    }

    fn producer(name: &str) -> Producer {
        match name {
            "raw" => Producer::Raw,
            "fmriprep" => Producer::Fmriprep,
            other => Producer::Layout(Box::new(
                bundled_layout(other).unwrap_or_else(|| panic!("bundled layout {other:?}")),
            )),
        }
    }

    #[rstest]
    #[case::raw("raw")]
    #[case::fmriprep("fmriprep")]
    #[case::freesurfer("freesurfer")]
    #[case::feat("feat")]
    fn every_producer_verifies_at_the_default_scale(#[case] name: &str) {
        let producer = producer(name);
        let adapters = producer.adapters();
        let schema = schema_with(&adapters);
        let term_maps: Vec<TermMap> = adapters
            .iter()
            .filter_map(|a| bids_schema::term_map::bundled_term_map(a))
            .collect();
        let plan = Plan::producer(producer);
        // Bound to a local: `tempdir().keep()` would hand back a path with no owner and leave a
        // whole generated tree in the system temp directory on every run.
        let dir = tempfile::tempdir().expect("tempdir");

        let manifest = plan.write(dir.path(), &schema).expect("writes");

        assert_eq!(manifest.verify(&schema, &term_maps), Ok(()), "{name}");
    }

    /// The benchmark thesis in one assertion: with hazards off, the file count is exactly linear
    /// in `--subjects`, so a superlinear ingest cost shows up as a bend in the curve rather than
    /// as noise from a tree that grew unevenly.
    #[rstest]
    #[case::raw("raw")]
    #[case::fmriprep("fmriprep")]
    #[case::freesurfer("freesurfer")]
    #[case::feat("feat")]
    fn the_file_count_is_linear_in_subjects(#[case] name: &str) {
        let schema = schema_with(&producer(name).adapters());
        let counts: Vec<usize> = [1usize, 2, 4]
            .into_iter()
            .map(|subjects| {
                let scale = Scale {
                    subjects,
                    ..Scale::default()
                };
                Plan::producer(producer(name))
                    .scale(scale)
                    .paths(&schema)
                    .expect("plans")
                    .len()
            })
            .collect();

        // Per-subject cost plus a constant (the dataset description, README, participants).
        let per_subject = counts[1] - counts[0];
        assert_eq!(
            counts[2],
            counts[0] + 3 * per_subject,
            "{name}: counts {counts:?} are not linear"
        );
    }

    #[test]
    fn hazards_are_off_by_default() {
        let schema = schema_with(&[]);

        let default = Plan::producer(Producer::Raw)
            .paths(&schema)
            .expect("plans")
            .len();

        let explicit_none = Plan::producer(Producer::Raw)
            .hazards(Hazards::NONE)
            .paths(&schema)
            .expect("plans")
            .len();
        assert_eq!(default, explicit_none);
    }

    /// Reader-populated suffixes: declared as vocabulary so a table can exist, but never carried
    /// by a filename, so no producer can emit one. `fsmeasures` is the case — the `# Measure`
    /// scalars inside a `.stats` file become `freesurfer_measures` rows, and its own description
    /// says "not a routing target".
    const NOT_A_FILENAME: &[&str] = &["fsmeasures"];

    /// Every suffix an overlay adds to the vocabulary must be emitted by some producer. Not a
    /// nicety: overlays declare no `rules.files`, so nothing else notices a suffix the generator
    /// never writes, and the benchmark would quietly stop covering a table.
    ///
    /// Each producer is planned against *its own* adapter's schema, one at a time, because that
    /// is how a producer is really built: `bidslake-synth` loads one adapter per producer, and a
    /// suffix is only reachable by the producer whose overlay declares it.
    ///
    /// It is also the safe way round. `bids_schema::naming` refuses to render an entity that two
    /// schema objects claim with different constraints, and a schema merging every overlay is
    /// where such a pair would appear (see that module's docs). None do today; planning per
    /// adapter means this test does not depend on that staying true.
    #[test]
    fn every_overlay_suffix_is_emitted_by_some_producer() {
        let names = ["fmriprep", "freesurfer", "feat"];
        let base = suffix_values(Schema::load(None).unwrap());

        let mut added = std::collections::BTreeSet::new();
        let mut emitted = std::collections::BTreeSet::new();
        for name in names {
            let schema = schema_with(&[name]);
            added.extend(
                suffix_values(schema_with(&[name]))
                    .difference(&base)
                    .cloned(),
            );

            let term_map = bids_schema::term_map::bundled_term_map(name);
            for file in Plan::producer(producer(name))
                .paths(&schema)
                .expect("plans")
            {
                // A BIDS-named file carries its suffix; a projected one has none in its name and
                // the term map supplies it.
                let basename = file.rel_path.rsplit('/').next().unwrap_or("");
                emitted.insert(bids_core::entities::read_entities(basename).suffix);
                if let Some(suffix) = term_map
                    .as_ref()
                    .and_then(|tm| tm.classify(&file.rel_path))
                    .and_then(|facts| facts.suffix)
                {
                    emitted.insert(suffix);
                }
            }
        }

        let missing: Vec<&String> = added
            .iter()
            .filter(|s| !emitted.contains(*s) && !NOT_A_FILENAME.contains(&s.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "overlay suffixes no producer emits: {missing:?}"
        );
    }

    fn suffix_values(schema: Schema) -> std::collections::BTreeSet<String> {
        schema
            .raw()
            .get("objects")
            .and_then(|o| o.get("suffixes"))
            .and_then(|s| s.as_object())
            .map(|m| {
                m.values()
                    .filter_map(|d| d.get("value").and_then(|v| v.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
}
