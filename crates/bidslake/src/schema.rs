//! The DuckDB schema — the database reference.
//!
//! `bidslake index` consolidates a BIDS dataset's metadata into a small set of
//! DuckDB tables. Most are **generated dynamically** from the vendored BIDS
//! schema — see [`dynamic`] (and [`Schema`]) for that machinery, which is the
//! heart of how bidslake maps BIDS onto SQL. The rest are **static**, their DDL
//! written out in this module: `bvals`/`bvecs` and the `diffusion` view,
//! `file_associations`, `tabular_undeclared_columns`, `dataset_roots`, and the
//! cross-dataset `dataset_links`/`dataset_identity`.
//!
//! Every table is scoped to one `dataset_id`, so multiple datasets coexist in one
//! database and stay isolated while being queried together. The one place datasets
//! are *related* to each other is the `dataset_relations` **view**, resolved at query
//! time (ADR 0003). A dataset may be built from several ingest **roots**
//! (`dataset_roots`, ADR 0005); a file-keyed table reaches its dataset through
//! `file_registry` rather than storing one.
//!
//! Every tabular file (`.tsv`/`.tsv.gz`) a dataset contains is accounted for — event
//! tables, channel/electrode/optode descriptions, motion recordings, blood curves,
//! participants, sessions, diffusion. Which table a file routes to, and that table's
//! columns and types, are **derived from the BIDS schema** (`rules.tabular_data` +
//! `objects.columns`; for the headerless recordings, `rules.sidecars`,
//! `meta.associations` and `rules.files` — see [`recording`]), never hardcoded;
//! [`tabular`] is the routing model. What bidslake then *does* with a file — read it,
//! catalog it unread, ignore it — is the ingestion schema's call (ADR 0002), and
//! `file_registry.status` records the outcome for every file it tried to read.
//!
//! # Conventions
//!
//! - **`dataset_id`** — a dataset's identity, from the `Name` in its
//!   `dataset_description.json` (falling back to the directory/prefix name).
//! - **`file_path`** — a **root**-relative path (`sub-01/func/sub-01_task-x_bold.nii.gz`).
//!   It identifies a file only together with the root it was walked from, which is why
//!   `file_registry` holds `(dataset_id, root_uri, file_path)` and everything else refers
//!   to a file by `file_id`.
//! - **`file_id`** — the surrogate key for a file: the first 128 bits of
//!   `SHA-256(dataset_id ␟ root_uri ␟ file_path)`, stamped RFC 9562 v8 and stored as a
//!   `UUID` (`bids::file_id`).
//!   Content-derived, so it is stable across runs and machines and a re-index upserts onto
//!   the same rows. Every file-keyed table keys on it; `scans`, `sidecars`, `bvals` and
//!   `bvecs` declare a real `FOREIGN KEY`, and the per-row tables and `file_associations`
//!   reference the registry by convention only. ADR 0006 argues the width and the partial
//!   enforcement.
//!
//!   **Which file** a table keys on is one rule (ADR 0003): the file whose rows it holds.
//!   A `*_events.tsv`'s rows key on the events file, a `.bval`'s on the `.bval`; the data
//!   files those rows are *about* are one join away, through `file_associations`. `scans`
//!   and `sidecars` key on the described file instead.
//! - **`other_data JSON`** — the overflow column: any source field without a dedicated
//!   column is preserved here, and a field that *does* have a column is not duplicated
//!   into it. Conditional, not universal — a table whose ingestion policy declares
//!   `undeclared: catalog` has no `other_data` column at all, its undeclared columns stay
//!   in the file on disk, and their names go to `tabular_undeclared_columns` (ADR 0004).
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
//!   `(dataset_id, root_uri)`, plus the [`Tenure`] asserted over each. One row for an
//!   ordinary dataset, N for subject-sharded pipeline output (ADR 0005). `root_uri` +
//!   a root-relative `file_path` is what turns a stored row back into an openable URI.
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
//!     configured (ADR 0002).
//!
//!   Only the files the walk never reached are absent: `.bidsignore`d ones, and ones an
//!   `ingest` rule dropped. It is the foreign-key target for every file-keyed table, which
//!   is only sound because it holds *all* the files (ADR 0006).
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
//!   file's contents — so a dataset shipping no `scans.tsv` has no rows here.
//! - **`sidecars`** — the JSON-sidecar metadata for each imaging file after BIDS
//!   inheritance (dataset-/subject-level sidecars merged, more-specific wins).
//!   PK `file_id` referencing `file_registry` — the *data* file's id, not the sidecar's
//!   (the sidecar has its own registry row, under its own path, as `kind = 'sidecar'`).
//!   Very wide — a column per BIDS metadata field, verbatim-named (`RepetitionTime`,
//!   `EchoTime`, …), plus `other_data`.
//! - **`events`** — task-event rows from `*_events.tsv` (`onset`, `duration`,
//!   `trial_type`, …, `other_data`); one row per line keyed by the `file_id` of the
//!   `*_events.tsv` itself, no primary key, and — alone among the per-row tables —
//!   **no `row_idx`**, because it is the one BIDS table declared unordered. Order
//!   events by `onset`; events sharing one have no tiebreak.
//! - **Per-modality tabular tables** — one per `rules.tabular_data` rule, named
//!   for it: `eeg_channels`/`meg_channels`/…, `eeg_electrodes`/…, `nirs_optodes`,
//!   `blood`, `asl_context`, `behavioral`, `samples`, `phenotype`, `descriptions`,
//!   `segmentation_lookup`. Each has `(file_id, row_idx)` — the `file_id` of the `.tsv`
//!   the rows came from — plus the rule's typed columns and `other_data`. They carry no
//!   concept columns of their own; join `all_files` on `file_id` for those.
//!
//!   **`row_idx` is a plain ordinal reproducing the source file's line order** — not a
//!   key, these tables having no primary key. It is present exactly on the tables the
//!   ingestion schema declares `ordered` (the default; ADR 0002), because a SQL table is
//!   unordered and several BIDS tables carry meaning in their line order and nowhere
//!   else: a `*_channels.tsv`'s order maps onto the columns of the binary recording
//!   beside it, a recording's rows *are* its time axis, a derivative `*timeseries.tsv`
//!   aligns row N with volume N of its 4D image. A table declared *un*ordered has **no
//!   `row_idx` column at all**, its files being read concurrently.
//!
//!   What that order *means* is the `describes.axis` a table may declare, and the
//!   generated view then exposes the same column as `<axis>_idx` — `volume_idx` on
//!   `timeseries`, `bval_volumes`, `asl_volumes` (ADR 0003).
//! - **Continuous recordings** — `physio`, `stim`, `physio_events`, `motion`: one
//!   row per sample, column names from the sidecar `Columns` or the associated
//!   `_channels.tsv`. Only *uncompressed* recordings (chiefly `motion`) are
//!   populated; the compressed `*.tsv.gz` physio/stim files are cataloged rather than
//!   read, a size policy the ingestion schema states, so those tables may be empty.
//! - **`bvals` / `bvecs`** — the gradient payloads, one row per value in a `.bval` and per
//!   column of a `.bvec`, PK `(file_id, row_idx)` where `file_id` is **the gradient file's
//!   own**. One table each, so every row is fully populated and a table's columns are
//!   exactly what its source file supplies.
//! - **`diffusion`** (VIEW) — the image-facing gradient table: one row per (image,
//!   `volume_idx`) with `bval`, `bvec_x/_y/_z`, plus `bval_file_id`/`bvec_file_id` naming
//!   where each half came from. Composed from `bval_volumes` and `bvec_volumes`, the views
//!   the `describes` declarations generate, so one stored copy of an inherited gradient set
//!   reaches every image below it (ADR 0003). The b-values define the volume axis.
//! - **`<describes>` views** — `timeseries` (fMRIPrep confounds), `asl_volumes`,
//!   `bval_volumes`, `bvec_volumes`: a per-row table re-keyed onto the data files its rows
//!   describe, resolved through `file_associations` at query time. Declared by the ingestion
//!   schema's `describes` block, never hand-written, and present only when the adapter that
//!   declares the underlying table is in use.
//! - **`file_associations`** — best-effort cross-references (chiefly an fmap's
//!   `IntendedFor`): `source_file_id`, nullable `target_file_id`, `target_file_path`,
//!   `association_type` (`fieldmap`/`sbref`/`mask`/`derivative`). PK
//!   `(source_file_id, target_file_path, association_type)`. No foreign keys, deliberately:
//!   `target_file_id` is nullable, because an `IntendedFor` may name a file the catalog
//!   does not hold, and such a target keeps its declared path with a NULL id rather than
//!   being dropped (ADR 0003).
//! - **`dataset_links`** — what a dataset declares it came from, one row per
//!   `SourceDatasets` entry / `--source-dataset` flag / `DatasetLinks` mapping:
//!   `link_type`, `link_name`, `declared_ref`, and the canonicalized
//!   `identity`/`identity_kind`/`identity_base` (see `links.rs`).
//! - **`dataset_identity`** — what identities a dataset *is* (`self`, `DatasetDOI`,
//!   `root_uri`), so a present source can be matched as a parent.
//! - **`dataset_relations`** (VIEW) — the resolved dataset-to-dataset relation:
//!   `(from_dataset_id, to_dataset_id, relation, via_identity)` where `relation` is
//!   `shares_source` (co-derivatives of one source), `derived_from`, or `source_of`.
//!   Resolved at query time, so ingest order is irrelevant (ADR 0003).
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
//! `objects.datatypes`, `rules.modalities`) and computed on read, costing nothing at
//! ingest. Defining them on the view — once, rather than on each file-keyed table — is
//! what lets `CREATE OR REPLACE VIEW` redefine them for rows already stored (ADR 0006).
//! See [`dynamic`] for how they're built.
//!
//! A path is not always the source. The registry also holds files a **term map** claimed
//! (`sub-01/mri/wmparc.mgz`), whose names carry almost no BIDS concepts; those store the
//! projection in a `projected JSON` column, and a concept it can supply is computed as
//! `COALESCE(<the projection>, <the path regex>)`, so one `WHERE seg = 'wmparc'` reaches
//! a FreeSurfer volume and a BIDS-named one alike. The column exists only when a term map
//! is configured (ADR 0002).
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
//! rows *describe* are one join away, through `file_associations` (ADR 0003):
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
pub mod recording;
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

