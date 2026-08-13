//! The DuckDB schema — the database reference.
//!
//! `bidslake index` consolidates a BIDS dataset's metadata into a small set of
//! DuckDB tables. Most are **generated dynamically** from the vendored BIDS
//! schema — see [`dynamic`] (and [`Schema`]) for that machinery, which is the
//! heart of how bidslake maps BIDS onto SQL. A few tables are **static** (defined
//! in this module): `diffusion` (one row per parsed bval/bvec volume),
//! `file_associations` (derived `IntendedFor`-style cross-references), and the
//! cross-dataset `dataset_links`/`dataset_identity` (see `docs/adr/0003`).
//!
//! Every table is scoped to one `dataset_id`, so multiple datasets can coexist in one
//! database and stay isolated while being queried together. The one place datasets
//! are *related* to each other is the `dataset_relations` **view**, which resolves
//! cross-dataset provenance at query time without any table crossing that boundary.
//! A dataset may be built from several ingest **roots** (`dataset_roots`, docs/adr/0005);
//! a file-keyed table reaches its dataset through `file_registry` rather than storing one.
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
//! `file_registry` records every file it saw with a `status`
//! (`ingested`/`on_disk`/`skipped`/`failed`) so nothing is dropped unnoticed. See
//! [`tabular`] for the routing model.
//!
//! # Conventions
//!
//! - **`dataset_id`** — a dataset's identity, from the `Name` in its
//!   `dataset_description.json` (falling back to the directory/prefix name).
//! - **`file_path`** — a **root**-relative path (`sub-01/func/sub-01_task-x_bold.nii.gz`).
//!   It identifies a file only together with the root it was walked from, which is why
//!   `file_registry` holds `(dataset_id, root_uri, file_path)` and everything else refers
//!   to a file by `file_id` (docs/adr/0006).
//! - **`file_id`** — the surrogate key for a file: the first 128 bits of
//!   `SHA-256(dataset_id ␟ root_uri ␟ file_path)`, as a `HUGEINT`. Content-derived, so it
//!   is stable across runs and machines and a re-index upserts onto the same rows. Every
//!   file-keyed table keys on it. Four of them — `scans`, `sidecars`, `bvals`, `bvecs` —
//!   declare an actual `FOREIGN KEY`; the per-row tables and `file_associations` do not (see
//!   the diagram below), so the reference is by convention there rather than enforced.
//!
//!   **Which file** a table keys on is one rule (docs/adr/0007): the file whose rows it
//!   holds. A `*_events.tsv`'s rows key on the events file, a `.bval`'s on the `.bval`. The
//!   data files those rows are *about* are reached through `file_associations`, because one
//!   describing file can serve many — which is what BIDS inheritance means for a tabular
//!   companion. `scans` and `sidecars` key on the described file instead, and neither is an
//!   exception: a `scans.tsv` row names its target outright in a `filename` column (a 1:1
//!   lookup, no inheritance), and a `sidecars` row is the *merge* of several JSON documents,
//!   so it has no single source file to key on.
//! - **`other_data JSON`** — an overflow column on most tables. Any source field
//!   without a dedicated column is preserved here; fields that *do* have a column are
//!   not duplicated into it.
//!
//!   Conditional, not universal: a table whose ingestion policy declares
//!   `undeclared: catalog` has no `other_data` column at all, and the columns it does
//!   not declare stay in the file on disk — still recorded in `file_registry`, with
//!   their names in `tabular_undeclared_columns`. A table stores what its schema
//!   declares it stores; see docs/adr/0004. fMRIPrep confounds are why: ~1,800 columns
//!   against ~13 declared, which cost 24 MB of database per file as per-row JSON.
//! - **Missing values** — BIDS `n/a`, and any non-numeric value in a numeric
//!   column (a censored age `89+`, a range `35-40`, an array), are stored as `NULL`.
//!
//! # Tables
//!
//! - **`dataset_description`** — one row per dataset. PK `dataset_id`. Mirrors
//!   `dataset_description.json`, whose fields keep their verbatim BIDS names
//!   (`Name`, `BIDSVersion`, `License`, …), plus `other_data`. A dataset with no
//!   description of its own still gets a row, holding only its id.
//! - **`dataset_roots`** — the ingest roots a dataset was built from, PK
//!   `(dataset_id, root_uri)`. One row for an ordinary dataset, N for subject-sharded
//!   pipeline output, which is one logical dataset with one root per subject
//!   (docs/adr/0005). `root_uri` + a root-relative `file_path` is what turns a stored row
//!   back into an openable URI.
//! - **`file_registry`** — **the manifest**: one row per file the walk saw, whatever
//!   bidslake did with it. PK `file_id`, plus `(dataset_id, root_uri, file_path)` — the
//!   triple that id hashes — and:
//!   - **`kind`** — what the file *is*: `data` (an image, a recording, a surface),
//!     `sidecar`, `tabular`, `gradient` (`.bval`/`.bvec`), `description`
//!     (`dataset_description.json`), or `other` (READMEs, code, stimuli).
//!   - **`status`** — what became of a file bidslake tried to *read*:
//!     `ingested`/`on_disk`/`skipped`/`failed`, or NULL for a file there was never any
//!     reading to report on.
//!   - **`projected JSON`** — what a term map computed, present only when one is
//!     configured (ADR 0002 §7).
//!
//!   Only the files the walk never reached are absent: `.bidsignore`d ones, and ones an
//!   `ingest` rule dropped. It is the foreign-key target for every file-keyed table, which
//!   is only sound because it holds *all* the files (docs/adr/0006).
//!
//!   Opaque *directory* datafiles (`.ds`/`.mefd`/`.ome.zarr`) get a **single** row and are
//!   never descended into, so their internal components are not indexed; the `pseudofile`
//!   concept column flags them. (Recordings that are genuinely several files — e.g.
//!   BrainVision `.vhdr`+`.vmrk`+`.eeg` — still get a row each.)
//! - **`all_files`** (VIEW) — `file_registry` widened with the BIDS-concept columns
//!   (see below). The query surface; `WHERE kind = 'data'` narrows it to primary data
//!   files, which is what `bidslake-py`'s `get()` iterates.
//! - **`participants`** — one row per subject. PK `(dataset_id, participant_id)`.
//!   Columns from the BIDS participants schema (`age`, `sex`, `handedness`, …).
//!   From `participants.tsv` and implicit `sub-` entities.
//! - **`sessions`** — one row per subject-session. PK
//!   `(dataset_id, session_id, participant_id)`.
//! - **`scans`** — the `scans.tsv` satellite, and *not* a file registry despite the name
//!   BIDS gives it: acquisition metadata (`acq_time`, `HED`, `other_data`) for the data
//!   files a `scans.tsv` describes, PK `file_id`. Built the way `sessions` is — from the
//!   file's contents — so a dataset with no `scans.tsv` has no rows here.
//! - **`sidecars`** — the JSON-sidecar metadata for each imaging file after BIDS
//!   inheritance (dataset-/subject-level sidecars merged, more-specific wins).
//!   PK `file_id` referencing `file_registry` — the *data* file's id, not the sidecar's
//!   (the sidecar has its own registry row, under its own path, as `kind = 'sidecar'`).
//!   Very wide — a column per BIDS metadata field, verbatim-named (`RepetitionTime`,
//!   `EchoTime`, …), plus `other_data`.
//! - **`events`** — task-event rows from `*_events.tsv` (`onset`, `duration`,
//!   `trial_type`, …, `other_data`); one row per line keyed by the `file_id` of the
//!   `*_events.tsv` itself, no primary key, and — alone among the per-row tables —
//!   **no `row_idx`**: order events by `onset` (see below).
//! - **Per-modality tabular tables** — one per `rules.tabular_data` rule, named
//!   for it: `eeg_channels`/`meg_channels`/…, `eeg_electrodes`/…, `nirs_optodes`,
//!   `blood`, `asl_context`, `behavioral`, `samples`, `phenotype`, `descriptions`,
//!   `segmentation_lookup`. Each has `(file_id, row_idx)` — the `file_id` of the `.tsv`
//!   the rows came from — plus the rule's typed columns and `other_data`. They carry no
//!   concept columns of their own; join `all_files` on `file_id` for those.
//!
//!   **`row_idx` exists because a SQL table is unordered and a TSV is not**, and it is
//!   present exactly on the tables where that matters. Several BIDS tables carry meaning
//!   in their line order and nowhere else: a `*_channels.tsv`'s order maps onto the
//!   columns of the binary recording beside it, a recording's rows *are* its time axis,
//!   and a derivative `*timeseries.tsv` aligns row N with volume N of its 4D image.
//!   `row_idx` preserves that through ingestion, so a consumer need not re-read the file
//!   to recover it. It is a plain ordinal, not a key — these tables have no primary key.
//!
//!   Which tables have it is declared by the ingestion schema (`Ingestion::ordered`,
//!   default true): those tables are read sequentially and their `row_idx` reproduces
//!   line order. A table declared *un*ordered has **no `row_idx` column at all** —
//!   its files are read concurrently, so no row order exists to record, and a column of
//!   arbitrary labels would only invite `ORDER BY row_idx`. `events` is the one BIDS
//!   table in that group: its rows are addressed by `onset`, and it has the highest row
//!   count, so reading its files concurrently is worth having. Order events by `onset`
//!   (events sharing an `onset` have no tiebreak — that information is not in the file
//!   either, beyond its line order).
//!
//!   `row_idx` records line order; what that order *means* is the `describes.axis` a table
//!   may declare (docs/adr/0007), and the generated view then exposes the same column as
//!   `<axis>_idx` — `volume_idx` on `timeseries`, `bval_volumes`, `asl_volumes`.
//! - **Continuous recordings** — `physio`, `stim`, `physio_events`, `motion`: one
//!   row per sample, column names from the sidecar `Columns` or the associated
//!   `_channels.tsv`. Only *uncompressed* recordings (chiefly `motion`) are
//!   populated; the compressed `*.tsv.gz` physio/stim files are left on disk (see
//!   the invariant above), so those tables may be empty.
//! - **`bvals` / `bvecs`** — the gradient payloads, one row per value in a `.bval` and per
//!   column of a `.bvec`, PK `(file_id, row_idx)` where `file_id` is **the gradient file's
//!   own**. One table each rather than one with nullable columns, so every row is fully
//!   populated and a table's columns are exactly what its source file supplies.
//! - **`diffusion`** (VIEW) — the image-facing gradient table: one row per (image,
//!   `volume_idx`) with `bval`, `bvec_x/_y/_z`, plus `bval_file_id`/`bvec_file_id` naming
//!   where each half came from. Composed from `bval_volumes` and `bvec_volumes`, the views
//!   the `describes` declarations generate, so a gradient set inherited from a higher
//!   directory level reaches every image below it from one stored copy — `ds114` ships one
//!   `dwi.bval` covering 20 images (docs/adr/0007). The b-values define the volume axis.
//! - **`<describes>` views** — `timeseries` (fMRIPrep confounds), `asl_volumes`,
//!   `bval_volumes`, `bvec_volumes`: a per-row table re-keyed onto the data files its rows
//!   describe, resolved through `file_associations` at query time. Declared by the ingestion
//!   schema's `describes` block, never hand-written, and present only when the adapter that
//!   declares the underlying table is in use.
//! - **`file_associations`** — best-effort cross-references (chiefly an fmap's
//!   `IntendedFor`): `source_file_id`, nullable `target_file_id`, `target_file_path`,
//!   `association_type` (`fieldmap`/`sbref`/`mask`/`derivative`). PK
//!   `(source_file_id, target_file_path, association_type)`. No foreign keys are declared:
//!   `target_file_id` would need one that tolerates NULL, because an `IntendedFor` may name
//!   a file the catalog does not hold — that target keeps its declared path with a NULL id
//!   rather than being dropped. The source side needs none: it is always a file the walk
//!   saw, since that is where the reference was read from.
//! - **`dataset_links`** — what a dataset declares it came from, one row per
//!   `SourceDatasets` entry / `--source-dataset` flag / `DatasetLinks` mapping:
//!   `link_type`, `link_name`, `declared_ref`, and the canonicalized
//!   `identity`/`identity_kind`/`identity_base` (see `links.rs`).
//! - **`dataset_identity`** — what identities a dataset *is* (`self`, `DatasetDOI`,
//!   `root_uri`), so a present source can be matched as a parent.
//! - **`dataset_relations`** (VIEW) — the resolved dataset-to-dataset relation:
//!   `(from_dataset_id, to_dataset_id, relation, via_identity)` where `relation` is
//!   `shares_source` (co-derivatives of one source), `derived_from`, or `source_of`.
//!   Resolved at query time, so ingest order is irrelevant (`docs/adr/0003`).
//!
//! ## Query `all_files` by BIDS concept
//!
//! `all_files` carries **columns derived from `file_path`**, so you filter on BIDS
//! concepts instead of `LIKE '%…%'` on paths:
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
//! at ingest. They live on the **view**, once, rather than on each of the 25
//! file-keyed tables — which is what lets a wider later run redefine them
//! retroactively, with `CREATE OR REPLACE VIEW`, for rows already stored.
//! See [`dynamic`] for how they're built.
//!
//! A path is not always the source. The registry holds every cataloged file,
//! including ones a **term map** claimed (`sub-01/mri/wmparc.mgz`), whose names
//! carry almost no BIDS concepts. Those files store what the term map projected in
//! a `projected JSON` column, and the concepts it can supply are computed as
//! `COALESCE(<the projection>, <the path regex>)` — so the same `WHERE seg =
//! 'wmparc'` reaches a FreeSurfer volume and a BIDS-named one alike. The column
//! exists only when a term map is configured (ADR 0002 §7).
//!
//! ```sql
//! SELECT dataset_id, sub, ses, run, file_path
//! FROM all_files
//! WHERE kind = 'data' AND task = 'rest' AND datatype = 'func' AND suffix = 'bold';
//! ```
//!
//! A satellite stores none of these, so reach them by joining on `file_id`:
//!
//! ```sql
//! SELECT f.sub, f.task, e.onset, e.trial_type
//! FROM events e JOIN all_files f USING (file_id)
//! WHERE f.task = 'rest';
//! ```
//!
//! # Relationships
//!
//! ```text
//! dataset_description (dataset_id)
//!   ├── dataset_roots (dataset_id, root_uri)   where the dataset was walked from
//!   ├── participants (dataset_id, participant_id)
//!   │     └── sessions (dataset_id, session_id, participant_id)
//!   ├── file_registry (file_id)  ⇒ all_files (VIEW)   + the BIDS-concept columns
//!   │     ├── scans             (file_id)              FOREIGN KEY → file_registry
//!   │     ├── sidecars          (file_id)              FOREIGN KEY
//!   │     ├── bvals             (file_id, row_idx)     FOREIGN KEY
//!   │     ├── bvecs             (file_id, row_idx)     FOREIGN KEY
//!   │     ├── events            (file_id)              by convention, unenforced
//!   │     ├── <per-modality>    (file_id, row_idx)     by convention, unenforced
//!   │     └── file_associations (source_file_id, target_file_id nullable)  unenforced
//!   ├── dataset_links    (dataset_id, …)   what this dataset declares it came from
//!   └── dataset_identity (dataset_id, …)   what this dataset *is*
//!         ⇒ dataset_relations (VIEW)        resolved dataset↔dataset edges (query-time)
//! ```
//!
//! A per-row table above keys on **the file its rows came from**; the data files those
//! rows *describe* are one join away, through `file_associations` (docs/adr/0007):
//!
//! ```text
//!   <per-row table> (file_id = the describing file)
//!         ↑ target_file_id
//!   file_associations (association_type = the schema's `meta.associations` key)
//!         ↓ source_file_id
//!   all_files (file_id = the data file)
//!         ⇒ <describes> VIEW: the rows, keyed by the file they are about
//!            bvals ⨝ bvecs ⇒ diffusion · fmriprep_confounds ⇒ timeseries
//!            asl_context ⇒ asl_volumes
//! ```
//!
//! The registry and `participants` aren't linked by an explicit column — a file
//! belongs to a participant via its `file_path` prefix
//! (`f.file_path LIKE p.participant_id || '/%'`). To filter sidecar metadata by
//! concept, join `sidecars` to `all_files` on `file_id`; entity values are raw, so join
//! to `participants` with `'sub-' || f.sub = p.participant_id`.

