//! The DuckDB schema — the database reference.
//!
//! `bidslake index` consolidates a BIDS dataset's metadata into a small set of
//! DuckDB tables. Most are **generated dynamically** from the vendored BIDS
//! schema — see [`dynamic`] (and [`Schema`]) for that machinery, which is the
//! heart of how bidslake maps BIDS onto SQL. Two tables are **static** (defined
//! in this module): `diffusion` (one row per parsed bval/bvec volume) and
//! `file_associations` (derived `IntendedFor`-style cross-references).
//!
//! Every table is keyed by `dataset_id`, so multiple datasets can coexist in one
//! database and stay isolated while being queried together.
//!
//! # The tabular-data invariant
//!
//! Every tabular file (`.tsv`/`.tsv.gz`) a dataset contains is accounted for —
//! event tables, channel/electrode/optode descriptions, motion recordings, blood
//! curves, participants, sessions, diffusion. Which table a file routes to, and
//! that table's columns and types, are **derived from the BIDS schema**
//! (`rules.tabular_data` + `objects.columns`; for the headerless recordings,
//! `rules.sidecars` and `meta.associations`), never hardcoded. Large compressed
//! recordings (`*.tsv.gz`) are left on disk for now as a size policy, but are still
//! recorded; a file the schema does not describe is skipped with a warning. The
//! `tabular_files` table records every tabular file with a `status`
//! (`ingested`/`on_disk`/`skipped`) so nothing is dropped unnoticed. See
//! [`tabular`] for the routing model.
//!
//! # Conventions
//!
//! - **`dataset_id`** — a dataset's identity, from the `Name` in its
//!   `dataset_description.json` (falling back to the directory/prefix name).
//! - **`file_path`** — a dataset-relative path (`sub-01/func/sub-01_task-x_bold.nii.gz`);
//!   how imaging files are referenced across tables.
//! - **`other_data JSON`** — an overflow column on most tables. Any source field
//!   without a dedicated column is preserved here, so nothing is lost; fields that
//!   *do* have a column are not duplicated into it.
//! - **Missing values** — BIDS `n/a`, and any non-numeric value in a numeric
//!   column (a censored age `89+`, a range `35-40`, an array), are stored as `NULL`.
//!
//! # Tables
//!
//! - **`dataset_description`** — one row per dataset. PK `dataset_id`. Mirrors
//!   `dataset_description.json` (`name`, `bids_version`, `license`, …) plus
//!   `root_uri` (the `file://`/`s3://` origin) and `other_data`.
//! - **`participants`** — one row per subject. PK `(dataset_id, participant_id)`.
//!   Columns from the BIDS participants schema (`age`, `sex`, `handedness`, …).
//!   From `participants.tsv` and implicit `sub-` entities.
//! - **`sessions`** — one row per subject-session. PK
//!   `(dataset_id, session_id, participant_id)`.
//! - **`scans`** — one row per primary **data file**, across modalities: NIfTI plus
//!   electrophysiology (`.edf`/`.vhdr`/`.set`/…), MEG (`.ds`/`.fif`/…), NIRS (`.snirf`),
//!   microscopy, etc. — a file in a datatype directory that is not a sidecar/tabular/gradient
//!   companion (`.json`/`.tsv`/`.bval`/`.bvec`). PK `(dataset_id, file_path)`. Every discovered
//!   data file gets a row (including ones a `scans.tsv` omits). It also carries **generated
//!   columns** (see below), including a boolean **`pseudofile`** flag for opaque *directory*
//!   datafiles (`.ds`/`.mefd`/`.ome.zarr`): these are indexed as a **single** row and never
//!   descended into, so their internal components are not indexed. (Recordings that are genuinely
//!   several files — e.g. BrainVision `.vhdr`+`.vmrk`+`.eeg` — still get a row each.)
//! - **`sidecars`** — the JSON-sidecar metadata for each imaging file after BIDS
//!   inheritance (dataset-/subject-level sidecars merged, more-specific wins).
//!   PK `(dataset_id, file_path)` referencing `scans`. Very wide — a column per
//!   BIDS metadata field (`repetition_time`, `echo_time`, …) plus `other_data`.
//! - **`events`** — task-event rows from `*_events.tsv` (`onset`, `duration`,
//!   `trial_type`, …, `other_data`); one row per line, no primary key.
//! - **Per-modality tabular tables** — one per `rules.tabular_data` rule, named
//!   for it: `eeg_channels`/`meg_channels`/…, `eeg_electrodes`/…, `nirs_optodes`,
//!   `blood`, `asl_context`, `behavioral`, `samples`, `phenotype`, `descriptions`,
//!   `segmentation_lookup`. Each has `(dataset_id, file_path, row_idx)`, the rule's
//!   typed columns, `other_data`, and the generated virtual columns.
//! - **Continuous recordings** — `physio`, `stim`, `physio_events`, `motion`: one
//!   row per sample, column names from the sidecar `Columns` or the associated
//!   `_channels.tsv`. Only *uncompressed* recordings (chiefly `motion`) are
//!   populated; the compressed `*.tsv.gz` physio/stim files are left on disk (see
//!   the invariant above), so those tables may be empty.
//! - **`tabular_files`** — provenance: one row per tabular file the walk saw,
//!   `(dataset_id, file_path, table_name, n_rows, status)`. `status` is
//!   `ingested` (rows in `table_name`), `on_disk` (a compressed recording left on
//!   disk), or `skipped` (a suffix the schema does not describe). Backs the
//!   tabular-data invariant above.
//! - **`diffusion`** — one row per diffusion volume, parsed from the sibling
//!   `.bval`/`.bvec` files: scalar `bval`, `bvec_x/_y/_z`, keyed by
//!   `(dataset_id, file_path, volume_idx)`.
//! - **`file_associations`** — best-effort cross-references (chiefly an fmap's
//!   `IntendedFor`): `source_file_path`, `target_file_path`, `association_type`
//!   (`fieldmap`/`sbref`/`mask`/`derivative`). No foreign keys are enforced (the
//!   source is often a sidecar JSON that isn't itself a `scans` row); targets are
//!   resolved to full dataset-relative paths so they still join to `scans`.
//!
//! ## Query `scans` by BIDS concept
//!
//! `scans` carries **generated (virtual) columns** derived from `file_path`, so
//! you filter on BIDS concepts instead of `LIKE '%…%'` on paths:
//!
//! - one column per BIDS **entity** — `sub`, `ses`, `task`, `run`, `acq`, `dir`,
//!   `echo`, … — holding the raw value (`task='rest'`), or `NULL` when absent (so
//!   `ses` is `NULL` for datasets without sessions, and one query spans a mixed
//!   pool);
//! - **`datatype`** (`func`/`anat`/…), **`suffix`** (`bold`/`T1w`/…),
//!   **`extension`** (`.nii.gz`), **`modality`** (`mri`/`eeg`/…), and **`pseudofile`**
//!   (boolean — an opaque directory datafile like `.ds`/`.ome.zarr`).
//!
//! They are generated from the BIDS schema itself (`objects.entities`,
//! `objects.datatypes`, `rules.modalities`) and computed on read, costing nothing
//! at ingest. See [`dynamic`] for how they're built.
//!
//! ```sql
//! SELECT dataset_id, sub, ses, run, file_path
//! FROM scans
//! WHERE task = 'rest' AND datatype = 'func' AND suffix = 'bold';
//! ```
//!
//! # Relationships
//!
//! ```text
//! dataset_description (dataset_id)
//!   ├── participants (dataset_id, participant_id)
//!   │     └── sessions (dataset_id, session_id, participant_id)
//!   └── scans (dataset_id, file_path)
//!         ├── sidecars          (dataset_id, file_path)   FK → scans
//!         ├── diffusion         (dataset_id, file_path, volume_idx)
//!         ├── events            (dataset_id, file_path)
//!         └── file_associations (target_file_path → scans.file_path, unenforced)
//! ```
//!
//! `scans` and `participants` aren't linked by an explicit column — a scan
//! belongs to a participant via its `file_path` prefix
//! (`s.file_path LIKE p.participant_id || '/%'`). To filter sidecar metadata by
//! concept, join `sidecars` to `scans` on `(dataset_id, file_path)`; entity
//! values are raw, so join to `participants` with `'sub-' || s.sub = p.participant_id`.