/// DDL for `tabular_undeclared_columns`: the column names a table saw but does not
/// declare — one row per distinct name, per table, for the **whole catalog**. It is the
/// one table with no `dataset_id`, so it cannot say which dataset a name came from.
///
/// Populated only for tables whose ingestion policy is `undeclared: catalog`; every other
/// table folds its extra columns into `other_data` and writes nothing here, so an empty
/// table means "no cataloging policy is in use", not "no undeclared columns were seen".
/// Written by [`crate::db::BidsDb::record_undeclared_columns`].
///
/// It answers "what confound regressors does this catalog's data contain?" without opening
/// a file. It deliberately cannot answer *which* file had which — the authoritative per-file
/// column set remains that file's own header, reachable through `file_registry.file_path`.
/// ADR 0004 argues that trade, and why the key is a name rather than a file or a header
/// signature.
pub const CREATE_TABULAR_UNDECLARED_COLUMNS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS tabular_undeclared_columns (
    table_name TEXT,
    name TEXT,
    PRIMARY KEY (table_name, name)
);
";

/// DDL for `bvals`: one row per value in a `.bval`, PK `(file_id, row_idx)`, one `b DOUBLE`
/// payload, and one of the four real `FOREIGN KEY`s in the catalog.
///
/// `file_id` is **the gradient file's own** — not the image's, which is the whole point: an
/// inherited gradient set applies to every image below it, so there is no single image to
/// key on, and a `.bval` is always a registry row under its own path, which is what makes
/// the foreign key satisfiable (ADR 0003). The image-facing shape is
/// [`CREATE_DIFFUSION_VIEW`]; querying `bvals` directly gets you the file's values, with no
/// idea which images inherit them.
///
/// `row_idx` is the ordinal along the *file's own* axis, as on every per-row table. That it
/// also *means* a volume of the image is a property of the association, declared in the
/// ingestion schema and surfaced by the generated `bval_volumes` view as `volume_idx`.
pub const CREATE_BVALS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS bvals (
    file_id UUID,
    row_idx BIGINT,
    b DOUBLE,
    PRIMARY KEY (file_id, row_idx),
    FOREIGN KEY (file_id) REFERENCES file_registry(file_id)
);
";