pub mod dynamic;
pub mod ingestion;
pub mod tabular;
pub use dynamic::{AppliedOverlay, Schema};
pub use ingestion::Ingestion;

/// The tables created from the static DDL in this module rather than generated from the
/// schema — the ones whose columns are bidslake's own rather than derived from
/// `rules.tabular_data`.
///
/// Named as a set because the view generator has to know a table exists before emitting a
/// view over it, and a static table is absent from `Schema::table_definitions` (which holds
/// only what was generated). Without this, a `describes` declaration on `bvals` would be
/// silently skipped as if the table were missing.
pub const STATIC_TABLES: &[&str] = &[
    "bvals",
    "bvecs",
    "file_associations",
    "tabular_undeclared_columns",
    "dataset_roots",
    "dataset_links",
    "dataset_identity",
];

// The column names a table saw but does not declare — one row per distinct name,
// per table, for the whole catalog.
//
// Only populated for tables whose ingestion policy is `undeclared: catalog`, where
// the names are otherwise nowhere in the database. It answers "what confound
// regressors does this catalog's data contain?" without opening a file; the
// authoritative per-file column set remains the file's own header, reachable via
// `file_registry.file_path`.
//
// Deliberately keyed by name rather than by file or by header signature. Real fMRIPrep
// output does not share headers between files — the aCompCor component count varies per
// run — so a per-header manifest grows with the study, at about one header per file. The
// names themselves are drawn from one shared space, bounded by the pipeline's vocabulary
// rather than by dataset size (see docs/adr/0004).
pub const CREATE_TABULAR_UNDECLARED_COLUMNS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS tabular_undeclared_columns (
    table_name TEXT,
    name TEXT,
    PRIMARY KEY (table_name, name)
);
";