pub mod dynamic;
pub mod tabular;
pub use dynamic::Schema;

// Provenance for the tabular-data invariant: one row per tabular file the walk
// encountered (minus `.bidsignore`d ones), so nothing is silently dropped.
// `status` is one of:
//   - `ingested`  — its rows are in `table_name` (`n_rows` of them).
//   - `on_disk`   — a compressed continuous recording (`*.tsv.gz`) left on disk;
//                   `table_name` names the table it *would* map to. This is a
//                   deliberate, crude size policy (see the crate README roadmap):
//                   the physio/stim recordings are row-per-sample and dwarf the
//                   metadata, so for now they stay as files.
//   - `skipped`   — a tabular file the BIDS schema does not describe (`table_name`
//                   NULL).
pub const CREATE_TABULAR_FILES_TABLE: &str = "
CREATE TABLE IF NOT EXISTS tabular_files (
    dataset_id TEXT,
    file_path TEXT,
    table_name TEXT,
    n_rows BIGINT,
    status TEXT,
    PRIMARY KEY (dataset_id, file_path)
);
";

// One row per diffusion volume, matching the row-per-sample shape of every other
// tabular table. `file_path` is the diffusion NIfTI; `volume_idx` is the 0-based
// position of the volume, and (bval, bvec_x/y/z) are that volume's scalar
// b-value and gradient direction, parsed from the sibling `.bval`/`.bvec` files.
pub const CREATE_DIFFUSION_TABLE: &str = "
CREATE TABLE IF NOT EXISTS diffusion (
    dataset_id TEXT,
    file_path TEXT,
    volume_idx BIGINT,
    bval DOUBLE,
    bvec_x DOUBLE,
    bvec_y DOUBLE,
    bvec_z DOUBLE,
    PRIMARY KEY (dataset_id, file_path, volume_idx)
);
";

// file_associations is best-effort, import-time derived metadata (e.g. an
// fmap's IntendedFor, or a coordsystem referencing an anatomical). Its source is
// often a sidecar/JSON that is not itself a `scans` row, so we deliberately do
// NOT enforce foreign keys here — doing so would drop otherwise-valid
// associations during import. Targets are resolved to full dataset-relative
// paths so they still join to `scans` when present.
pub const CREATE_FILE_ASSOCIATIONS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS file_associations (
    dataset_id TEXT,
    source_file_path TEXT,
    target_file_path TEXT,
    association_type TEXT,
    PRIMARY KEY (dataset_id, source_file_path, target_file_path, association_type)
);
";
