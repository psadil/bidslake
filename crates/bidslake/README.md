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
- **Beyond BIDS.** The opaque-files + SQL-catalog model isn't BIDS-specific, and the read-only half already works: an **adapter** supplies a vocabulary overlay, a [BEP043](https://bids.neuroimaging.io/extensions/beps/bep_043.html) term map projecting paths onto BIDS concepts, and an ingestion policy — so a recon-all or FEAT tree is queryable by `sub`/`seg`/`suffix` beside the BIDS data it came from ([ADR 0002](../../docs/adr/0002-layout-adapters.md)). bidslake *consumes* BEP043 rather than replacing it. What is still ahead is bringing such trees under *management*, where bidslake owns their storage rather than only reading it.

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

### Building without S3

Reading datasets from `s3://` is the `s3` feature, on by default. If you only ever
index local datasets, turning it off drops the AWS SDK — most of the dependency tree,
and none of it on the local path:

```bash
cargo build --release --no-default-features
```

Everything else is unchanged; DuckDB's httpfs extension, which is what actually reads
`s3://` tabular files, is part of the bundled engine either way. An `s3://` input is then
refused with an explanation rather than mistaken for a directory name.

## Quickstart

Index a dataset into a DuckDB file:

```bash
cargo run --release -- index \
    --input path/to/bids/dataset \
    --output dataset.duckdb
```

The input may also be an S3 URI (`s3://bucket/prefix`); pass `--no-sign-request` for anonymous access to public buckets like OpenNeuro. S3 ingest is full-fidelity: object listing and JSON metadata go through the AWS SDK, and `.tsv` contents stream straight into DuckDB via its `httpfs` extension — so working with a dataset on S3 is the same as one on local disk. (The first S3 ingest runs `INSTALL httpfs`, which needs network; the region comes from `AWS_REGION`, default `us-east-1`.)

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

## Tabular data is in the database

BIDS keeps a surprising amount of information in `.tsv` tables — event timings, channel and electrode descriptions, physiological and motion recordings, blood curves, participant and session variables, diffusion b-values. bidslake treats **all of it as a first-class, tracked invariant**:

> Every tabular file a dataset contains is accounted for. Header-bearing tables are ingested into the database; large compressed recordings (`*.tsv.gz`) are, for now, left on disk (a size policy — see the roadmap) but still recorded. Files excluded by `.bidsignore` are never read; a tabular file the BIDS schema does not describe is skipped with a warning and recorded — never silently dropped.

Accounted for is not the same as *stored verbatim*. What a table stores is what the schema declares it stores; a table may be configured to leave undeclared columns in the file rather than in the database, in which case the file — still on disk, still in `tabular_files` — is the record of them.

The tables and their columns are **derived from the BIDS schema** (`rules.tabular_data`, `objects.columns`, and — for the headerless recordings — `rules.sidecars` and `meta.associations`), not hardcoded. Each modality gets its own table (`eeg_channels`, `meg_channels`, `blood`, `physio`, …); uncompressed continuous recordings (chiefly `motion`) are stored one row per sample, with their column names taken from the sidecar `Columns` field or the associated `_channels.tsv`. A provenance table, `tabular_files`, records every tabular file with a `status` (`ingested` / `on_disk` / `skipped` / `failed`) and the table it maps to, and a test asserts nothing is silently dropped (`tests/tabular_coverage.rs` — `#[ignore]`d because it ingests the whole corpus, so run it with `cargo test -- --ignored`).

*What bidslake does* with each file — read its contents, catalog it unread, or ignore it — is a separate, equally declarative layer: the **ingestion schema** ([ADR 0002](../../docs/adr/0002-layout-adapters.md)), a bidslake-specific document whose rules select over projected BIDS concepts and are validated against their own metaschema. That is what routes `.bval`/`.bvec` to the diffusion reader and leaves `*.tsv.gz` on disk, and it is where per-table policy lives (row ordering, materialized concepts, and whether a table stores columns the schema does not declare — see the roadmap).

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

