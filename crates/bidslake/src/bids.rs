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
//! 4. **Flush deferred work**: `IntendedFor` file associations, parsed
//!    `.bval`/`.bvec` diffusion arrays, and the `scans` table (every imaging file
//!    gets a row, whether or not a `*_scans.tsv` listed it).
//! 5. **Apply BIDS inheritance** to build `sidecars`: for each imaging file, the
//!    applicable dataset-/subject-level JSON sidecars are merged (more-specific
//!    wins), indexed by `(dataset_id, suffix)` to keep matching near-linear.
//!
//! Two performance notes worth knowing: `events` rows (by far the highest row
//! count) are written with the DuckDB `Appender` (bulk path, bypassing SQL
//! planning), and the entire parse runs inside a single transaction (opened by
//! the caller in `main`), so it commits atomically.

use crate::db::{BidsDb, FileAssociation, TabularStatus};
use crate::fs::BidsFileSystem;
use crate::readers::{self, ContentReader};
use crate::schema::Schema;
use crate::schema::dynamic::{quote_ident, sql_in_list, sql_lit};
use crate::schema::ingestion::Disposition;
use crate::schema::tabular::{ColumnSpec, FileContext, RowIdentity, TableSpec};
use anyhow::{Context, Result};
use bids_core::entities::read_entities;
use bids_schema::term_map::{FileFacts, TermMap};
use duckdb::Connection;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

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

/// Wall-time accountant for [`BidsParser::parse`], active only when
/// `BIDSLAKE_TIMING` is set. It isolates the per-file tabular `read_csv` cost
/// (Lever 1b's target) from the rest of the process pass — a split sampling
/// can't cleanly recover — and reports a phase breakdown at the end of the run.
struct PhaseTimer {
    /// True when `BIDSLAKE_TIMING` is set; skips all clock reads otherwise.
    enabled: bool,
    /// Accumulated time inside the tabular `read_csv` ingest, summed across files.
    tabular: Duration,
}

impl PhaseTimer {
    fn new() -> Self {
        Self {
            enabled: std::env::var_os("BIDSLAKE_TIMING").is_some(),
            tabular: Duration::ZERO,
        }
    }

    /// Start a phase clock (or `None` when timing is off, so the caller pays
    /// nothing).
    fn mark(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    /// Add the elapsed time since `start` to the tabular accumulator.
    fn add_tabular(&mut self, start: Option<Instant>) {
        if let Some(start) = start {
            self.tabular += start.elapsed();
        }
    }
}

/// S3/httpfs configuration for reading `s3://` tabular data via DuckDB. Passed to
/// [`BidsParser::new`] so the read-preflight connection is configured as part of
/// construction — there is no separate must-call-before-`parse` step to forget.
pub struct S3Httpfs {
    /// The AWS region httpfs should target.
    pub region: String,
    /// Whether to use anonymous (unsigned) access — public buckets like OpenNeuro's.
    pub anonymous: bool,
}

pub struct BidsParser {
    fs: Box<dyn BidsFileSystem>,
    dataset_id: Option<String>,
    /// S3/httpfs config for the read-preflight connection, applied at the start of
    /// [`Self::parse`]. `None` for local datasets.
    s3_httpfs: Option<S3Httpfs>,
    ignore_set: Gitignore,
    /// Whether to honor the dataset's `.bidsignore`. False (via `--no-bidsignore`)
    /// walks and classifies every file, so overlay-described derivative outputs a
    /// pipeline hides (e.g. fMRIPrep's `*_timeseries.tsv`, `*_xfm.*`) are indexed.
    apply_bidsignore: bool,
    pending_associations: Vec<FileAssociation>,
    pending_diffusion: HashMap<String, PendingDiffusion>,
    schema: Schema,
    imaging_files: Vec<ImagingFile>,
    has_scans_tsv: bool,
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
    /// Opt-in wall-time accountant (`BIDSLAKE_TIMING`); zero-overhead otherwise.
    phase_timer: PhaseTimer,
    /// Prefetched JSON file contents (rel path → content), filled concurrently
    /// before the serial passes so each read isn't a separate round-trip on a
    /// network filesystem.
    json_content: HashMap<String, String>,
    /// Prefetched TSV headers (rel path → parsed header), same rationale.
    tabular_header: HashMap<String, Option<(String, Vec<String>)>>,
    /// Term maps (FreeSurfer, …) that recognize standardized non-BIDS files. Empty for an
    /// ordinary BIDS ingest, so `process_file` pays only one `is_empty()` check per file.
    term_maps: Vec<TermMap>,
    /// Content readers keyed by name (`fs_stats`, …); parse a recognized file's body into
    /// rows. Selected by the ingestion schema's `reader`.
    readers: HashMap<String, Box<dyn ContentReader>>,
}

#[derive(Clone)]
struct ImagingFile {
    dataset_id: String,
    file_path: String,
}

struct PendingDiffusion {
    dataset_id: String,
    bval: Option<Vec<f64>>,
    bvec_x: Option<Vec<f64>>,
    bvec_y: Option<Vec<f64>>,
    bvec_z: Option<Vec<f64>>,
}

struct SidecarInfo {
    dataset_id: String,
    file_path: String, // Relative path in dataset
    entities: HashMap<String, String>,
    suffix: String,
    content: Value,
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

/// A header-bearing per-row tabular file deferred so it can be ingested in a
/// **batch** with its siblings (Lever 1b). Files sharing a table and an identical
/// header signature go into one `read_csv([f1,…,fN])` INSERT, amortizing the
/// ~4–8 ms of fixed per-file `read_csv` setup (open + dialect sniff + plan +
/// state-machine build/teardown) that dominates ingest — measured ~7.5× on the
/// tabular phase. Only `RowIdentity::PerRow` files are deferred; the few
/// per-entity/per-file tables (`participants`/`sessions`/`scans`) stay on the
/// per-file path, whose per-file structural derivation doesn't batch cleanly.
struct PendingTabular {
    spec: TableSpec,
    /// Dataset-relative path (→ `file_path`).
    rel_path: String,
    /// Canonical absolute path, passed to `read_csv` and joined back to `rel_path`
    /// via the emitted `filename` column.
    local_path: String,
    /// Raw header line (see [`read_tsv_header`]). The batch key is
    /// `(table, group_key)`, so every file in a group has byte-identical header
    /// bytes — one `read_csv` dialect, and `other_data` stays exact (no
    /// `union_by_name` NULL fillers).
    group_key: String,
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
    ) -> Self {
        let datatypes = schema.datatypes().into_iter().collect();
        Self {
            fs,
            dataset_id,
            s3_httpfs,
            ignore_set: Gitignore::empty(),
            apply_bidsignore,
            pending_associations: Vec::new(),
            pending_diffusion: HashMap::new(),
            schema,
            imaging_files: Vec::new(),
            has_scans_tsv: false,
            sidecars: Vec::new(),
            dataset_type: None,
            datatypes,
            datatype_dirs: HashSet::new(),
            pending_recordings: Vec::new(),
            pending_tabular: Vec::new(),
            validator: Connection::open_in_memory().expect("open in-memory validator connection"),
            seen_participants: HashSet::new(),
            seen_sessions: HashSet::new(),
            phase_timer: PhaseTimer::new(),
            json_content: HashMap::new(),
            tabular_header: HashMap::new(),
            term_maps: Vec::new(),
            readers: readers::default_readers(),
        }
    }

    /// Attach term maps that recognize standardized non-BIDS files (FreeSurfer, …).
    /// Ordinary BIDS ingestion configures none, so the classify hot path in
    /// [`Self::process_file`] short-circuits on an empty term-map list.
    pub fn with_term_maps(mut self, term_maps: Vec<TermMap>) -> Self {
        self.term_maps = term_maps;
        self
    }