/// DDL for `bvecs`: one row per **column** of a `.bvec` — the file's three lines transposed
/// into `x`/`y`/`z` — keyed and constrained exactly as [`CREATE_BVALS_TABLE`], whose doc
/// carries the reasoning for keying on the gradient file rather than the image.
///
/// Two tables rather than one with nullable columns, because a `.bval` and a `.bvec` are two
/// files: one table each means every row is fully populated, and each has exactly one
/// association to name, which is what keeps the generated re-keying views (`bval_volumes`,
/// `bvec_volumes`) a plain one-to-one join (ADR 0003).
pub const CREATE_BVECS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS bvecs (
    file_id UUID,
    row_idx BIGINT,
    x DOUBLE,
    y DOUBLE,
    z DOUBLE,
    PRIMARY KEY (file_id, row_idx),
    FOREIGN KEY (file_id) REFERENCES file_registry(file_id)
);
";

/// DDL for the `diffusion` **view**: the image-facing gradient table, one row per (diffusion
/// image, volume), composed from the two views the `describes` declarations generate
/// (`bval_volumes`, `bvec_volumes`), each of which re-keys its payload table onto the images
/// that inherit it. Because it reads generated views rather than tables, it must be executed
/// after them — last of everything, in [`crate::db::BidsDb::create_tables`].
///
/// Hand-written rather than generated because it joins *two* generated views, one
/// composition step beyond what a per-table `describes` block models (ADR 0003).
///
/// The zip is decided by what the image inherits, not by filename surgery: a root-level
/// `dwi.bval` and a per-image `sub-01_dwi.bvec` reach the same `(file_id, volume_idx)`
/// through their own association edges. `bval_file_id`/`bvec_file_id` report which file each
/// half came from, which is the only way to see that they differ.
///
/// `LEFT JOIN` from the b-values, so they define the volume axis and a short `.bvec`
/// NULL-fills. A `.bvec` with no `.bval` therefore contributes **no** `diffusion` rows at
/// all; its values are not lost, though — they are in `bvecs`, and keyed by image in
/// `bvec_volumes`.
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