// The gradient payloads: one row per value in a `.bval`, one per column of a `.bvec`,
// keyed by **the gradient file's own** `file_id` — not by the image, which is the whole
// point (docs/adr/0007).
//
// A gradient set inherited from a higher directory level applies to every image below it,
// so there is no single image to key on: `ds114` ships one root `dwi.bval` covering 20
// images. Keying on the file the values were read from is what every other per-row table
// already does, and it makes the foreign key satisfiable — a `.bval` is always a registry
// row under its own path, whereas the image the old code synthesized by swapping the
// extension on the stem often was not. The image-facing shape is the `diffusion` view.
//
// Two tables rather than one with nullable columns, because a `.bval` and a `.bvec` are
// two files: one table each means every row is fully populated and a table's columns are
// exactly what its source file supplies. It also gives each a single association to name,
// which is what keeps the generated re-keying views a plain one-to-one join.
//
// `row_idx`, like every other per-row table: what is stored is the ordinal along the
// file's own axis. That it *means* a volume of the image is a property of the association,
// declared in the ingestion schema and surfaced by the view as `volume_idx`.
pub const CREATE_BVALS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS bvals (
    file_id HUGEINT,
    row_idx BIGINT,
    b DOUBLE,
    PRIMARY KEY (file_id, row_idx),
    FOREIGN KEY (file_id) REFERENCES file_registry(file_id)
);
";

