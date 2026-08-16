//! The ingestion pipeline: BIDS dataset → DuckDB rows.
//!
//! [`BidsParser::parse`] runs the whole ingest against a [`BidsFileSystem`]
//! (local or S3), driven by a [`Schema`]. The steps:
//!
//! 1. **Walk & categorize.** List every file and bucket it: `dataset_description.json`,
//!    `participants.tsv`, `sessions.tsv`, and everything else.
//! 2. **Resolve the dataset id** from the root `dataset_description.json` (nested
//!    ones under `derivatives/` are sorted shallowest-first so the root wins).
//! 3. **Process in passes** — dataset_description, then participants, then
//!    sessions, then all other files — via `process_file`. Filename
//!    entities are parsed here (`sub-01` → `sub`), participants/sessions are
//!    implicitly created (deduped in-memory via `seen_participants`/
//!    `seen_sessions`), and TSV/JSON/bval-bvec files are dispatched to handlers.
//! 4. **Flush deferred work**: file associations (`IntendedFor` plus the schema's
//!    structural ones), parsed `.bval`/`.bvec` values, and the `scans` table (a row per
//!    data file a `*_scans.tsv` describes).
//! 5. **Apply BIDS inheritance** to build `sidecars`: for each imaging file, the
//!    applicable dataset-/subject-level JSON sidecars are merged (more-specific wins).
//!    Candidates are found through a `SidecarIndex` keyed by
//!    `(dataset_id, suffix, directory)`, so a file consults only the sidecars on its own
//!    ancestor path rather than every sidecar the dataset holds.
//!
//! Two things about how rows reach the database. Tabular files are deferred and ingested in
//! header-grouped batches — one `read_csv` over many files — while `scans` and `sidecars` go
//! through the DuckDB `Appender` (see [`BidsDb::append_rows`]). And the entire parse runs
//! inside a single transaction (opened by the caller in `main`), so it commits atomically.

use crate::db::{BidsDb, BvalFile, BvecFile, FileAssociation, TabularStatus};
use crate::fs::BidsFileSystem;
use crate::links;
use crate::readers::{self, ContentReader};
use crate::schema::dynamic::{quote_ident, sql_lit};
use crate::schema::ingestion::{Disposition, Undeclared};
use crate::schema::recording::ColumnNames;
use crate::schema::tabular::{ColumnSpec, FileContext, RowIdentity, TableSpec};
use crate::schema::{Schema, Tenure};
use crate::timing::{self, Counter, Phase};
use anyhow::{Context, Result};
use bids_core::entities::read_entities;
use bids_core::filetree::FileTree;
use bids_schema::term_map::{FileFacts, TermMap};
use duckdb::Connection;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// The `read_csv` relaxations that make a tabular read non-poisoning: a malformed
/// row is padded or dropped rather than aborting the ingest transaction (so
/// `bids-validator-rs`, not bidslake, owns malformation). Shared verbatim by the
/// header-bearing [`HEADER_READ_OPTS`] and the headerless recording read so the two
/// can't drift out of sync. A macro (not a `const`) so the literal can be spliced
/// into both a `const concat!` and a runtime `format!`.
macro_rules! non_poisoning_read_flags {
    () => {
        "delim='\\t', all_varchar=true, nullstr='n/a', strict_mode=false, null_padding=true, ignore_errors=true"
    };
}

/// The same relaxations without `all_varchar`, for a read that states its `columns`
/// outright — the types are in that struct, and asking for both is a conflict.
macro_rules! non_poisoning_read_flags_typed {
    () => {
        "delim='\\t', nullstr='n/a', strict_mode=false, null_padding=true, ignore_errors=true"
    };
}

/// S3/httpfs configuration for reading `s3://` tabular data via DuckDB. Passed to
/// [`BidsParser::new`] so the read-preflight connection is configured as part of
/// construction — there is no separate must-call-before-`parse` step to forget.
///
/// Defined unconditionally, though only a build with the `s3` feature can act on it:
/// it names two plain values and pulls in no AWS types, and keeping it means
/// [`BidsParser::new`]'s signature does not change between feature configurations.
/// Supplying one to a build without the feature is an error, not a silent no-op.
pub struct S3Httpfs {
    /// The AWS region httpfs should target.
    pub region: String,
    /// Whether to use anonymous (unsigned) access — public buckets like OpenNeuro's.
    pub anonymous: bool,
}

/// The `dataset_description.json` fields that decide whether two ingest roots are the
/// same dataset, when the `dataset_id` was inferred rather than asserted
/// (`BidsParser::resolve_root`, docs/adr/0005).
///
/// `Name` and `BIDSVersion` say what the dataset is and which standard it targets;
/// `GeneratedBy` pins the pipeline and version that wrote it; `SourceDatasets` says what
/// it was derived from. That last one carries the weight: a pipeline writes its *own*
/// name into `Name`, identically for every study it ever processes, so `SourceDatasets`
/// is what separates two studies' fMRIPrep output from two shards of one study's.
///
/// Deliberately not the whole file. `GeneratedBy` already varies between shards processed
/// months apart, and fields like `Authors` or `HowToAcknowledge` drifting between shards
/// is untidy, not evidence of a different dataset.
const IDENTITY_FIELDS: [&str; 4] = ["Name", "BIDSVersion", "GeneratedBy", "SourceDatasets"];

/// Whether the text stored in a `dataset_description` column represents `want`.
///
/// Mirrors how [`Schema::row_values`](crate::schema::Schema::row_values) writes one: a
/// string goes in verbatim, anything else as its JSON rendering. Non-scalars are compared
/// by parsing the stored text back, so key order does not make two equal descriptions
/// look different.
fn stored_matches(have: Option<&str>, want: Option<&Value>) -> bool {
    match (have, want) {
        (None, None) => true,
        // A string column holds the string itself, which need not be valid JSON
        // (`7t_trt`, `fMRIPrep - fMRI PREProcessing workflow`).
        (Some(have), Some(Value::String(want))) => have == want,
        // Everything else was written as its JSON rendering, so text that will not parse
        // cannot be one — report a difference rather than guessing. That is the safe
        // direction: on the inferred path an unexplained difference refuses the merge.
        (Some(have), Some(want)) => {
            serde_json::from_str::<Value>(have).is_ok_and(|parsed| &parsed == want)
        }
        _ => false,
    }
}

pub struct BidsParser {
    fs: Box<dyn BidsFileSystem>,
    dataset_id: Option<String>,
    /// This run's ingest root, as the URI every row it writes is resolved against.
    /// A run walks exactly one root, so this is `self.fs.root()` cached — that call
    /// allocates, and step-3 write paths need the value once per row. Settled by
    /// [`Self::resolve_root`]; empty before `parse` reaches that point, never read before.
    root_uri: String,
    /// S3/httpfs config for the read-preflight connection, applied at the start of
    /// [`Self::parse`]. `None` for local datasets.
    s3_httpfs: Option<S3Httpfs>,
    ignore_set: Gitignore,
    /// Whether to honor the dataset's `.bidsignore`. False (via `--no-bidsignore`)
    /// walks and classifies every file, so overlay-described derivative outputs a
    /// pipeline hides (e.g. fMRIPrep's `*_timeseries.tsv`, `*_xfm.*`) are indexed.
    apply_bidsignore: bool,
    pending_associations: Vec<PendingAssociation>,
    /// Parsed gradient files, keyed by the gradient file's **own** dataset-relative path.
    pending_gradients: Vec<(String, String, PendingGradient)>,
    schema: Schema,
    imaging_files: Vec<ImagingFile>,
    /// Every walked file that is **not** a primary data file, for the file registry
    /// (docs/adr/0006): sidecars, tabular files, gradients, READMEs — everything an `ignore`
    /// rule did not claim.
    ///
    /// Separate from `imaging_files` rather than one mixed list because
    /// `promote_orphan_sidecars` builds its `by_dir` index with positions aligned to
    /// `imaging_files`; a mixed list would put sidecars and READMEs into the orphan check.
    /// The two are merged at the flush, where a path in both (a promoted metadata-only
    /// record) resolves to its data-file row.
    registry_extra: Vec<RegistryEntry>,
    /// Size and mtime per dataset-relative path, from one `stat_many` after the walk.
    ///
    /// Empty when the run was given `--no-stat`, and missing an entry for a file the
    /// backend could not stat. Both cases leave the registry columns NULL, which is what
    /// `verify` reads as "presence is all this catalog can tell you".
    file_stats: HashMap<String, crate::fs::FileStat>,
    /// Whether to run that pass at all (`--no-stat` turns it off).
    stat_files: bool,
    /// The tenure this run asserts for its root (docs/adr/0009). Defaults to
    /// [`Tenure::Attached`]; `--managed` is what raises it.
    tenure: Tenure,
    /// Statuses decided during the **walk**, keyed by dataset-relative path.
    ///
    /// The registry is written after the walk, so a `record_file_status` UPDATE issued while
    /// walking would match no row and be silently lost. These are folded into the rows
    /// instead; statuses decided later (a batch insert's `ingested`/`failed`) do use the
    /// UPDATE, by which time the row exists.
    walk_status: HashMap<String, TabularStatus>,
    /// Whether a `dataset_description.json` was found and inserted. When it wasn't — the
    /// normal case for a dataset ingested through a layout adapter, which by definition
    /// has none — a minimal row is synthesized at the end of the walk so the dataset still
    /// records its `root_uri`.
    has_dataset_description: bool,
    sidecars: Vec<SidecarInfo>,
    /// `dataset_description.json`'s `DatasetType` (`raw`/`derivative`), needed to
    /// evaluate the `derivatives.*` tabular selectors (e.g. `dseg` lookups).
    dataset_type: Option<String>,
    /// The BIDS datatype directory names, cached from the schema for classifying
    /// each file's datatype from its path.
    datatypes: HashSet<String>,
    /// Every datatype directory in the dataset, as `(dir_path, datatype)` — e.g.
    /// (`sub-01/ses-meg/meg`, `meg`). Used to infer the datatype of a tabular file
    /// that sits *above* a datatype directory (a session- or subject-level
    /// `channels.tsv` that applies to the `meg/` runs below it) so it can still be
    /// routed.
    datatype_dirs: HashSet<(String, String)>,
    /// Headerless recordings, ingested in the flush once all sidecars are known.
    pending_recordings: Vec<PendingRecording>,
    /// Per-row tabular files deferred for batched ingestion (Lever 1b).
    pending_tabular: Vec<PendingTabular>,
    /// A throwaway in-memory connection used to pre-flight `read_csv` on a file
    /// before the real INSERT. A malformed TSV (empty, truncated, or a non-gzip
    /// git-annex placeholder with a `.gz` name) makes `read_csv` error, and inside
    /// the ingest transaction that error would poison every later statement. Testing
    /// the read here first — off the main connection — keeps a bad file from
    /// aborting the whole dataset's ingest.
    validator: Connection,
    // Track which implicit participants/sessions we've already inserted so the
    // per-file loop doesn't re-issue an insert for every file of a subject.
    seen_participants: HashSet<(String, String)>, // (dataset_id, participant_id)
    seen_sessions: HashSet<(String, String, String)>, // (dataset_id, participant_id, session_id)
    /// Prefetched file bodies read in Rust (rel path → content): JSON sidecars,
    /// `.bval`/`.bvec`, and adapter `read`-disposition files. Filled concurrently
    /// before the serial passes so each read isn't a separate round-trip on a
    /// network filesystem. (TSV bodies are read by DuckDB, not here — only their
    /// headers are prefetched, into `tabular_header`.)
    content_cache: HashMap<String, String>,
    /// Prefetched TSV headers (rel path → parsed header), same rationale.
    tabular_header: HashMap<String, Option<(String, Vec<String>)>>,
    /// Term maps (FreeSurfer, …) that recognize standardized non-BIDS files. Empty for an
    /// ordinary BIDS ingest, so `process_file` pays only one `is_empty()` check per file.
    term_maps: Vec<TermMap>,
    /// Content readers keyed by name (`fs_stats`, …); parse a recognized file's body into
    /// rows. Selected by the ingestion schema's `reader`.
    readers: HashMap<String, Box<dyn ContentReader>>,
    /// The root `dataset_description.json` (captured on its first, shallowest processing),
    /// read at finalize by [`Self::record_links`] for `SourceDatasets`/`DatasetLinks`/`DatasetDOI`.
    /// Nested descriptions under `derivatives/` describe *other* datasets and are ignored.
    root_description_json: Option<Value>,
    /// `--source-dataset` references declared on the CLI; recorded as `declared` links.
    declared_sources: Vec<String>,
}

#[derive(Clone)]
struct ImagingFile {
    dataset_id: String,
    file_path: String,
    /// The term-map projection for this file, serialized, or `None` for a BIDS-named
    /// file (whose concepts the generated columns read straight off `file_path`).
    ///
    /// A projected path carries almost none of its concepts in its name, so without
    /// this the `FileFacts` a term map computed would be used to pick an ingestion
    /// rule and then thrown away — leaving the row's `datatype`/`suffix`/entity
    /// columns NULL. Lands in the `projected` column that
    /// `Schema::generated_bids_columns` makes the concept columns fall back from.
    projected: Option<Value>,
}

/// A cross-reference collected during the walk, still in path form.
///
/// Distinct from [`FileAssociation`], the row that reaches the database, because a target is
/// resolved to a `file_id` only at finalize — that is the first point at which the whole set of
/// walked paths is known, and an `IntendedFor` may name a file the dataset does not ship.
struct PendingAssociation {
    source_file: String,
    target_file: String,
    assoc_type: String,
}

/// A walked file that is not a primary data file, held until the registry is written
/// (docs/adr/0006).
///
/// `dataset_id` and `root_uri` are not on it: a run resolves one of each, so carrying them per
/// row would clone two strings per file for values the flush already has.
#[derive(Clone)]
struct RegistryEntry {
    /// Relative to this run's ingest root.
    file_path: String,
    kind: Kind,
    /// What a term map projected onto this file, if one claimed it — the same value an
    /// [`ImagingFile`] carries, kept so a non-data projected file still answers concept
    /// queries through the registry.
    projected: Option<Value>,
}

/// Serialize the concepts a term map projected onto a file, for the `projected` column.
///
/// `extension` is deliberately omitted: it is read off the filename, which is
/// authoritative even for a projected path, so storing it would cost a column in every
/// row's JSON to restate what the generated column already computes. `None` when the
/// projection is empty, so a mapping that binds nothing writes no JSON at all.
///
/// Returns the `Value` itself rather than serialized text: handing the appender a
/// `Value::String` of JSON stores a JSON *string* (`"{\"seg\":\"wmparc\"}"`, whose
/// `json_type` is VARCHAR), and `json_extract_string` then returns NULL for every key.
fn projected_json(facts: &FileFacts) -> Option<Value> {
    let mut map = serde_json::Map::new();
    for (k, v) in &facts.entities {
        map.insert(k.clone(), Value::String(v.clone()));
    }
    if let Some(dt) = &facts.datatype {
        map.insert("datatype".to_string(), Value::String(dt.clone()));
    }
    if let Some(sfx) = &facts.suffix {
        map.insert("suffix".to_string(), Value::String(sfx.clone()));
    }
    if map.is_empty() {
        return None;
    }
    Some(Value::Object(map))
}

/// One parsed gradient file, held until the file registry has been written (the `bvals`/
/// `bvecs` foreign key needs its row to exist).
///
/// An enum rather than a struct of four `Option`s because a path is a `.bval` **or** a
/// `.bvec`, never both. The old shape keyed a struct by the *image* path so the two files
/// could meet in it — which is exactly the bug: a pair split across inheritance levels
/// hashed to different keys and both halves were dropped. Nothing pairs them at write time
/// any more; the `diffusion` view does it through the image they are associated with.
enum PendingGradient {
    Bvals(Vec<f64>),
    Bvecs(Vec<f64>, Vec<f64>, Vec<f64>),
}

struct SidecarInfo {
    dataset_id: String,
    file_path: String, // Relative path in dataset
    entities: HashMap<String, String>,
    suffix: String,
    content: Value,
}

/// Collected sidecars indexed by `(dataset_id, suffix, directory)`, for BIDS inheritance.
///
/// Inheritance asks, of a given file, which sidecars apply: same dataset and suffix,
/// directory an ancestor of the file's, entities a subset of the file's. Keying on all
/// three of those makes the answer a handful of lookups — one per ancestor directory —
/// where indexing on dataset and suffix alone left the directory test as a scan over
/// every same-suffix sidecar. In ordinary raw BIDS each run ships its own `_bold.json`,
/// so that bucket holds one sidecar per data file and the scan was quadratic in dataset
/// size.
struct SidecarIndex<'a> {
    sidecars: &'a [SidecarInfo],
    by_dir: HashMap<(&'a str, &'a str, &'a Path), Vec<usize>>,
}

