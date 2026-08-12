# ADR 0004 — Storage is a policy, not an invariant

Status: accepted (2026-07-30)

Relates to: the ingestion schema (ADR 0002 §4/§6), the `other_data` overflow column
(`schema.rs`), and the tabular ingest (`bids.rs`).

## Context

`other_data` has been in bidslake since the initial commit, documented as an invariant:
*"an overflow column on most tables. Any source field without a dedicated column is
preserved here, so nothing is lost."* No ADR ever revisited it.

Profiling a real catalog — 12 fMRIPrep derivative datasets, 2 subjects, 48 confounds
files — showed what that costs:

| | size | share of used space |
|---|---|---|
| `fmriprep_confounds.other_data` | **1,160 MB** | **97.3%** |
| `sidecars.other_data` (48 confounds sidecars, 366 KB each) | 16.8 MB | 1.4% |
| everything else (40 tables) | ~19 MB | 1.3% |

The whole catalog held ~70,000 rows and was 1.74 GB.

fMRIPrep confounds TSVs carry ~1,800 columns; the fmriprep overlay declares 13. The
other ~1,785 — mostly CompCor components — went into `other_data` as a per-row JSON
object of ~56 KB, of which **53% was repeated key names**: the column list
re-serialized once per volume, where the source TSV writes its header once. DuckDB
stores the column `Uncompressed`, because at 56 KB every value is an overflow string
and dictionary/FSST encoding never applies.

At ~24 MB of database per confounds file, the projection to a large study is the whole
problem: it scales linearly in confounds files, which is participants × sessions × runs, so
a real multi-session study reaches terabytes from confounds alone.

### Encoding is not the fix

Four alternatives were measured on the real table (21,332 rows):

| | table size | vs today |
|---|---|---|
| today — JSON `other_data` | 1,160 MB | — |
| `MAP(VARCHAR, DOUBLE)` | 267 MB | 4.3× |
| long/EAV `(file, row, name, value)` | ~350 MB | 3.3× |
| all ~1,800 as real `DOUBLE` columns | ~280 MB | 4.1× |

They cluster around what 38M float values cost to store — 7.0–7.4 bytes per value across
the three alternatives, against 8 bytes raw, so DuckDB is compressing them a little but
there is no encoding that makes the data small. **Encoding
buys a constant factor; it does not change the asymptote.** Only declining to store the
values does — and that is a policy question, not a representation question.

### The mechanism already existed; the granularity did not

ADR 0002 §6 made read-vs-catalog declarative: `disposition: read | catalog | ignore`,
selector-addressed, metaschema-validated, with `*.tsv.gz` → `catalog` as the working
precedent for not ingesting something. That machinery is sound.

What it could not express is a *per-column* disposition. A confounds file needs `read`
(the motion parameters, FD, DVARS are the point) *and* needs its ~1,785 undeclared
columns not stored. The existing model could only say "read all of it" or "read none of
it".

## Decision

**1. `other_data` becomes conditional.** A table's ingestion policy may declare
`undeclared: catalog`, and such a table has no `other_data` column at all. The
invariant is restated: *a table stores what its schema declares it stores.* What a
catalog holds is a decision its schema makes, not a property of the file it read.

Omitting the column rather than nulling it is load-bearing: `Schema::row_values`
iterates `table_columns`, so with no such field there is no branch to populate, and the
Rust producer follows with no code of its own.

**2. Losslessness by reference, not by value.** The file stays on disk and stays in
`tabular_files`, whose `file_path` is the authoritative record of its full column set —
one line of I/O away. Declared-ness becomes the storage dial rather than a wall: a user
who wants `a_comp_cor_00` declares it in the overlay and pays 8 B/row.

**3. Discovery via a global name dictionary, not a per-file manifest.** This is the part that
had to be measured rather than assumed. Keying a manifest by header signature looked free —
until the data showed that confounds headers are *not* shared between files: 48 files produced
**38 distinct headers**, because the aCompCor component count varies per run. So a per-header
manifest grows with the study, at roughly the size of one header per file. The *names*, by
contrast, come from one shared space bounded by the pipeline's vocabulary rather than by
dataset size — a few thousand distinct names, tens of kilobytes, however many files there are.
So `tabular_undeclared_columns` records `(table_name, name)` and nothing per-file.

**4. Two forms, because `sidecars` is one table for every file in the catalog.**
`undeclared` is table-wide and static, so it can drive DDL. `undeclaredWhen` is
selector-scoped and per-row, and does *not* change the table's shape. A table-wide flag
on `sidecars` would strip custom metadata from all raw BIDS to reach one derivative's
sidecars; the scoped form is what lets a 366 KB confounds sidecar be dropped while an
ordinary BOLD sidecar in the same database keeps its custom fields.

**5. Default `store`, and `base.json` untouched.** Plain BIDS behavior is unchanged.
This also keeps the generated Python column tables byte-identical, since
`emit-types` builds from `Ingestion::base()` — a constraint worth stating explicitly,
because moving this policy into `base.json` would break codegen.

**6. Rejected: `additional_columns: "not_allowed"`.** It is the BIDS-native field and
the wrong lever — a *validation* assertion. Setting it would make
`bids-validator-rs` declare every real fMRIPrep confounds file invalid.

## Consequences

Measured end to end with the release binary, on a dataset reconstructed from the
profiled catalog (8 confounds files, 1,798 columns × 450 rows, real 2,151-key sidecars):

    overlay only          207.26 MB   829 blocks
    --adapter fmriprep      8.76 MB    35 blocks

**23.7× smaller.** There is no size/speed trade to make either: the policy only shrinks the
SELECT list, so it does no more work than storing the columns would. At 16 subjects (64 files,
28,800 rows) the file is still 8.7 MB, i.e. dominated by DuckDB's 256 KB per-segment minimum
and the embedded schema blob rather than by data.

Batching is unaffected in shape — the header group key, the pre-`DELETE` and the single
`INSERT … BY NAME` all still apply, so ADR 0002 §5's "Lever 1b" risk is not engaged. (The
batch has since gained per-group windowing, a `__src` source column and a declared column
list; none of that interacts with which columns the SELECT projects.)

A consequence worth naming: `BidsFile.metadata` for a file under `undeclared: catalog`
now carries only declared fields. Reading the rest means reading the file, which
`BidsLake.resolve` exists to make easy.

### What this does not settle

The whole-file boundary is still crude. One rule — `extension == ".tsv.gz"` → `catalog`
— is all that stands between a catalog and two million rows of physio, and it is a proxy
(BIDS happens to compress those recordings) rather than a principle. This ADR adds a
second, finer lever; it does not replace the first. See the crate README roadmap.

Substrate was considered and deliberately not changed. At tens of GB a single DuckDB
file is unremarkable, and without this policy no substrate would have helped — 2.3 TB of
floats is 2.3 TB in Parquet too. DuckLake's real argument here is operational
(concurrent writers, incremental publish, no rewrite needed to reclaim free blocks), and
should be made on those grounds when it is made.