/// DDL for `file_associations`: best-effort, import-time-derived cross-references (an fmap's
/// `IntendedFor`, a `coordsystem` naming an anatomical, a schema `meta.associations` edge),
/// PK `(source_file_id, target_file_path, association_type)`. What each column holds is
/// documented on the row type, [`crate::db::FileAssociation`].
///
/// **No foreign keys, deliberately** (ADR 0003). `target_file_id` is nullable and
/// `target_file_path` travels beside it, because an `IntendedFor` may name a file the
/// dataset does not ship; such a target stays recorded as its resolved path with a NULL id
/// rather than being dropped, and any key would have to tolerate that NULL. The source side
/// needs none either: it is always a file the walk saw, since that is where the reference
/// was read from.
///
/// The target path is normalized at import, so a target the catalog *does* hold joins to
/// `file_registry` (and through it to `all_files`) on the id, and is findable by path even
/// when the id is NULL and the file arrives in a later run.
pub const CREATE_FILE_ASSOCIATIONS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS file_associations (
    source_file_id UUID,
    target_file_id UUID,
    target_file_path TEXT,
    association_type TEXT,
    PRIMARY KEY (source_file_id, target_file_path, association_type)
);
";

/// DDL for `dataset_roots`: every ingest root a dataset was built from, PK
/// `(dataset_id, root_uri)`, plus the [`Tenure`] asserted over each (ADR 0005). One row for
/// the ordinary single-root dataset; N for subject-sharded pipeline output, which is one
/// logical dataset with one root per subject.
///
/// A `file_path` is relative to the `root_uri` it was walked from, so the two together
/// address a file: two roots of one dataset can hold the same relative path without
/// colliding, and everything dataset-scoped (`dataset_description`, `participants`,
/// `dataset_links`, `dataset_identity`) stays single. The root's URI is its own identifier,
/// with no separate short label to derive, disambiguate and keep stable (ADR 0005).
///
/// Explicit rather than `SELECT DISTINCT root_uri FROM file_registry` because it is
/// authoritative for a root that contributed no rows at all — an ingest that found nothing,
/// or whose every file an `ignore` rule claimed.
///
/// `tenure` says what may be concluded from those rows (ADR 0007), defaulting to `attached`
/// so a row written without one means the weaker claim. Note that only this statement
/// carries the two-value `CHECK`: a column added to an older catalog by
/// [`ADD_DATASET_ROOTS_TENURE`] is unconstrained, which is why the read path tolerates an
/// unexpected token.
pub const CREATE_DATASET_ROOTS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS dataset_roots (
    dataset_id TEXT,
    root_uri TEXT,
    tenure TEXT NOT NULL DEFAULT 'attached'
        CHECK (tenure IN ('attached', 'managed')),
    PRIMARY KEY (dataset_id, root_uri)
);
";

