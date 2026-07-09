# Database schema

`bidslake index` consolidates a BIDS dataset's metadata into a small set of
DuckDB tables. Most tables are **generated dynamically** from the vendored BIDS
schema (`src/data/schema.json`), so their columns track the BIDS specification;
`diffusion` and `file_associations` are static (`src/schema.rs`).

Every table is keyed by `dataset_id` so that **multiple datasets can coexist**
in one database and stay isolated.

## Conventions

- **`dataset_id`** — a dataset's identity, taken from the `Name` in its
  `dataset_description.json` (falling back to the directory/prefix name). All
  tables carry it.
- **`file_path`** — a dataset-relative path, e.g.
  `sub-01/func/sub-01_task-x_bold.nii.gz`. This is how imaging files are
  referenced across tables.
- **`other_data JSON`** — an overflow column present on most tables. Any field
  that does not map to a dedicated column is preserved here as JSON, so nothing
  from the source metadata is lost. Fields that *do* have a dedicated column are
  **not** duplicated into `other_data`.
- **Missing values** — BIDS `n/a`, and any non-numeric string in a numeric
  column (e.g. a censored age `89+` or a range `35-40`), are stored as `NULL`.

## Tables

### `dataset_description`
One row per dataset. PK: `dataset_id`. Columns mirror
`dataset_description.json` (`name`, `bids_version`, `license`, `authors`, …) plus
`root_uri` (the `file://` or `s3://` location the data was ingested from) and
`other_data`.

### `participants`
One row per subject. PK: `(dataset_id, participant_id)`. Columns come from the
BIDS participants schema (`age DOUBLE`, `sex`, `handedness`, `species`, …) plus
`other_data`. Populated from `participants.tsv`, and implicitly from `sub-`
entities seen in filenames.

### `sessions`
One row per subject-session. PK: `(dataset_id, session_id, participant_id)`.
Populated from `sessions.tsv` and implicit `ses-` entities.

### `scans`
One row per imaging file. PK: `(dataset_id, file_path)`. Columns from the BIDS
scans schema (`acq_time_scans`, …) plus `other_data`. Populated from
`*_scans.tsv` where present, and augmented so that **every** discovered imaging
file has a row (including files a `scans.tsv` omits, such as derivatives).

**Query by BIDS concept, not by path.** `scans` also carries *generated* columns
derived from `file_path`, so you filter on BIDS concepts directly instead of
`LIKE '%…%'` on paths:

- one column per BIDS **entity** — `sub`, `ses`, `task`, `run`, `acq`, `dir`,
  `echo`, `ce`, `rec`, `space`, `desc`, … — holding the raw entity value
  (`task='rest'`, `run='01'`), or `NULL` when the entity is absent (so `ses` is
  `NULL` for datasets without sessions, and one query spans a mixed pool);
- **`datatype`** — the datatype directory (`func`, `anat`, `dwi`, `fmap`, …);
- **`suffix`** — the file suffix (`bold`, `T1w`, `dwi`, …);
- **`extension`** — the file extension (`.nii.gz`, …);
- **`modality`** — the broad modality (`mri`, `eeg`, `meg`, …).

These are generated from the BIDS schema itself (its `objects.entities`,
`objects.datatypes`, and `rules.modalities`), so they track the spec. They are
DuckDB `GENERATED … VIRTUAL` columns — computed on read, costing nothing at
ingest. Example:

```sql
SELECT dataset_id, sub, ses, run, file_path
FROM scans
WHERE task = 'rest' AND datatype = 'func' AND suffix = 'bold';
```

To filter sidecar metadata by concept, join back to `scans`:
`… FROM sidecars sc JOIN scans s USING (dataset_id, file_path) WHERE s.task='rest'`.
Entity values are raw (`sub='01'`); join to `participants` with
`'sub-' || s.sub = p.participant_id`.

### `sidecars`
The consolidated JSON-sidecar metadata for each imaging file, after applying
**BIDS inheritance** (dataset- and subject-level sidecars merged into the most
specific one; more-specific values win). PK: `(dataset_id, file_path)`,
referencing `scans`. This table is **wide** — it has a column for every metadata
field in the BIDS schema (hundreds of them, e.g. `repetition_time`,
`echo_time`, `flip_angle`) — plus `other_data` for non-standard fields.

### `events`
Task-event rows from `*_events.tsv`. Columns: `dataset_id`, `file_path`,
`onset FLOAT`, `duration FLOAT`, `other_data`. Numeric cells are coerced to
numbers. No primary key (many rows per file).

### `diffusion`
Parsed `.bval` / `.bvec` arrays for each diffusion nifti. PK:
`(dataset_id, file_path)`. Columns: `bval DOUBLE[]`, `bvec_x/…_y/…_z DOUBLE[]`.

### `file_associations`
Best-effort cross-references derived at import time — chiefly an fmap's
`IntendedFor`. Columns: `dataset_id`, `source_file_path`, `target_file_path`,
`association_type` (`fieldmap`, `sbref`, `mask`, `derivative`). PK is all four
columns.

> **No foreign keys** are enforced on this table. Its source is often a sidecar
> JSON that is not itself a `scans` row, so enforcing referential integrity would
> drop otherwise-valid associations during import. `target_file_path` is resolved
> to a full dataset-relative path, so it still joins to `scans` when the target
> is present.

## Relationships

```
dataset_description (dataset_id)
        │
        ├── participants (dataset_id, participant_id)
        │        └── sessions (dataset_id, session_id, participant_id)
        │
        └── scans (dataset_id, file_path)
                 ├── sidecars           (dataset_id, file_path)   FK → scans
                 ├── diffusion          (dataset_id, file_path)
                 ├── events             (dataset_id, file_path)
                 └── file_associations  (…target_file_path → scans.file_path, unenforced)
```

`scans` and `participants` are not linked by an explicit column; a scan belongs
to a participant via its `file_path` prefix (`sub-01/…`). Join them with
`s.file_path LIKE p.participant_id || '/%'`.