impl<'a> SidecarIndex<'a> {
    fn new(sidecars: &'a [SidecarInfo]) -> Self {
        let mut by_dir: HashMap<(&'a str, &'a str, &'a Path), Vec<usize>> = HashMap::new();
        for (i, s) in sidecars.iter().enumerate() {
            let dir = Path::new(&s.file_path)
                .parent()
                .unwrap_or_else(|| Path::new(""));
            by_dir
                .entry((s.dataset_id.as_str(), s.suffix.as_str(), dir))
                .or_default()
                .push(i);
        }
        Self { sidecars, by_dir }
    }

    /// The metadata applying to `rel_path`, merged under BIDS inheritance.
    ///
    /// The *nearer* sidecar wins. Visiting the file's ancestor directories
    /// shallowest-first and merging in that order lets a deeper sidecar overwrite a
    /// shallower one, matching the tree-based reference resolver — and it replaces a
    /// sort by directory depth, since the visit order *is* depth order. Two sidecars at
    /// equal depth necessarily share a directory, so that tie is broken within a bucket,
    /// by entity count (the invalid-BIDS case of two sidecars side by side).
    fn merged(
        &self,
        dataset_id: &str,
        rel_path: &str,
        suffix: &str,
        entities: &HashMap<String, String>,
        scratch: &mut Vec<usize>,
    ) -> serde_json::Map<String, Value> {
        let dir = Path::new(rel_path)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        // `ancestors()` runs deepest-first; collect and reverse.
        let mut ancestors: Vec<&Path> = dir.ancestors().collect();
        ancestors.reverse();

        scratch.clear();
        for ancestor in ancestors {
            let Some(candidates) = self.by_dir.get(&(dataset_id, suffix, ancestor)) else {
                continue;
            };
            let start = scratch.len();
            // Dataset, suffix, and directory are all answered by the key; all that is
            // left is the entity-subset test.
            scratch.extend(candidates.iter().copied().filter(|&i| {
                self.sidecars[i]
                    .entities
                    .iter()
                    .all(|(key, value)| entities.get(key) == Some(value))
            }));
            scratch[start..].sort_by_key(|&i| self.sidecars[i].entities.len());
        }

        let mut merged = serde_json::Map::new();
        for &i in scratch.iter() {
            if let Value::Object(map) = &self.sidecars[i].content {
                for (k, v) in map {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }
        merged
    }
}

/// What matching a sidecar to a data file needs to know about that file: which dataset
/// and suffix it belongs to, and what its filename names. Precomputed so the orphan
/// check parses each data file's entities once rather than once per sidecar.
struct DataFileFacts<'a> {
    dataset_id: &'a str,
    suffix: String,
    entities: HashMap<String, String>,
}

/// Data files indexed by `(dataset_id, suffix, ancestor directory)` — each file
/// registered under *every* directory it sits at or below.
///
/// The orphan check asks "is there a data file at or below this sidecar's directory?",
/// which the reverse index (file → its own directory) could only answer by scanning
/// every same-suffix file and testing the prefix — quadratic once a dataset has one
/// sidecar per run. Registering each file under its ancestors instead costs one entry
/// per path component and turns the question into a single lookup.
type DataFilesByDir<'a> = HashMap<(&'a str, &'a str, &'a Path), Vec<usize>>;

/// Whether `sidecar` describes any indexed data file, by the same rule inheritance uses:
/// same dataset and suffix, the data file at or below the sidecar's directory (both
/// already answered by the index key), and the sidecar's entities a subset of the data
/// file's.
fn describes_a_data_file(
    sidecar: &SidecarInfo,
    by_dir: &DataFilesByDir,
    facts: &[DataFileFacts],
) -> bool {
    let dir = Path::new(&sidecar.file_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let key = (sidecar.dataset_id.as_str(), sidecar.suffix.as_str(), dir);
    let Some(candidates) = by_dir.get(&key) else {
        return false;
    };
    candidates.iter().any(|&i| {
        sidecar
            .entities
            .iter()
            .all(|(key, value)| facts[i].entities.get(key) == Some(value))
    })
}

/// A headerless continuous recording (`*_physio.tsv.gz`, `*_stim.tsv.gz`,
/// `*_physioevents.tsv.gz`, `*_motion.tsv`) deferred to the flush phase: its column
/// names come from the merged sidecar's `Columns` (or, for motion, the associated
/// `_channels.tsv`), which is only known once every sidecar has been collected.
struct PendingRecording {
    dataset_id: String,
    rel_path: String,
    suffix: String,
    entities: HashMap<String, String>,
}

/// A header-bearing tabular file deferred so it can be ingested in a **batch** with its
/// siblings (Lever 1b). Files sharing a table and an identical header signature go into one
/// `read_csv([f1,…,fN])` INSERT rather than N, which amortizes the fixed per-file `read_csv`
/// setup — open, plan, state-machine build and teardown — over the whole group.
///
/// **Every** routed tabular file is deferred, whatever its [`RowIdentity`]. The keyed tables
/// (`participants`/`sessions`/`scans`) need a per-file value the batch cannot derive from the
/// rows themselves, so it travels with the file in [`Self::aux`]; and because batching moves
/// their reads to after the walk has synthesized its stub rows, they insert with
/// `INSERT OR REPLACE` so the file's own row wins (see [`BidsParser::flush_tabular`]).
struct PendingTabular {
    spec: TableSpec,
    /// Dataset-relative path (→ `file_path`).
    rel_path: String,
    /// Ready-to-use `read_csv` source (canonical local path or `s3://` URL), passed to
    /// `read_csv` and joined back to `rel_path` through the emitted `__src` column — named
    /// that rather than taking `filename=true`'s default because `scans.tsv` has a literal
    /// `filename` column of its own (see [`build_tabular_batch_select`]).
    source: String,
    /// Raw header line (see [`read_tsv_header`]). The batch key is
    /// `(table, group_key)`, so every file in a group has byte-identical header
    /// bytes — one `read_csv` dialect, and `other_data` stays exact (no
    /// `union_by_name` NULL fillers).
    group_key: String,
    /// The per-file value this table's row identity needs, carried into the batch's
    /// path map: the containing directory for a `PerFile` table (whose `file_path` is
    /// built from its `filename` column), or the `sub-…` label for a session table.
    /// Empty when the identity needs neither.
    aux: String,
    /// Normalized header column names, used to build the batch SQL.
    columns: Vec<String>,
}

impl BidsParser {
    pub fn new(
        fs: Box<dyn BidsFileSystem>,
        dataset_id: Option<String>,
        schema: Schema,
        s3_httpfs: Option<S3Httpfs>,
        apply_bidsignore: bool,
        stat_files: bool,
    ) -> Self {
        let datatypes = schema.datatypes().into_iter().collect();
        Self {
            fs,
            dataset_id,
            root_uri: String::new(),
            s3_httpfs,
            ignore_set: Gitignore::empty(),
            apply_bidsignore,
            stat_files,
            tenure: Tenure::default(),
            pending_associations: Vec::new(),
            pending_gradients: Vec::new(),
            schema,
            imaging_files: Vec::new(),
            registry_extra: Vec::new(),
            file_stats: HashMap::new(),
            walk_status: HashMap::new(),
            has_dataset_description: false,
            sidecars: Vec::new(),
            dataset_type: None,
            datatypes,
            datatype_dirs: HashSet::new(),
            pending_recordings: Vec::new(),
            pending_tabular: Vec::new(),
            validator: Connection::open_in_memory().expect("open in-memory validator connection"),
            seen_participants: HashSet::new(),
            seen_sessions: HashSet::new(),
            content_cache: HashMap::new(),
            tabular_header: HashMap::new(),
            term_maps: Vec::new(),
            readers: readers::default_readers(),
            root_description_json: None,
            declared_sources: Vec::new(),
        }
    }

    /// Assert that this run's root is bidslake-managed (docs/adr/0009).
    ///
    /// A builder rather than another `new` parameter because it is the CLI's `--managed` and
    /// nothing else: every other construction — tests, benches, embedders — wants the
    /// `attached` default, which is the tier that promises nothing beyond durability.
    #[must_use]
    pub fn with_tenure(mut self, tenure: Tenure) -> Self {
        self.tenure = tenure;
        self
    }

    /// Record CLI `--source-dataset` references as `declared` cross-dataset links, so a
    /// dataset with no `SourceDatasets` DOI can still be tied to a catalog dataset.
    pub fn with_declared_sources(mut self, sources: Vec<String>) -> Self {
        self.declared_sources = sources;
        self
    }

    /// Attach term maps that recognize standardized non-BIDS files (FreeSurfer, …).
    /// Ordinary BIDS ingestion configures none, so the classify hot path in
    /// `process_file` short-circuits on an empty term-map list.
    pub fn with_term_maps(mut self, term_maps: Vec<TermMap>) -> Self {
        self.term_maps = term_maps;
        self
    }

    /// Enable httpfs on the read-preflight [`Self::validator`] connection (from the
    /// [`S3Httpfs`] config given to [`Self::new`]) so its `read_csv` sniff can open
    /// `s3://` tabular files, mirroring the write connection. Called once at the
    /// start of [`Self::parse`]; a no-op for local datasets.
    #[cfg(feature = "s3")]
    fn configure_s3_httpfs(&self) -> Result<()> {
        if let Some(cfg) = &self.s3_httpfs {
            crate::s3::configure_httpfs(&self.validator, &cfg.region, cfg.anonymous)?;
        }
        Ok(())
    }

    /// Without the `s3` feature there is no httpfs configuration to apply.
    ///
    /// A caller that supplied an [`S3Httpfs`] asked for something this build cannot
    /// do, so this refuses rather than proceeding with the preflight connection
    /// silently unconfigured — which would surface much later as an unexplained
    /// `read_csv` failure on the first `s3://` tabular file.
    #[cfg(not(feature = "s3"))]
    fn configure_s3_httpfs(&self) -> Result<()> {
        if self.s3_httpfs.is_some() {
            anyhow::bail!(
                "an S3 configuration was supplied, but this bidslake was built without \
                 the `s3` feature; rebuild with `--features s3` (the default)"
            );
        }
        Ok(())
    }

    /// Whether DuckDB can read this `read_csv(...)` call — tested on the throwaway
    /// [`Self::validator`] connection so a parse error can't poison the main ingest
    /// transaction. A readable-but-empty file returns `true` (it just yields no
    /// rows); an unreadable one (bad gzip, malformed) returns `false`.
    fn read_csv_ok(&self, read_csv_from: &str) -> bool {
        let sql = format!("SELECT 1 FROM {read_csv_from} LIMIT 1");
        self.validator
            .prepare(&sql)
            .is_ok_and(|mut stmt| stmt.query([]).is_ok())
    }

    /// This run's registry key for a dataset-relative path — the value every satellite table
    /// stores instead of `(dataset_id, file_path)`.
    ///
    /// `root_uri` is the run's, because a run walks exactly one root and every path it hands
    /// this is relative to that root.
    fn file_key(&self, dataset_id: &str, rel_path: &str) -> u64 {
        file_id(dataset_id, &self.root_uri, rel_path)
    }

    /// One `file_registry` row (docs/adr/0006).
    ///
    /// `root_uri` comes from the run rather than the file: a run walks exactly one root, and
    /// that root is what this file's `file_path` is relative to. Together with `dataset_id`
    /// they give `file_id`, which is the key every satellite table points at.
    fn registry_row(
        &self,
        dataset_id: &str,
        file_path: &str,
        kind: Kind,
        projected: Option<&Value>,
    ) -> Value {
        let mut row = serde_json::Map::new();
        row.insert(
            "file_id".to_string(),
            // A JSON *number*, not a string: a `u64` is exactly what
            // `serde_json::Number` holds, so the id needs no decimal-string detour to
            // reach the writer (see [`file_id`]).
            Value::Number(file_id(dataset_id, &self.root_uri, file_path).into()),
        );
        row.insert(
            "dataset_id".to_string(),
            Value::String(dataset_id.to_string()),
        );
        row.insert("root_uri".to_string(), Value::String(self.root_uri.clone()));
        row.insert(
            "file_path".to_string(),
            Value::String(file_path.to_string()),
        );
        row.insert("kind".to_string(), Value::String(kind.as_str().to_string()));
        // What the walk observed about the file itself. Absent for a file the backend could
        // not stat, and for every file under `--no-stat`: left out of the row rather than
        // written as a JSON null, so the column is NULL by the write path's own default and
        // "unknown" has one representation.
        if let Some(st) = self.file_stats.get(file_path) {
            row.insert(
                "size_bytes".to_string(),
                Value::Number(st.size_bytes.into()),
            );
            row.insert("mtime_ns".to_string(), Value::Number(st.mtime_ns.into()));
        }
        // A status decided during the walk, before this row existed to UPDATE.
        if let Some(status) = self.walk_status.get(file_path) {
            row.insert(
                "status".to_string(),
                Value::String(status.as_str().to_string()),
            );
        }
        if let Some(projected) = projected {
            row.insert("projected".to_string(), projected.clone());
        }
        Value::Object(row)
    }

    /// Every dataset-relative path the walk registered, for resolving a reference to a
    /// `file_id`. A reference to a path not in here names a file this dataset does not ship.
    fn registered_paths(&self) -> HashSet<&str> {
        self.registry_extra
            .iter()
            .map(|e| e.file_path.as_str())
            .chain(self.imaging_files.iter().map(|f| f.file_path.as_str()))
            .collect()
    }

    /// Every walked file, as registry rows: the non-data files the walk collected, then the
    /// data files.
    ///
    /// A path in both lists is a **promoted** metadata-only record — a sidecar whose data file
    /// the dataset does not ship (MRIQC), which `promote_orphan_sidecars` turns into a data
    /// file after the walk already classified it as a sidecar. The data-file row wins, so the
    /// registry agrees with `scans` about what it is. Filtered rather than left to the upsert
    /// to resolve, because both rows share a `file_id` and which one landed last would
    /// otherwise depend on ordering.
    fn registry_rows(&self, dataset_id: &str) -> Vec<Value> {
        let data_paths: HashSet<&str> = self
            .imaging_files
            .iter()
            .map(|f| f.file_path.as_str())
            .collect();
        let mut rows = Vec::with_capacity(self.registry_extra.len() + self.imaging_files.len());
        rows.extend(
            self.registry_extra
                .iter()
                .filter(|e| !data_paths.contains(e.file_path.as_str()))
                .map(|e| self.registry_row(dataset_id, &e.file_path, e.kind, e.projected.as_ref())),
        );
        rows.extend(self.imaging_files.iter().map(|f| {
            self.registry_row(
                &f.dataset_id,
                &f.file_path,
                Kind::Data,
                f.projected.as_ref(),
            )
        }));
        rows
    }

    /// Bind this run's ingest root to `dataset_id`, and decide whether an existing dataset
    /// of that name is *this* dataset (docs/adr/0005).
    ///
    /// A dataset may have many roots — subject-sharded pipeline output, the normal way
    /// fMRIPrep and FreeSurfer are run at scale, is one logical dataset with one root per
    /// subject — so a second root is additive rather than refused. `root_id` is what keeps
    /// them apart: `file_path` is relative to the root it came from, and resolution joins
    /// through `dataset_roots`.
    ///
    /// Two roots is only unambiguous when the *user* said so. When `--dataset-id` was
    /// asserted, this trusts it and merely warns if the descriptions disagree. When the id
    /// was **inferred**, it cannot be trusted on its own: `Name` is not an identity —
    /// every fMRIPrep output declares `"fMRIPrep - fMRI PREProcessing workflow"`, the
    /// tool's name — so two unrelated studies infer the same id. There the descriptions
    /// must agree on [`IDENTITY_FIELDS`] before the roots merge; `SourceDatasets` is what
    /// actually separates those two studies.
    ///
    /// Returns this run's `root_uri`, now registered.
    fn resolve_root(&self, db: &BidsDb, dataset_id: &str, id_was_asserted: bool) -> Result<String> {
        let root_uri = self.fs.root();
        let existing = db.dataset_roots(dataset_id)?;

        // Re-indexing a root already registered: nothing to decide about *identity*. Still
        // re-register, because tenure may have been raised since — `--managed` on a re-index
        // has to take effect, and the attached default is a no-op here by construction.
        if existing.iter().any(|uri| uri == &root_uri) {
            db.register_dataset_root(dataset_id, &root_uri, self.tenure)?;
            return Ok(root_uri);
        }

        // A new root joining an existing dataset — the one genuinely ambiguous case.
        if !existing.is_empty() {
            let differing = self.description_mismatches(db, dataset_id)?;
            if !differing.is_empty() {
                if id_was_asserted {
                    // The user named the dataset, so the merge stands. Say what disagrees
                    // anyway: under an asserted id, a differing `Name` is the strongest
                    // signal available that --dataset-id was mistyped.
                    eprintln!(
                        "Warning: adding root {root_uri} to dataset {dataset_id:?}, whose \
                         dataset_description.json differs from the stored one ({}). \
                         Proceeding because --dataset-id was given; check it if that is a \
                         surprise.",
                        differing.join(", ")
                    );
                } else {
                    let roots = existing
                        .iter()
                        .map(|uri| format!("\n  {uri}"))
                        .collect::<String>();
                    anyhow::bail!(
                        "dataset {dataset_id:?} is already in this catalog with {} root(s):{roots}\n\
                         and its dataset_description.json differs from this run's ({}).\n\n\
                         The dataset name was inferred from that file, so bidslake cannot tell \
                         whether {root_uri} is another root of it or a different dataset. Say \
                         which:\n\n  \
                         --dataset-id {dataset_id:?}   another root of it\n  \
                         --dataset-id <other-name>     a different dataset",
                        existing.len(),
                        differing.join(", ")
                    );
                }
            }
        }

        db.register_dataset_root(dataset_id, &root_uri, self.tenure)?;
        Ok(root_uri)
    }

    /// Which of [`IDENTITY_FIELDS`] differ between this run's root
    /// `dataset_description.json` and the row already stored for `dataset_id`. Empty when
    /// they agree, and empty when there is no stored row yet (nothing to disagree with).
    ///
    /// Compared as parsed [`Value`]s rather than as text, so that two files carrying the
    /// same `SourceDatasets` with their keys in a different order still count as equal.
    /// The stored side is text because these are `VARCHAR` columns — `row_values` writes an
    /// array or object into one as compact JSON — so it is parsed back before comparing.
    fn description_mismatches(&self, db: &BidsDb, dataset_id: &str) -> Result<Vec<&'static str>> {
        let cols = IDENTITY_FIELDS
            .iter()
            .map(|f| quote_ident(f))
            .collect::<Vec<_>>()
            .join(", ");
        let mut stmt = db.conn.prepare(&format!(
            "SELECT {cols} FROM dataset_description WHERE dataset_id = ? LIMIT 1"
        ))?;
        let stored: Option<Vec<Option<String>>> = stmt
            .query_map([dataset_id], |r| {
                (0..IDENTITY_FIELDS.len())
                    .map(|i| r.get::<_, Option<String>>(i))
                    .collect()
            })?
            .next()
            .transpose()?;
        let Some(stored) = stored else {
            return Ok(Vec::new());
        };

        let incoming = self.root_description_json.as_ref();
        Ok(IDENTITY_FIELDS
            .iter()
            .zip(stored)
            .filter(|(field, have)| {
                let want = incoming
                    .and_then(|d| d.get(**field))
                    .filter(|v| !v.is_null());
                !stored_matches(have.as_deref(), want)
            })
            .map(|(field, _)| *field)
            .collect())
    }

    pub async fn parse(&mut self, db: &BidsDb) -> Result<()> {
        // Whether the caller named the dataset. Captured before the walk resolves an
        // inferred one, because `resolve_root` treats the two differently: an asserted id
        // is the user's claim that this root belongs to that dataset, an inferred one is
        // only a `Name` — which pipelines reuse across every study they process.
        let id_was_asserted = self.dataset_id.is_some();

        // Configure httpfs on the read-preflight connection (if this is an S3
        // ingest) before any `read_csv` sniff runs. No-op for local datasets.
        self.configure_s3_httpfs()?;

        // Opt-in phase accounting (`BIDSLAKE_TIMING`); see `crate::timing`.
        let walk_phase = timing::scope(Phase::Walk);

        // Load .bidsignore patterns before parsing (unless `--no-bidsignore`, which
        // leaves `ignore_set` empty so nothing is filtered on the parser side either).
        if self.apply_bidsignore {
            self.load_bidsignore().await?;
        }

        // Collect all file paths first
        let mut dataset_description: Vec<std::path::PathBuf> = Vec::new();
        let mut participants_tsv: Vec<std::path::PathBuf> = Vec::new();
        let mut sessions_tsv: Vec<std::path::PathBuf> = Vec::new();
        let mut other_files: Vec<std::path::PathBuf> = Vec::new();

        // Pseudo-file extensions (`.ds/`, `.ome.zarr/`, …) from the schema, so opaque BIDS
        // directories are emitted as single files (and become association sources).
        let pseudo_exts = bids_schema::pseudo_file_extensions(self.schema.raw());
        let files: Vec<std::path::PathBuf> =
            self.fs.walk(&pseudo_exts, self.apply_bidsignore).await?;
        timing::count(Counter::Files, files.len() as u64);
        if let Some(tree) = self.fs.file_tree() {
            timing::count(Counter::Dirs, tree.walk_directories().count() as u64);
        }

        // One concurrent pass over everything the walk found. Free on S3 (the listing
        // already carried both) and ~2 µs per file locally; on a parallel filesystem it is
        // a metadata-server round trip each, which is why `stat_many` overlaps them rather
        // than paying them in turn. `--no-stat` skips it, leaving both columns NULL.
        if self.stat_files {
            let _t = timing::scope(Phase::Walk);
            let stats = self.fs.stat_many(&files).await?;
            self.file_stats = files
                .iter()
                .zip(stats)
                .filter_map(|(p, st)| st.map(|st| (p.to_string_lossy().into_owned(), st)))
                .collect();
        }

        for path in files {
            let file_name = path.file_name().unwrap().to_str().unwrap();

            // Skip dotfiles
            if file_name.starts_with('.') {
                continue;
            }

            // Skip files matching .bidsignore patterns. `matched_path_or_any_parents`
            // applies gitignore semantics — crucially it also tests parent dirs, so a
            // directory pattern like `logs/` excludes everything beneath it.
            if self
                .ignore_set
                .matched_path_or_any_parents(&path, false)
                .is_ignore()
            {
                continue;
            }

            // Record datatype directories so a tabular file sitting above one (a
            // session- or subject-level channels.tsv) can still be routed. For a
            // path like `sub-01/ses-meg/meg/…`, note the datatype dir
            // (`sub-01/ses-meg/meg`, `meg`).
            let comps: Vec<&str> = path.iter().filter_map(|c| c.to_str()).collect();
            for (i, comp) in comps.iter().enumerate() {
                if self.datatypes.contains(*comp) {
                    self.datatype_dirs
                        .insert((comps[..=i].join("/"), comp.to_string()));
                }
            }

            // Categorize files
            if file_name == "dataset_description.json" {
                dataset_description.push(path);
            } else if file_name == "participants.tsv" {
                participants_tsv.push(path);
            } else if file_name == "sessions.tsv" {
                sessions_tsv.push(path);
            } else {
                other_files.push(path);
            }
        }

        drop(walk_phase);

        // Concurrently prefetch the file contents the serial passes will read —
        // JSON sidecars (full) and TSV headers (first 64 KiB). On a network
        // filesystem these are per-file round-trips; reading them with bounded
        // concurrency overlaps the latency instead of paying it one file at a time.
        // Warm local disk sees a negligible change.
        {
            let _t = timing::scope(Phase::Prefetch);
            self.prefetch_contents(&dataset_description, &other_files)
                .await;
        }

        // Starts after the prefetch, so `process` is the serial passes alone.
        let process_phase = timing::scope(Phase::Process);

        // Datasets can carry nested dataset_description.json files (e.g. under
        // derivatives/). Sort shallowest-first so the dataset root wins when we
        // resolve the dataset_id and insert the description.
        dataset_description.sort_by_key(|p| p.components().count());

        // Pass 0: read the descriptions to resolve the dataset_id, and to capture the
        // ROOT one — `resolve_root` compares it against what the catalog already holds,
        // and `record_links` reads its provenance fields at finalize. Captured here rather
        // than during the walk because `resolve_root` runs before it. The bodies are
        // already prefetched, and `read_cached` leaves them in place for the second pass.
        for path in &dataset_description {
            if self.dataset_id.is_some() && self.root_description_json.is_some() {
                break;
            }
            let content = self.read_cached(path, &path.to_string_lossy()).await?;
            let desc = match serde_json::from_str::<Value>(&content) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!(
                        "Warning: skipping unparseable dataset_description.json at {}: {}",
                        path.display(),
                        e
                    );
                    continue;
                }
            };
            // Shallowest-first, so the first parseable one is the dataset's own; a nested
            // description under `derivatives/` describes a different dataset.
            if self.root_description_json.is_none() {
                self.root_description_json = Some(desc.clone());
            }
            if self.dataset_id.is_none()
                && let Some(name) = desc.get("Name").and_then(|v| v.as_str())
            {
                println!("Using dataset name from dataset_description.json: {}", name);
                self.dataset_id = Some(name.to_string());
            }
        }