pub const CREATE_BVECS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS bvecs (
    file_id HUGEINT,
    row_idx BIGINT,
    x DOUBLE,
    y DOUBLE,
    z DOUBLE,
    PRIMARY KEY (file_id, row_idx),
    FOREIGN KEY (file_id) REFERENCES file_registry(file_id)
);
";

// The image-facing gradient table: one row per (diffusion image, volume), composed from
// the two views the `describes` declarations generate (`bval_volumes`, `bvec_volumes`),
// each of which re-keys its payload table onto the images that inherit it.
//
// Hand-written rather than generated because it joins *two* generated views, which is one
// composition step beyond what a per-table `describes` block models — and a composition of
// the general mechanism rather than an exception to it.
//
// The zip is decided by what the image inherits, not by filename surgery: a root-level
// `dwi.bval` and a per-image `sub-01_dwi.bvec` reach the same `(file_id, volume_idx)`
// through their own association edges, so a pair split across inheritance levels resolves
// correctly instead of silently annihilating.
//
// `LEFT JOIN` from the b-values, so they define the volume axis — the same rule the old
// row-per-volume writer applied when it NULL-filled a short `.bvec`. A `.bvec` with no
// `.bval` therefore contributes no `diffusion` rows, but unlike before its values are not
// lost: they are in `bvecs`, and keyed by image in `bvec_volumes`.
pub const CREATE_DIFFUSION_VIEW: &str = "
CREATE OR REPLACE VIEW diffusion AS
SELECT v.file_id,
       v.volume_idx,
       v.b AS bval,
       c.x AS bvec_x,
       c.y AS bvec_y,
       c.z AS bvec_z,
       v.source_file_id AS bval_file_id,
       c.source_file_id AS bvec_file_id
