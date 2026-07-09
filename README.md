# bidslake

**A lakehouse for [BIDS](https://bids-specification.readthedocs.io/) datasets — DuckLake for neuroimaging.**

BIDS documents a dataset rigorously, but it scatters that metadata across JSON
sidecars, `.tsv` tables, and filename entities. That makes datasets self-describing
but painful to *work with*: renaming a participant, or selecting files by some
metadata criterion, means touching many files by hand.

bidslake borrows the [DuckLake](https://ducklake.select/manifesto/) insight:
**metadata goes in SQL; the bulky data files stay on disk.** It walks a BIDS
dataset and consolidates all of that scattered metadata into a single
[DuckDB](https://duckdb.org/) database. You then query and edit the dataset with
ordinary SQL, while the niftis remain plain files that any neuroimaging tool can
read.

## Two ways to use it

1. **Query engine (read-only).** Point bidslake at an existing BIDS dataset and
   get a DuckDB database. Run SQL to select files, filter by metadata, and audit
   the dataset. Nothing on disk changes.

2. **Fully-managed (bidslake owns the dataset).** Once ingested, bidslake is the
   source of truth. All metadata lives in SQL — there are no JSON sidecars or
   metadata TSVs on disk, just the data files. Editing metadata (renaming a
   participant, fixing a value) is a plain SQL `UPDATE`; it never touches the
   files. This mode is under active development — see
   [docs/managed-mode.md](docs/managed-mode.md).

The vision is for bidslake to *supplant* BIDS as the working format, not to
round-trip back to it.

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

The input may also be an S3 URI (`s3://bucket/prefix`); pass `--no-sign-request`
for anonymous access to public buckets like OpenNeuro.

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

See [docs/workflow.md](docs/workflow.md) for a full walkthrough (selecting files
by metadata, renaming participants, inspecting sidecar metadata) and
[docs/schema.md](docs/schema.md) for the table reference.

## Documentation

- [docs/schema.md](docs/schema.md) — the database schema (tables, keys, the
  `other_data` overflow column).
- [docs/workflow.md](docs/workflow.md) — worked examples of common tasks in SQL.
- [docs/managed-mode.md](docs/managed-mode.md) — the fully-managed design.

## Status

Early and unstable; major architectural changes are expected. Ingestion is
tested against the official
[bids-examples](https://github.com/bids-standard/bids-examples) corpus (vendored
as a submodule under `tests/bids-examples`). Run the suite with:

```bash
git submodule update --init
cargo test
```

`cargo test` runs the curated deep tests and unit tests. The broad smoke test
that ingests *every* dataset in the corpus is slow and marked `#[ignore]`; run it
explicitly with:

```bash
cargo test --test smoke_bids_examples -- --ignored --nocapture
```