        // If still no dataset_id, use root name or default
        if self.dataset_id.is_none() {
            // For S3, root might be s3://bucket/prefix/
            // We can try to extract the last part of the prefix
            let root = self.fs.root();
            let dir_name = if root.starts_with("s3://") {
                root.trim_end_matches('/')
                    .split('/')
                    .next_back()
                    .unwrap_or("unknown")
                    .to_string()
            } else {
                Path::new(&root)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string()
            };

            println!("Using directory/prefix name as dataset_id: {}", dir_name);
            self.dataset_id = Some(dir_name);
        }

        let dataset_id = self.dataset_id.as_ref().unwrap().clone();

        // The id is settled, so this root can be bound to it — additively, since a dataset
        // may have many roots. Deliberately here rather than before the walk: an inferred
        // id does not exist until now, and it is the inferred case that needs deciding.
        self.root_uri = self.resolve_root(db, &dataset_id, id_was_asserted)?;

        // Process dataset_description.json again to insert it
        for path in dataset_description {
            self.process_file(&path, db, &dataset_id).await?;
        }

        // Pass 1: Process participants.tsv files
        for path in participants_tsv {
            self.process_file(&path, db, &dataset_id).await?;
        }

        // Pass 2: Process sessions.tsv files
        for path in sessions_tsv {
            self.process_file(&path, db, &dataset_id).await?;
        }

        // Pass 3: Process all other files
        for path in other_files {
            self.process_file(&path, db, &dataset_id).await?;
        }

        // The prefetch caches are dead once the passes are done: every reader of
        // `content_cache`/`tabular_header` is inside the passes above. Most of those readers
        // take their entry rather than copying it (see `take_cached`), so the bodies drain as
        // the passes advance; this drops whatever is left — chiefly the `dataset_description`
        // entries, which are read twice and so cannot be taken.
        //
        // Releasing here matters at scale rather than being tidiness: a derivative dataset's
        // per-run column dictionaries are hundreds of kilobytes each, so tens of thousands of
        // them is gigabytes, and they would otherwise stay resident through inheritance, which
        // is itself the peak (it builds one merged row per data file before appending any).
        self.content_cache = HashMap::new();
        self.tabular_header = HashMap::new();

        // A metadata-only derivative (MRIQC's IQM sidecars, …) ships no data file for its
        // records, so promote those sidecars to data files before anything is written. Runs
        // here, at the end of the walk, because it changes a file's `kind` and the registry
        // below must record the outcome rather than the guess.
        self.promote_orphan_sidecars();

        // The registry, before any table that points at it (docs/adr/0006) — including the
        // batched tabular flush below, whose `scans.tsv` rows resolve their `file_id` by
        // joining it. Every accumulator it reads is complete once the walk is done.
        //
        // Written through the staged upsert rather than the Appender: the registry has a
        // primary key and no generated columns, so a re-index replaces a row rather than
        // colliding with itself.
        //
        // Fatal, not a warning. Every table below points at the registry, so a run that
        // failed to write it has produced no catalog at all — and swallowing the error
        // let that run go on to print "Conversion complete!" and exit 0. Returning here
        // drops the ingest transaction instead, leaving the catalog as it was.
        let registry_rows = self.registry_rows(&dataset_id);
        db.upsert_rows(&self.schema, "file_registry", &registry_rows)
            .with_context(|| {
                format!(
                    "writing {} file registry rows for {dataset_id}",
                    registry_rows.len()
                )
            })?;

        // Lever 1b: ingest the deferred per-row tabular files in header-grouped
        // batches now that all of them are collected.
        {
            let _t = timing::scope(Phase::TabularReadCsv);
            self.flush_tabular(db).await?;
        }

        drop(process_phase);
        let finalize_phase = timing::scope(Phase::Finalize);

        // Every ingested dataset gets a `dataset_description` row, even one that has no
        // `dataset_description.json` — the normal case for a dataset ingested through a
        // layout adapter (FreeSurfer `recon-all`, …), which by definition has none. The row
        // holds nothing but the id, and that is the point: `lake.datasets()` reads this
        // table, and the wide `files` view LEFT JOINs it, so without a row an adapter
        // dataset would be absent from both. (Resolution does not depend on it — the roots
        // are in `dataset_roots`, registered before the walk.)
        //
        // Guarded, and after the walk: a bare row must never replace a real description.
        if !self.has_dataset_description {
            let mut row = serde_json::Map::new();
            row.insert(
                "dataset_id".to_string(),
                Value::String(dataset_id.to_string()),
            );
            db.insert(&self.schema, "dataset_description", &Value::Object(row))
                .with_context(|| {
                    format!("inserting synthesized dataset_description for {dataset_id}")
                })?;
        }

        // Cross-dataset links: record what this dataset declares it came from and what it
        // *is*, for the query-time `dataset_relations` view (docs/adr/0003).
        self.record_links(db, &dataset_id)?;

        // File associations: the `IntendedFor` rows collected during the walk, plus the schema's
        // structural associations (events↔bold, bval/bvec↔dwi, channels↔eeg, …) resolved via the
        // shared `bids_schema` resolver. Deduped on the `file_associations` primary key (cheaper
        // than a DB `ON CONFLICT`; the table is tiny), then inserted.
        let mut associations = std::mem::take(&mut self.pending_associations);
        {
            let _t = timing::scope(Phase::Associations);
            associations.extend(self.resolve_structural_associations());
        }
        timing::count(Counter::Associations, associations.len() as u64);

        // Resolving a target to an id needs the whole set of walked paths, which is why this
        // happens here rather than where the association was collected: an `IntendedFor` may
        // name a file the dataset does not ship, and that reference is kept (as a path with a
        // NULL id) rather than dropped.
        let registered = self.registered_paths();
        // Dedup on borrowed keys — cloning the key per candidate allocated far more than the
        // table itself holds.
        let mut seen: HashSet<(&str, &str, &str)> = HashSet::new();
        let deduped: Vec<FileAssociation> = associations
            .iter()
            .filter(|a| {
                seen.insert((
                    a.source_file.as_str(),
                    a.target_file.as_str(),
                    a.assoc_type.as_str(),
                ))
            })
            .map(|a| FileAssociation {
                source_file_id: self.file_key(&dataset_id, &a.source_file),
                target_file_id: registered
                    .contains(a.target_file.as_str())
                    .then(|| self.file_key(&dataset_id, &a.target_file)),
                target_file_path: a.target_file.clone(),
                assoc_type: a.assoc_type.clone(),
            })
            .collect();
        // That dedup is against this run's own candidates, and the staged upsert *depends* on
        // it without being able to check it: a duplicate within one batch is dropped silently
        // rather than refused (see `BidsDb::upsert_staged`). It holds because the key deduped
        // above — `(source_file, target_file, assoc_type)` — is the primary key
        // `(source_file_id, target_file_path, association_type)` under a different name:
        // `dataset_id` is fixed here, so `file_key` maps source paths to ids one-for-one.
        //
        // A row this dataset already has from a previous run is a different matter, and is
        // what the `OR REPLACE` is for (see `BidsDb::upsert_file_associations`), so a re-index
        // refreshes rather than colliding.
        //
        // One statement for the whole dataset, so a failure here loses every association
        // it has — with nothing in the catalog to say so. Fatal, like the registry.
        {
            let _t = timing::scope(Phase::Writes);
            db.upsert_file_associations(&deduped)
                .with_context(|| format!("writing {} file associations", deduped.len()))?;
        }

        // Gradient payloads, keyed by the gradient file itself. No lookup and no pairing:
        // the path is a registered file by construction, so nothing can be skipped for
        // naming an image that does not exist, and a `.bval` shipped without its `.bvec`
        // (or either half inherited from a different level) is stored rather than dropped.
        // Which images they describe is `file_associations`' answer, and the `diffusion`
        // view's (docs/adr/0007).
        //
        // Split by kind and written in two batches rather than a call per file, because each
        // call is one staged upsert: per file that would be a temp table per file, which costs
        // more than the row-at-a-time path it replaces.
        //
        // `(file_id, row_idx)` must be unique across a batch, and the staged upsert cannot
        // check it (see `BidsDb::upsert_staged`). It holds because `pending_gradients` takes
        // one entry per walked gradient file and the walk yields a path once, so no two
        // entries share a `file_key`.
        let mut bvals: Vec<BvalFile<'_>> = Vec::new();
        let mut bvecs: Vec<BvecFile<'_>> = Vec::new();
        for (dataset, path, gradient) in &self.pending_gradients {
            let file_id = self.file_key(dataset, path);
            match gradient {
                PendingGradient::Bvals(b) => bvals.push((file_id, b)),
                PendingGradient::Bvecs(x, y, z) => bvecs.push((file_id, x, y, z)),
            }
        }
        {
            let _t = timing::scope(Phase::Writes);
            // Fatal, unlike the tabular flush's per-file tolerance: a failed tabular file is
            // still recorded, with `status = "failed"`, so the catalog says the rows are
            // missing. Gradients have no such bookkeeping — a swallowed error here is a file
            // whose values are simply absent, indistinguishable from one that never had any.
            //
            // Batching costs the file name in that error, the same trade the associations
            // above already make: one statement for the whole dataset means a failure loses
            // all of them at once, so there is nothing narrower to name.
            db.upsert_bvals(&bvals)
                .with_context(|| format!("writing b-values for {} gradient files", bvals.len()))?;
            db.upsert_bvecs(&bvecs)
                .with_context(|| format!("writing b-vectors for {} gradient files", bvecs.len()))?;
        }

        // An empty catalog is almost always a `.bidsignore` that hid the very files the
        // caller wanted. MRIQC, for instance, lists its own `*_T1w.json`/`*_bold.json`
        // IQMs there (they are deliberately not valid BIDS on their own), so a plain
        // index silently yields nothing. Say so, rather than reporting success over an
        // empty database.
        if self.imaging_files.is_empty()
            && self.apply_bidsignore
            && self.ignore_set.num_ignores() > 0
        {
            eprintln!(
                "Note: no data files were indexed, and this dataset's .bidsignore is in \
                 force. Pipelines hide their non-standard outputs there (MRIQC hides the \
                 very JSON sidecars holding its metrics); re-run with --no-bidsignore to \
                 index them."
            );
        }

        // `scans` is the `scans.tsv` satellite, not a file registry (docs/adr/0006), so it is
        // filled from that file's contents and nothing else — the way `sessions` is filled from
        // `sessions.tsv`. A dataset without one simply has no rows here.
        //
        // It used to be seeded with a row per discovered data file, so that sidecars and
        // associations had a referent to point at. That reason is gone: they point at
        // `file_registry`, which holds every file whether or not a `scans.tsv` mentions it. The
        // seeding survived the split only as habit, and it made `scans` claim to describe
        // acquisitions it knew nothing about — 80 all-NULL rows on `ds001`, which ships no
        // `scans.tsv` at all.

        drop(finalize_phase);
        let inherit_phase = timing::scope(Phase::Inherit);

        println!(
            "Applying BIDS inheritance for {} imaging files...",
            self.imaging_files.len()
        );
        // Inheritance merges the JSON sidecars already collected in memory during
        // the walk (`process_json_file`) — it never re-reads them from disk. This
        // matches the tree-based reference resolver exactly (verified row-for-row
        // across the whole `bids-examples` corpus) and is the single path for every
        // backend, so a shared sidecar is read once regardless of how many imaging
        // files inherit it.
        self.apply_inheritance_collected(db)?;

        drop(inherit_phase);
        let flush_phase = timing::scope(Phase::Flush);

        // Ingest the deferred headerless recordings now that every sidecar and
        // channels file is available (their columns come from those).
        self.flush_recordings(db).await?;

        drop(flush_phase);