/// Bring a pre-tenure `dataset_roots` up to date (docs/adr/0007).
///
/// Idempotent, and applied on every `create_tables` so a catalog indexed before this column
/// existed keeps working rather than failing on the first read of `tenure`. The `DEFAULT` is
/// what makes the backfill correct as well as cheap: a root registered before tenure existed
/// promised only that its files were there, which is exactly `attached`.
pub const ADD_DATASET_ROOTS_TENURE: &str =
    "ALTER TABLE dataset_roots ADD COLUMN IF NOT EXISTS tenure TEXT DEFAULT 'attached'";

/// What was promised about a root, and so what the catalog may conclude from its rows
/// (ADR 0007).
///
/// Both tiers are permanent: `Attached` is not a waypoint on the way to `Managed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tenure {
    /// Somebody else writes there. Indexing it promised only that the files will stay put, so
    /// a row here is a record of a past observation: enough to *find* work, not enough to
    /// *skip* it without confirming. [`crate::verify`] is what audits that promise.
    #[default]
    Attached,
    /// bidslake owns the storage, so the catalog is the commit point and its rows are current
    /// by construction. This is what unlocks the verbs that move or rewrite files.
    Managed,
}

impl Tenure {
    /// The token this tenure is stored as in `dataset_roots.tenure` — `"attached"` or
    /// `"managed"`, the only two values [`CREATE_DATASET_ROOTS_TABLE`]'s `CHECK` admits.
    ///
    /// The write half of the round trip only, and lossless in that direction. The read half,
    /// [`crate::db::BidsDb::dataset_root_tenure`], is asymmetric on purpose: it recognizes
    /// `"managed"` and reads *anything else* — including a token from an older or a
    /// hand-edited catalog, whose column the `CHECK` never covered — as [`Tenure::Attached`],
    /// so an unrecognized value degrades to the weaker claim instead of failing the query.
    pub fn as_str(self) -> &'static str {
        match self {
            Tenure::Attached => "attached",
            Tenure::Managed => "managed",
        }
    }
}

// Cross-dataset association (ADR 0003). The dataset-to-dataset relation is deliberately
// NOT stored — it is resolved at query time by the `dataset_relations` view over these two
// tables. Each table stays keyed by a single `dataset_id`, so each ingest writes only its
// own rows, and neither takes foreign keys: a declared source absent from the catalog, the
// usual case for a derivative, is kept rather than dropped.

/// DDL for `dataset_links`: what a dataset declares about other datasets. One row per
/// `SourceDatasets` entry, per `--source-dataset` flag, per `DatasetLinks` mapping, and per
/// `bidslake link alias`; PK `(dataset_id, link_type, link_name, identity)`. `declared_ref`
/// keeps the reference verbatim as it was written, for reporting; the canonicalized
/// `identity`/`identity_kind`/`identity_base` beside it are [`crate::links::Identity`]'s three
/// fields, and string equality on `identity` alone is the entire matching mechanism.
///
/// The table holds two **different kinds of statement**, distinguished by `link_type`, and
/// they are worth naming because only one of them is provenance:
///
/// ```text
///   PROVENANCE  "this dataset CAME FROM S"     -> feeds `dataset_relations`
///   NAMING      "here, the name N REFERS TO L" -> feeds `dataset_link_targets`
/// ```
///
/// Crossed with where the statement came from, the four `link_type` values are a 2x2:
///
/// ```text
///              | derived from dataset_description.json | asserted by the user |
///   provenance | 'source'   (SourceDatasets)           | 'declared'           |
///   naming     | 'named'    (DatasetLinks)             | 'alias'              |
/// ```
///
/// [`crate::db::BidsDb::clear_derived_links`] deletes exactly the left column on every
/// re-index, so the derived rows track the file while the user's survive. That is why
/// `alias` is grouped with `declared` rather than being given a rule of its own.
///
/// A naming link never produces a provenance edge: every arm of
/// [`CREATE_DATASET_RELATIONS_VIEW`] filters both naming types out (ADR 0003).
///
/// `link_name` is empty for a provenance link, which has no name to carry; the naming view
/// filters those rows out on exactly that.
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