FROM bval_volumes v
LEFT JOIN bvec_volumes c USING (file_id, volume_idx);
";

// file_associations is best-effort, import-time derived metadata (e.g. an
// fmap's IntendedFor, or a coordsystem referencing an anatomical). Its source is
// often a sidecar/JSON that is not itself a `scans` row, so we deliberately do
// NOT enforce foreign keys here — doing so would drop otherwise-valid
// associations during import. Targets are resolved to full dataset-relative
// paths so they still join to `scans` when present.
// `target_file_id` is deliberately **nullable**, and `target_file_path` is kept beside it:
// an `IntendedFor` may name a file the dataset does not ship, and dropping those would trade
// this table's keep-everything contract for referential tidiness. A dangling reference stays
// recorded as its raw path with no id. `source_file_id` has no such problem — the source is
// always a file the walk saw, since that is where the reference was read from.
//
// Still no foreign keys: `target_file_id` would need one that tolerates NULL, and the source
// side is already guaranteed by construction.
pub const CREATE_FILE_ASSOCIATIONS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS file_associations (
    source_file_id HUGEINT,
    target_file_id HUGEINT,
    target_file_path TEXT,
    association_type TEXT,
    PRIMARY KEY (source_file_id, target_file_path, association_type)
);
";

// Every ingest root a dataset was built from (see docs/adr/0005). One row for the
// ordinary single-root dataset; N for subject-sharded pipeline output, which is one
// logical dataset with one root per subject.
//
// This is what `dataset_id` no longer has to do. A `file_path` is relative to the
// `root_uri` it was walked from, so the two together address a file: two roots of one
// dataset can hold the same relative path without colliding, and everything
// dataset-scoped (`dataset_description`, `participants`, `dataset_links`,
// `dataset_identity`) stays single.
//
// The root's URI is its own identifier — no separate short label. A label would need a
// rule for deriving one, an escape hatch for when that rule collides (two roots whose
// directories share a basename, `scratch/sub-01/fmriprep` and `scratch/sub-02/fmriprep`),
// and a guarantee it never moves, since it is a stored key that `file_path` resolves
// through. A URI is unique by construction and needs none of that. DuckDB
// dictionary-encodes the column, so repeating it per row costs a code, not a path.
//
// The registry is explicit rather than `SELECT DISTINCT root_uri FROM file_registry`
// because it is authoritative for a root that contributed no rows at all — an ingest that
// found nothing, or whose every file an `ignore` rule claimed.
pub const CREATE_DATASET_ROOTS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS dataset_roots (
    dataset_id TEXT,
    root_uri TEXT,
    PRIMARY KEY (dataset_id, root_uri)
);
";