    /// Enable httpfs on the read-preflight [`Self::validator`] connection (from the
    /// [`S3Httpfs`] config given to [`Self::new`]) so its `read_csv` sniff can open
    /// `s3://` tabular files, mirroring the write connection. Called once at the
    /// start of [`Self::parse`]; a no-op for local datasets.
    fn configure_s3_httpfs(&self) -> Result<()> {
        if let Some(cfg) = &self.s3_httpfs {
            crate::s3::configure_httpfs(&self.validator, &cfg.region, cfg.anonymous)?;
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

    /// Column names of a header-bearing file, sniffed on the validator connection.
    /// `None` if the file can't be read (so it is skipped, not ingested).
    fn sniff_columns(&self, read_csv_from: &str) -> Option<Vec<String>> {
        let sql = format!("DESCRIBE SELECT * FROM {read_csv_from}");
        let mut stmt = self.validator.prepare(&sql).ok()?;
        let cols = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .ok()?
            .filter_map(|r| r.ok())
            .collect();
        Some(cols)
    }

    pub async fn parse(&mut self, db: &BidsDb) -> Result<()> {
        // Configure httpfs on the read-preflight connection (if this is an S3
        // ingest) before any `read_csv` sniff runs. No-op for local datasets.
        self.configure_s3_httpfs()?;

        // Opt-in phase timing (`BIDSLAKE_TIMING`). `t_walk` brackets the walk;
        // later phases are timed against a rolling `phase_start`.
        let t_walk = self.phase_timer.mark();

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

        let d_walk = t_walk.map(|t| t.elapsed());
        let t_process = self.phase_timer.mark();

        // Concurrently prefetch the file contents the serial passes will read —
        // JSON sidecars (full) and TSV headers (first 64 KiB). On a network
        // filesystem these are per-file round-trips; reading them with bounded
        // concurrency overlaps the latency instead of paying it one file at a time.
        // Warm local disk sees a negligible change.
        self.prefetch_contents(&dataset_description, &other_files)
            .await;

        // Datasets can carry nested dataset_description.json files (e.g. under
        // derivatives/). Sort shallowest-first so the dataset root wins when we
        // resolve the dataset_id and insert the description.
        dataset_description.sort_by_key(|p| p.components().count());

        // Pass 0: Process dataset_description.json first to resolve dataset_id
        for path in &dataset_description {
            if self.dataset_id.is_none() {
                let content = self.read_cached(path, &path.to_string_lossy()).await?;
                match serde_json::from_str::<Value>(&content) {
                    Ok(dataset_desc) => {
                        if let Some(name) = dataset_desc.get("Name").and_then(|v| v.as_str()) {
                            println!("Using dataset name from dataset_description.json: {}", name);
                            self.dataset_id = Some(name.to_string());
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "Warning: skipping unparseable dataset_description.json at {}: {}",
                            path.display(),
                            e
                        );
                    }
                }
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

        // Lever 1b: ingest the deferred per-row tabular files in header-grouped
        // batches now that all of them are collected.
        let t = self.phase_timer.mark();
        self.flush_tabular(db).await?;
        self.phase_timer.add_tabular(t);

        let d_process = t_process.map(|t| t.elapsed());
        let t_finalize = self.phase_timer.mark();

        // File associations: the `IntendedFor` rows collected during the walk, plus the schema's
        // structural associations (events↔bold, bval/bvec↔dwi, channels↔eeg, …) resolved via the
        // shared `bids_schema` resolver. Deduped on the `file_associations` primary key (cheaper
        // than a DB `ON CONFLICT`; the table is tiny), then inserted.
        let mut associations = self.pending_associations.clone();
        associations.extend(self.resolve_structural_associations(&dataset_id));

        let mut seen: HashSet<(String, String, String, String)> = HashSet::new();
        for assoc in associations {
            let key = (
                assoc.dataset_id.clone(),
                assoc.source_file.clone(),
                assoc.target_file.clone(),
                assoc.assoc_type.clone(),
            );
            if seen.insert(key)
                && let Err(e) = db.insert_file_association(&assoc)
            {
                eprintln!("Failed to insert file association {:?}: {}", assoc, e);
            }
        }

        // Insert pending diffusion data — only when we have both bval and bvec.
        for (nifti_path, diff) in &self.pending_diffusion {
            if let (Some(bval), Some(bvec_x), Some(bvec_y), Some(bvec_z)) =
                (&diff.bval, &diff.bvec_x, &diff.bvec_y, &diff.bvec_z)
                && let Err(e) =
                    db.insert_diffusion(&diff.dataset_id, nifti_path, bval, bvec_x, bvec_y, bvec_z)
            {
                eprintln!("Failed to insert diffusion data for {}: {}", nifti_path, e);
            }
        }

        // Ensure every discovered imaging file has a scans row. scans.tsv rows
        // (inserted earlier, with richer metadata) win via insert-if-not-exists;
        // this adds any imaging files a scans.tsv omitted (derivatives, files not
        // listed) so their sidecars/associations have a referent. Running it
        // unconditionally is what keeps the sidecars FK satisfied.
        if !self.imaging_files.is_empty() {
            println!(
                "Populating scans table with {} imaging files ({}scans.tsv present).",
                self.imaging_files.len(),
                if self.has_scans_tsv { "" } else { "no " }
            );
            // Rows already in `scans` (e.g. inserted from an explicit `scans.tsv`
            // with richer metadata) must win; the old per-row insert used an
            // insert-if-not-exists guard, but the bulk Appender doesn't dedup, so
            // seed the seen-set from what's there and skip those (and any duplicate
            // imaging file) before appending.
            let mut seen: HashSet<(String, String)> = HashSet::new();
            if let Ok(mut stmt) = db.conn.prepare("SELECT dataset_id, file_path FROM scans")
                && let Ok(rows) =
                    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            {
                seen.extend(rows.flatten());
            }

            let mut scan_rows: Vec<Value> = Vec::with_capacity(self.imaging_files.len());
            for img_file in &self.imaging_files {
                if !seen.insert((img_file.dataset_id.clone(), img_file.file_path.clone())) {
                    continue;
                }
                let mut scan_data = serde_json::Map::new();
                scan_data.insert(
                    "dataset_id".to_string(),
                    Value::String(img_file.dataset_id.clone()),
                );
                // file_path contains the full relative path (e.g., sub-01/anat/sub-01_T1w.nii.gz)
                scan_data.insert(
                    "file_path".to_string(),
                    Value::String(img_file.file_path.clone()),
                );

                // `filename` has no dedicated column, so it lands in `other_data`.
                if let Some(filename) = img_file.file_path.split('/').next_back() {
                    scan_data.insert("filename".to_string(), Value::String(filename.to_string()));
                }
                scan_rows.push(Value::Object(scan_data));
            }

            // One bulk Appender insert instead of one prepared INSERT per imaging
            // file — avoids re-parsing the generated-column regexes per row.
            if let Err(e) = db.append_rows(&self.schema, "scans", &scan_rows) {
                eprintln!("Failed to bulk-insert auto-generated scans: {e}");
            }
        }

        let d_finalize = t_finalize.map(|t| t.elapsed());
        let t_inherit = self.phase_timer.mark();

        // Apply BIDS inheritance to populate the `sidecars` table. On local disk we
        // reuse bids-core's tree-based resolver (nearest-wins, matching the BIDS spec
        // / reference validator); other backends (S3) fall back to the in-memory
        // resolver over the sidecars collected during the walk.
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
        self.apply_inheritance_collected(db);

        let d_inherit = t_inherit.map(|t| t.elapsed());
        let t_flush = self.phase_timer.mark();

        // Ingest the deferred headerless recordings now that every sidecar and
        // channels file is available (their columns come from those).
        self.flush_recordings(db).await?;

        let d_flush = t_flush.map(|t| t.elapsed());
        self.report_phase_timing(d_walk, d_process, d_finalize, d_inherit, d_flush);

        Ok(())
    }

    /// Print the phase breakdown to stderr when `BIDSLAKE_TIMING` is set. `tabular`
    /// is the slice of `process` spent in `read_csv` INSERTs — the Lever 1b target.
    fn report_phase_timing(
        &self,
        walk: Option<Duration>,
        process: Option<Duration>,
        finalize: Option<Duration>,
        inherit: Option<Duration>,
        flush: Option<Duration>,
    ) {
        let (Some(walk), Some(process), Some(finalize), Some(inherit), Some(flush)) =
            (walk, process, finalize, inherit, flush)
        else {
            return;
        };
        let total = walk + process + finalize + inherit + flush;
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        eprintln!(
            "[timing] walk+categorize={:.0}ms  process={:.0}ms (tabular read_csv={:.0}ms)  \
             finalize={:.0}ms  inherit={:.0}ms  flush={:.0}ms  total={:.0}ms",
            ms(walk),
            ms(process),
            ms(self.phase_timer.tabular),
            ms(finalize),
            ms(inherit),
            ms(flush),
            ms(total),
        );
    }

    /// Build one merged-sidecar row: the merged metadata as `other_data`, plus each
    /// field also flattened to its own column (schema-known fields get typed
    /// columns). `None` when `merged` is empty. Collected and bulk-inserted via the
    /// Appender (`sidecars` is very wide and carries the generated columns, so a
    /// per-row INSERT is especially slow).
    fn build_sidecar_row(
        &self,
        dataset_id: &str,
        file_path: &str,
        merged: serde_json::Map<String, Value>,
    ) -> Option<Value> {
        if merged.is_empty() {
            return None;
        }
        let mut sidecar_entry = serde_json::Map::new();
        sidecar_entry.insert(
            "dataset_id".to_string(),
            Value::String(dataset_id.to_string()),
        );
        sidecar_entry.insert(
            "file_path".to_string(),
            Value::String(file_path.to_string()),
        );
        // Flatten metadata into top-level fields for known columns (borrowing
        // `merged`), then move the whole map into `other_data` — no clone.
        for (k, v) in &merged {
            sidecar_entry.insert(k.clone(), v.clone());
        }
        sidecar_entry.insert("other_data".to_string(), Value::Object(merged));
        Some(Value::Object(sidecar_entry))
    }

    /// BIDS inheritance for the `sidecars` table, merging the JSON sidecars already
    /// collected in memory during the walk — no disk re-read. Sidecars are keyed by
    /// `(dataset_id, suffix)` with a directory-prefix + entity-subset match, and
    /// merged shallowest-first so a nearer (deeper) sidecar overrides. This is the
    /// sole inheritance path (local and S3); it reproduces the tree-based reference
    /// resolver row-for-row across the corpus.
    fn apply_inheritance_collected(&self, db: &BidsDb) {
        // A sidecar can only apply to an imaging file of the same dataset and
        // suffix, so index sidecars by (dataset_id, suffix) and precompute each
        // one's parent directory. This turns inheritance matching from
        // O(imaging_files x all_sidecars) into O(imaging_files x same-suffix
        // sidecars) — the dominant ingestion cost for sidecar-heavy datasets.
        let sidecar_dirs: Vec<&Path> = self
            .sidecars
            .iter()
            .map(|s| {
                Path::new(&s.file_path)
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
            })
            .collect();
        let mut sidecar_index: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
        for (i, s) in self.sidecars.iter().enumerate() {
            sidecar_index
                .entry((s.dataset_id.as_str(), s.suffix.as_str()))
                .or_default()
                .push(i);
        }

        let mut rows: Vec<Value> = Vec::new();
        for img_file in &self.imaging_files {
            let mut merged_metadata = serde_json::Map::new();

            // Extract entities and suffix from imaging file
            let file_name = img_file.file_path.split('/').next_back().unwrap();
            let img_parts = read_entities(file_name);
            let img_entities = img_parts.entities;
            let img_suffix = img_parts.suffix;

            // Candidates already share dataset_id + suffix; keep those whose
            // directory is a prefix of the image's and whose entities are a
            // subset of the image's.
            let img_dir = Path::new(&img_file.file_path)
                .parent()
                .unwrap_or_else(|| Path::new(""));
            let mut applicable: Vec<usize> = Vec::new();
            if let Some(candidates) =
                sidecar_index.get(&(img_file.dataset_id.as_str(), img_suffix.as_str()))
            {
                for &i in candidates {
                    if !img_dir.starts_with(sidecar_dirs[i]) {
                        continue;
                    }
                    let entities = &self.sidecars[i].entities;
                    if entities
                        .iter()
                        .all(|(key, value)| img_entities.get(key) == Some(value))
                    {
                        applicable.push(i);
                    }
                }
            }

            // BIDS inheritance: the *nearer* sidecar wins, where nearness is
            // directory depth (a sidecar deeper in the tree overrides a shallower
            // one), matching the tree-based reference. Merge shallowest-first so
            // deeper values overwrite; entity count breaks the (invalid-BIDS) tie of
            // two sidecars at the same depth.
            applicable.sort_by_key(|&i| {
                (
                    sidecar_dirs[i].components().count(),
                    self.sidecars[i].entities.len(),
                )
            });

            // Merge metadata
            for &i in &applicable {
                if let Value::Object(map) = &self.sidecars[i].content {
                    for (k, v) in map {
                        merged_metadata.insert(k.clone(), v.clone());
                    }
                }
            }

            if let Some(row) =
                self.build_sidecar_row(&img_file.dataset_id, &img_file.file_path, merged_metadata)
            {
                rows.push(row);
            }
        }
        if let Err(e) = db.append_rows(&self.schema, "sidecars", &rows) {
            eprintln!("Failed to bulk-insert sidecars: {e}");
        }
    }

    /// Read, with bounded concurrency, the JSON contents and TSV headers the
    /// serial passes will consume, into [`Self::json_content`] /
    /// [`Self::tabular_header`]. Failed reads are simply left uncached — the
    /// consuming pass falls back to a direct read (and handles the error there).
    async fn prefetch_contents(
        &mut self,
        dataset_description: &[std::path::PathBuf],
        other_files: &[std::path::PathBuf],
    ) {
        use futures::stream::StreamExt;
        /// Bounded so a huge dataset can't open thousands of sockets at once.
        const CONCURRENCY: usize = 16;

        let rel = |p: &std::path::PathBuf| p.to_string_lossy().to_string();
        let json_paths: Vec<String> = dataset_description
            .iter()
            .chain(other_files.iter())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .map(rel)
            .collect();
        // Header candidates: uncompressed `.tsv` (per-row files sniff a header;
        // `.tsv.gz` is never read here).
        let tsv_paths: Vec<String> = other_files
            .iter()
            .filter(|p| p.to_string_lossy().ends_with(".tsv"))
            .map(rel)
            .collect();

        let (json_res, hdr_res) = {
            let fs = &self.fs;
            let json_fut = futures::stream::iter(json_paths)
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
            futures::join!(json_fut, hdr_fut)
        };

        for (p, c) in json_res {
            if let Some(c) = c {
                self.json_content.insert(p, c);
            }
        }
        for (p, h) in hdr_res {
            self.tabular_header.insert(p, h);
        }
    }

    /// The file's content from the concurrent prefetch, or a direct read if it
    /// wasn't prefetched. `rel` is the dataset-relative key the prefetch used.
    async fn read_cached(&self, path: &Path, rel: &str) -> Result<String> {
        match self.json_content.get(rel) {
            Some(c) => Ok(c.clone()),
            None => Ok(self.fs.read_to_string(path).await?),
        }
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

        let participant_id = entities.get("sub").map(|s| format!("sub-{}", s));
        let session_id = entities.get("ses").map(|s| format!("ses-{}", s));

        // Auto-create participant/session if they don't exist (implicit).
        // Only hit the DB the first time we see each one; every file of a subject
        // would otherwise re-issue an identical (guarded, no-op) insert.
        if let Some(ref pid) = participant_id {
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

                // A duplicate (e.g. from participants.tsv) is a no-op: the
                // insert carries a `WHERE NOT EXISTS` primary-key guard
                // (see `schema::dynamic`), so `?` only surfaces real failures.
                db.insert(
                    &self.schema,
                    "participants",
                    &Value::Object(participant_data),
                )
                .with_context(|| format!("inserting implicit participant {pid}"))?;
            }

            if let Some(ref sid) = session_id
                && self
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

                // Duplicate is a no-op via the `WHERE NOT EXISTS` guard; `?`
                // surfaces only real failures.
                db.insert(&self.schema, "sessions", &Value::Object(session_data))
                    .with_context(|| format!("inserting implicit session {sid} for {pid}"))?;
            }
        }

        // JSON (sidecars + `dataset_description.json`) is handled directly: it is neither
        // read into a data table nor cataloged, but drives inheritance and associations.
        if file_name == "dataset_description.json" {
            self.process_dataset_description(path, db, dataset_id)
                .await?;
            return Ok(());
        }
        if file_name.ends_with(".json") {
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
        if is_datafile(rel_path, &extension, self.schema.raw()) {
            self.imaging_files.push(ImagingFile {
                dataset_id: dataset_id.to_string(),
                file_path: rel_path.to_string(), // Use rel_path not file_name
            });
            return Ok(());
        }

        // Tabular + diffusion companions: the ingestion schema selects on the projected
        // concepts (extension/suffix) and returns the disposition + reader, replacing the
        // former hardcoded `.tsv`/`.bval`/`.bvec` gates. `read` runs the named reader
        // (`csv` = the batched tabular ingest, `diffusion` = the bval/bvec accumulator);
        // `catalog` records the file in the `tabular_files` registry with its contents
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
        match disposition {
            Some((Disposition::Read, reader)) => match reader.as_deref() {
                Some("diffusion") => {
                    self.process_diffusion_file(path, db, rel_path, file_name, dataset_id)
                        .await?;
                }
                Some("csv") => {
                    self.process_tabular_file(db, rel_path, file_name, dataset_id, &entities)
                        .await?;
                }
                other => {
                    eprintln!(
                        "Warning: ingestion `read` rule for {rel_path} names unknown reader {other:?}; skipping"
                    );
                }
            },
            Some((Disposition::Catalog, _)) => {
                // Left on disk, recorded in the tabular registry so queries surface it
                // (chiefly compressed continuous recordings `*_physio.tsv.gz`).
                let table = recording_table_of(&suffix);
                db.record_tabular_file(dataset_id, rel_path, table, 0, TabularStatus::OnDisk)?;
            }
            Some((Disposition::Ignore, _)) => {}
            // A non-datafile, non-JSON file with no ingestion rule (READMEs, CHANGES, …):
            // nothing to ingest.
            None => {}
        }

        Ok(())
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
                Some(rule) => (rule.disposition, rule.reader.clone()),
                None => return Ok(()), // recognized but no ingestion rule -> leave it alone
            }
        };

        // `read` and `catalog` both register the file in the standard `scans` registry.
        if matches!(disposition, Disposition::Read | Disposition::Catalog) {
            self.imaging_files.push(ImagingFile {
                dataset_id: dataset_id.to_string(),
                file_path: rel_path.to_string(),
            });
        }

        if disposition != Disposition::Read {
            return Ok(());
        }

        let Some(reader_name) = reader else {
            eprintln!("Warning: `read` rule for {rel_path} has no reader; skipping");
            return Ok(());
        };
        let content = match self.read_cached(path, rel_path).await {
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
        match rdr.read(dataset_id, rel_path, &content, &facts) {
            Ok(batches) => {
                let mut total = 0i64;
                let mut primary: Option<String> = None;
                for batch in &batches {
                    match db.append_rows(&self.schema, &batch.table, &batch.rows) {
                        Ok(()) => {
                            total += batch.rows.len() as i64;
                            primary.get_or_insert_with(|| batch.table.clone());
                        }
                        Err(e) => {
                            eprintln!(
                                "Warning: insert into {} failed for {rel_path}: {e}",
                                batch.table
                            )
                        }
                    }
                }
                let _ = db.record_tabular_file(
                    dataset_id,
                    rel_path,
                    primary.as_deref(),
                    total,
                    TabularStatus::Ingested,
                );
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
        let content = self.read_cached(path, rel_path).await?;
        let json_value: Value = serde_json::from_str(&content).unwrap_or(Value::Null);

        // Extract the BIDS suffix from the filename via the shared bids-core parser.
        let file_name = path.file_name().unwrap().to_str().unwrap();
        let suffix = read_entities(file_name).suffix;

        // Check for IntendedFor field to create associations (borrows the parsed
        // value before it is moved into the sidecar store below).
        self.process_intended_for(rel_path, &json_value, dataset_id)?;

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

        if let Value::Object(ref mut map) = json_value {
            map.insert(
                "dataset_id".to_string(),
                Value::String(dataset_id.to_string()),
            );
            map.insert("root_uri".to_string(), Value::String(self.fs.root()));
        }

        db.insert(&self.schema, "dataset_description", &json_value)
            .with_context(|| format!("inserting dataset_description for {dataset_id}"))?;
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
        // Read the bval or bvec file
        let content = self.fs.read_to_string(path).await?;

        // Find the base name (both ".bval" and ".bvec" are 5 chars).
        let base_name = &rel_path[..rel_path.len() - 5];

        // The NIfTI file path
        let nifti_path = format!("{}.nii.gz", base_name);

        // Parse content first (before borrowing self mutably)
        if file_name.ends_with(".bval") {
            let bval_vec = self.parse_bval(&content)?;

            // Get or create entry in HashMap
            let entry =
                self.pending_diffusion
                    .entry(nifti_path.clone())
                    .or_insert(PendingDiffusion {
                        dataset_id: dataset_id.to_string(),
                        bval: None,
                        bvec_x: None,
                        bvec_y: None,
                        bvec_z: None,
                    });

            entry.bval = Some(bval_vec);
        } else if file_name.ends_with(".bvec") {
            let (x, y, z) = self.parse_bvec(&content)?;

            // Get or create entry in HashMap
            let entry =
                self.pending_diffusion
                    .entry(nifti_path.clone())
                    .or_insert(PendingDiffusion {
                        dataset_id: dataset_id.to_string(),
                        bval: None,
                        bvec_x: None,
                        bvec_y: None,
                        bvec_z: None,
                    });

            entry.bvec_x = Some(x);
            entry.bvec_y = Some(y);
            entry.bvec_z = Some(z);
        }

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
    /// or unmatched — is recorded in `tabular_files` so nothing is silently dropped.
    async fn process_tabular_file(
        &mut self,
        db: &BidsDb,
        rel_path: &str,
        file_name: &str,
        dataset_id: &str,
        entities: &HashMap<String, String>,
    ) -> Result<()> {
        let (suffix, extension) = split_suffix_ext(file_name);
        if suffix == "scans" {
            self.has_scans_tsv = true;
        }

        // Uncompressed headerless recordings — chiefly the motion time-series —
        // are still ingested. They have no header row; their column names come from
        // the merged sidecar `Columns` or the associated channels file, so they are
        // deferred to the flush once every sidecar has been collected.
        if is_recording_suffix(&suffix) {
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
            Some(spec) if spec.identity == RowIdentity::PerRow => {
                // Lever 1b: defer per-row files so siblings sharing a header can be
                // ingested in one batched `read_csv`. The header is read in Rust
                // (no per-file DuckDB sniff — that fixed cost is what batching
                // exists to remove) purely to group by signature; the batch's
                // dry-run rebinds these names authoritatively before any write, so
                // a Rust/DuckDB header mismatch can only trigger a per-file
                // fallback, never wrong data.
                let t = self.phase_timer.mark();
                let materialized = self.fs.materialize(Path::new(rel_path)).await?;
                let materialized = materialized.to_string_lossy().to_string();
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
                // `read_csv` opens the `s3://` URL directly (httpfs) or the
                // canonical local path.
                let local = if materialized.starts_with("s3://") {
                    materialized
                } else {
                    std::fs::canonicalize(&materialized)
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or(materialized)
                };
                self.phase_timer.add_tabular(t);

                match header {
                    None => {
                        // Unreadable or column-less: contributes no rows, but is
                        // still recorded so the tabular-coverage invariant holds.
                        db.record_tabular_file(
                            dataset_id,
                            rel_path,
                            Some(&spec.table),
                            0,
                            TabularStatus::Ingested,
                        )?;
                    }
                    Some((_, columns)) if columns.iter().any(|c| c == "filename") => {
                        // A real `filename` column would collide with `read_csv`'s
                        // `filename=true`; such files fall back to the per-file path.
                        let t = self.phase_timer.mark();
                        let n = self
                            .ingest_tabular(db, &spec, rel_path, dataset_id, entities)
                            .await?;
                        self.phase_timer.add_tabular(t);
                        db.record_tabular_file(
                            dataset_id,
                            rel_path,
                            Some(&spec.table),
                            n,
                            TabularStatus::Ingested,
                        )?;
                    }
                    Some((group_key, columns)) => {
                        self.pending_tabular.push(PendingTabular {
                            spec,
                            rel_path: rel_path.to_string(),
                            local_path: local,
                            group_key,
                            columns,
                        });
                    }
                }
            }
            Some(spec) => {
                let t = self.phase_timer.mark();
                let n = self
                    .ingest_tabular(db, &spec, rel_path, dataset_id, entities)
                    .await?;
                self.phase_timer.add_tabular(t);
                db.record_tabular_file(
                    dataset_id,
                    rel_path,
                    Some(&spec.table),
                    n,
                    TabularStatus::Ingested,
                )?;
            }
            None => {
                // A validated dataset should not reach here (all its tabular files
                // are schema-described). Warn rather than fail so a newer BIDS
                // extension than the vendored schema doesn't abort ingest.
                eprintln!("Warning: no tabular_data rule for {rel_path}; skipping");
                db.record_tabular_file(dataset_id, rel_path, None, 0, TabularStatus::Skipped)?;
            }
        }
        Ok(())
    }

    /// Ingest a header-bearing tabular file into `spec`'s table via a single
    /// `read_csv` INSERT. Returns the number of rows written.
    async fn ingest_tabular(
        &self,
        db: &BidsDb,
        spec: &TableSpec,
        rel_path: &str,
        dataset_id: &str,
        entities: &HashMap<String, String>,
    ) -> Result<i64> {
        let local = self.fs.materialize(Path::new(rel_path)).await?;
        let local = local.to_string_lossy().to_string();

        // Sniff the actual headers on the validator connection (so a parse error
        // can't poison the ingest transaction). An unreadable or column-less file
        // is classified but contributes no rows.
        let read_from = format!("read_csv({}, {HEADER_READ_OPTS})", sql_lit(&local));
        let sniffed = self.sniff_columns(&read_from).unwrap_or_default();
        if sniffed.is_empty() {
            return Ok(0);
        }

        // Re-index idempotency for keyless row tables: clear this file's prior rows
        // before re-inserting. (PK tables dedup via INSERT OR IGNORE.)
        if matches!(spec.identity, RowIdentity::PerRow) {
            let del = format!(
                "DELETE FROM {} WHERE dataset_id = {} AND file_path = {}",
                spec.table,
                sql_lit(dataset_id),
                sql_lit(rel_path)
            );
            db.conn.execute(&del, [])?;
        }

        let sub = entities.get("sub").map(|s| s.as_str());
        // Positional per-row tables (`*timeseries.tsv` &c., reached here when the file
        // has a literal `filename` column) must keep TSV line order so `row_idx` is a
        // faithful row number; the ordering policy lives in the ingestion schema
        // (`Ingestion::ordered`); see bids-2-devel#98.
        let preserve_order = self.schema.ingestion().ordered(&spec.table);
        let sql = build_tabular_insert_sql(
            spec,
            &local,
            rel_path,
            dataset_id,
            sub,
            &sniffed,
            HEADER_READ_OPTS,
            preserve_order,
        );
        // A single malformed TSV must not abort the whole dataset's ingest — log
        // and move on (the file is still recorded in `tabular_files`, with 0 rows).
        match db.conn.execute(&sql, []) {
            Ok(n) => Ok(n as i64),
            Err(e) => {
                eprintln!("Warning: failed to ingest tabular file {rel_path}: {e}");
                Ok(0)
            }
        }
    }

    /// Ingest every deferred per-row tabular file (Lever 1b), grouped by
    /// `(table, header signature)` so each group is one `read_csv([f1,…,fN])`
    /// INSERT instead of N. Grouping by exact header keeps `other_data` precise
    /// (no `union_by_name` NULL fillers); `row_idx` reproduces TSV line order for
    /// positional tables and is an arbitrary unique key for order-insensitive ones
    /// (see [`build_tabular_batch_select`]).
    ///
    /// Malformed rows can't poison the ingest transaction: `read_csv` uses the
    /// non-erroring relaxations in [`HEADER_READ_OPTS`] (`ignore_errors` /
    /// `null_padding` / `strict_mode=false`), so a bad row is padded or dropped
    /// rather than aborting the statement — `bids-validator-rs`, not bidslake, is
    /// the authority on tabular malformation. There is **no** dry-run and no
    /// per-file fallback (both were removed once the reads became non-poisoning).
    /// Trade-off: each group is one pre-`DELETE` + one batch `INSERT`, so if that
    /// `INSERT` itself errors (e.g. an IO/read failure) the group's rows are
    /// dropped for this run with no per-file isolation, and the affected files are
    /// recorded with `status = "failed"` (see below) rather than `"ingested"`.
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

        for idxs in groups.values() {
            let members: Vec<&PendingTabular> = idxs.iter().map(|&i| &pending[i]).collect();
            let spec = &members[0].spec;
            let columns = &members[0].columns;
            let files: Vec<(&str, &str)> = members
                .iter()
                .map(|m| (m.local_path.as_str(), m.rel_path.as_str()))
                .collect();

            // Re-index idempotency (per-row tables have no PK): clear these files'
            // prior rows in one DELETE before re-inserting.
            let rel_list = sql_in_list(members.iter().map(|m| m.rel_path.as_str()));
            let del = format!(
                "DELETE FROM {} WHERE dataset_id = {} AND file_path IN ({rel_list})",
                spec.table,
                sql_lit(&dataset_id),
            );
            db.conn.execute(&del, [])?;

            // Write the batch directly — no dry-run. `read_csv`'s non-erroring
            // relaxations (see `HEADER_READ_OPTS`) mean a malformed row is
            // padded/dropped rather than aborting, so it can't poison the ingest
            // transaction; dropping the old dry-run halves the reads (the dominant
            // cost on a network filesystem).
            //
            // Row order matters for positional tabular files — notably derivative
            // `*timeseries.tsv` (e.g. fMRIPrep confounds), where row N aligns with
            // volume N of the associated 4D image — so their line order is preserved
            // and `row_idx` records the row number. The ordering policy lives in the
            // ingestion schema (`Ingestion::ordered`);
            // see https://github.com/bids-standard/bids-2-devel/issues/98.
            let preserve_order = self.schema.ingestion().ordered(&spec.table);
            let select =
                build_tabular_batch_select(spec, &dataset_id, &files, columns, preserve_order);
            let sql = format!("INSERT INTO {} BY NAME {select}", spec.table);
            // A batch-INSERT execution failure (e.g. an IO/read error) drops this
            // group's rows for the run — record its members as `failed` so the
            // `tabular_files` catalog can distinguish that from an empty-but-
            // successful ingest, rather than claiming `ingested` with 0 rows.
            let status = if let Err(e) = db.conn.execute(&sql, []) {
                eprintln!(
                    "Warning: batched tabular insert into {} failed: {e}",
                    spec.table
                );
                TabularStatus::Failed
            } else {
                TabularStatus::Ingested
            };
            // Counts come from the table itself (a cheap local query, not a re-read
            // of the source files), so `tabular_files` reflects exactly what landed
            // — including any rows `ignore_errors` dropped.
            let counts = self.table_row_counts(db, spec, &dataset_id, &members);
            for m in &members {
                let n = counts.get(&m.rel_path).copied().unwrap_or(0);
                db.record_tabular_file(&dataset_id, &m.rel_path, Some(&spec.table), n, status)?;
            }
        }
        Ok(())
    }

    /// Per-file row counts for a just-inserted batch, read back from the table (a
    /// cheap local query — not a re-read of the source files). Keyed by
    /// dataset-relative path; a file that landed no rows is absent (recorded as 0).
    fn table_row_counts(
        &self,
        db: &BidsDb,
        spec: &TableSpec,
        dataset_id: &str,
        members: &[&PendingTabular],
    ) -> HashMap<String, i64> {
        let rel_list = sql_in_list(members.iter().map(|m| m.rel_path.as_str()));
        let sql = format!(
            "SELECT file_path, count(*) FROM {} WHERE dataset_id = {} AND file_path IN ({rel_list}) \
             GROUP BY file_path",
            spec.table,
            sql_lit(dataset_id),
        );
        let mut counts = HashMap::new();
        if let Ok(mut stmt) = db.conn.prepare(&sql)
            && let Ok(rows) =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        {
            counts.extend(rows.flatten());
        }
        counts
    }

    /// The BIDS datatype for a file, taken from *any* datatype directory in its
    /// path. This deliberately differs from [`bids_schema::datatypes::find_datatype`]
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
        for rec in &self.pending_recordings {
            let (table, n) = self.ingest_recording(db, rec).await.unwrap_or_else(|e| {
                eprintln!(
                    "Warning: failed to ingest recording {}: {}",
                    rec.rel_path, e
                );
                (None, 0)
            });
            let status = if table.is_some() {
                TabularStatus::Ingested
            } else {
                TabularStatus::Skipped
            };
            db.record_tabular_file(&rec.dataset_id, &rec.rel_path, table.as_deref(), n, status)?;
        }
        Ok(())
    }

    /// Ingest one headerless recording. Returns `(table, rows)`, or `(None, 0)` if
    /// its column names could not be resolved (so it is recorded as skipped).
    async fn ingest_recording(
        &self,
        db: &BidsDb,
        rec: &PendingRecording,
    ) -> Result<(Option<String>, i64)> {
        // Map suffix → target table + column strategy via the single descriptor table.
        let Some(kind) = recording_kind(rec.suffix.as_str()) else {
            return Ok((None, 0));
        };
        let table = kind.table;
        let columns: Vec<ColumnSpec> = if kind.schema_columns {
            self.recording_columns(table)
        } else {
            Vec::new()
        };

        // Column names, in file order: from the associated channels file (motion) or
        // the merged sidecar `Columns` (physio/stim/physioevents).
        let colnames = if kind.colnames_from_channels {
            self.motion_columns(db, rec)?
        } else {
            self.sidecar_columns(rec)
        };
        if colnames.is_empty() {
            return Ok((None, 0)); // headerless file with no column names → skip
        }

        let local = self.fs.materialize(Path::new(&rec.rel_path)).await?;
        let local = local.to_string_lossy().to_string();

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
        let read_from = format!("read_csv({}, {read_opts})", sql_lit(&local));
        if !self.read_csv_ok(&read_from) {
            return Ok((Some(table.to_string()), 0));
        }

        // Re-index idempotency.
        let del = format!(
            "DELETE FROM {} WHERE dataset_id = {} AND file_path = {}",
            table,
            sql_lit(&rec.dataset_id),
            sql_lit(&rec.rel_path)
        );
        db.conn.execute(&del, [])?;

        let sub = rec.entities.get("sub").map(|s| s.as_str());
        // Recordings are positional (row N is sample N), so preserve line order.
        let preserve_order = self.schema.ingestion().ordered(&spec.table);
        let sql = build_tabular_insert_sql(
            &spec,
            &local,
            &rec.rel_path,
            &rec.dataset_id,
            sub,
            &colnames,
            &read_opts,
            preserve_order,
        );
        match db.conn.execute(&sql, []) {
            Ok(n) => Ok((Some(table.to_string()), n as i64)),
            Err(e) => {
                eprintln!(
                    "Warning: failed to ingest recording {}: {}",
                    rec.rel_path, e
                );
                Ok((Some(table.to_string()), 0))
            }
        }
    }

    /// The schema-declared columns of a rule-based recording table (`physio`,
    /// `physio_events`).
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
    fn sidecar_columns(&self, rec: &PendingRecording) -> Vec<String> {
        let merged =
            self.merged_sidecar_map(&rec.dataset_id, &rec.rel_path, &rec.suffix, &rec.entities);
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

    /// Column names for a motion recording: the `name` column of its associated
    /// `_channels.tsv` (per `meta.associations.channels`), already ingested into
    /// `motion_channels`.
    fn motion_columns(&self, db: &BidsDb, rec: &PendingRecording) -> Result<Vec<String>> {
        let Some(base) = rec.rel_path.strip_suffix("_motion.tsv") else {
            return Ok(Vec::new());
        };
        let channels_path = format!("{base}_channels.tsv");
        let sql = format!(
            "SELECT name FROM motion_channels WHERE dataset_id = {} AND file_path = {} ORDER BY row_idx",
            sql_lit(&rec.dataset_id),
            sql_lit(&channels_path)
        );
        let mut stmt = db.conn.prepare(&sql)?;
        let names = stmt
            .query_map([], |r| r.get::<_, Option<String>>(0))?
            .filter_map(|r| r.ok().flatten())
            .collect();
        Ok(names)
    }

    /// The merged sidecar for a file via BIDS inheritance: every collected `*.json`
    /// sidecar of the same dataset and suffix whose directory is a prefix of the
    /// file's and whose entities are a subset, merged least-specific-first.
    fn merged_sidecar_map(
        &self,
        dataset_id: &str,
        rel_path: &str,
        suffix: &str,
        entities: &HashMap<String, String>,
    ) -> serde_json::Map<String, Value> {
        let file_dir = Path::new(rel_path)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let mut applicable: Vec<&SidecarInfo> = self
            .sidecars
            .iter()
            .filter(|s| s.dataset_id == dataset_id && s.suffix == suffix)
            .filter(|s| {
                let sdir = Path::new(&s.file_path)
                    .parent()
                    .unwrap_or_else(|| Path::new(""));
                file_dir.starts_with(sdir)
                    && s.entities.iter().all(|(k, v)| entities.get(k) == Some(v))
            })
            .collect();
        // Depth-first (shallowest merged first, deeper overrides), matching the
        // tree-based reference; entity count breaks same-depth ties.
        applicable.sort_by_key(|s| {
            (
                Path::new(&s.file_path)
                    .parent()
                    .map_or(0, |p| p.components().count()),
                s.entities.len(),
            )
        });
        let mut merged = serde_json::Map::new();
        for s in applicable {
            if let Value::Object(m) = &s.content {
                for (k, v) in m {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }
        merged
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
    fn process_intended_for(
        &mut self,
        source_file: &str,
        sidecar: &Value,
        dataset_id: &str,
    ) -> Result<()> {
        if let Some(intended_for) = sidecar.get("IntendedFor") {
            // Association type = the source file's BIDS datatype (fmap → "fieldmap"), derived from
            // the schema rather than guessed from path substrings.
            let assoc_type =
                match bids_schema::datatypes::find_datatype(source_file, self.schema.raw()) {
                    Some(dt) if dt == "fmap" => "fieldmap".to_string(),
                    Some(dt) => dt,
                    None => "intended_for".to_string(),
                };

            match intended_for {
                Value::String(target) => {
                    // Single target
                    let normalized_target = self.normalize_path(target, source_file);
                    self.pending_associations.push(FileAssociation {
                        dataset_id: dataset_id.to_string(),
                        source_file: source_file.to_string(),
                        target_file: normalized_target,
                        assoc_type: assoc_type.clone(),
                    });
                }
                Value::Array(targets) => {
                    // Multiple targets
                    for target in targets {
                        if let Some(target_str) = target.as_str() {
                            let normalized_target = self.normalize_path(target_str, source_file);
                            self.pending_associations.push(FileAssociation {
                                dataset_id: dataset_id.to_string(),
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

    /// Normalize an IntendedFor target into a full dataset-relative path so it
    /// matches `scans.file_path`.
    ///
    /// BIDS allows two forms:
    /// - dataset-relative, e.g. `bids::sub-01/ses-mri/func/..._bold.nii.gz`
    ///   (optionally with a `bids::` prefix or leading `/`);
    /// - subject-relative (legacy), e.g. `ses-mri/func/..._bold.nii.gz`, which is
    ///   relative to the declaring file's subject directory.
    ///
    /// We strip URI decoration, and for the subject-relative form prepend the
    /// `sub-XX/` taken from `source_file`.
    fn normalize_path(&self, target: &str, source_file: &str) -> String {
        let target = target.trim_start_matches("bids::").trim_start_matches('/');

        // Already dataset-relative.
        if target.starts_with("sub-") {
            return target.to_string();
        }

        // Subject-relative: prepend the source file's subject directory.
        if let Some(sub) = source_file.split('/').next()
            && sub.starts_with("sub-")
        {
            return format!("{}/{}", sub, target);
        }

        target.to_string()
    }

    /// Schema-driven structural associations for the whole dataset: for each data file in the
    /// tree, resolve the schema's `meta.associations` (via the shared `bids_schema` resolver)
    /// into `(source data file → discovered associated file)` rows — events↔bold, bval/bvec↔dwi,
    /// channels/electrodes/coordsystem↔electrophysiology, physio, … Local backend only (needs the
    /// in-memory `FileTree`; the S3 path has none — the same limitation as sidecar inheritance).
    fn resolve_structural_associations(&self, dataset_id: &str) -> Vec<FileAssociation> {
        let Some(tree) = self.fs.file_tree() else {
            return Vec::new();
        };
        let schema = self.schema.raw();
        let meta_assoc = self.schema.associations();
        // The entity abbreviation→key map depends only on the schema, so derive it once here
        // instead of rebuilding it inside `build_file_context` for every file in the tree.
        let name_to_key = bids_schema::context::entity_name_to_key(schema);

        let mut out = Vec::new();
        for file in tree.walk_files() {
            // Only data files (inside a datatype directory) can be association sources; skipping
            // the rest avoids evaluating selectors on `dataset_description.json`, READMEs, etc.
            if bids_schema::datatypes::find_datatype(&file.path, schema).is_none() {
                continue;
            }
            let file_ctx = bids_schema::context::build_file_context(file, schema, &name_to_key);
            for h in
                bids_schema::associations::resolve_associations(meta_assoc, file, &tree, &file_ctx)
            {
                out.push(FileAssociation {
                    dataset_id: dataset_id.to_string(),
                    source_file: file.path.trim_start_matches('/').to_string(),
                    target_file: h.target_file.path.trim_start_matches('/').to_string(),
                    assoc_type: h.name,
                });
            }
        }
        out
    }
}

/// How one headerless continuous-recording suffix maps to a table and how its
/// columns are built. Single source of truth for the recording suffix set, the
/// suffix→table map (note `physioevents` → `physio_events`), and each table's
/// column strategy — so adding a recording kind is a one-line change here rather
/// than edits to three disjoint functions.
struct RecordingKind {
    /// The BIDS suffix (`physio`, `physioevents`, `stim`, `motion`).
    suffix: &'static str,
    /// The DuckDB table it maps to.
    table: &'static str,
    /// Whether the table carries schema-declared typed columns (`physio`,
    /// `physio_events`) or is a bare all-VARCHAR table (`stim`, `motion`).
    schema_columns: bool,
    /// Where column names come from: the associated `_channels.tsv` (`motion`) or
    /// the merged sidecar `Columns` (everything else).
    colnames_from_channels: bool,
}

const RECORDING_KINDS: &[RecordingKind] = &[
    RecordingKind {
        suffix: "physio",
        table: "physio",
        schema_columns: true,
        colnames_from_channels: false,
    },
    RecordingKind {
        suffix: "physioevents",
        table: "physio_events",
        schema_columns: true,
        colnames_from_channels: false,
    },
    RecordingKind {
        suffix: "stim",
        table: "stim",
        schema_columns: false,
        colnames_from_channels: false,
    },
    RecordingKind {
        suffix: "motion",
        table: "motion",
        schema_columns: false,
        colnames_from_channels: true,
    },
];

/// The [`RecordingKind`] for a suffix, or `None` if it is not a continuous recording.
fn recording_kind(suffix: &str) -> Option<&'static RecordingKind> {
    RECORDING_KINDS.iter().find(|k| k.suffix == suffix)
}

/// Whether a suffix names a headerless continuous recording (columns come from the
/// sidecar `Columns` or the associated channels file, not a header row).
fn is_recording_suffix(suffix: &str) -> bool {
    recording_kind(suffix).is_some()
}

/// The table a recording suffix maps to (for provenance), or `None` for suffixes
/// with no dedicated table.
fn recording_table_of(suffix: &str) -> Option<&'static str> {
    recording_kind(suffix).map(|k| k.table)
}

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
            | "HUGEINT"
            | "BOOLEAN"
            | "TIMESTAMP"
            | "DATE"
            | "TIME"
    )
}

/// Build the `INSERT … SELECT … FROM read_csv(…)` that ingests one tabular file
/// into its table. DuckDB does the parsing (gzip, `n/a`→NULL, typing); we shape
/// the SELECT so that:
/// - structural columns are filled from the file's location/identity (`dataset_id`
///   constant; `scans.file_path` from the `filename` column + directory prefix;
///   `participant_id`/`session_id` normalized; `row_idx` an ordinal for row tables);
/// - each schema-declared column present in the file is `TRY_CAST` to its type;
/// - every other column is folded into `other_data` as JSON.
///
/// `sniffed` is the file's column names — from a `DESCRIBE` for a header-bearing
/// file, or the sidecar `Columns` / associated channels for a headerless one.
/// Columns the schema declares but the file lacks are simply omitted — `INSERT …
/// BY NAME` leaves them NULL. `read_opts` is the `read_csv` argument list after the
/// path (it differs only by `header=`/`columns=` between the two cases).
// A SQL builder with many distinct inputs; grouping them into a struct would add
// indirection without clarity, and `preserve_order` mirrors `build_tabular_batch_select`.
#[allow(clippy::too_many_arguments)]
fn build_tabular_insert_sql(
    spec: &TableSpec,
    local_path: &str,
    rel_path: &str,
    dataset_id: &str,
    sub: Option<&str>,
    sniffed: &[String],
    read_opts: &str,
    preserve_order: bool,
) -> String {
    let present: HashSet<&str> = sniffed.iter().map(|s| s.as_str()).collect();
    let mut selects: Vec<String> = vec![format!("{} AS dataset_id", sql_lit(dataset_id))];
    // TSV headers consumed structurally (become a key/path, not a data column and
    // not `other_data`).
    let mut structural: HashSet<&str> = HashSet::new();

    match spec.identity {
        RowIdentity::PerFile => {
            structural.insert("filename");
            if present.contains("filename") {
                // scans.tsv `filename` is relative to the file's directory.
                let prefix = rel_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                let expr = if prefix.is_empty() {
                    "filename".to_string()
                } else {
                    format!("{} || '/' || filename", sql_lit(prefix))
                };
                selects.push(format!("{expr} AS file_path"));
            }
        }
        RowIdentity::PerEntity if spec.table == "participants" => {
            structural.insert("participant_id");
            if present.contains("participant_id") {
                selects.push(
                    "CASE WHEN participant_id LIKE 'sub-%' THEN participant_id ELSE 'sub-' || participant_id END AS participant_id"
                        .to_string(),
                );
            }
        }
        RowIdentity::PerEntity => {
            // sessions: session_id from the file, participant_id from its location.
            structural.insert("session_id");
            if present.contains("session_id") {
                selects.push(
                    "CASE WHEN session_id LIKE 'ses-%' THEN session_id ELSE 'ses-' || session_id END AS session_id"
                        .to_string(),
                );
            }
            if let Some(s) = sub {
                selects.push(format!(
                    "{} AS participant_id",
                    sql_lit(&format!("sub-{s}"))
                ));
            }
        }
        RowIdentity::PerRow => {
            selects.push(format!("{} AS file_path", sql_lit(rel_path)));
            // `row_number() OVER ()` numbers rows in physical read order; under the
            // `parallel=false` read forced below for order-sensitive tables, that is
            // TSV line order. For order-insensitive tables it is an arbitrary but
            // unique 0-based key (order doesn't matter — e.g. `events` sorts by onset).
            selects.push("(row_number() OVER () - 1)::BIGINT AS row_idx".to_string());
        }
    }

    // Schema-declared data columns present in the file, TRY_CAST to their type.
    let mut known: HashSet<&str> = HashSet::new();
    for c in &spec.columns {
        if structural.contains(c.name.as_str()) {
            continue;
        }
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

    // Everything else → other_data JSON (in file order).
    let extras: Vec<&str> = sniffed
        .iter()
        .map(|s| s.as_str())
        .filter(|c| !structural.contains(c) && !known.contains(c))
        .collect();
    if !extras.is_empty() {
        let pairs: Vec<String> = extras
            .iter()
            .map(|c| format!("{}, {}", sql_lit(c), quote_ident(c)))
            .collect();
        selects.push(format!("json_object({}) AS other_data", pairs.join(", ")));
    }

    // PK tables (participants/sessions/scans) dedup by key so an explicit TSV row
    // and an implicit one don't collide; row tables have no key (re-index dedup is
    // a DELETE by file_path in the caller).
    let verb = match spec.identity {
        RowIdentity::PerRow => "INSERT",
        _ => "INSERT OR IGNORE",
    };
    // Order-sensitive per-row tables (positional `*timeseries.tsv`, `*_physio`,
    // recordings, …) must be read sequentially so `row_number()` above reproduces TSV
    // line order; a parallel read would scramble `row_idx`. PK tables carry no
    // `row_idx`, so the flag is a no-op for them. See bids-standard/bids-2-devel#98.
    let sequential = if preserve_order && matches!(spec.identity, RowIdentity::PerRow) {
        ", parallel=false"
    } else {
        ""
    };
    format!(
        "{verb} INTO {} BY NAME SELECT {} FROM read_csv({}, {read_opts}{sequential})",
        spec.table,
        selects.join(", "),
        sql_lit(local_path),
    )
}

/// Build the batched `SELECT` for a group of per-row tabular files that share a
/// table and header (Lever 1b) — one `read_csv([f1,…,fN])` in place of N. The
/// caller prefixes it with `INSERT INTO … BY NAME` for the real write. `files` is
/// `(canonical local path, dataset-relative path)`.
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
/// Callers must exclude files whose header contains a literal `filename` column
/// (it would clash with `filename=true`); such files take the per-file path.
fn build_tabular_batch_select(
    spec: &TableSpec,
    dataset_id: &str,
    files: &[(&str, &str)],
    columns: &[String],
    preserve_order: bool,
) -> String {
    let present: HashSet<&str> = columns.iter().map(|s| s.as_str()).collect();

    let row_idx = if preserve_order {
        "(raw.__grn - MIN(raw.__grn) OVER (PARTITION BY raw.filename))::BIGINT AS row_idx"
    } else {
        "(row_number() OVER (PARTITION BY raw.filename) - 1)::BIGINT AS row_idx"
    };
    let mut selects: Vec<String> = vec![
        format!("{} AS dataset_id", sql_lit(dataset_id)),
        "m.rel AS file_path".to_string(),
        row_idx.to_string(),
    ];

    // Schema-declared data columns present in the file, TRY_CAST to their type.
    let mut known: HashSet<&str> = HashSet::new();
    for c in &spec.columns {
        known.insert(c.name.as_str());
        if !present.contains(c.name.as_str()) {
            continue; // omitted → BY NAME leaves it NULL
        }
        let q = quote_ident(&c.name);
        if needs_try_cast(&c.sql_type) {
            selects.push(format!("TRY_CAST(raw.{q} AS {}) AS {q}", c.sql_type));
        } else {
            selects.push(format!("raw.{q} AS {q}"));
        }
    }

    // Everything else → other_data JSON. Identical column set across the group, so
    // these are exactly each file's real extras.
    let extras: Vec<&str> = columns
        .iter()
        .map(|s| s.as_str())
        .filter(|c| !known.contains(c) && *c != "filename")
        .collect();
    if !extras.is_empty() {
        let pairs: Vec<String> = extras
            .iter()
            .map(|c| format!("{}, raw.{}", sql_lit(c), quote_ident(c)))
            .collect();
        selects.push(format!("json_object({}) AS other_data", pairs.join(", ")));
    }

    let locals = files
        .iter()
        .map(|(l, _)| sql_lit(l))
        .collect::<Vec<_>>()
        .join(", ");
    let map_values = files
        .iter()
        .map(|(l, r)| format!("({}, {})", sql_lit(l), sql_lit(r)))
        .collect::<Vec<_>>()
        .join(", ");

    // Order-preserving read needs a sequential scan (`parallel=false`) plus the
    // global row number; the order-insensitive read drops both so DuckDB can read
    // files concurrently.
    let from = if preserve_order {
        format!(
            "(SELECT *, row_number() OVER () AS __grn \
             FROM read_csv([{locals}], {HEADER_READ_OPTS}, filename=true, parallel=false)) AS raw"
        )
    } else {
        format!("read_csv([{locals}], {HEADER_READ_OPTS}, filename=true) AS raw")
    };

    format!(
        "SELECT {selects} FROM {from} \
         JOIN (VALUES {map_values}) AS m(abs, rel) ON raw.filename = m.abs",
        selects = selects.join(", "),
    )
}

/// Parse a TSV file's header from its first line — read in Rust (via
/// [`BidsFileSystem::read_head`]) instead of a per-file DuckDB `read_csv` sniff,
/// whose ~4 ms fixed cost is what Lever 1b's batching removes. Returns
/// `(group_key, column_names)`:
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

/// The `read_csv` options for a header-bearing tabular file.
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
/// validator. The `tabular_files` row count reflects exactly what landed, so a
/// file that lost rows is still observable; DuckDB's reject-table can surface the
/// specifics if a hard accounting is ever needed. Not erroring is also what lets
/// the batched flush skip its validator dry-run — halving the reads over a
/// network filesystem.
const HEADER_READ_OPTS: &str = concat!("header=true, ", non_poisoning_read_flags!());

/// Determine if a file is an imaging data file that should go in scans table
/// Whether a file is a primary BIDS **data file** (→ one `scans` row): it sits in a datatype
/// directory and is not a sidecar/tabular/gradient companion (`.json` / `.tsv` / `.tsv.gz` /
/// `.bval` / `.bvec`). Covers NIfTI plus electrophysiology (`.edf`/`.vhdr`/`.set`/…), MEG
/// (`.ds`/`.fif`/…), NIRS (`.snirf`), microscopy, etc., so every modality's datafiles are
/// queryable by concept. Datatype is derived from the path via the schema.
///
/// Note: for multi-file recordings (e.g. BrainVision `.vhdr`+`.vmrk`+`.eeg`) each component is a
/// separate data file and gets its own `scans` row; filter by extension for the primary header.
fn is_datafile(rel_path: &str, extension: &str, schema: &Value) -> bool {
    const COMPANION_EXTS: &[&str] = &[".json", ".tsv", ".tsv.gz", ".bval", ".bvec"];
    bids_schema::datatypes::find_datatype(rel_path, schema).is_some()
        && !COMPANION_EXTS.contains(&extension)
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
    use super::build_bidsignore;
    use std::path::Path;

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

    /// The single-file tabular/recording path must force `parallel=false` for
    /// order-sensitive per-row tables so `row_idx` reproduces TSV line order — the
    /// gap that positional `*timeseries.tsv`/recordings would otherwise hit (the
    /// batched path already does this). See bids-standard/bids-2-devel#98.
    #[test]
    fn order_sensitive_per_row_reads_sequentially() {
        use super::{HEADER_READ_OPTS, build_tabular_insert_sql};
        use crate::schema::tabular::{RowIdentity, TableSpec};

        let spec = |table: &str, identity| TableSpec {
            table: table.to_string(),
            columns: Vec::new(),
            identity,
            file_based: true,
            rule_ids: Vec::new(),
        };
        let sql = |spec: &TableSpec, preserve| {
            build_tabular_insert_sql(
                spec,
                "/t/f.tsv",
                "sub-01/func/f.tsv",
                "ds",
                None,
                &[],
                HEADER_READ_OPTS,
                preserve,
            )
        };

        // Positional per-row table → sequential read + a row_idx.
        let ordered = sql(&spec("fmriprep_confounds", RowIdentity::PerRow), true);
        assert!(ordered.contains("parallel=false"), "{ordered}");
        assert!(ordered.contains("AS row_idx"));

        // Order-insensitive per-row (e.g. events) → no forced sequential read.
        let unordered = sql(&spec("events", RowIdentity::PerRow), false);
        assert!(!unordered.contains("parallel=false"), "{unordered}");

        // PK tables carry no row_idx → never forced sequential, even with preserve_order.
        let pk = sql(&spec("participants", RowIdentity::PerEntity), true);
        assert!(!pk.contains("parallel=false"), "{pk}");
    }
}
