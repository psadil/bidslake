//! The DuckDB schema — the database reference.
//!
//! `bidslake index` consolidates a BIDS dataset's metadata into a small set of
//! DuckDB tables. Most are **generated dynamically** from the vendored BIDS
//! schema — see [`dynamic`] (and [`Schema`]) for that machinery, which is the
//! heart of how bidslake maps BIDS onto SQL. Two tables are **static** (defined
//! in this module): `diffusion` (parsed bval/bvec arrays) and `file_associations`
//! (derived `IntendedFor`-style cross-references).
//!
//! Every table is keyed by `dataset_id`, so multiple datasets can coexist in one
//! database and stay isolated while being queried together.
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
//! - **`scans`** — one row per imaging file. PK `(dataset_id, file_path)`.
//!   Every discovered imaging file gets a row (including ones a `scans.tsv`
//!   omits). It also carries **generated columns** (see below).
//! - **`sidecars`** — the JSON-sidecar metadata for each imaging file after BIDS
//!   inheritance (dataset-/subject-level sidecars merged, more-specific wins).
//!   PK `(dataset_id, file_path)` referencing `scans`. Very wide — a column per
//!   BIDS metadata field (`repetition_time`, `echo_time`, …) plus `other_data`.
//! - **`events`** — task-event rows from `*_events.tsv` (`onset`, `duration`,
//!   `other_data`); no primary key.
//! - **`diffusion`** — parsed `.bval`/`.bvec` arrays; `bval DOUBLE[]`,
//!   `bvec_x/_y/_z DOUBLE[]`. PK `(dataset_id, file_path)`.
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
//!   **`extension`** (`.nii.gz`), and **`modality`** (`mri`/`eeg`/…).
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
//!         ├── diffusion         (dataset_id, file_path)
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
pub use dynamic::Schema;

pub const CREATE_DIFFUSION_TABLE: &str = "
CREATE TABLE IF NOT EXISTS diffusion (
    dataset_id TEXT,
    file_path TEXT,
    bval DOUBLE[],
    bvec_x DOUBLE[],
    bvec_y DOUBLE[],
    bvec_z DOUBLE[],
    PRIMARY KEY (dataset_id, file_path)
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