// Cross-dataset association (see docs/adr/0003). The dataset-to-dataset relation is
// deliberately NOT stored — it is resolved at query time by the `dataset_relations`
// view over these two tables, so the catalog is correct regardless of ingest order
// (a source dataset added later just makes the edge appear on the next query; nothing
// to re-index). Each table stays keyed by a single `dataset_id`, so the crate
// invariant "every table is keyed by dataset_id" holds and each ingest writes only
// its own rows. Like `file_associations`: best-effort, no foreign keys — a declared
// source that is not in the catalog (the usual case for a derivative) is kept, not dropped.

// What a dataset declares it came from: one row per `SourceDatasets` entry, per
// `--source-dataset` flag, and per `DatasetLinks` mapping.
pub const CREATE_DATASET_LINKS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS dataset_links (
    dataset_id TEXT,
    link_type TEXT,
    link_name TEXT,
    declared_ref TEXT,
    identity TEXT,
    identity_kind TEXT,
    identity_base TEXT,
    PRIMARY KEY (dataset_id, link_type, link_name, identity)
);
";

// What identities a dataset *is* (its own `dataset:<id>`, its `DatasetDOI`, its
// `root_uri`), so a source dataset that *is* present in the catalog can be matched
// as the parent of the datasets that declare it.
pub const CREATE_DATASET_IDENTITY_TABLE: &str = "
CREATE TABLE IF NOT EXISTS dataset_identity (
    dataset_id TEXT,
    identity TEXT,
    identity_kind TEXT,
    source TEXT,
    PRIMARY KEY (dataset_id, identity)
);
";