/// DDL for `dataset_identity`: the identities a dataset *is*, PK `(dataset_id, identity)`,
/// so a source dataset that **is** present in the catalog can be matched as the parent of the
/// datasets that declare it. The mirror of [`CREATE_DATASET_LINKS_TABLE`], which records what
/// a dataset points *at*, in the same canonicalized [`crate::links::Identity`] vocabulary.
///
/// `source` says which fact produced the row, and takes exactly three values: `self` (the
/// dataset's own `dataset:<id>`), `root_uri` (one row per registered root), and `DatasetDOI`.
/// The first two are unconditional — even a dataset ingested through a layout adapter, with no
/// `dataset_description.json` at all, is identifiable — while `DatasetDOI` appears only when
/// the root description declares one. Every row is derived, and
/// [`crate::db::BidsDb::clear_derived_links`] drops the lot on each re-index before they are
/// rewritten, which is why the roots are read back from `dataset_roots` rather than from the
/// run in progress.
///
/// A missing row is not an error: an absent source dataset simply has no identity here, so
/// `derived_from`/`source_of` cannot fire for it while `shares_source` still can.
pub const CREATE_DATASET_IDENTITY_TABLE: &str = "
CREATE TABLE IF NOT EXISTS dataset_identity (
    dataset_id TEXT,
    identity TEXT,
    identity_kind TEXT,
    source TEXT,
    PRIMARY KEY (dataset_id, identity)
);
";

/// DDL for the `dataset_relations` **view**: the dataset-to-dataset relation, resolved at
/// query time over [`CREATE_DATASET_LINKS_TABLE`] and [`CREATE_DATASET_IDENTITY_TABLE`] rather
/// than stored, so ingest order is irrelevant and a source indexed later makes its edges
/// appear on the next query with nothing to re-index (ADR 0003).
///
/// Depth-1 only — no transitive closure, so a grandparent is two queries away. `UNION` dedups
/// and `from <> to` drops self-links, so cycles cannot arise. Three relations:
///
/// - `shares_source`: two datasets declare the **same** source identity. This arm reads no
///   `dataset_identity` row, which is why it still works when the shared source is absent
///   from the catalog — the ds001761 fMRIPrep/MRIQC case, where the two derivatives are held
///   but their common parent is not.
/// - `derived_from` / `source_of`: one dataset declares an identity that **another** catalog
///   dataset *is* (its DOI or its `dataset:<id>`). Emitted as a symmetric pair, so a caller
///   may filter on either direction without a second query.
///
/// Every arm reads provenance links only (`source`, `declared`). `named`/`alias` are naming
/// statements — "here, this word refers to that dataset" — and a reference is not a
/// derivation (ADR 0003). Naming is resolved separately, by
/// [`CREATE_DATASET_LINK_TARGETS_VIEW`].
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
    AND l.link_type IN ('source', 'declared')
  UNION
  SELECT i.dataset_id, l.dataset_id, 'source_of', l.identity
  FROM dataset_links l JOIN dataset_identity i ON i.identity = l.identity
  WHERE l.dataset_id <> i.dataset_id
    AND l.link_type IN ('source', 'declared')
;
";

/// DDL for the `dataset_link_targets` **view**: the naming half of
/// [`CREATE_DATASET_LINKS_TABLE`] — what a *name* refers to — resolved at query time.
///
/// [`CREATE_DATASET_RELATIONS_VIEW`] answers "where did this dataset come from"; this answers
/// "in this dataset, which catalog dataset does the name `freesurfer` mean". A consumer names
/// the link and the catalog supplies the id, so a query is portable across catalogs whose
/// dataset ids differ (ADR 0003). A view rather than a stored column for the same reason the
/// relations view is one: a resolved target is a cache, and the catalog grows.
///
/// `LEFT JOIN`, so `target_dataset_id` is NULL when nothing in the catalog holds the
/// identity — which is how a caller tells "you never indexed that" from "you misspelled the
/// name": the row is present either way, the target is not. A `DatasetLinks` value written as
/// a path relative to the dataset root only resolves because the identity was canonicalized
/// against that root at ingest (see [`crate::links::canonicalize_relative_to`]).
///
/// Self-references are kept, unlike in the relations view: a dataset *naming* itself is the
/// only way a query scoped by name can mean "my own dataset".
pub const CREATE_DATASET_LINK_TARGETS_VIEW: &str = "
CREATE OR REPLACE VIEW dataset_link_targets AS
  SELECT l.dataset_id AS from_dataset_id,
         l.link_name,
         i.dataset_id AS target_dataset_id,
         l.link_type,
         l.declared_ref,
         l.identity AS via_identity
  FROM dataset_links l
  LEFT JOIN dataset_identity i ON i.identity = l.identity
  WHERE l.link_type IN ('named', 'alias')
    AND l.link_name <> ''
;
";
