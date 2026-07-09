# bidslake

**A lakehouse for [BIDS](https://bids-specification.readthedocs.io/) datasets — DuckLake for neuroimaging.**

BIDS documents a dataset rigorously, but it scatters that metadata across JSON sidecars, `.tsv` tables, and filename entities. That makes datasets self-describing but painful to *work with*: renaming a participant, or selecting files by some metadata criterion, means touching many files by hand.

bidslake borrows the [DuckLake](https://ducklake.select/manifesto/) insight: **metadata goes in SQL; the bulky data files stay on disk.** It walks a BIDS dataset and consolidates all of that scattered metadata into a single [DuckDB](https://duckdb.org/) database. You then query and edit the dataset with ordinary SQL, while the niftis remain plain files that any neuroimaging tool can read.

## Two ways to use it

1. **Query engine (read-only).** Point bidslake at an existing BIDS dataset and get a DuckDB database. Run SQL to select files, filter by metadata, and audit the dataset. Nothing on disk changes.

2. **Fully-managed (bidslake owns the dataset).** Once ingested, bidslake is the source of truth. All metadata lives in SQL — there are no JSON sidecars or metadata TSVs on disk, just the data files. Editing metadata (renaming a participant, fixing a value) is a plain SQL `UPDATE`; it never touches the files. This mode is under active development.

The vision is for bidslake to *supplant* BIDS as the working format, not to round-trip back to it.

### Managed mode (design)

Managed mode is where bidslake is headed; the notes below are design, and the CLI subcommands for it are stubs that return "not yet implemented".

- **Storage is decoupled from metadata.** A nifti's on-disk path is an opaque storage location bidslake assigns — it does *not* encode `sub-01`, `task-x`, or `run-02`. So metadata edits are pure SQL `UPDATE`s that never move files, and cross-dataset queries/aggregation come for free (many datasets in one database, keyed by `dataset_id`). This is the DuckLake analogy applied to BIDS: opaque data files + a SQL catalog that gives them meaning.
- **Ingestion is one-way.** Standard BIDS → managed store. Exporting back to a standard BIDS layout is an explicit non-goal — the aim is to supplant BIDS, not round-trip.
- **The CLI acts on the store, not the metadata** (metadata is edited with SQL): `index` brings data under management (today's command), `verify` *(stub)* integrity-checks the managed files, and `transcode` *(stub)* changes the on-disk storage format (e.g. `.nii.gz` → `.nii.zst`). A managed database carries a mode marker so destructive operations refuse to run against a read-only index.
- **Beyond BIDS (longer term).** The opaque-files + SQL-catalog model isn't BIDS-specific; a future direction is managing non-BIDS neuroimaging datasets too, supplanting extension efforts such as [BEP043](https://bids.neuroimaging.io/extensions/beps/bep_043.html). The schema/ingestion abstractions avoid hard-coding BIDS-only assumptions with this in mind.

## Install

Requires a Rust toolchain. DuckDB is bundled (no system library needed).

```bash
git clone --recurse-submodules <repo-url>
cd bids2
cargo build --release
```

If you cloned without `--recurse-submodules`, fetch the test corpus with:

```bash
git submodule update --init
```

## Quickstart

Index a dataset into a DuckDB file:

```bash
cargo run --release -- index \
    --input path/to/bids/dataset \
    --output dataset.duckdb
```

The input may also be an S3 URI (`s3://bucket/prefix`); pass `--no-sign-request` for anonymous access to public buckets like OpenNeuro.

Then open it and query:

```bash
duckdb dataset.duckdb
```

```sql
-- Files belonging to participants under 30
SELECT p.participant_id, p.age, s.file_path
FROM participants p
JOIN scans s
  ON s.dataset_id = p.dataset_id
 AND s.file_path LIKE p.participant_id || '/%'
WHERE p.age < 30;
```

## Documentation

Everything else lives in the API docs — build and open them with `cargo doc --open`:

- The **crate page** has worked, **runnable** examples for the common tasks (select files by metadata or BIDS concept, iterate the results into a pipeline, rename a participant, find associated files, query across datasets). Each is a doctest, so `cargo test --doc` runs them and they cannot drift from the code.
- The **`schema` module** is the database reference — every table, its keys, the `other_data` overflow column, and the generated BIDS-concept columns on `scans`.
- Module docs cover the architecture (how the DuckDB schema is generated from the BIDS schema, and the ingestion pipeline).

## Status

Early and unstable; major architectural changes are expected. Ingestion is tested against the official [bids-examples](https://github.com/bids-standard/bids-examples) corpus (vendored as a submodule under `tests/bids-examples`). Run the suite with:

```bash
git submodule update --init
cargo test
```

`cargo test` runs the curated deep tests and unit tests, along with a broad smoke test that ingests *every* dataset in the corpus