        Ok(())
    }

    /// Build one merged-sidecar row: the merged metadata as `other_data`, plus each
    /// field also flattened to its own column (schema-known fields get typed
    /// columns). `None` when `merged` is empty. Collected and bulk-inserted via the
    /// Appender (`sidecars` is very wide and carries the generated columns, so a
    /// per-row INSERT is especially slow).
    ///
    /// When the ingestion policy scopes `undeclared: catalog` onto this file, only the
    /// fields with dedicated columns are kept — the sidecar on disk stays the record of
    /// the rest. fMRIPrep's `desc-confounds_timeseries.json` is the case that motivates
    /// it: a 366 KB column dictionary describing ~2,200 confound regressors, one per
    /// BOLD run, and 100% of the sidecar JSON bytes in a real derivatives catalog.
    ///
    /// The filter has to happen *here*, on the flatten loop, not by dropping the
    /// `other_data` insert below: [`Schema::row_values`] treats `other_data` as one of
    /// its own `schema_keys`, so it discards the map passed here and recomputes the
    /// column from these flattened top-level keys. Removing that insert is a no-op.
    fn build_sidecar_row(
        &self,
        dataset_id: &str,
        file_path: &str,
        merged: serde_json::Map<String, Value>,
    ) -> Option<Value> {
        if merged.is_empty() {
            return None;
        }
        // This runs once per data file, inside the inheritance loop, so the context is built
        // only when the policy will actually look at it — i.e. when some fragment scopes
        // `undeclared` with `undeclaredWhen`. Otherwise the answer is a property of the table
        // alone, and parsing the filename to derive a suffix nothing reads is pure waste.
        let ingestion = self.schema.ingestion();
        let sidecar = Value::Null;
        let keep_undeclared = if ingestion.undeclared_needs_context("sidecars") {
            let (suffix, extension) = split_suffix_ext(file_path);
            let datatype = self.datatype_dir_in_path(file_path);
            // BIDS selector paths are dataset-relative with a leading slash.
            let path_with_slash = format!("/{file_path}");
            ingestion.undeclared_for(
                "sidecars",
                &FileContext {
                    path: &path_with_slash,
                    datatype: datatype.as_deref(),
                    suffix: Some(&suffix),
                    extension: Some(&extension),
                    sidecar: &sidecar,
                    dataset_type: self.dataset_type.as_deref(),
                },
            )
        } else {
            ingestion.undeclared_for("sidecars", &FileContext::default())
        } == Undeclared::Store;

        // The merged map *is* the row: `Schema::row_values` reads each declared column
        // from it by key and rebuilds the `other_data` overflow from whatever is left
        // over, so nothing has to be copied into a fresh map first.
        //
        // What this replaces was two full copies of the metadata per row — one from
        // cloning every entry into a new map, and a second from also storing the whole
        // map under `other_data`, which `row_values` then ignored in favour of
        // recomputing the overflow itself. Sidecar values are not reliably small (a real
        // `_bold.json` can run to megabytes), so those copies dominated inheritance on a
        // metadata-heavy dataset.
        let mut sidecar_entry = merged;
        if !keep_undeclared {
            sidecar_entry.retain(|k, _| self.schema.declares("sidecars", k));
        }
        // The structural key, added only if the metadata does not already claim the name.
        // That is the same precedence as before: it was inserted first and a sidecar key of
        // the same name overwrote it, so the sidecar's value still wins. Note the key is the
        // *data file*'s — a sidecars row is metadata about the file described, not about the
        // `.json` it was read from, which has a registry row of its own.
        sidecar_entry
            .entry("file_id".to_string())
            .or_insert_with(|| Value::Number(self.file_key(dataset_id, file_path).into()));
        Some(Value::Object(sidecar_entry))
    }

    /// Promote metadata-only records: file-level JSON sidecars whose data file this
    /// dataset does not ship.
    ///
    /// `sidecars` rows are keyed by the data file a sidecar describes (and foreign-key
    /// into `scans`), so a sidecar with no data file is silently dropped. That loses a
    /// whole class of derivative: MRIQC publishes its image-quality metrics as
    /// `sub-…_T1w.json` and never writes the matching `.nii.gz`, so every IQM vanished.
    /// For such a file the JSON *is* the record — treat it as a data file, so it earns a
    /// `scans` row (satisfying the FK, and making it queryable by concept) and its
    /// metrics reach `sidecars` through the ordinary inheritance path.
    ///
    /// Only a genuinely orphaned, *file-level* sidecar qualifies: one that describes no
    /// indexed data file, sits in a datatype directory, and names a subject. That
    /// excludes inheritance templates (a dataset- or subject-level `task-nback_bold.json`),
    /// which describe files elsewhere and must never become records themselves.
    fn promote_orphan_sidecars(&mut self) {
        // Structural candidates first: a template — no subject, or outside a datatype
        // directory — can never be promoted, so an ordinary dataset usually stops here.
        let candidates: Vec<usize> = self
            .sidecars
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.entities.contains_key("sub") && self.datatype_dir_in_path(&s.file_path).is_some()
            })
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() {
            return;
        }

        let promoted: Vec<ImagingFile> = {
            // Parse each data file's name once…
            let facts: Vec<DataFileFacts> = self
                .imaging_files
                .iter()
                .map(|f| {
                    let name = f.file_path.split('/').next_back().unwrap_or_default();
                    let parts = read_entities(name);
                    DataFileFacts {
                        dataset_id: f.dataset_id.as_str(),
                        suffix: parts.suffix,
                        entities: parts.entities,
                    }
                })
                .collect();
            // …then register it under every directory it sits at or below, so the
            // orphan check is a lookup rather than a scan (see `DataFilesByDir`).
            let mut by_dir: DataFilesByDir = HashMap::new();
            for (i, f) in facts.iter().enumerate() {
                let dir = Path::new(&self.imaging_files[i].file_path)
                    .parent()
                    .unwrap_or_else(|| Path::new(""));
                for ancestor in dir.ancestors() {
                    by_dir
                        .entry((f.dataset_id, f.suffix.as_str(), ancestor))
                        .or_default()
                        .push(i);
                }
            }
            candidates
                .into_iter()
                .filter(|&i| !describes_a_data_file(&self.sidecars[i], &by_dir, &facts))
                .map(|i| ImagingFile {
                    dataset_id: self.sidecars[i].dataset_id.clone(),
                    file_path: self.sidecars[i].file_path.clone(),
                    projected: None,
                })
                .collect()
        };

        if !promoted.is_empty() {
            println!(
                "Promoting {} metadata-only record(s): sidecars whose data file this dataset does not ship.",
                promoted.len()
            );
            self.imaging_files.extend(promoted);
        }
    }

    /// BIDS inheritance for the `sidecars` table, merging the JSON sidecars already
    /// collected in memory during the walk — no disk re-read. Sidecars are keyed by
    /// `(dataset_id, suffix, directory)` with an entity-subset match, and visited
    /// ancestor-directory order so a nearer (deeper) sidecar overrides. This is the
    /// sole inheritance path (local and S3); it reproduces the tree-based reference
    /// resolver row-for-row across the corpus.
    ///
    /// Errors rather than warning if the write fails: these rows are the dataset's
    /// metadata, and a run that lost them has not produced the catalog it reports.
    fn apply_inheritance_collected(&self, db: &BidsDb) -> Result<()> {
        timing::count(Counter::ImagingFiles, self.imaging_files.len() as u64);
        timing::count(Counter::Sidecars, self.sidecars.len() as u64);
        let index = SidecarIndex::new(&self.sidecars);
        let mut rows: Vec<Value> = Vec::new();
        // Reused across files rather than reallocated per file.
        let mut scratch: Vec<usize> = Vec::new();
        // Split from the append below: the two are different kinds of work — merging
        // and shaping happen in Rust, appending happens inside DuckDB — and which of
        // them dominates decides where a fix goes.
        let merge_phase = timing::scope(Phase::InheritMerge);
        for img_file in &self.imaging_files {
            // Extract entities and suffix from imaging file
            let file_name = img_file.file_path.split('/').next_back().unwrap();
            let img_parts = read_entities(file_name);

            let merged_metadata = index.merged(
                &img_file.dataset_id,
                &img_file.file_path,
                &img_parts.suffix,
                &img_parts.entities,
                &mut scratch,
            );
            // How much metadata inheritance actually moves, which the row count alone
            // does not say: a dataset whose sidecars carry ten keys and one whose
            // sidecars carry three hundred look identical by row count.
            timing::count(Counter::MergedKeys, merged_metadata.len() as u64);

            if let Some(row) =
                self.build_sidecar_row(&img_file.dataset_id, &img_file.file_path, merged_metadata)
            {
                rows.push(row);
            }
        }
        drop(merge_phase);
        let _append_phase = timing::scope(Phase::InheritAppend);
        // Upserted, not appended: every row here is rebuilt from the sidecars on disk, so a
        // re-index rewrites rows it already wrote. It cannot lean on `scans`' read-back `seen`
        // set either — a file's metadata can change without its path changing, so a row already
        // present still has to be replaced rather than skipped.
        db.upsert_rows(&self.schema, "sidecars", &rows)
            .with_context(|| format!("writing {} sidecars rows", rows.len()))
    }

    /// Read, with bounded concurrency, the file bodies and TSV headers the serial
    /// passes will consume, into [`Self::content_cache`] / [`Self::tabular_header`],
    /// so a network filesystem's per-file latency is overlapped rather than paid one
    /// round-trip at a time.
    ///
    /// A file's body is read either by Rust or by DuckDB; we prefetch every body Rust
    /// reads. Full bodies (→ `content_cache`): JSON sidecars, `.bval`/`.bvec` (the
    /// `diffusion` reader), and adapter `read`-disposition files (`fs_stats`/…) — see
    /// [`Self::body_read_in_rust`]. A `.tsv` body is DuckDB's (`read_csv`), so only
    /// its header is prefetched (→ `tabular_header`). Failed reads are left uncached —
    /// the consuming pass falls back to a direct read (and handles the error there).
    async fn prefetch_contents(
        &mut self,
        dataset_description: &[std::path::PathBuf],
        other_files: &[std::path::PathBuf],
    ) {
        use futures::stream::StreamExt;
        /// Bounded so a huge dataset can't open thousands of sockets at once.
        const CONCURRENCY: usize = 16;

        let rel = |p: &std::path::PathBuf| p.to_string_lossy().to_string();
        // Full-body reads: JSON (sidecars + dataset_description), plus every other
        // file whose body a pass reads in Rust (`.bval`/`.bvec` and adapter `read`
        // files, decided by `body_read_in_rust`).
        let body_paths: Vec<String> = dataset_description
            .iter()
            .chain(other_files.iter())
            .filter(|p| {
                p.extension().is_some_and(|e| e == "json")
                    || self.body_read_in_rust(&p.to_string_lossy())
            })
            .map(rel)
            .collect();
        // Header candidates: uncompressed `.tsv` (per-row files sniff a header;
        // `.tsv.gz` is never read here).
        let tsv_paths: Vec<String> = other_files
            .iter()
            .filter(|p| p.to_string_lossy().ends_with(".tsv"))
            .map(rel)
            .collect();

        let (body_res, hdr_res) = {
            let fs = &self.fs;
            let body_fut = futures::stream::iter(body_paths)
                .map(|p| async move {
                    let c = fs.read_to_string(Path::new(&p)).await.ok();
                    (p, c)
                })
                .buffer_unordered(CONCURRENCY)
                .collect::<Vec<_>>();
            let hdr_fut = futures::stream::iter(tsv_paths)
                .map(|p| async move {
                    let h = fs
                        .read_head(Path::new(&p), 64 * 1024)
                        .await
                        .ok()
                        .and_then(|c| tsv_header_from_line(c.split('\n').next().unwrap_or("")));
                    (p, h)
                })
                .buffer_unordered(CONCURRENCY)
                .collect::<Vec<_>>();
            futures::join!(body_fut, hdr_fut)
        };

        for (p, c) in body_res {
            if let Some(c) = c {
                timing::count(Counter::BodiesRead, 1);
                self.content_cache.insert(p, c);
            }
        }
        for (p, h) in hdr_res {
            timing::count(Counter::HeadsRead, 1);
            self.tabular_header.insert(p, h);
        }
    }

    /// Whether a pass reads this file's body **in Rust** (so [`prefetch_contents`]
    /// should cache it). True for `.bval`/`.bvec` (the native `diffusion` reader) and
    /// for term-map adapter files the ingestion schema classifies as `read`
    /// (`fs_stats`/…) — mirroring [`Self::ingest_projected`]. JSON is decided by the
    /// caller; `.tsv` bodies are DuckDB's and return false here.
    ///
    /// `dataset_type` isn't known until `dataset_description.json` is processed (after
    /// prefetch), so it's passed as `None`; the diffusion (`extension`) and adapter
    /// (`suffix`) `read` rules don't reference it, and a misclassification only
    /// forgoes a prefetch — the pass still does a direct read.
    fn body_read_in_rust(&self, rel_path: &str) -> bool {
        if rel_path.ends_with(".bval") || rel_path.ends_with(".bvec") {
            return true;
        }
        if self.term_maps.is_empty() {
            return false;
        }
        let Some(facts) = self.term_maps.iter().find_map(|tm| tm.classify(rel_path)) else {
            return false;
        };
        let leading = format!("/{rel_path}");
        let null = Value::Null;
        let ctx = FileContext {
            path: &leading,
            datatype: facts.datatype.as_deref(),
            suffix: facts.suffix.as_deref(),
            extension: facts.extension.as_deref(),
            sidecar: &null,
            dataset_type: None,
        };
        matches!(
            self.schema
                .ingestion()
                .classify(&ctx)
                .map(|r| r.disposition),
            Some(Disposition::Read)
        )
    }

    /// The file's content from the concurrent prefetch, or a direct read if it
    /// wasn't prefetched. `rel` is the dataset-relative key the prefetch used.
    ///
    /// Leaves the entry in the cache, so use it only where the body is read more than once
    /// (`dataset_description.json` is, by the id pass and then by the insert pass). Everywhere
    /// else prefer [`Self::take_cached`], which does not copy the body.
    async fn read_cached(&self, path: &Path, rel: &str) -> Result<String> {
        match self.content_cache.get(rel) {
            Some(c) => Ok(c.clone()),
            None => Ok(self.fs.read_to_string(path).await?),
        }
    }

    /// Like [`Self::read_cached`], but *moves* the body out of the cache.
    ///
    /// For the single-consumer cases — a JSON sidecar, a `.bval`/`.bvec`, an adapter file — the
    /// body is parsed once and dropped, so cloning it out of the cache copies a whole file for
    /// nothing. Sidecars are not always small (a converter can attach megabytes of per-slice
    /// DICOM metadata to one `_bold.json`), and the copies are live at the same time as the
    /// originals. Moving also lets the cache drain as the passes advance, rather than every body
    /// staying resident until the passes finish and the caches are released.
    async fn take_cached(&mut self, path: &Path, rel: &str) -> Result<String> {
        match self.content_cache.remove(rel) {
            Some(c) => Ok(c),
            None => Ok(self.fs.read_to_string(path).await?),
        }
    }

    /// Register the subject (and session) a file belongs to, so `participants`/`sessions`
    /// list every entity the dataset actually contains rather than only the ones a
    /// `participants.tsv` happens to name.
    ///
    /// `sub`/`ses` are raw entity values (`01`, not `sub-01`); the `sub-`/`ses-` prefixes are
    /// added here so the one normalization lives in one place. Called with values from a BIDS
    /// filename, and — for a dataset whose filenames carry no entities at all — with the ones a
    /// term map projected: a `recon-all` tree's subject is in its *directory*, so gating this on
    /// filename entities left `participants` empty for every adapter dataset while `scans.sub`
    /// was populated, and any `participants` ⋈ `scans` join silently dropped them.
    ///
    /// Only hits the database the first time each entity is seen; every other file of a subject
    /// would otherwise re-issue an identical (guarded, no-op) insert.
    fn record_implicit_entities(
        &mut self,
        db: &BidsDb,
        dataset_id: &str,
        sub: Option<&str>,
        ses: Option<&str>,
    ) -> Result<()> {
        let Some(sub) = sub else { return Ok(()) };
        let pid = format!("sub-{sub}");

        if self
            .seen_participants
            .insert((dataset_id.to_string(), pid.clone()))
        {
            let mut participant_data = serde_json::Map::new();
            participant_data.insert(
                "dataset_id".to_string(),
                Value::String(dataset_id.to_string()),
            );
            participant_data.insert("participant_id".to_string(), Value::String(pid.clone()));

            // A duplicate (e.g. from participants.tsv) is a no-op: the insert carries a
            // `WHERE NOT EXISTS` primary-key guard (see `schema::dynamic`), so `?` only
            // surfaces real failures.
            db.insert(
                &self.schema,
                "participants",
                &Value::Object(participant_data),
            )
            .with_context(|| format!("inserting implicit participant {pid}"))?;
        }

        if let Some(ses) = ses {
            let sid = format!("ses-{ses}");
            if self
                .seen_sessions
                .insert((dataset_id.to_string(), pid.clone(), sid.clone()))
            {
                let mut session_data = serde_json::Map::new();
                session_data.insert(
                    "dataset_id".to_string(),
                    Value::String(dataset_id.to_string()),
                );
                session_data.insert("session_id".to_string(), Value::String(sid.clone()));
                session_data.insert("participant_id".to_string(), Value::String(pid.clone()));

                // Duplicate is a no-op via the same guard.
                db.insert(&self.schema, "sessions", &Value::Object(session_data))
                    .with_context(|| format!("inserting implicit session {sid} for {pid}"))?;
            }
        }
        Ok(())
    }

    async fn process_file(&mut self, path: &Path, db: &BidsDb, dataset_id: &str) -> Result<()> {
        let file_name = path.file_name().unwrap().to_str().unwrap();

        // path from walk() is already relative to dataset root
        let rel_path = path.to_str().unwrap();

        if file_name.starts_with('.') {
            return Ok(());
        }

        // Standardized non-BIDS files (FreeSurfer, …) are recognized by a term map and
        // handled by the schema-driven ingestion path — they never fall through to BIDS
        // processing (a term map never claims a BIDS-named file). Consulted only when a term
        // map is configured, so an ordinary BIDS ingest pays one `is_empty()` check per file.
        if !self.term_maps.is_empty()
            && let Some(facts) = self.term_maps.iter().find_map(|tm| tm.classify(rel_path))
        {
            self.ingest_projected(db, dataset_id, rel_path, path, facts)
                .await?;
            return Ok(());
        }

        // Parse BIDS filename entities + suffix + extension via the shared bids-core parser.
        let parts = read_entities(file_name);
        let suffix = parts.suffix;
        let extension = parts.extension;
        let entities = parts.entities;

        self.record_implicit_entities(
            db,
            dataset_id,
            entities.get("sub").map(String::as_str),
            entities.get("ses").map(String::as_str),
        )?;

        // What this file *is*, for its registry row. A BIDS-named file's datatype is its
        // immediate parent directory; a term-mapped one never reaches here (it returned
        // above), so `kind_of` sees `None` for anything outside a datatype directory.
        let kind = kind_of(
            rel_path,
            &extension,
            bids_core::datatype::parent_datatype(rel_path, &self.datatypes),
        );

        // JSON (sidecars + `dataset_description.json`) is handled directly: it is neither
        // read into a data table nor cataloged, but drives inheritance and associations.
        // Both still earn a registry row under their own path — which is the whole point of
        // the registry, since `sidecars` is keyed by the *data file* a sidecar describes and
        // so leaves the JSON itself unaddressable (docs/adr/0006).
        if file_name == "dataset_description.json" {
            self.register(rel_path, kind, None);
            self.process_dataset_description(path, db, dataset_id)
                .await?;
            return Ok(());
        }
        if file_name.ends_with(".json") {
            self.register(rel_path, kind, None);
            self.process_json_file(path, db, dataset_id, rel_path, &entities)
                .await?;
            return Ok(());
        }

        // Primary data files — imaging plus non-NIfTI datafiles (EEG/MEG/iEEG/NIRS/
        // microscopy/…, including pseudo-files like `.ds`) — are tracked in `scans` so they
        // are queryable by concept. They are recognized *structurally* (they carry a datatype
        // and are not tabular/diffusion companions) and short-circuit here, before the
        // ingestion dispatch below: imaging files are cataloged by structure, not by ingestion
        // policy. This also spares them `Ingestion::classify`'s selector evaluation — now a
        // minor saving rather than load-bearing (selector ASTs are cached in
        // `bids_schema::expression`), but imaging files are the bulk of a dataset and match no
        // base ingestion rule, so running classify on them would be waste either way.
        //
        // A data file's registry row comes from `imaging_files` at the flush, so it is not
        // registered here.
        if kind == Kind::Data {
            self.imaging_files.push(ImagingFile {
                dataset_id: dataset_id.to_string(),
                file_path: rel_path.to_string(), // Use rel_path not file_name
                projected: None,                 // BIDS-named: its concepts are in the filename
            });
            return Ok(());
        }

        // Tabular + diffusion companions: the ingestion schema selects on the projected
        // concepts (extension/suffix) and returns the disposition + reader, replacing the
        // former hardcoded `.tsv`/`.bval`/`.bvec` gates. `read` runs the named reader
        // (`csv` = the batched tabular ingest, `diffusion` = the bval/bvec accumulator);
        // `catalog` records the file in `file_registry` with its contents
        // left on disk (chiefly compressed continuous recordings, read later with tools
        // like polars); `ignore` skips it. `datatype` is intentionally not bound here so a
        // configured adapter's datatype-keyed rules can't claim ordinary BIDS files.
        let path_with_slash = format!("/{rel_path}");
        let null = Value::Null;
        let disposition = {
            let ctx = FileContext {
                path: &path_with_slash,
                datatype: None,
                suffix: Some(&suffix),
                extension: Some(&extension),
                sidecar: &null,
                dataset_type: self.dataset_type.as_deref(),
            };
            self.schema
                .ingestion()
                .classify(&ctx)
                .map(|r| (r.disposition, r.reader.clone()))
        };
        // `ignore` is the one disposition that yields no registry row: its metaschema text is
        // "neither read nor register it", and a producer uses it to keep files out of the
        // catalog deliberately (FEAT hides `report.html`/`pyfix.log`). Everything else the
        // walk saw is in the dataset and so is in the manifest.
        if !matches!(disposition, Some((Disposition::Ignore, _))) {
            self.register(rel_path, kind, None);
        }

        match disposition {
            Some((Disposition::Read, reader)) => match reader.as_deref() {
                Some("diffusion") => {
                    self.process_diffusion_file(path, db, rel_path, file_name, dataset_id)
                        .await?;
                }
                Some("csv") => {
                    self.process_tabular_file(rel_path, file_name, dataset_id, &entities)
                        .await?;
                }
                other => {
                    eprintln!(
                        "Warning: ingestion `read` rule for {rel_path} names unknown reader {other:?}; skipping"
                    );
                }
            },
            Some((Disposition::Catalog, _)) => {
                // Left on disk, its registry row marked `on_disk` so queries surface it
                // (chiefly compressed continuous recordings `*_physio.tsv.gz`).
                self.walk_status
                    .insert(rel_path.to_string(), TabularStatus::OnDisk);
            }
            Some((Disposition::Ignore, _)) => {}
            // A non-datafile, non-JSON file with no ingestion rule (READMEs, CHANGES, …):
            // nothing to *ingest*, but it is still a file the dataset contains, so it has a
            // registry row from the call above.
            None => {}
        }

        Ok(())
    }

    /// Record a walked file in the registry accumulator (docs/adr/0006).
    ///
    /// Data files are not registered here — they go through `imaging_files`, which
    /// `promote_orphan_sidecars` and the inheritance pass also read, and are folded into the
    /// registry at the flush by [`Self::registry_rows`].
    fn register(&mut self, rel_path: &str, kind: Kind, projected: Option<Value>) {
        self.registry_extra.push(RegistryEntry {
            file_path: rel_path.to_string(),
            kind,
            projected,
        });
    }

    /// Ingest a file recognized by a term map: project → build the routing context → let the
    /// ingestion schema decide read / catalog / ignore. `read` parses the body with the named
    /// content reader and bulk-inserts the rows (and registers the file); `catalog` registers
    /// the file in `scans` (contents unread, left on disk); `ignore` skips it. Every failure
    /// is non-fatal (logged, then skipped) so one bad file can't poison the ingest txn.
    async fn ingest_projected(
        &mut self,
        db: &BidsDb,
        dataset_id: &str,
        rel_path: &str,
        path: &Path,
        facts: FileFacts,
    ) -> Result<()> {
        // Register the subject/session the projection found, before the disposition is decided:
        // if a term map recognized a path and read a subject out of it, that subject is in the
        // tree whether or not this particular file earns a `scans` row.
        self.record_implicit_entities(db, dataset_id, facts.get("sub"), facts.get("ses"))?;

        // The ingestion selectors run over the projected concepts. `path` is dataset-relative
        // with a leading slash, matching the tabular selector convention.
        let leading = format!("/{rel_path}");
        let dataset_type = self.dataset_type.clone();
        let (disposition, reader) = {
            let ctx = FileContext {
                path: &leading,
                datatype: facts.datatype.as_deref(),
                suffix: facts.suffix.as_deref(),
                extension: facts.extension.as_deref(),
                sidecar: &Value::Null,
                dataset_type: dataset_type.as_deref(),
            };
            match self.schema.ingestion().classify(&ctx) {
                Some(rule) => (Some(rule.disposition), rule.reader.clone()),
                // Recognized by a term map but claimed by no ingestion rule — a `recon-all`
                // bookkeeping file, say. Nothing to read or catalog, but it is still a file
                // the dataset contains, so it earns a registry row like any other.
                None => (None, None),
            }
        };

        // What the projection says this file is. A term-mapped path is a data file when the
        // mapping gave it a datatype (`mri/wmparc.mgz` → `anat`) and something else when it
        // did not (`scripts/recon-all.log` → `other`), which is the same rule the BIDS path
        // uses — `kind_of` takes the datatype rather than deriving it precisely so both
        // paths can share it.
        let kind = kind_of(
            rel_path,
            facts.extension.as_deref().unwrap_or(""),
            facts.datatype.as_deref(),
        );

        // `read` and `catalog` both register the file in the standard `scans` registry,
        // carrying the projection so the registry row answers "what is this file?" the
        // way the term map said it would. Without it a cataloged projected file reaches
        // `scans` as a bare path and reads back with every concept NULL — including the
        // `datatype` its term map states outright.
        match disposition {
            Some(Disposition::Read | Disposition::Catalog) => {
                self.imaging_files.push(ImagingFile {
                    dataset_id: dataset_id.to_string(),
                    file_path: rel_path.to_string(),
                    projected: projected_json(&facts),
                });
            }
            // `ignore` is the deliberate opt-out and gets no row at all; a file no rule
            // claimed still belongs in the manifest.
            Some(Disposition::Ignore) => {}
            None => self.register(rel_path, kind, projected_json(&facts)),
        }

        let Some(disposition) = disposition else {
            return Ok(());
        };

        if disposition != Disposition::Read {
            return Ok(());
        }

        let Some(reader_name) = reader else {
            eprintln!("Warning: `read` rule for {rel_path} has no reader; skipping");
            return Ok(());
        };
        let content = match self.take_cached(path, rel_path).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Warning: cannot read {rel_path}: {e}");
                return Ok(());
            }
        };
        let Some(rdr) = self.readers.get(&reader_name) else {
            eprintln!("Warning: reader `{reader_name}` is not registered; skipping {rel_path}");
            return Ok(());
        };
        match rdr.read(self.file_key(dataset_id, rel_path), &content, &facts) {
            Ok(batches) => {
                for batch in &batches {
                    // Clear this file's prior rows before re-inserting them. These tables are
                    // per-row and so carry no primary key — nothing for an upsert to conflict
                    // on — which means a re-index does not *fail*: it silently doubles the
                    // table. Scoped per file, exactly as the batched tabular path scopes its own
                    // pre-`DELETE` for the same class of table.
                    if let Err(e) =
                        db.clear_file_rows(&batch.table, self.file_key(dataset_id, rel_path))
                    {
                        eprintln!(
                            "Warning: clearing previous {} rows for {rel_path}: {e}",
                            batch.table
                        );
                    }
                    match db.append_rows(&self.schema, &batch.table, &batch.rows) {
                        Ok(()) => {}
                        Err(e) => {
                            eprintln!(
                                "Warning: insert into {} failed for {rel_path}: {e}",
                                batch.table
                            )
                        }
                    }
                }
                self.walk_status
                    .insert(rel_path.to_string(), TabularStatus::Ingested);
            }
            Err(e) => eprintln!("Warning: reader `{reader_name}` failed on {rel_path}: {e}"),
        }
        Ok(())
    }

    async fn process_json_file(
        &mut self,
        path: &Path,
        _db: &BidsDb, // db not used here anymore
        dataset_id: &str,
        rel_path: &str,
        entities: &HashMap<String, String>,
    ) -> Result<()> {
        let content = self.take_cached(path, rel_path).await?;
        let mut json_value: Value = serde_json::from_str(&content).unwrap_or(Value::Null);

        // Drop the keys the ingestion policy says `sidecars` never stores, here at the
        // parse rather than at the insert. A key dropped now is never merged for the
        // files that inherit it, never copied into a row, and never held in memory for
        // the rest of the run — which is the whole point, since what motivates the
        // policy is single keys of a few megabytes.
        let ignore = self.schema.ingestion().ignore_keys("sidecars");
        if !ignore.is_empty()
            && let Some(obj) = json_value.as_object_mut()
        {
            obj.retain(|k, _| !ignore.iter().any(|i| i == k));
        }
        let json_value = json_value;

        // Extract the BIDS suffix from the filename via the shared bids-core parser.
        let file_name = path.file_name().unwrap().to_str().unwrap();
        let suffix = read_entities(file_name).suffix;

        // Check for IntendedFor field to create associations (borrows the parsed
        // value before it is moved into the sidecar store below).
        self.process_intended_for(rel_path, &json_value)?;

        // Store sidecar info for later inheritance processing.
        self.sidecars.push(SidecarInfo {
            dataset_id: dataset_id.to_string(),
            file_path: rel_path.to_string(),
            entities: entities.clone(),
            suffix,
            content: json_value,
        });

        Ok(())
    }

    async fn process_dataset_description(
        &mut self,
        path: &Path,
        db: &BidsDb,
        dataset_id: &str,
    ) -> Result<()> {
        let content = self.read_cached(path, &path.to_string_lossy()).await?;
        let mut json_value: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "Warning: skipping unparseable dataset_description.json at {}: {}",
                    path.display(),
                    e
                );
                return Ok(());
            }
        };

        // Remember DatasetType (raw/derivative) — the `derivatives.*` tabular
        // selectors (dseg lookups) gate on it. Only the root description sets it;
        // nested ones are processed later and must not clobber it.
        if self.dataset_type.is_none()
            && let Some(dt) = json_value.get("DatasetType").and_then(|v| v.as_str())
        {
            self.dataset_type = Some(dt.to_string());
        }

        // Only the dataset's OWN description is stored — the shallowest, which this pass
        // reaches first. A nested one under `derivatives/` describes a *different* dataset,
        // and the write below replaces rather than defers, so letting one through here
        // would overwrite this dataset's row with a derivative's.
        //
        // (`root_description_json`, which `record_links` reads at finalize, is captured in
        // `parse`'s pass 0 instead: `resolve_root` needs it before the walk gets here.)
        if self.has_dataset_description {
            return Ok(());
        }

        if let Value::Object(ref mut map) = json_value {
            map.insert(
                "dataset_id".to_string(),
                Value::String(dataset_id.to_string()),
            );
        }

        // Replace rather than defer to whatever is stored (the `eh-04` follow-up). A
        // dataset's description is re-read on every index run, so first-writer-wins meant a
        // re-index kept a stale row: a `dataset_description.json` added or corrected since
        // the first run never reached the catalog. It matters more now that a dataset can
        // have several roots, since every one of them re-states this row.
        db.insert_or_replace(&self.schema, "dataset_description", &json_value)
            .with_context(|| format!("inserting dataset_description for {dataset_id}"))?;
        self.has_dataset_description = true;
        Ok(())
    }

    /// Record this dataset's cross-dataset link declarations for the query-time
    /// `dataset_relations` view (docs/adr/0003). Refreshes the ingest-derived rows
    /// (`SourceDatasets`, `DatasetLinks`) and all identities so a re-index reflects the
    /// current `dataset_description.json`; user-provided `declared` links (`--source-dataset`,
    /// `bidslake link add`) are merged idempotently and never cleared here. Runs for every
    /// dataset, including adapter-ingested ones with no description (they still get their
    /// `self`/`root_uri` identity and any `--source-dataset`).
    fn record_links(&self, db: &BidsDb, dataset_id: &str) -> Result<()> {
        db.clear_derived_links(dataset_id)?;

        // What this dataset IS: its own id, and each of its roots.
        db.record_dataset_identity(
            dataset_id,
            &links::canonicalize(&format!("dataset:{dataset_id}")),
            "self",
        )?;
        // Every root, read back from `dataset_roots` rather than just this run's — because
        // `clear_derived_links` above dropped *all* of this dataset's identities, so
        // re-recording only `self.fs.root()` would leave a multi-root dataset having
        // silently forgotten the roots this run did not walk.
        for root in db.dataset_roots(dataset_id)? {
            db.record_dataset_identity(dataset_id, &links::canonicalize(&root), "root_uri")?;
        }

        if let Some(desc) = &self.root_description_json {
            if let Some(doi) = desc.get("DatasetDOI").and_then(Value::as_str) {
                db.record_dataset_identity(dataset_id, &links::canonicalize(doi), "DatasetDOI")?;
            }
            // SourceDatasets: prefer each entry's DOI, else its URL.
            if let Some(sources) = desc.get("SourceDatasets").and_then(Value::as_array) {
                for src in sources {
                    if let Some(reference) = src
                        .get("DOI")
                        .and_then(Value::as_str)
                        .or_else(|| src.get("URL").and_then(Value::as_str))
                    {
                        db.record_dataset_link(
                            dataset_id,
                            "source",
                            "",
                            reference,
                            &links::canonicalize(reference),
                        )?;
                    }
                }
            }
            // DatasetLinks: a name → location map. This is the *naming* half of
            // `dataset_links` — "here, `fs` refers to that dataset" — and is resolved by the
            // `dataset_link_targets` view, never by `dataset_relations`: a reference is not a
            // derivation.
            //
            // Canonicalized against this run's root, because BIDS writes these values
            // relative to the dataset root far more often than as absolute URIs, and a
            // relative one has no meaning without it.
            if let Some(named) = desc.get("DatasetLinks").and_then(Value::as_object) {
                let root_uri = self.fs.root();
                for (name, uri) in named {
                    if let Some(uri) = uri.as_str() {
                        db.record_dataset_link(
                            dataset_id,
                            "named",
                            name,
                            uri,
                            &links::canonicalize_relative_to(uri, &root_uri),
                        )?;
                    }
                }
            }
        }

        for reference in &self.declared_sources {
            db.record_dataset_link(
                dataset_id,
                "declared",
                "",
                reference,
                &links::canonicalize(reference),
            )?;
        }
        Ok(())
    }

    async fn process_diffusion_file(
        &mut self,
        path: &Path,
        _db: &BidsDb,
        rel_path: &str,
        file_name: &str,
        dataset_id: &str,
    ) -> Result<()> {
        // Read the bval or bvec file (from the concurrent prefetch when available).
        let content = self.take_cached(path, rel_path).await?;

        // The row is keyed by *this* file, so there is nothing to derive from its name — no
        // stem surgery, no synthesized `.nii.gz`, and therefore no dependence on the image
        // being a sibling, being compressed, or existing at all. Which images these values
        // describe is resolved from the schema's `meta.associations`, which already handles
        // the inherited case, `.nii` as well as `.nii.gz`, and `epi` as well as `dwi`.
        let gradient = if file_name.ends_with(".bval") {
            PendingGradient::Bvals(self.parse_bval(&content)?)
        } else {
            let (x, y, z) = self.parse_bvec(&content)?;
            PendingGradient::Bvecs(x, y, z)
        };
        self.pending_gradients
            .push((dataset_id.to_string(), rel_path.to_string(), gradient));

        Ok(())
    }

    fn parse_bval(&self, content: &str) -> Result<Vec<f64>> {
        content
            .split_whitespace()
            .map(|s| {
                s.parse::<f64>()
                    .map_err(|e| anyhow::anyhow!("Failed to parse bval: {}", e))
            })
            .collect()
    }

    fn parse_bvec(&self, content: &str) -> Result<(Vec<f64>, Vec<f64>, Vec<f64>)> {
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

        if lines.len() != 3 {
            return Err(anyhow::anyhow!(
                "bvec file must have exactly 3 rows, found {}",
                lines.len()
            ));
        }

        let parse_row = |line: &str| -> Result<Vec<f64>> {
            line.split_whitespace()
                .map(|s| {
                    s.parse::<f64>()
                        .map_err(|e| anyhow::anyhow!("Failed to parse bvec: {}", e))
                })
                .collect()
        };

        let x = parse_row(lines[0])?;
        let y = parse_row(lines[1])?;
        let z = parse_row(lines[2])?;

        // Verify all rows have the same length
        if x.len() != y.len() || y.len() != z.len() {
            return Err(anyhow::anyhow!("bvec rows must have equal length"));
        }

        Ok((x, y, z))
    }

    /// Route one header-bearing tabular file to its table and ingest it with DuckDB
    /// `read_csv`. This is the ingestion schema's `csv` reader — reached only for `.tsv`
    /// files the dispatch classified as `read` (compressed `.tsv.gz` recordings are
    /// `catalog`ed upstream, before this point).
    ///
    /// The file's `(path, suffix, extension, datatype, dataset_type)` are matched against
    /// `rules.tabular_data`. Uncompressed headerless recordings (`*_motion`, …) are
    /// deferred to the recordings flush; every other tabular file — ingested, deferred,
    /// or unmatched — records a `status` on its registry row so nothing is silently dropped.
    async fn process_tabular_file(
        &mut self,
        rel_path: &str,
        file_name: &str,
        dataset_id: &str,
        entities: &HashMap<String, String>,
    ) -> Result<()> {
        let (suffix, extension) = split_suffix_ext(file_name);

        // Uncompressed headerless recordings — chiefly the motion time-series —
        // are still ingested. They have no header row; their column names come from
        // the merged sidecar `Columns` or the associated channels file, so they are
        // deferred to the flush once every sidecar has been collected.
        if self.schema.recordings().contains(&suffix) {
            self.pending_recordings.push(PendingRecording {
                dataset_id: dataset_id.to_string(),
                rel_path: rel_path.to_string(),
                suffix,
                entities: entities.clone(),
            });
            return Ok(());
        }

        // Datatype from the path, or — for a file above a datatype directory, like
        // a session-level channels.tsv — inferred from the datatype dirs beneath it.
        let datatype = self
            .datatype_dir_in_path(rel_path)
            .or_else(|| self.infer_datatype(rel_path, &suffix, &extension));
        // BIDS selector paths are dataset-relative with a leading slash.
        let path_with_slash = format!("/{rel_path}");
        let sidecar = Value::Null;
        let ctx = FileContext {
            path: &path_with_slash,
            datatype: datatype.as_deref(),
            suffix: Some(&suffix),
            extension: Some(&extension),
            sidecar: &sidecar,
            dataset_type: self.dataset_type.as_deref(),
        };

        let table = self.schema.tabular().route(&ctx).cloned();
        match table {
            Some(spec) => {
                // Defer every tabular file, whatever its identity, so siblings sharing a
                // header are ingested in one batched `read_csv`. The header is read here in
                // Rust purely to group by signature; the batch declares it to `read_csv`
                // outright, so no file pays for a dialect sniff.
                //
                // Statement-per-file is what this replaces, and the cost it avoided grows
                // with the width of the target table: a keyed table like `scans` carries a
                // generated concept column per BIDS entity, and a row-at-a-time INSERT
                // re-binds every one of them.
                //
                // Scoped so the timer covers only the reads, not the routing and bookkeeping
                // in the match below.
                let (source, header) = {
                    let _t = timing::scope(Phase::TabularReadCsv);
                    // A ready-to-use `read_csv` source (absolute local path or `s3://`
                    // URL); the backend has already resolved the scheme.
                    let source = self.fs.read_csv_source(Path::new(rel_path)).await?;
                    // Header from the concurrent prefetch; fall back to a direct read
                    // if it wasn't prefetched (shouldn't happen for a per-row `.tsv`).
                    let header = match self.tabular_header.get(rel_path) {
                        Some(h) => h.clone(),
                        None => self
                            .fs
                            .read_head(Path::new(rel_path), 64 * 1024)
                            .await
                            .ok()
                            .and_then(|c| tsv_header_from_line(c.split('\n').next().unwrap_or(""))),
                    };
                    (source, header)
                };

                match header {
                    None => {
                        // Unreadable or column-less: contributes no rows, but is
                        // still recorded so the tabular-coverage invariant holds.
                        self.walk_status
                            .insert(rel_path.to_string(), TabularStatus::Ingested);
                    }
                    Some((group_key, columns)) => {
                        // `scans.tsv` builds its `file_path` from its own `filename`
                        // column relative to the directory it sits in; a session table
                        // stamps the subject it belongs to. Both vary per file, so they
                        // travel with it into the batch's path map.
                        let aux = match spec.identity {
                            RowIdentity::PerFile => rel_path
                                .rsplit_once('/')
                                .map(|(dir, _)| dir)
                                .unwrap_or("")
                                .to_string(),
                            RowIdentity::PerEntity if spec.table != "participants" => entities
                                .get("sub")
                                .map(|s| format!("sub-{s}"))
                                .unwrap_or_default(),
                            _ => String::new(),
                        };
                        self.pending_tabular.push(PendingTabular {
                            aux,
                            spec,
                            rel_path: rel_path.to_string(),
                            source,
                            group_key,
                            columns,
                        });
                    }
                }
            }
            None => {
                // A validated dataset should not reach here (all its tabular files
                // are schema-described). Warn rather than fail so a newer BIDS
                // extension than the vendored schema doesn't abort ingest.
                eprintln!("Warning: no tabular_data rule for {rel_path}; skipping");
                self.walk_status
                    .insert(rel_path.to_string(), TabularStatus::Skipped);
            }
        }
        Ok(())
    }

    /// Ingest every deferred tabular file (Lever 1b), grouped by
    /// `(table, header signature)` so each group is one `read_csv([f1,…,fN])` INSERT
    /// instead of N. Grouping by exact header keeps `other_data` precise (no
    /// `union_by_name` NULL fillers); `row_idx` reproduces TSV line order for positional
    /// tables and is an arbitrary unique key for order-insensitive ones (see
    /// [`build_tabular_batch_select`]).
    ///
    /// Malformed rows can't poison the ingest transaction: `read_csv` uses the
    /// non-erroring relaxations in [`HEADER_READ_OPTS`] (`ignore_errors` /
    /// `null_padding` / `strict_mode=false`), so a bad row is padded or dropped
    /// rather than aborting the statement — `bids-validator-rs`, not bidslake, is
    /// the authority on tabular malformation. Because a read cannot error, there is no
    /// dry-run and no per-file fallback.
    ///
    /// The trade-off that buys: each group is one pre-`DELETE` plus one batch `INSERT`, so if
    /// the `INSERT` itself fails (an IO or read error, say) the whole group's rows are dropped
    /// for this run with no per-file isolation. The affected files are recorded with
    /// `status = "failed"` rather than `"ingested"` on their registry rows, so the loss is
    /// visible instead of looking like an empty-but-successful ingest.
    async fn flush_tabular(&mut self, db: &BidsDb) -> Result<()> {
        if self.pending_tabular.is_empty() {
            return Ok(());
        }
        let dataset_id = self.dataset_id.as_ref().unwrap().clone();
        // Move the pending list out so the group loop can borrow `&self`.
        let pending = std::mem::take(&mut self.pending_tabular);

        // Group by (table, raw header). Files in a group have byte-identical header
        // bytes, so `read_csv` reads them under one dialect and every column
        // resolves identically.
        let mut groups: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for (i, p) in pending.iter().enumerate() {
            groups
                .entry((p.spec.table.clone(), p.group_key.clone()))
                .or_default()
                .push(i);
        }

        // Undeclared column names already recorded this run. An undeclared name is a
        // property of the *table*, not of the file it was seen in, and a wide derivative
        // table repeats the same names in every one of its files — so recording each name
        // once per run yields the same catalog as recording it once per file.
        let mut recorded_undeclared: HashSet<(String, String)> = HashSet::new();

        timing::count(Counter::PendingTabular, pending.len() as u64);
        timing::count(Counter::TabularGroups, groups.len() as u64);
        timing::count_max(
            Counter::TabularGroupMax,
            groups.values().map(Vec::len).max().unwrap_or(0) as u64,
        );

        // Each group is ingested in windows rather than one statement, to bound the size of
        // the SQL text: every member's path is embedded more than once — in the DELETE, in
        // the `read_csv` list, and in the `__src` join map — so a group of tens of thousands
        // of files would otherwise be a statement tens of megabytes long.
        //
        // Safe to split because nothing in the body is group-wide: `row_idx` is partitioned
        // by `__src`, so it is already per file, and the DELETE and the count-back are both
        // scoped to the window's own paths.
        for idxs in groups.values() {
            for window in idxs.chunks(BATCH_WINDOW_FILES) {
                let members: Vec<&PendingTabular> = window.iter().map(|&i| &pending[i]).collect();
                let spec = &members[0].spec;
                let columns = &members[0].columns;
                let files: Vec<(&str, &str, &str)> = members
                    .iter()
                    .map(|m| (m.source.as_str(), m.rel_path.as_str(), m.aux.as_str()))
                    .collect();

                // Re-index idempotency for keyless row tables: clear these files' prior rows
                // in one DELETE before re-inserting. Keyed tables (participants, sessions,
                // scans) have a primary key to collide on instead, and resolve it on insert
                // with `INSERT OR REPLACE` — see the verb below.
                if matches!(spec.identity, RowIdentity::PerRow) {
                    // Scoped by `file_id`, which already carries the dataset and the root —
                    // so a re-index of one root of a multi-root dataset cannot reach another
                    // root's rows, even where the two hold the same relative path.
                    // Unsigned integer literals: no quoting, and nothing for
                    // `sql_in_list`'s escaping to do.
                    let id_list = members
                        .iter()
                        .map(|m| self.file_key(&dataset_id, &m.rel_path).to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let del = format!("DELETE FROM {} WHERE file_id IN ({id_list})", spec.table);
                    db.conn.execute(&del, [])?;
                }

                // Write the batch directly — no dry-run. `read_csv`'s non-erroring relaxations
                // (see `HEADER_READ_OPTS`) mean a malformed row is padded or dropped rather
                // than aborting, so it cannot poison the ingest transaction, which is what
                // made the dry-run's second read of every file unnecessary.
                //
                // Row order matters for positional tabular files — notably derivative
                // `*timeseries.tsv` (e.g. fMRIPrep confounds), where row N aligns with
                // volume N of the associated 4D image — so their line order is preserved
                // and `row_idx` records the row number. The ordering policy lives in the
                // ingestion schema (`Ingestion::ordered`);
                // see https://github.com/bids-standard/bids-2-devel/issues/98.
                let preserve_order = self.schema.ingestion().ordered(&spec.table);
                let store_undeclared =
                    self.schema.ingestion().undeclared(&spec.table) == Undeclared::Store;
                let (select, undeclared) = build_tabular_batch_select(
                    spec,
                    &dataset_id,
                    &self.root_uri,
                    &files,
                    columns,
                    preserve_order,
                    store_undeclared,
                );
                let fresh: Vec<String> = if store_undeclared {
                    Vec::new()
                } else {
                    undeclared
                        .into_iter()
                        .filter(|n| recorded_undeclared.insert((spec.table.clone(), n.clone())))
                        .collect()
                };
                if !fresh.is_empty()
                    && let Err(e) = db.record_undeclared_columns(&spec.table, &fresh)
                {
                    eprintln!(
                        "Warning: recording undeclared columns for {}: {e}",
                        spec.table
                    );
                }
                // Keyed tables take the TSV's row over the stub the walk synthesized. Both
                // always exist by now — the walk creates a stub for every subject and session
                // it sees, and this flush runs after it — so the two do collide on the primary
                // key, and the verb is what decides the winner. `OR REPLACE` states that
                // precedence outright rather than leaving it to whichever row is written first.
                let verb = match spec.identity {
                    RowIdentity::PerRow => "INSERT",
                    _ => "INSERT OR REPLACE",
                };
                let sql = format!("{verb} INTO {} BY NAME {select}", spec.table);
                // A batch-INSERT execution failure (e.g. an IO/read error) drops this
                // group's rows for the run — record its members as `failed` so the
                // registry can distinguish that from an empty-but-successful ingest,
                // rather than claiming `ingested` with 0 rows.
                let status = if let Err(e) = db.conn.execute(&sql, []) {
                    eprintln!(
                        "Warning: batched tabular insert into {} failed: {e}",
                        spec.table
                    );
                    TabularStatus::Failed
                } else {
                    TabularStatus::Ingested
                };
                for m in &members {
                    db.record_file_status(self.file_key(&dataset_id, &m.rel_path), status)?;
                }
            }
        }
        Ok(())
    }

    /// The BIDS datatype for a file, taken from *any* datatype directory in its
    /// path. This deliberately differs from [`bids_core::datatype::parent_datatype`]
    /// (which matches only the immediate parent dir): the any-component match here
    /// mirrors the `datatype` DuckDB virtual column's `/({alt})/` regex (see
    /// `schema::dynamic`), so both classify nested/derivative layouts the same way.
    fn datatype_dir_in_path(&self, rel_path: &str) -> Option<String> {
        rel_path
            .split('/')
            .find(|c| self.datatypes.contains(*c))
            .map(|s| s.to_string())
    }

    /// Infer the datatype of a tabular file that has no datatype directory of its
    /// own (a session-/subject-level `channels.tsv`/`electrodes.tsv` that applies
    /// to the runs below it). Among the datatypes appearing in directories *below*
    /// the file, pick the one under which the file actually routes — unique or
    /// nothing, so an ambiguous layout is left unrouted rather than guessed.
    fn infer_datatype(&self, rel_path: &str, suffix: &str, extension: &str) -> Option<String> {
        let dir = Path::new(rel_path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("");
        // Datatype directories beneath the file's directory. A root-level file
        // (dir empty) is above every datatype directory in the dataset.
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let mut candidates: Vec<&str> = self
            .datatype_dirs
            .iter()
            .filter(|(p, _)| p.starts_with(&prefix))
            .map(|(_, dt)| dt.as_str())
            .collect();
        candidates.sort_unstable();
        candidates.dedup();

        let path_with_slash = format!("/{rel_path}");
        let sidecar = Value::Null;
        let mut routable = candidates.into_iter().filter(|dt| {
            let ctx = FileContext {
                path: &path_with_slash,
                datatype: Some(dt),
                suffix: Some(suffix),
                extension: Some(extension),
                sidecar: &sidecar,
                dataset_type: self.dataset_type.as_deref(),
            };
            self.schema.tabular().route(&ctx).is_some()
        });
        let first = routable.next()?;
        match routable.next() {
            None => Some(first.to_string()), // exactly one datatype routes
            Some(_) => None,                 // ambiguous — leave unrouted
        }
    }

    /// Ingest the deferred headerless recordings. Run in the flush phase, after all
    /// sidecars are collected (physio/stim column names come from the merged
    /// sidecar `Columns`) and all channels are ingested (motion column names come
    /// from the associated `_channels.tsv`).
    async fn flush_recordings(&self, db: &BidsDb) -> Result<()> {
        if self.pending_recordings.is_empty() {
            return Ok(());
        }
        println!(
            "Ingesting {} continuous recordings (physio/stim/motion)...",
            self.pending_recordings.len()
        );
        // Built once for the whole flush: every recording asks the same index which
        // sidecars apply to it.
        let index = SidecarIndex::new(&self.sidecars);
        for rec in &self.pending_recordings {
            let table = self
                .ingest_recording(db, rec, &index)
                .await
                .unwrap_or_else(|e| {
                    eprintln!(
                        "Warning: failed to ingest recording {}: {}",
                        rec.rel_path, e
                    );
                    None
                });
            let status = if table.is_some() {
                TabularStatus::Ingested
            } else {
                TabularStatus::Skipped
            };
            db.record_file_status(self.file_key(&rec.dataset_id, &rec.rel_path), status)?;
        }
        Ok(())
    }

    /// Ingest one headerless recording. Returns `(table, rows)`, or `(None, 0)` if
    /// its column names could not be resolved (so it is recorded as skipped).
    async fn ingest_recording(
        &self,
        db: &BidsDb,
        rec: &PendingRecording,
        index: &SidecarIndex<'_>,
    ) -> Result<Option<String>> {
        // Suffix → target table, from the schema-derived recording set.
        let Some(kind) = self.schema.recordings().get(rec.suffix.as_str()) else {
            return Ok(None);
        };
        let table = kind.table.as_str();
        // Typed columns if the schema declares any for this table (`physio`,
        // `physio_events`), empty otherwise — which is what makes `stim`/`motion` bare.
        let columns: Vec<ColumnSpec> = self.recording_columns(table);

        // Column names, in file order: from the associated channels file (motion) or
        // the merged sidecar `Columns` (physio/stim/physioevents).
        let colnames = match kind.names {
            ColumnNames::Channels => self.channel_columns(db, rec)?,
            ColumnNames::Sidecar => self.sidecar_columns(rec, index),
        };
        if colnames.is_empty() {
            return Ok(None); // headerless file with no column names → skip
        }

        let source = self.fs.read_csv_source(Path::new(&rec.rel_path)).await?;

        let spec = TableSpec {
            table: table.to_string(),
            columns,
            identity: RowIdentity::PerRow,
            file_based: true,
            rule_ids: Vec::new(),
        };

        // Headerless read: supply the column names explicitly, all as VARCHAR (the
        // SELECT TRY_CASTs the schema-typed ones). `auto_detect=false` skips the
        // dialect sniffer — it trusts our explicit spec, and (crucially) an empty
        // or truncated file then yields zero rows instead of a sniff error that
        // would poison the whole ingest transaction.
        let cols_spec: Vec<String> = colnames
            .iter()
            .map(|c| format!("{}: 'VARCHAR'", sql_lit(c)))
            .collect();
        // Same non-poisoning relaxations as `HEADER_READ_OPTS`, from the shared
        // `non_poisoning_read_flags!` fragment, plus the headerless-recording
        // specifics (`header=false`, `auto_detect=false`, explicit `columns`).
        let read_opts = format!(
            "header=false, auto_detect=false, {}, columns={{{}}}",
            non_poisoning_read_flags!(),
            cols_spec.join(", ")
        );

        // Pre-flight on the validator connection: many recordings are non-gzip
        // git-annex placeholders whose read errors would otherwise poison the
        // transaction. A readable-but-empty file passes and simply yields 0 rows.
        let read_from = format!("read_csv({}, {read_opts})", sql_lit(&source));
        if !self.read_csv_ok(&read_from) {
            return Ok(Some(table.to_string()));
        }

        // Re-index idempotency.
        let del = format!(
            "DELETE FROM {} WHERE file_id = {}",
            table,
            self.file_key(&rec.dataset_id, &rec.rel_path)
        );
        db.conn.execute(&del, [])?;

        // Recordings are positional (row N is sample N), so preserve line order.
        let preserve_order = self.schema.ingestion().ordered(&spec.table);
        let store_undeclared = self.schema.ingestion().undeclared(&spec.table) == Undeclared::Store;
        let (sql, undeclared) = build_tabular_insert_sql(
            &spec,
            &source,
            self.file_key(&rec.dataset_id, &rec.rel_path),
            &colnames,
            &read_opts,
            preserve_order,
            store_undeclared,
        );
        if !store_undeclared {
            db.record_undeclared_columns(&spec.table, &undeclared)?;
        }
        match db.conn.execute(&sql, []) {
            Ok(_) => Ok(Some(table.to_string())),
            Err(e) => {
                eprintln!(
                    "Warning: failed to ingest recording {}: {}",
                    rec.rel_path, e
                );
                Ok(Some(table.to_string()))
            }
        }
    }

    /// The schema-declared columns of a recording table (`physio`, `physio_events`),
    /// or empty when the schema declares none — which is what makes `stim` and
    /// `motion` bare, every value landing untyped in `other_data`.
    fn recording_columns(&self, table: &str) -> Vec<ColumnSpec> {
        self.schema
            .tabular()
            .tables()
            .iter()
            .find(|t| t.table == table)
            .map(|t| t.columns.clone())
            .unwrap_or_default()
    }

    /// Column names for a physio/stim/physioevents recording, from the merged sidecar's
    /// `Columns` array (BIDS requires it for these files). Merged from the sidecars
    /// collected in memory during the walk — no disk re-read.
    fn sidecar_columns(&self, rec: &PendingRecording, index: &SidecarIndex) -> Vec<String> {
        let merged = index.merged(
            &rec.dataset_id,
            &rec.rel_path,
            &rec.suffix,
            &rec.entities,
            &mut Vec::new(),
        );
        merged
            .get("Columns")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Column names for a recording whose associated `_channels.tsv` names them
    /// (per `meta.associations.channels`) — `motion` in base BIDS.
    ///
    /// Which table holds those rows is routed through the schema rather than named:
    /// a `_channels.tsv` in this file's datatype is the same file that rule already
    /// ingested, so `motion` reads `motion_channels` without either name appearing
    /// here.
    fn channel_columns(&self, db: &BidsDb, rec: &PendingRecording) -> Result<Vec<String>> {
        let Some(base) = rec.rel_path.strip_suffix(&format!("_{}.tsv", rec.suffix)) else {
            return Ok(Vec::new());
        };
        let channels_path = format!("{base}_channels.tsv");
        let path_with_slash = format!("/{channels_path}");
        let datatype = self.datatype_dir_in_path(&rec.rel_path);
        let sidecar = Value::Null;
        let Some(spec) = self.schema.tabular().route(&FileContext {
            path: &path_with_slash,
            datatype: datatype.as_deref(),
            suffix: Some("channels"),
            extension: Some(".tsv"),
            sidecar: &sidecar,
            dataset_type: self.dataset_type.as_deref(),
        }) else {
            return Ok(Vec::new());
        };
        let channels_table = spec.table.clone();
        // Keyed by the channels file's own `file_id`, not its path: the per-row tables carry
        // only `file_id` now (docs/adr/0006). `row_idx` is what makes this correct — a
        // `_channels.tsv`'s line order maps onto the columns of the recording beside it.
        let sql = format!(
            "SELECT name FROM {channels_table} WHERE file_id = {} ORDER BY row_idx",
            self.file_key(&rec.dataset_id, &channels_path)
        );
        let mut stmt = db.conn.prepare(&sql)?;
        let names = stmt
            .query_map([], |r| r.get::<_, Option<String>>(0))?
            .filter_map(|r| r.ok().flatten())
            .collect();
        Ok(names)
    }

    /// Load the dataset-root `.bidsignore` and compile it with full gitignore
    /// semantics.
    ///
    /// BIDS specifies that `.bidsignore` follows gitignore rules, so we use the
    /// `ignore` crate rather than a bare glob set. This is what makes directory
    /// patterns (`logs/`, `figures/`), anchoring (`/derivatives`), and negation
    /// (`!keep.tsv`) behave correctly — a plain `GlobSet` silently mishandled all
    /// three. Only the root `.bidsignore` is consulted, per spec (nested datasets'
    /// ignore files are not applied when walking a parent).
    async fn load_bidsignore(&mut self) -> Result<()> {
        let bidsignore_path = Path::new(".bidsignore");

        let content = match self.fs.read_to_string(bidsignore_path).await {
            Ok(c) => c,
            Err(_) => return Ok(()), // no .bidsignore → nothing ignored
        };

        self.ignore_set = build_bidsignore(&content)?;
        Ok(())
    }

    /// Process IntendedFor field in sidecar to create file associations. Takes
    /// the already-parsed sidecar value so the JSON isn't re-parsed here.
    fn process_intended_for(&mut self, source_file: &str, sidecar: &Value) -> Result<()> {
        if let Some(intended_for) = sidecar.get("IntendedFor") {
            // Association type = the source file's BIDS datatype (fmap → "fieldmap"), derived from
            // the schema rather than guessed from path substrings.
            let assoc_type =
                match bids_core::datatype::parent_datatype(source_file, &self.datatypes) {
                    Some("fmap") => "fieldmap".to_string(),
                    Some(dt) => dt.to_string(),
                    None => "intended_for".to_string(),
                };

            match intended_for {
                Value::String(target) => {
                    // Single target — skipped if it names another dataset (unresolved).
                    if let Some(normalized_target) = Self::normalize_path(target, source_file) {
                        self.pending_associations.push(PendingAssociation {
                            source_file: source_file.to_string(),
                            target_file: normalized_target,
                            assoc_type: assoc_type.clone(),
                        });
                    }
                }
                Value::Array(targets) => {
                    // Multiple targets
                    for target in targets {
                        if let Some(target_str) = target.as_str()
                            && let Some(normalized_target) =
                                Self::normalize_path(target_str, source_file)
                        {
                            self.pending_associations.push(PendingAssociation {
                                source_file: source_file.to_string(),
                                target_file: normalized_target,
                                assoc_type: assoc_type.clone(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Normalize an IntendedFor target into a full dataset-relative path so it matches
    /// `scans.file_path`, or `None` when it cannot be resolved to one *within this dataset*.
    ///
    /// BIDS allows:
    /// - a **BIDS URI** `bids:<name>:<path>` — an empty `<name>` (`bids::…`) means "this
    ///   dataset"; a non-empty `<name>` points into *another* dataset, which we cannot resolve
    ///   yet (no `DatasetLinks` resolution — see docs/adr/0003), so we skip it rather than
    ///   fabricate a wrong path (the old code produced `sub-01/bids:deriv:sub-01/…`);
    /// - a **dataset-relative** path, e.g. `sub-01/ses-mri/func/..._bold.nii.gz` (optionally
    ///   with a leading `/`);
    /// - a **subject-relative** (legacy) path, e.g. `ses-mri/func/..._bold.nii.gz`, relative to
    ///   the declaring file's subject directory.
    fn normalize_path(target: &str, source_file: &str) -> Option<String> {
        let target = match target.strip_prefix("bids:") {
            Some(rest) => match rest.split_once(':') {
                Some(("", path)) => path, // bids::<path> — this dataset
                Some((name, _)) => {
                    eprintln!(
                        "Note: IntendedFor target names dataset {name:?}; cross-dataset BIDS \
                         URIs are not resolved yet — skipping."
                    );
                    return None;
                }
                None => return None, // malformed: `bids:` with no `<name>:`
            },
            None => target,
        };
        let target = target.trim_start_matches('/');

        // Already dataset-relative.
        if target.starts_with("sub-") {
            return Some(target.to_string());
        }

        // Subject-relative: prepend the source file's subject directory.
        if let Some(sub) = source_file.split('/').next()
            && sub.starts_with("sub-")
        {
            return Some(format!("{}/{}", sub, target));
        }

        Some(target.to_string())
    }

    /// Schema-driven structural associations for the whole dataset: for each data file in the
    /// tree, resolve the schema's `meta.associations` (via the shared `bids_schema` resolver)
    /// into `(source data file → discovered associated file)` rows — events↔bold, bval/bvec↔dwi,
    /// channels/electrodes/coordsystem↔electrophysiology, physio, …
    ///
    /// The tree comes from the **registry path set**, not from the backend, so this runs on
    /// every backend rather than only the ones that can produce a `read_file_tree`. The
    /// resolver is pure path matching — it reads no file content — so a content-less
    /// [`FileTree::from_paths`] is exactly enough, and `registered_paths` is the same set the
    /// walk produced (`test_file_registry::every_walked_file_has_a_registry_row` asserts the
    /// two are equal). It differs only where an ingestion `ignore` rule fired, which is the
    /// reading we want: a file bidslake was told not to register is not an association target
    /// either, so a structural association's `target_file_id` is never NULL.
    fn resolve_structural_associations(&self) -> Vec<PendingAssociation> {
        let tree = FileTree::from_paths("", self.registered_paths());
        let meta_assoc = self.schema.associations();
        // The entity, datatype and modality lookups depend only on the schema, so derive them
        // once here rather than re-walking the schema JSON for every file in the tree.
        let index = bids_schema::context::SchemaIndex::new(self.schema.raw());

        let mut out = Vec::new();
        for file in tree.walk_files() {
            let file_ctx = bids_schema::context::FileContext::derive(file, &index);
            // Only data files (inside a datatype directory) can be association sources; skipping
            // the rest avoids evaluating selectors on `dataset_description.json`, READMEs, etc.
            // Filtering here rather than before the derive keeps one entry point: the derive is
            // string work, and the selectors — the expensive part — are still skipped.
            if file_ctx.datatype.is_none() {
                continue;
            }
            let ctx_value = file_ctx.to_selector_value();
            for h in
                bids_schema::associations::resolve_associations(meta_assoc, file, &tree, &ctx_value)
            {
                out.push(PendingAssociation {
                    source_file: file.path.trim_start_matches('/').to_string(),
                    target_file: h.target_file.path.trim_start_matches('/').to_string(),
                    assoc_type: h.name,
                });
            }
        }
        out
    }
}

/// Files per batched-tabular window (see the loop in [`BidsParser::flush_tabular`]).
///
/// Not calibrated: the window exists to bound the length of the generated SQL, and any value
/// that keeps a statement to a few megabytes serves. Note the tradeoff runs both ways, so
/// smaller is not safer — the batch also declares the file's full column list once per window,
/// which for a table hundreds of columns wide is itself a large share of the statement.
const BATCH_WINDOW_FILES: usize = 512;

/// Split a tabular filename into its BIDS `(suffix, extension)` via the shared
/// bids-core parser. The suffix is the trailing token (or the stem for
/// `participants.tsv` / `samples.tsv`); the extension is `.tsv` or `.tsv.gz`.
fn split_suffix_ext(file_name: &str) -> (String, String) {
    let parts = read_entities(file_name);
    (parts.suffix, parts.extension)
}

/// Whether a DuckDB type needs a `TRY_CAST` when read from an all-varchar TSV
/// (so a `n/a` or otherwise unparseable cell becomes NULL rather than erroring).
fn needs_try_cast(sql_type: &str) -> bool {
    matches!(
        sql_type,
        "DOUBLE"
            | "BIGINT"
            | "FLOAT"
            | "REAL"
            | "INTEGER"
            | "UBIGINT"
            | "BOOLEAN"
            | "TIMESTAMP"
            | "DATE"
            | "TIME"
    )
}

/// Build the `INSERT … SELECT … FROM read_csv(…)` that ingests **one** tabular file into its
/// table. DuckDB does the parsing (gzip, `n/a`→NULL, typing); we shape the SELECT so that:
/// - structural columns are filled from the file's location (`dataset_id` constant,
///   `file_path` the file's own path, `row_idx` an ordinal);
/// - each schema-declared column present in the file is `TRY_CAST` to its type;
/// - every other column is folded into `other_data` as JSON.
///
/// This is the **headerless continuous-recording** path, and its only caller is
/// [`BidsParser::ingest_recording`] — every header-bearing file is deferred and written by
/// [`build_tabular_batch_select`] instead, so nothing keyed (`participants`/`sessions`/`scans`)
/// reaches here. Hence the plain `RowIdentity::PerRow` shape: a recording is one row per
/// sample, keyed by nothing.
///
/// `colnames` is the file's column names, from the sidecar `Columns` or the associated
/// channels table (a headerless file has no header to read them from). Columns the schema
/// declares but the file lacks are simply omitted — `INSERT … BY NAME` leaves them NULL.
/// `read_opts` is the `read_csv` argument list after the path.
// A SQL builder with many distinct inputs; grouping them into a struct would add
// indirection without clarity, and `preserve_order` mirrors `build_tabular_batch_select`.
#[allow(clippy::too_many_arguments)]
fn build_tabular_insert_sql(
    spec: &TableSpec,
    source: &str,
    file_id: u64,
    colnames: &[String],
    read_opts: &str,
    preserve_order: bool,
    store_undeclared: bool,
) -> (String, Vec<String>) {
    debug_assert!(
        matches!(spec.identity, RowIdentity::PerRow),
        "only the headerless-recording path reaches here, and a recording is per-row; \
         a keyed table must go through build_tabular_batch_select"
    );
    let present: HashSet<&str> = colnames.iter().map(|s| s.as_str()).collect();
    let mut selects: Vec<String> = Vec::new();

    // An unsigned integer literal needs no quoting and no cast.
    selects.push(format!("{file_id} AS file_id"));
    // `row_number() OVER ()` numbers rows in physical read order; under the `parallel=false`
    // read forced below, that is file line order — which for a recording is sample order, so it
    // is load-bearing rather than cosmetic. Gated on the same flag as the column's existence in
    // the DDL: a table whose order is not load-bearing has no `row_idx` to write to.
    if preserve_order {
        selects.push("(row_number() OVER () - 1)::BIGINT AS row_idx".to_string());
    }

    // Schema-declared data columns present in the file, TRY_CAST to their type.
    let mut known: HashSet<&str> = HashSet::new();
    for c in &spec.columns {
        known.insert(c.name.as_str());
        if !present.contains(c.name.as_str()) {
            continue; // omitted → BY NAME leaves it NULL
        }
        let q = quote_ident(&c.name);
        if needs_try_cast(&c.sql_type) {
            selects.push(format!("TRY_CAST({q} AS {}) AS {q}", c.sql_type));
        } else {
            selects.push(format!("{q} AS {q}"));
        }
    }

    // Everything else → other_data JSON (in file order). An empty name is dropped: it would
    // emit `json_object('', "")`, whose zero-length delimited identifier is a parser error
    // that drops the whole file. Under `undeclared: catalog` the table has no `other_data`
    // column, so these are not projected — the file on disk is the record of them — but they
    // are still computed, so the caller can record their names.
    let extras: Vec<&str> = colnames
        .iter()
        .map(|s| s.as_str())
        .filter(|c| !c.is_empty() && !known.contains(c))
        .collect();
    if store_undeclared && !extras.is_empty() {
        let pairs: Vec<String> = extras
            .iter()
            .map(|c| format!("{}, {}", sql_lit(c), quote_ident(c)))
            .collect();
        selects.push(format!("json_object({}) AS other_data", pairs.join(", ")));
    }

    // Re-index dedup for these tables is a DELETE by `file_path` in the caller, so the insert
    // itself needs no conflict clause.
    //
    // Order matters here: a recording is positional, so the read must be sequential for the
    // `row_number()` above to reproduce file line order — a parallel read would scramble
    // `row_idx` and with it the sample order. See bids-standard/bids-2-devel#98.
    let sequential = if preserve_order {
        ", parallel=false"
    } else {
        ""
    };
    let sql = format!(
        "INSERT INTO {} BY NAME SELECT {} FROM read_csv({}, {read_opts}{sequential})",
        spec.table,
        selects.join(", "),
        sql_lit(source),
    );
    (sql, extras.into_iter().map(str::to_string).collect())
}

/// Build the batched `SELECT` for a group of per-row tabular files that share a
/// table and header (Lever 1b) — one `read_csv([f1,…,fN])` in place of N. The
/// caller prefixes it with `INSERT INTO … BY NAME` for the real write. `files` is
/// (read_csv source — canonical local path or `s3://` URL, dataset-relative path).
///
/// Shape mirrors [`build_tabular_insert_sql`]'s `PerRow` arm, generalized to many
/// files:
/// - `file_path` comes from `read_csv`'s emitted `filename` column, joined back to
///   the dataset-relative path through a `VALUES` map (the abs path is unique per
///   file, so the join is 1:1 and never changes row multiplicity);
/// - `row_idx`: when `preserve_order`, a **global** `row_number()` (assigned in
///   physical read order — `parallel=false` makes that TSV line order) minus each
///   file's first, so it is the same 0-based per-file line index the single-file
///   path produces. When not (order-insensitive tables — see the ingestion
///   schema `ordered` policy), a per-file `row_number()` under the default
///   parallel read: still a unique 0-based key, but in arbitrary order, which lets
///   DuckDB read the batch's files concurrently (a network-FS win).
/// - data columns `TRY_CAST` to their declared type; every remaining header column
///   folds into `other_data`. Because the group shares one header, `other_data`
///   carries exactly each file's real columns — no `union_by_name` NULL fillers.
///
/// `files` is `(read_csv source, dataset-relative path, aux)`, where `aux` is the
/// per-file value the identity needs — see [`PendingTabular::aux`].
///
/// The source column is named `__src` rather than taking `filename=true`'s default,
/// because `scans.tsv` has a column called `filename` of its own. Naming it out of the
/// way is what lets a per-file table batch at all; the collision previously sent those
/// files down a statement-per-file path.
fn build_tabular_batch_select(
    spec: &TableSpec,
    dataset_id: &str,
    root_uri: &str,
    files: &[(&str, &str, &str)],
    columns: &[String],
    preserve_order: bool,
    store_undeclared: bool,
) -> (String, Vec<String>) {
    // Only a per-row table has a `row_idx` for an order to be recorded in, so only a per-row
    // table can want an order-preserving read. That matters rather than being tidy: preserving
    // order costs a sequential scan *and* an unpartitioned `row_number()` window that buffers
    // the whole input, and `Ingestion::ordered` defaults to true — so without this the keyed
    // tables (`scans`, `sessions`, `participants`) would pay both to compute a `__grn` their
    // arms never select.
    let preserve_order = preserve_order && matches!(spec.identity, RowIdentity::PerRow);

    let present: HashSet<&str> = columns.iter().map(|s| s.as_str()).collect();

    let mut selects: Vec<String> = Vec::new();
    // Which of the file's own columns the identity consumes structurally rather than
    // storing as data.
    let mut structural: HashSet<&str> = HashSet::new();
    // The file's own columns this select reads. Collected so the order-preserving
    // subquery below can project just these — see the comment there.
    let mut referenced: Vec<&str> = Vec::new();

    match spec.identity {
        // One row per data file, named by the table's own `filename` column, relative
        // to the directory the `scans.tsv` sits in — which is `m.aux`.
        RowIdentity::PerFile => {
            structural.insert("filename");
            if present.contains("filename") {
                referenced.push("filename");
                // Unlike every other arm, the key cannot come from the map: a `scans.tsv`
                // lists *other* files, one per row, so the row's own `filename` decides which
                // registry entry it is about. Resolved by joining the registry on the path
                // the row constructs (relative to the TSV's own directory, `m.aux`).
                //
                // An inner join, so a `scans.tsv` naming a file the walk never saw
                // contributes no row — which is what the foreign key would enforce anyway,
                // and better than a row pointing at nothing.
                selects.push("fr.file_id AS file_id".to_string());
            }
        }
        RowIdentity::PerEntity if spec.table == "participants" => {
            // Entity tables are dataset-keyed, not file-keyed: a participant is a property of
            // the dataset, and lives on whether or not any one file mentions them.
            selects.push(format!("{} AS dataset_id", sql_lit(dataset_id)));
            structural.insert("participant_id");
            if present.contains("participant_id") {
                referenced.push("participant_id");
                selects.push(
                    "CASE WHEN raw.participant_id LIKE 'sub-%' THEN raw.participant_id                      ELSE 'sub-' || raw.participant_id END AS participant_id"
                        .to_string(),
                );
            }
        }
        RowIdentity::PerEntity => {
            selects.push(format!("{} AS dataset_id", sql_lit(dataset_id)));
            structural.insert("session_id");
            if present.contains("session_id") {
                referenced.push("session_id");
                selects.push(
                    "CASE WHEN raw.session_id LIKE 'ses-%' THEN raw.session_id                      ELSE 'ses-' || raw.session_id END AS session_id"
                        .to_string(),
                );
            }
            // The subject the sessions file belongs to, from its filename.
            selects.push("NULLIF(m.aux, '') AS participant_id".to_string());
        }
        RowIdentity::PerRow => {
            selects.push("m.fid AS file_id".to_string());
            // `row_idx` only when the table has the column, which is only when its order is
            // load-bearing — the same condition, so the two cannot disagree. The global
            // `__grn` minus each file's first gives the per-file line index; the sequential
            // read below is what makes it line order rather than arrival order.
            if preserve_order {
                selects.push(
                    "(raw.__grn - MIN(raw.__grn) OVER (PARTITION BY raw.__src))::BIGINT AS row_idx"
                        .to_string(),
                );
            }
        }
    }

    // Schema-declared data columns present in the file, TRY_CAST to their type. A
    // column the identity already consumed above is skipped: emitting it again would
    // give the SELECT two outputs of the same name, which `INSERT ... BY NAME` sees as
    // a column the table does not have.
    let mut known: HashSet<&str> = HashSet::new();
    for c in &spec.columns {
        if structural.contains(c.name.as_str()) {
            continue;
        }
        known.insert(c.name.as_str());
        if !present.contains(c.name.as_str()) {
            continue; // omitted → BY NAME leaves it NULL
        }
        referenced.push(c.name.as_str());
        let q = quote_ident(&c.name);
        if needs_try_cast(&c.sql_type) {
            selects.push(format!("TRY_CAST(raw.{q} AS {}) AS {q}", c.sql_type));
        } else {
            selects.push(format!("raw.{q} AS {q}"));
        }
    }

    // Everything else → other_data JSON. Identical column set across the group, so
    // these are exactly each file's real extras. Empty names are dropped for the same
    // reason as in `build_tabular_insert_sql`. Computed even when they will not be
    // projected, because the caller records the names in
    // `tabular_undeclared_columns` — deriving both from this one list is what keeps
    // "what we dropped" and "what we recorded dropping" in agreement.
    let extras: Vec<&str> = columns
        .iter()
        .map(|s| s.as_str())
        .filter(|c| !c.is_empty() && !known.contains(c) && !structural.contains(c))
        .collect();
    if store_undeclared && !extras.is_empty() {
        let pairs: Vec<String> = extras
            .iter()
            .map(|c| format!("{}, raw.{}", sql_lit(c), quote_ident(c)))
            .collect();
        selects.push(format!("json_object({}) AS other_data", pairs.join(", ")));
        referenced.extend(extras.iter().copied());
    }

    // Hand DuckDB the schema instead of letting it sniff one. The header is already known
    // here — it is what the group was formed by, and every member shares it byte-for-byte —
    // so the sniffer would only be re-deriving what we can state. The sniff is paid per
    // *file*, not per group, and its cost scales with the header's width, so on a wide
    // derivative table it dominates the read: measured 2026-08 on a 387-file fMRIPrep
    // confounds tree (1,852 columns, ~451 rows, ~4.1 GB), 0.67 s per file sniffing against
    // 0.03 s with the columns declared.
    //
    // Every physical column must be listed, so a file whose header carries an empty
    // name (a trailing tab) falls back to sniffing rather than being described wrongly.
    let read_opts = if columns.iter().any(|c| c.is_empty()) {
        HEADER_READ_OPTS.to_string()
    } else {
        let spec = columns
            .iter()
            .map(|c| format!("{}: 'VARCHAR'", sql_lit(c)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "header=true, auto_detect=false, columns={{{spec}}}, {}",
            non_poisoning_read_flags_typed!()
        )
    };

    let locals = files
        .iter()
        .map(|(l, _, _)| sql_lit(l))
        .collect::<Vec<_>>()
        .join(", ");
    // `fid` is each source file's registry key, computed in Rust and carried into the SQL as
    // a literal: `file_id` is a SHA-256 (see [`file_id`]) and DuckDB cannot reproduce it.
    let map_values = files
        .iter()
        .map(|(l, r, aux)| {
            format!(
                // The id is an unsigned integer literal now, so it needs no quoting
                // and no cast to get out of a string.
                "({}, {}, {}, {})",
                sql_lit(l),
                sql_lit(r),
                sql_lit(aux),
                file_id(dataset_id, root_uri, r)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    // Order-preserving read needs a sequential scan (`parallel=false`) plus the
    // global row number; the order-insensitive read drops both so DuckDB can read
    // files concurrently.
    let from = if preserve_order {
        // Project inside the subquery rather than `SELECT *`. The window has no
        // `PARTITION BY` — a global row number is the whole point — so the operator buffers
        // its entire input, and `*` makes that every column of every row where the outer
        // select reads a handful. On a table hundreds of columns wide that is the difference
        // between buffering what the insert needs and buffering the whole file. The unordered
        // branch needs no such care: with no subquery, DuckDB pushes the outer projection
        // into `read_csv`.
        let mut projected: Vec<String> = vec!["__src".to_string()];
        projected.extend(referenced.iter().map(|c| quote_ident(c)));
        projected.dedup();
        let projected = projected.join(", ");
        format!(
            "(SELECT {projected}, row_number() OVER () AS __grn \
             FROM read_csv([{locals}], {read_opts}, filename='__src', parallel=false)) AS raw"
        )
    } else {
        format!("read_csv([{locals}], {read_opts}, filename='__src') AS raw")
    };

    // A `scans.tsv` names a different file on every row, so its key is resolved against the
    // registry by the path the row builds rather than carried in the map. Every other identity
    // is about the source file itself, whose key `m.fid` already holds.
    let registry_join = if spec.identity == RowIdentity::PerFile {
        format!(
            " JOIN file_registry fr ON fr.dataset_id = {ds} AND fr.root_uri = {root} \
             AND fr.file_path = CASE WHEN m.aux = '' THEN raw.filename \
             ELSE m.aux || '/' || raw.filename END",
            ds = sql_lit(dataset_id),
            root = sql_lit(root_uri),
        )
    } else {
        String::new()
    };

    let sql = format!(
        "SELECT {selects} FROM {from} \
         JOIN (VALUES {map_values}) AS m(abs, rel, aux, fid) ON raw.__src = m.abs{registry_join}",
        selects = selects.join(", "),
    );
    (sql, extras.into_iter().map(str::to_string).collect())
}

/// Parse a TSV file's header from its first line, read in Rust (via
/// [`BidsFileSystem::read_head`]).
///
/// Read here rather than asked of DuckDB because the header is what files are *grouped by*,
/// so it is needed before any `read_csv` runs — and once known it is declared to `read_csv`
/// outright, which is what lets the batch skip the dialect sniffer entirely.
///
/// Returns `(group_key, column_names)`:
///
/// - `group_key` is the raw header line with only the trailing `\n` removed, so a
///   `\r` (CRLF) or a UTF-8 BOM is **kept**. Batches key on it, which quarantines
///   files whose byte-level header differs — DuckDB's multi-file `read_csv`
///   auto-detects one dialect (line terminator, …) from the first file and applies
///   it to all, so mixing e.g. CRLF and LF files in one read misparses the others.
///   Same `group_key` ⇒ identical header bytes ⇒ one consistent dialect.
/// - `column_names` normalize that line (strip a trailing `\r` and a leading BOM,
///   split on the fixed tab) to match the names DuckDB emits once it has detected
///   the dialect, so the batch SQL's column references resolve.
///
/// Accepts the line with or without a trailing newline, so it serves both the
/// local and remote header reads. `None` if the header is empty.
fn tsv_header_from_line(line: &str) -> Option<(String, Vec<String>)> {
    let group_key = line.strip_suffix('\n').unwrap_or(line).to_string();
    let names_line = group_key.strip_suffix('\r').unwrap_or(&group_key);
    let names_line = names_line.strip_prefix('\u{feff}').unwrap_or(names_line);
    if names_line.is_empty() {
        return None;
    }
    let names = names_line.split('\t').map(str::to_string).collect();
    Some((group_key, names))
}

/// The `read_csv` options for a header-bearing tabular file whose columns are *not* declared
/// to `read_csv` — so the dialect is sniffed. The batched read declares its columns instead
/// and uses [`non_poisoning_read_flags_typed`]; this remains the fallback for a header that
/// cannot be declared, namely one carrying an empty column name (see
/// [`build_tabular_batch_select`]).
///
/// Three relaxations make `read_csv` **non-erroring** on real-world-but-imperfect
/// TSVs, so a bad file can never abort (poison) the ingest transaction:
/// - `strict_mode=false` accepts CSV-standard violations that are still valid
///   BIDS — most concretely inconsistent line endings *within* a file (mixed
///   CRLF/LF), which the reference validator doesn't even flag (its newline check
///   only catches CR-only files). Strict mode rejects these at sniff time.
/// - `null_padding=true` pads a short row (too few fields) with NULLs instead of
///   erroring.
/// - `ignore_errors=true` skips any row that still can't be parsed rather than
///   failing the whole read.
///
/// This is a deliberate division of labour. Because these never error, bidslake
/// ingests every good row and **relies on `bids-validator-rs` — not itself — to
/// be the authority on tabular malformation**: a genuinely malformed row is
/// padded/dropped rather than refusing the dataset. It's a catalog, not a
/// validator. A file's rows in its data table reflect exactly what landed, so a
/// file that lost rows is still observable; DuckDB's reject-table can surface the
/// specifics if a hard accounting is ever needed. Not erroring is also what lets the batched
/// flush skip its validator dry-run, so each file is read once rather than twice.
const HEADER_READ_OPTS: &str = concat!("header=true, ", non_poisoning_read_flags!());

/// What a file *is* — the `kind` column of the file registry, and what a consumer filters
/// `all_files` on (docs/adr/0006).
///
/// Deliberately coarse. BIDS has no data/metadata/table axis to borrow: `rules.files` bundles a
/// data file and its sidecar into one rule (149 of its 169 extension-bearing rules list `.json`
/// beside a non-JSON extension), `objects.extensions` is a glossary, and upstream itself defines
/// "is a sidecar" as `extension == ".json"` plus a hand-written exception. So this is bidslake's
/// own vocabulary, in the sense ADR 0002 §4 established for read-vs-catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A primary data file — an image, a recording, a surface. `all_files WHERE kind = 'data'`
    /// is exactly these rows.
    Data,
    /// A JSON metadata file. Its *contents* reach `sidecars` under the path of the data file
    /// it describes; its registry row is the sidecar file itself, under its own path.
    Sidecar,
    /// A table of rows (`.tsv`, `.tsv.gz`), whose rows reach a data table.
    Tabular,
    /// A diffusion companion (`.bval`/`.bvec`), whose values reach `diffusion`.
    Gradient,
    /// `dataset_description.json` — dataset metadata rather than file metadata.
    Description,
    /// Everything else the walk saw and no `ignore` rule claimed: READMEs, CHANGES, code,
    /// stimuli. Recorded so the registry is a manifest of the dataset rather than of the files
    /// bidslake happened to understand.
    Other,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Data => "data",
            Kind::Sidecar => "sidecar",
            Kind::Tabular => "tabular",
            Kind::Gradient => "gradient",
            Kind::Description => "description",
            Kind::Other => "other",
        }
    }
}

/// The stable identity of one file in the catalog: the first **64 bits** of
/// `SHA-256(dataset_id \x1f root_uri \x1f file_path)`, stored as a `UBIGINT`.
///
/// This is a **stored** primary key that every satellite table foreign-keys to, so it must be
/// reproducible from the three identity columns alone — across runs, machines, ingest orders and
/// bidslake versions. Hence SHA-256, which is fixed by specification, rather than DuckDB's
/// `hash()` (an implementation detail with no cross-version guarantee) or Rust's `DefaultHasher`
/// (explicitly not stable across releases). An id that shifted would make every re-index insert
/// duplicates instead of matching.
///
/// `\x1f` (ASCII unit separator) delimits the parts because it cannot occur in a path or URI, so
/// no combination of the three can be confused for another.
///
/// # Why 64 bits and not 128
///
/// It used to be 128, stored as `HUGEINT`. That does not survive the trip to Python: the Arrow
/// bridge maps `HUGEINT` to `Decimal128(38, 0)`, whose maximum is `10^38 - 1` while `HUGEINT`
/// reaches `2^127 - 1 ≈ 1.7 × 10^38` — so **41% of the id space was outside the type the value
/// was handed over in**. The value read back fine but could not be used to build a new frame:
///
/// ```text
/// >>> pl.DataFrame([{"file_id": df["file_id"][0]}])
/// RuntimeError: BindingsError: "Decimal is too large to fit in Decimal128"
/// ```
///
/// Serializing a query result is an ordinary thing to do, and it failed for about two files in
/// five, chosen by hash — so it presented as flakiness rather than as a type error. Widening the
/// decimal is not available: polars caps precision at 38 (`precision must be between 1 and 38`),
/// because its `Decimal` is Decimal128-backed, so the ceiling is upstream of both crates.
///
/// `UBIGINT` crosses as `UInt64` and arrives as a plain Python `int` — no decimal, no rounding,
/// no range to fall off. It also fits `serde_json::Number` exactly, which is why this returns an
/// integer rather than the decimal string the 128-bit version needed, and why
/// [`Schema::row_values`] no longer parses an id back out of a string.
///
/// The cost is collision resistance, and it is worth stating rather than assuming. By the
/// birthday bound the probability of any collision in a catalog of `n` files is `≈ n² / 2^65`:
///
/// | files in one catalog | P(collision) |
/// |---|---|
/// | 10⁴ | 3 × 10⁻¹² |
/// | 10⁶ | 3 × 10⁻⁸ |
/// | 10⁷ | 3 × 10⁻⁶ |
/// | 10⁸ | 3 × 10⁻⁴ |
///
/// A million-file catalog — a study of ~10,000 fMRI runs, counting its FreeSurfer trees — is at
/// one in thirty million, below the rate at which the disk under it corrupts a block. It stops
/// being comfortable somewhere past 10⁸ files, which is where this decision should be revisited;
/// `TODO.md` carries the note.
pub(crate) fn file_id(dataset_id: &str, root_uri: &str, file_path: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(dataset_id.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(root_uri.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(file_path.as_bytes());
    let digest = hasher.finalize();
    let bytes: [u8; 8] = digest[..8].try_into().expect("sha256 yields 32 bytes");
    u64::from_be_bytes(bytes)
}

/// Classify a walked file.
///
/// `datatype` is the file's datatype however it was arrived at — the immediate parent directory
/// for a BIDS-named file, or the projection for a term-mapped one. Taking it as an argument is
/// what lets one function serve both paths: a FreeSurfer `mri/wmparc.mgz` sits under no datatype
/// directory, but its term map states `datatype: anat`, and it is a data file either way.
///
/// The order matters and encodes the companion rule: the four companion extensions are claimed
/// before `datatype.is_some()` is consulted, so a `.json` beside a `.nii.gz` is a sidecar rather
/// than a second data file.
///
/// Note: for multi-file recordings (e.g. BrainVision `.vhdr`+`.vmrk`+`.eeg`) each component is a
/// separate data file and gets its own row; filter by extension for the primary header.
fn kind_of(rel_path: &str, extension: &str, datatype: Option<&str>) -> Kind {
    if rel_path == "dataset_description.json" {
        return Kind::Description;
    }
    match extension {
        ".json" => Kind::Sidecar,
        ".tsv" | ".tsv.gz" => Kind::Tabular,
        ".bval" | ".bvec" => Kind::Gradient,
        _ if datatype.is_some() => Kind::Data,
        _ => Kind::Other,
    }
}

/// Whether a file is a primary BIDS **data file**.
///
/// Superseded by [`kind_of`], and kept as its executable specification: the test
/// `kind_of_agrees_with_is_datafile` asserts the two agree over every path in the corpus.
/// Do not call from ingest.
#[cfg(test)]
fn is_datafile(rel_path: &str, extension: &str, datatypes: &HashSet<String>) -> bool {
    const COMPANION_EXTS: &[&str] = &[".json", ".tsv", ".tsv.gz", ".bval", ".bvec"];
    if COMPANION_EXTS.contains(&extension) {
        return false;
    }
    bids_core::datatype::parent_datatype(rel_path, datatypes).is_some()
}

/// Compile `.bidsignore` file content into a [`Gitignore`] matcher.
///
/// Patterns are relative to the dataset root; `walk()` yields root-relative paths,
/// so an empty builder root keeps both sides in the same frame. Comments and blank
/// lines are skipped; an individually malformed pattern is warned about and
/// skipped rather than failing the whole load.
///
/// Public so the tabular-coverage test can reproduce exactly which files ingest
/// would ignore.
pub fn build_bidsignore(content: &str) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new("");
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Err(e) = builder.add_line(None, trimmed) {
            eprintln!("Warning: invalid .bidsignore pattern '{}': {}", trimmed, e);
        }
    }
    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::{BidsParser, build_bidsignore};
    use std::path::Path;

    #[test]
    fn normalize_path_skips_cross_dataset_bids_uri() {
        // A named BIDS URI points into ANOTHER dataset — skip, don't fabricate a path.
        // (The old code produced `sub-02/bids:deriv:sub-01/x.nii.gz`.)
        assert_eq!(
            BidsParser::normalize_path("bids:deriv:sub-01/x.nii.gz", "sub-02/fmap/y.json"),
            None
        );
        // Malformed (`bids:` with no second colon) is also skipped.
        assert_eq!(
            BidsParser::normalize_path("bids:sub-01", "sub-02/fmap/y.json"),
            None
        );
    }

    #[test]
    fn normalize_path_resolves_this_dataset_and_relative_forms() {
        // `bids::<path>` — this dataset.
        assert_eq!(
            BidsParser::normalize_path("bids::sub-01/func/x.nii.gz", "sub-01/fmap/y.json"),
            Some("sub-01/func/x.nii.gz".to_string())
        );
        // Already dataset-relative.
        assert_eq!(
            BidsParser::normalize_path("sub-01/func/x.nii.gz", "sub-01/fmap/y.json"),
            Some("sub-01/func/x.nii.gz".to_string())
        );
        // Subject-relative (legacy): prepend the source file's `sub-XX/`.
        assert_eq!(
            BidsParser::normalize_path("ses-1/func/x.nii.gz", "sub-03/fmap/y.json"),
            Some("sub-03/ses-1/func/x.nii.gz".to_string())
        );
    }

    fn ignored(content: &str, path: &str) -> bool {
        build_bidsignore(content)
            .unwrap()
            .matched_path_or_any_parents(Path::new(path), false)
            .is_ignore()
    }

    /// A directory pattern must exclude everything beneath it — the case the old
    /// bare-GlobSet handling silently got wrong.
    #[test]
    fn directory_pattern_excludes_contents() {
        assert!(ignored("logs/\n", "logs/run-01.log"));
        assert!(ignored("logs/\n", "sub-01/logs/x.txt"));
        assert!(ignored("figures/\n", "derivatives/figures/a.svg"));
        assert!(!ignored("logs/\n", "sub-01/func/sub-01_bold.nii.gz"));
    }

    /// `*` glob still works, and matches across directories for a bare pattern.
    #[test]
    fn glob_patterns_match() {
        assert!(ignored(
            "*_mixing.tsv\n",
            "sub-16/func/sub-16_desc-x_mixing.tsv"
        ));
        assert!(ignored("*.html\n", "sub-01/report.html"));
        assert!(!ignored("*_mixing.tsv\n", "sub-16/func/sub-16_bold.nii.gz"));
    }

    /// A leading slash anchors a pattern to the dataset root.
    #[test]
    fn anchored_pattern_matches_only_at_root() {
        assert!(ignored("/derivatives\n", "derivatives/sub-01/x.nii.gz"));
        // Anchored at root, so a nested `derivatives` is NOT matched.
        assert!(!ignored("/derivatives\n", "sub-01/derivatives/x.nii.gz"));
    }

    /// Negation re-includes a file excluded by an earlier pattern.
    #[test]
    fn negation_reincludes() {
        let content = "*.tsv\n!keep.tsv\n";
        assert!(ignored(content, "sub-01/drop.tsv"));
        assert!(!ignored(content, "sub-01/keep.tsv"));
    }

    /// Both writers must force `parallel=false` when order matters, so `row_idx` reproduces
    /// file line order — the gap positional `*timeseries.tsv` and recordings would otherwise
    /// hit. See bids-standard/bids-2-devel#98.
    #[test]
    fn order_sensitive_per_row_reads_sequentially() {
        use super::{HEADER_READ_OPTS, build_tabular_batch_select, build_tabular_insert_sql};
        use crate::schema::tabular::{RowIdentity, TableSpec};

        let spec = |table: &str, identity| TableSpec {
            table: table.to_string(),
            columns: Vec::new(),
            identity,
            file_based: true,
            rule_ids: Vec::new(),
        };

        // The single-file (headerless recording) writer.
        let single = |spec: &TableSpec, preserve| {
            build_tabular_insert_sql(
                spec,
                "/t/f.tsv",
                12345,
                &[],
                HEADER_READ_OPTS,
                preserve,
                true,
            )
            .0
        };
        let ordered = single(&spec("physio", RowIdentity::PerRow), true);
        assert!(ordered.contains("parallel=false"), "{ordered}");
        assert!(ordered.contains("AS row_idx"), "{ordered}");
        let unordered = single(&spec("physio", RowIdentity::PerRow), false);
        assert!(!unordered.contains("parallel=false"), "{unordered}");

        // The batched writer, which is what every header-bearing file goes through — including
        // the keyed tables, whose identities used to have their own arms in the single-file
        // builder.
        let batched = |spec: &TableSpec, preserve| {
            build_tabular_batch_select(
                spec,
                "ds",
                "file:///r",
                &[("/t/f.tsv", "sub-01/func/f.tsv", "")],
                &[],
                preserve,
                true,
            )
            .0
        };
        let ordered = batched(&spec("fmriprep_confounds", RowIdentity::PerRow), true);
        assert!(ordered.contains("parallel=false"), "{ordered}");
        assert!(ordered.contains("AS row_idx"), "{ordered}");

        // Order-insensitive per-row (e.g. events) → no forced sequential read.
        let unordered = batched(&spec("events", RowIdentity::PerRow), false);
        assert!(!unordered.contains("parallel=false"), "{unordered}");

        // Keyed tables carry no row_idx → never forced sequential, even with preserve_order.
        let pk = batched(&spec("participants", RowIdentity::PerEntity), true);
        assert!(!pk.contains("parallel=false"), "{pk}");
        assert!(!pk.contains("row_idx"), "{pk}");
    }

    /// `store_undeclared` gates the `other_data` projection in *both* builders. The
    /// declared columns must survive either way — this is a column policy, not a
    /// read/skip policy: the file is still parsed, just not hoarded.
    #[test]
    fn store_undeclared_gates_other_data_in_both_builders() {
        use super::{HEADER_READ_OPTS, build_tabular_batch_select, build_tabular_insert_sql};
        use crate::schema::tabular::{ColumnSpec, RowIdentity, TableSpec};

        let spec = TableSpec {
            table: "confounds".to_string(),
            columns: vec![ColumnSpec {
                key: "trans_x__confounds".to_string(),
                name: "trans_x".to_string(),
                sql_type: "DOUBLE".to_string(),
            }],
            identity: RowIdentity::PerRow,
            file_based: true,
            rule_ids: Vec::new(),
        };
        let header: Vec<String> = ["trans_x", "a_comp_cor_00"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        for store in [true, false] {
            let (single, dropped_single) = build_tabular_insert_sql(
                &spec,
                "/t/f.tsv",
                12345,
                &header,
                HEADER_READ_OPTS,
                true,
                store,
            );
            let (batched, dropped_batched) = build_tabular_batch_select(
                &spec,
                "ds",
                "file:///r",
                &[("/t/f.tsv", "sub-01/func/f.tsv", "")],
                &header,
                true,
                store,
            );
            // The undeclared names are reported either way — that is what the caller
            // records in `tabular_undeclared_columns` when it is not storing them.
            for names in [&dropped_single, &dropped_batched] {
                assert_eq!(names, &["a_comp_cor_00".to_string()]);
            }
            for (which, sql) in [("single-file", &single), ("batched", &batched)] {
                assert_eq!(
                    sql.contains("other_data"),
                    store,
                    "{which} builder with store_undeclared={store}: {sql}"
                );
                // Checked against the projection rather than the whole statement: the
                // batched builder now declares the file's full schema to `read_csv`
                // (which is what lets it skip the sniffer), so an undeclared column is
                // *named* either way. What the policy governs is whether its value is
                // carried into `other_data`.
                assert_eq!(
                    sql.contains("json_object("),
                    store,
                    "{which}: the undeclared column is only projected when stored"
                );
                if store {
                    let payload = &sql[sql.find("json_object(").unwrap()..];
                    assert!(
                        payload.contains("a_comp_cor_00"),
                        "{which}: a stored undeclared column belongs in the overflow"
                    );
                }
                assert!(
                    sql.contains("trans_x"),
                    "{which}: declared columns are unaffected by the policy"
                );
            }
        }
    }

    // `is_datafile_agrees_with_find_datatype` lived here, pinning this crate's own copy of the
    // datatype-directory rule against bids-schema's. There is one implementation now
    // (`bids_core::datatype`), and its cases went with it.

    /// `kind_of` replaced `is_datafile` plus the two hardcoded JSON arms of `process_file`.
    /// `is_datafile` is kept as its executable specification: `Kind::Data` must hold exactly
    /// where it was true, over every path in the vendored corpus rather than a sample. A
    /// divergence would silently change which files count as data files.
    #[test]
    fn kind_of_agrees_with_is_datafile() {
        use super::{Kind, is_datafile, kind_of};
        let schema: serde_json::Value = serde_json::from_str(bids_schema::SCHEMA_JSON).unwrap();
        let datatypes: std::collections::HashSet<String> = schema["objects"]["datatypes"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();

        let check = |rel: &str| {
            let name = rel.rsplit('/').next().unwrap_or(rel);
            let ext = bids_core::entities::read_entities(name).extension;
            let kind = kind_of(
                rel,
                &ext,
                bids_core::datatype::parent_datatype(rel, &datatypes),
            );
            assert_eq!(
                kind == Kind::Data,
                is_datafile(rel, &ext, &datatypes),
                "disagreement on {rel} (kind = {kind:?})"
            );
        };

        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/bids-examples");
        let mut seen = 0usize;
        if corpus.is_dir() {
            let mut stack = vec![corpus.clone()];
            while let Some(dir) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let name = entry.file_name();
                    if name.to_string_lossy().starts_with('.') {
                        continue;
                    }
                    if path.is_dir() {
                        stack.push(path);
                    } else if let Ok(rel) = path.strip_prefix(&corpus) {
                        // Strip the dataset directory so paths are dataset-relative, the
                        // frame `process_file` works in.
                        let rel = rel.to_string_lossy();
                        if let Some((_, inner)) = rel.split_once('/') {
                            check(inner);
                            seen += 1;
                        }
                    }
                }
            }
        }
        assert!(
            seen > 1000,
            "expected a substantial corpus walk, saw {seen} files — run \
             `git submodule update --init`"
        );
    }

    /// `file_id` is a *stored* primary key that every satellite foreign-keys to, so it must
    /// be reproducible from the three identity columns and nothing else. Pin the exact value:
    /// a change to the hash, the separator, or the byte order would make every re-index
    /// insert duplicates rather than match, and no other test would notice.
    #[test]
    fn file_id_is_stable_and_separator_safe() {
        use super::file_id;
        // Cross-checked against an independent implementation:
        //   int.from_bytes(sha256(b"ds001\x1ffile:///data/ds001\x1fsub-01/anat/sub-01_T1w.nii.gz")
        //                  .digest()[:8], "big")
        assert_eq!(
            file_id(
                "ds001",
                "file:///data/ds001",
                "sub-01/anat/sub-01_T1w.nii.gz"
            ),
            4_099_505_605_929_783_485
        );

        // Same three parts, same id — whatever else is in the catalog.
        assert_eq!(
            file_id("ds", "file:///r", "a/b.nii.gz"),
            file_id("ds", "file:///r", "a/b.nii.gz")
        );

        // The unit separator is what stops the parts running together: without it,
        // ("ab", "c", …) and ("a", "bc", …) would hash identically.
        assert_ne!(file_id("ab", "c", "p"), file_id("a", "bc", "p"));
        assert_ne!(file_id("a", "bc", "p"), file_id("a", "b", "cp"));

        // Each part is load-bearing — a dataset's two roots holding the same relative path
        // is the case the whole registry exists to keep apart.
        assert_ne!(
            file_id("ds", "file:///r1", "desc-aseg_dseg.tsv"),
            file_id("ds", "file:///r2", "desc-aseg_dseg.tsv")
        );
    }

    /// The case `is_datafile` cannot express, and the reason `kind_of` takes `datatype` as an
    /// argument rather than deriving it: a FreeSurfer tree has no datatype *directories*, so a
    /// term-mapped path is a data file only by virtue of what the projection says it is.
    #[test]
    fn kind_of_reads_a_projected_datatype() {
        use super::{Kind, kind_of};
        // What `data/term-maps/freesurfer.json` projects for this path.
        assert_eq!(
            kind_of("sub-01/mri/wmparc.mgz", ".mgz", Some("anat")),
            Kind::Data
        );
        // Without the projection the same path is not a data file — `mri` is not a BIDS
        // datatype directory, which is exactly what `is_datafile` could only answer "no" to.
        assert_eq!(kind_of("sub-01/mri/wmparc.mgz", ".mgz", None), Kind::Other);
    }

    /// The non-`Data` kinds, including the ordering that makes a sidecar beside a data file a
    /// sidecar rather than a second data file.
    #[test]
    fn kind_of_classifies_companions_and_documentation() {
        use super::{Kind, kind_of};
        let cases = [
            ("dataset_description.json", ".json", None, Kind::Description),
            // A companion wins over the datatype it sits beside.
            (
                "sub-01/anat/sub-01_T1w.json",
                ".json",
                Some("anat"),
                Kind::Sidecar,
            ),
            (
                "sub-01/func/sub-01_task-x_events.tsv",
                ".tsv",
                Some("func"),
                Kind::Tabular,
            ),
            (
                "sub-01/func/sub-01_task-x_physio.tsv.gz",
                ".tsv.gz",
                Some("func"),
                Kind::Tabular,
            ),
            (
                "sub-01/dwi/sub-01_dwi.bval",
                ".bval",
                Some("dwi"),
                Kind::Gradient,
            ),
            ("participants.tsv", ".tsv", None, Kind::Tabular),
            ("README", "", None, Kind::Other),
            ("CHANGES", "", None, Kind::Other),
            // A nested description is NOT the dataset's own; only the exact root path is.
            (
                "derivatives/fmriprep/dataset_description.json",
                ".json",
                None,
                Kind::Sidecar,
            ),
        ];
        for (path, ext, datatype, want) in cases {
            assert_eq!(kind_of(path, ext, datatype), want, "on {path}");
        }
    }
}