// The dataset-to-dataset relation, resolved at query time. Depth-1 only (no transitive
// closure): `UNION` dedups, `from <> to` drops self-links, so cycles cannot arise.
//   - `shares_source`: two datasets declare the SAME source identity. This needs no
//     `dataset_identity` row, which is why it works when the shared source is absent
//     from the catalog (the ds001761 fMRIPrep/MRIQC case).
//   - `derived_from` / `source_of`: one dataset declares an identity that ANOTHER
//     catalog dataset *is* (its DOI or its `dataset:<id>`).
// `named` links (`DatasetLinks`) are excluded from `shares_source`: they name a
// referenced dataset, not a shared origin.
pub const CREATE_DATASET_RELATIONS_VIEW: &str = "
CREATE OR REPLACE VIEW dataset_relations AS
  SELECT a.dataset_id AS from_dataset_id, b.dataset_id AS to_dataset_id,
         'shares_source' AS relation, a.identity AS via_identity
  FROM dataset_links a JOIN dataset_links b ON a.identity = b.identity
  WHERE a.dataset_id <> b.dataset_id
    AND a.link_type IN ('source', 'declared')
    AND b.link_type IN ('source', 'declared')
  UNION
  SELECT l.dataset_id, i.dataset_id, 'derived_from', l.identity
  FROM dataset_links l JOIN dataset_identity i ON i.identity = l.identity
  WHERE l.dataset_id <> i.dataset_id
  UNION
  SELECT i.dataset_id, l.dataset_id, 'source_of', l.identity
  FROM dataset_links l JOIN dataset_identity i ON i.identity = l.identity
  WHERE l.dataset_id <> i.dataset_id
;
";