Datasets accumulate in one catalog, with one constraint: `scans` is created once and keeps
the shape of the run that created it, so **the adapter set describes the catalog, not the
dataset being added**. Name every adapter the catalog uses on every run and order stops
mattering; a run that would need a concept column `scans` lacks is refused rather than
silently dropping it.

Adding another layout is authoring those documents, not writing an ingester — see
[ADR 0002](../../docs/adr/0002-layout-adapters.md).

## Documentation

Everything else lives in the API docs — build and open them with `cargo doc --open`:

- The **crate page** has worked, **runnable** examples for the common tasks (select files by metadata or BIDS concept, iterate the results into a pipeline, rename a participant, find associated files, query across datasets). Each is a doctest, so `cargo test --doc` runs them and they cannot drift from the code.
- The **`schema` module** is the database reference — every table, its keys, the `other_data` overflow column, and the generated BIDS-concept columns on `scans`.
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

**Where large tabular data lives.** High-rate continuous recordings do not belong in the catalog as-is: stored one row per sample, a single `*_physio` recording can run to millions of rows, dwarfing the metadata it accompanies.

The **mechanism** for this is settled. [ADR 0002](../../docs/adr/0002-layout-adapters.md) replaced bidslake's hardcoded read-vs-catalog logic with the ingestion schema: selector-driven `read` / `catalog` / `ignore` dispositions, metaschema-validated, with per-table policy alongside. Nothing about deciding what to ingest requires new machinery — it requires writing rules.

What remains crude is the **criterion**. Two levers exist, at different granularities:

- *Whole file* — one rule, `extension == ".tsv.gz"` → `catalog`, is all that stands between a catalog and two million rows of physio. It is a proxy, not a principle: it happens to catch the physio/stim recordings because BIDS always compresses them. The files stay recorded in `tabular_files` (`status = 'on_disk'`), tracked and findable, just unread. Candidate replacements — row count, byte size, sampling rate from the sidecar, the BIDS suffix — all have failure modes, and the choice interacts with the (stubbed) `transcode` and `verify` commands.
- *Per column* — `undeclared: "catalog"` in a table's policy, which stores the columns a table declares and leaves the rest in the file. This is what keeps fMRIPrep confounds tractable: the file has ~1,800 columns, the schema declares ~13, and storing the remainder as per-row JSON cost 24 MB of database per confounds file. See [ADR 0004](../../docs/adr/0004-undeclared-column-policy.md).

In managed mode the likely answer for the whole-file case is the DuckLake split applied one level down: small tabular data stays in the catalog, while large continuous recordings are written to partitioned Parquet (or [Vortex](https://vortex.dev/)) files on disk and exposed as views, so SQL still sees one table.

**Reclaiming space after a re-index.** Re-indexing a dataset deletes its rows and re-inserts them; DuckDB reuses the vacated blocks for later writes but never returns them to the OS, and `CHECKPOINT` does not shrink the file. A catalog that has been re-indexed a few times can be substantially holes — the one that motivated ADR 0004 was 28% free blocks, 478 MB of a 1.74 GB file. `bidslake compact` rewrites it:

```bash
bidslake compact -d study.duckdb
```

It preserves every table, row, key, constraint, and view, verifies per-table row counts before replacing the original, and reports what it reclaimed. An index run that leaves a substantial fraction free says so rather than waiting to be discovered. It is deliberately not automatic: a rewrite transiently needs twice the disk, and a first index has nothing to reclaim.

**S3.** Ingesting a dataset straight from S3 works end-to-end: object listing and JSON metadata via the AWS SDK, `.tsv` contents streamed into DuckDB via the `httpfs` extension (`read_csv` opens `s3://` directly), and Rust-side reads (JSON sidecars, `.bval`/`.bvec`) issued concurrently to overlap network latency. Remaining gaps are minor: dataset-embedded overlay auto-discovery (`.bidslake/overlay.json`) is skipped for remote inputs, and the S3 integration tests are network-gated (`#[ignore]`, run with `cargo test --test s3_ingest -- --ignored`).