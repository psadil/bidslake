# bidslake

**A lakehouse for [BIDS](https://bids-specification.readthedocs.io/) datasets — DuckLake for neuroimaging.**

BIDS documents a dataset rigorously, but it scatters that metadata across JSON sidecars, `.tsv` tables, and filename entities. That makes datasets self-describing but painful to *work with*: renaming a participant, or selecting files by some metadata criterion, means touching many files by hand.

bidslake borrows the [DuckLake](https://ducklake.select/manifesto/) insight: **metadata goes in SQL; the bulky data files stay on disk.** It walks a BIDS dataset and consolidates all of that scattered metadata into a single [DuckDB](https://duckdb.org/) database. You then query and edit the dataset with ordinary SQL, while the niftis remain plain files that any neuroimaging tool can read.

## Two ways to use it

1. **Query engine (read-only).** Point bidslake at an existing BIDS dataset and get a DuckDB database. Run SQL to select files, filter by metadata, and audit the dataset. Nothing on disk changes.

2. **Fully-managed (bidslake owns the dataset).** Once ingested, bidslake is the source of truth. All metadata lives in SQL — there are no JSON sidecars or metadata TSVs on disk, just the data files. Editing metadata (renaming a participant, fixing a value) is a plain SQL `UPDATE`; it never touches the files. This mode is under active development, and its subcommands are stubs; the design is in the book's [roadmap](../../docs/roadmap.md).

The vision is for bidslake to *supplant* BIDS as the working format, not to round-trip back to it.

Which of the two applies is a property of each ingest root, not of the database: a root is `attached` by default (somebody else writes there, bidslake only reads) or `managed` (`index --managed`), and destructive verbs refuse to run against an attached root ([ADR 0007](../../docs/adr/0007-root-tenure.md)).

## Install

Requires a Rust toolchain. DuckDB is bundled (no system library needed).

```bash
git clone --recurse-submodules <repo-url>
cd bidslake
cargo build --release
```

If you cloned without `--recurse-submodules`, fetch the test corpus with:

```bash
git submodule update --init
```

### Building with S3

Reading datasets from `s3://` is the `s3` feature, off by default: the AWS SDK behind
it is most of the dependency tree, and none of it runs on the local path. Opt in to
index straight from S3:

```bash
cargo build --release --features s3
```

DuckDB's httpfs extension, which is what actually reads `s3://` tabular files, is part
of the bundled engine either way. Without the feature, an `s3://` input is refused with
an explanation rather than mistaken for a directory name.

## Quickstart

Index a dataset into a DuckDB file:

```bash
cargo run --release -- index \
    --input path/to/bids/dataset \
    --output dataset.duckdb
```

With the `s3` feature (see above), the input may also be an S3 URI (`s3://bucket/prefix`); pass `--no-sign-request` for anonymous access to public buckets like OpenNeuro. S3 ingest is full-fidelity: object listing and JSON metadata go through the AWS SDK, and `.tsv` contents stream straight into DuckDB via its `httpfs` extension — so working with a dataset on S3 is the same as one on local disk. (The first S3 ingest runs `INSTALL httpfs`, which needs network; the region comes from `AWS_REGION`, default `us-east-1`.)

Then open it and query:

```bash
duckdb dataset.duckdb
```

```sql
-- Files belonging to participants under 30
SELECT p.participant_id, p.age, f.file_path
FROM participants p
JOIN all_files f
  ON f.dataset_id = p.dataset_id
 AND f.extension = '.nii.gz'
 AND f.file_path LIKE p.participant_id || '/%'
WHERE p.age < 30;
```

`all_files` is the file registry: one row per file the walk saw, with its BIDS concepts
(`sub`, `ses`, `task`, `datatype`, `suffix`, …) derived from the path. Everything else that
is about a file — its sidecar metadata, its events, its channels — keys on `file_id` and
joins back to it ([ADR 0006](../../docs/adr/0006-file-registry.md)).

## Tabular data is in the database

BIDS keeps a surprising amount of information in `.tsv` tables — event timings, channel and electrode descriptions, physiological and motion recordings, blood curves, participant and session variables, diffusion b-values. bidslake treats **all of it as a first-class, tracked invariant**:

> Every tabular file a dataset contains is accounted for. Header-bearing tables are ingested into the database; large compressed recordings (`*.tsv.gz`) are, for now, left on disk (a size policy — see the [roadmap](../../docs/roadmap.md)) but still recorded. Files excluded by `.bidsignore` are never read; a tabular file the BIDS schema does not describe is skipped with a warning and recorded — never silently dropped.

Accounted for is not the same as *stored verbatim*. What a table stores is what the schema declares it stores; a table may be configured to leave undeclared columns in the file rather than in the database, in which case the file — still on disk, still in the file registry — is the record of them.

The tables and their columns are **derived from the BIDS schema** (`rules.tabular_data`, `objects.columns`, and — for the headerless recordings — `rules.sidecars` and `meta.associations`), not hardcoded. Each modality gets its own table (`eeg_channels`, `meg_channels`, `blood`, `physio`, …); uncompressed continuous recordings (chiefly `motion`) are stored one row per sample, with their column names taken from the sidecar `Columns` field or the associated `_channels.tsv`. The file registry records every file with a `status` (`ingested` / `on_disk` / `skipped` / `failed`, or NULL for a file there was nothing to read), and a test asserts nothing is silently dropped (`tests/tabular_coverage.rs` — `#[ignore]`d because it ingests the whole corpus, so run it with `cargo test -- --ignored`).

*What bidslake does* with each file — read its contents, catalog it unread, or ignore it — is a separate, equally declarative layer: the **ingestion schema** ([ADR 0002](../../docs/adr/0002-adapters-and-layouts.md)), a bidslake-specific document whose rules select over projected BIDS concepts and are validated against their own metaschema. That is what routes `.bval`/`.bvec` to the diffusion reader and leaves `*.tsv.gz` on disk, and it is where per-table policy lives (row ordering, materialized concepts, and whether a table stores columns the schema does not declare — [ADR 0004](../../docs/adr/0004-undeclared-column-policy.md)).

## Adapters: indexing what BIDS does not describe

A great deal of real data is *standardized but not BIDS*: FreeSurfer `recon-all`, an FSL
FEAT tree. Their files carry no BIDS entities in their names — `stats/aseg.stats` is
identified by its position in the tree — so no amount of added vocabulary reaches them.

An **adapter** is what does. `--adapter freesurfer` resolves whatever bidslake bundles under
that name: an overlay (vocabulary), a BEP-043 **term map** projecting a path onto BIDS
concepts, and an ingestion fragment (read/catalog/ignore). Bundled today: `fmriprep`,
`mriqc`, `qsiprep`, `freesurfer`, `feat`, `dcmstack`.

```bash
bidslake index --input <study>/derivatives/freesurfer --output study.duckdb \
    --dataset-id freesurfer --adapter freesurfer
```

The projection is stored, so those files answer concept queries rather than only path
matches — `WHERE seg = 'wmparc'` reaches a recon-all volume and a BIDS-named one alike.

Not every adapter is a pipeline. `dcmstack` names a *converter convention*: dcmstack's
DcmMeta extension attaches per-slice DICOM dumps to an otherwise ordinary sidecar under
`global` and `time`, which on a real study runs to megabytes per sidecar. Those keys
describe the conversion, not the data, and nothing queries them, so its fragment declares
them `ignoreKeys` and they are dropped as the sidecar is parsed:

```bash
bidslake index --input <study>/rawdata --output study.duckdb --adapter dcmstack
```

The saving is the whole reason the dial exists: measured 2026-08 on a 1,800-scan tree whose
`_bold.json` sidecars ran to ~3.6 MB each, dropping those two keys took the catalog from
**2.8 GB to 8 MB**. `ignoreKeys` is deliberately narrower than the table-wide
`undeclared: catalog` dial, which would have discarded every other custom field along with
these two.

Datasets accumulate in one catalog, and a dataset may itself span several ingest roots: naming
the same `--dataset-id` on a later run adds a root rather than being refused
([ADR 0005](../../docs/adr/0005-multi-root-datasets.md)).

**Name every adapter the catalog uses on every index run, not only the one for the dataset being
added** — a term map's `projected` column is physical, so a catalog first built without it cannot
gain it later ([ADR 0006](../../docs/adr/0006-file-registry.md)). Do that and run order stops
mattering.

Adding another adapter is authoring those documents, not writing an ingester — see
[ADR 0002](../../docs/adr/0002-adapters-and-layouts.md). The write direction — naming a file a
pipeline has not produced yet — is a fourth, separate artifact, the **layout**
([ADR 0002](../../docs/adr/0002-adapters-and-layouts.md)).

## Documentation

Everything else lives in the API docs — build and open them with `cargo doc --open`:

- The **crate page** has worked, **runnable** examples for the common tasks (select files by metadata or BIDS concept, iterate the results into a pipeline, rename a participant, find associated files, query across datasets). Each is a doctest, so `cargo test --doc` runs them and they cannot drift from the code.
- The **`schema` module** is the database reference — every table, its keys, the `other_data` overflow column, and the BIDS-concept columns on `all_files`.
- Module docs cover the architecture (how the DuckDB schema is generated from the BIDS schema, and the ingestion pipeline).

## Status

Early and unstable; major architectural changes are expected. Ingestion is tested against the official [bids-examples](https://github.com/bids-standard/bids-examples) corpus (a submodule at `third_party/bids-examples`, reached from the crate as `tests/bids-examples`). Run the suite with:

```bash
git submodule update --init
cargo test
```

`cargo test` runs the curated deep tests and the unit tests. The two whole-corpus tests — the broad smoke test that ingests *every* dataset, and the tabular-coverage invariant — take minutes, so they are `#[ignore]`d and excluded from the default run (and from CI):

```bash
cargo test -- --ignored
```

## Roadmap

What is not settled — where large tabular data should live, managed mode, reclaiming space with
`bidslake compact` after a re-index, and the remaining S3 gaps — is in the book:
[docs/roadmap.md](../../docs/roadmap.md).
