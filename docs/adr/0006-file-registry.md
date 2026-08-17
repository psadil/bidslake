# ADR 0006 — A real file registry, and a surrogate key for a file

```
ADR: 0006
Title: A real file registry, and a surrogate key for a file
Status: Provisional
Type: Design
Created: 12-Aug-2026
Requires: 0002, 0005
```

## Abstract

`file_registry` is one row per file the walk sees, keyed by `file_id` — the first 64 bits of
`SHA-256(dataset_id ␟ root_uri ␟ file_path)`. Every file-keyed table keys on that id, four of them
through a real foreign key. `all_files` is a view over the registry adding the BIDS-concept columns
computed from `file_path`, defined once rather than per table. `scans` is the `scans.tsv` satellite.

## Motivation

A catalog needs one relation answering "what files does this dataset contain", and one row per file
for a file-keyed table to point at; a relation restricted to primary data files, or to tabular ones,
is neither. On the vendored `ds000117` (2,209 walked files) 1,492 are primary data and 298 tabular;
the other 419 — **19%** — are 103 sidecar `.json`s, 22 `.bval`/`.bvec`s,
`dataset_description.json`, and 293 files of documentation, code and stimuli. It also leaves a
foreign key unsatisfiable, the file whose rows a satellite holds usually not being a data file: of
the 256 files `events` draws rows from, none is; of 909 `file_associations` rows, 416 are non-data.

A path is not an identifier either. A dataset may span several ingest roots
([ADR 0005](0005-multi-root-datasets.md)), so `(dataset_id, file_path)` can name two files, and a
`*_channels.tsv` row has no business knowing a path in the first place. The BIDS concepts (`sub`,
`task`, `datatype`, `suffix`, …) are functions of `file_path`, so absent one relation holding every
file each of the 26 file-keyed tables a plain-BIDS catalog builds carries its own copy: 40
expressions apiece on BIDS schema 1.11.1, 1,040 generated columns across the catalog.

## Rationale

### On registering every walked file

Membership is decided by the walk alone, so the registry is a manifest of the dataset rather than of
the files bidslake understood — checkable on `ds000117`, where it holds 2,209 rows against 2,447
non-hidden files on disk, its two `.bidsignore` patterns accounting for the 238 exactly, and
`test_file_registry.rs` asserts set equality against the ingest's own walker.

### On the concepts living on a view

A view over a table with no generated columns buys two things: widening the concept set is free, and
the bulk staged upsert can write the base table, which a table full of generated columns refuses.
The same mechanism cuts the other way: a later *narrower* run redefines the view without the
concepts the wider run added and without the `COALESCE` over `projected`. A catalog built with the
`fmriprep` and `freesurfer` adapters, indexed again with neither, loses `from`, `to`, `mode` and
`parc`, and its FreeSurfer rows read `datatype` NULL — which `check_registry_shape` cannot see, no
*physical* column being missing.

The view spans every kind because the concepts are functions of `file_path`, as meaningful for a
sidecar or a `*_events.tsv` as for the image beside it — and the per-row tables key on *tabular*
files, so a data-file-only view would leave "which subject is this row from?" unanswerable for them.

### On the surrogate key

SHA-256 is fixed by specification, and `file_id` is a *stored* key that satellites point at;
deriving it from content rather than a sequence is what makes a re-index upsert onto the same rows,
and two machines agree on one tree.

64 bits and not 128, because `HUGEINT` does not survive the trip to Python: the Arrow bridge hands
it over as `Decimal128(38, 0)`, whose maximum is `10^38 - 1` against `HUGEINT`'s `2^127 - 1`, about
`1.7 × 10^38` — so two ids in five fall outside the type they are handed over in and cannot rebuild
a frame, and widening is unavailable, polars capping decimal precision at 38. The cost is collision
resistance: by the birthday bound a catalog of `n` files collides with probability `n² / 2^65` —
≈ 3 × 10⁻⁸ at 10⁶ files, ≈ 3 × 10⁻⁴ at 10⁸.

A key of this width goes wrong silently on the write path: `UBIGINT` sits in the same numeric set as
the float types, whose generic string fallback is `parse::<i64>()` then `parse::<f64>()`, and 53
bits of mantissa round any id past `2^53`. So `row_values` has `UBIGINT` arms taking a `u64` number
or a decimal string and NULLing anything else, a negative included: NULL fails the primary key
loudly, a wrapped id would not.

Enforcement is partial for cost, not principle. Without the constraint a payload row can key on a
file the dataset does not ship — `ds114`'s root-level inherited `dwi.bval`/`dwi.bvec` are that case,
and the payload keys on the gradient file itself, where the constraint holds by construction
([ADR 0003](0003-associations.md)). `file_associations` cannot take one at all: `target_file_id` is
nullable by design.

### On the DuckDB constraints that fix this shape

None of the three is documented upstream; all hold on DuckDB 1.5.5, the bundled engine.

- **A foreign key against a VIEW is refused** (`cannot reference a VIEW with a FOREIGN KEY`), so the
  FK target is `file_registry`, the table, while the concepts are on `all_files`, the view. The
  split is not aesthetic.
- **A table cannot be replaced by a same-named view.** `CREATE OR REPLACE VIEW` over a table errors
  outright (`Existing object t is of type Table, trying to replace with type View`), and
  `CREATE VIEW IF NOT EXISTS` is a silent no-op — no error, no view, every later query reading the
  table. A shipped table's *name* is therefore frozen, so `all_files` cannot take over `scans`.
- **`INSERT OR REPLACE` fails on a table carrying more than one UNIQUE/PK constraint**, so `file_id`
  is the *sole* key and the identity triple is not re-asserted as a second UNIQUE.

## Specification

### 1. `file_registry` is the walked file tree, persisted

```sql
CREATE TABLE file_registry (
    file_id UBIGINT PRIMARY KEY,
    dataset_id TEXT, root_uri TEXT, file_path TEXT, kind TEXT, status TEXT,
    size_bytes UBIGINT, mtime_ns BIGINT   -- , projected JSON   (only with a term map)
);
```

Every file the walk sees, not every file that produced rows: a `*_physio.tsv.gz` left unread has a
row, carrying `status = 'on_disk'`. The one exclusion is what the walk never reached —
`.bidsignore`d files, and files an ingestion `ignore` disposition rejected. `size_bytes` and
`mtime_ns` are what the filesystem said, NULL under `--no-stat` and where a backend could not stat
it; `bidslake verify` is their consumer ([ADR 0007](0007-root-tenure.md)).

### 2. `kind` classifies the file; `status` records what bidslake did with it

`kind` ∈ `data`, `sidecar`, `tabular`, `gradient`, `description`, `other` — bidslake's own
vocabulary, and what lets one table hold every class of file. `kind_of` encodes the companion rule
in its order: the four companion extensions are claimed before `datatype.is_some()` is consulted, so
a `.json` beside a `.nii.gz` is a sidecar, not a second data file. `status` ∈ `ingested`, `on_disk`,
`skipped`, `failed` — the fate of a file bidslake *tried to read*, NULL for one it never would have,
an image having no reading to report.

### 3. The concepts live on `all_files`, a view — once, not 26 times

```sql
CREATE OR REPLACE VIEW all_files AS
  SELECT *, <sub>, <ses>, <task>, …, <datatype>, <suffix>, <extension>, <modality>
  FROM file_registry;
```

Each concept select item is a regex over `file_path`, wrapped as
`COALESCE(json_extract(projected, …), <regex>)` where a term map can supply the concept — so a
term-mapped file's `datatype` comes from the projection and a BIDS-named file's from its path. The
view covers every `kind`; "data files only" is `WHERE kind = 'data'`, spelled by the caller. It is
emitted `CREATE OR REPLACE`, so its concept set is whatever the current run's schema yields, for
rows already stored as much as for new ones; `projected`, a physical column, is not.

### 4. `file_id` is a surrogate key over the identity triple

```
file_id = first 64 bits of SHA-256(dataset_id ␟ root_uri ␟ file_path)   as UBIGINT
```

`␟` is ASCII unit separator, a byte that does not occur in the paths bidslake walks, so the three
parts cannot run together: `("ab", "c", p)` and `("a", "bc", p)` hash differently, which
`file_id_is_stable_and_separator_safe` pins. It is computed in Rust, never by DuckDB, and `root_uri`
is in the hash, so the same relative path under two roots of one dataset gets two ids. Every
file-keyed table keys on `file_id`; `scans`, `sidecars`, `bvals` and `bvecs` declare
`FOREIGN KEY (file_id) REFERENCES file_registry(file_id)`, while the 22 per-row tables and
`file_associations` reference the registry by convention only.

### 5. `scans` is the `scans.tsv` satellite

`scans` holds `acq_time`, `HED` and `other_data`, keyed by `file_id` — one row per data file *that a
`scans.tsv` describes*, built the way `sessions` is built from `sessions.tsv`. A dataset shipping no
`scans.tsv` has no rows in it. Nor is there a separate table of tabular files — a tabular file's
registry row carries its `status`, and which table its rows landed in is a join on `file_id`.

### 6. A file-keyed table is queried by concept through `all_files`

`BidsLake._relation` joins a file-keyed table back to `all_files` on `file_id` and `_filter_columns`
reports the union, so `lake.get(table="scans", suffix="bold")` works against a table storing neither
`sub` nor `suffix`. `get()` defaults to `all_files`, hence to every kind.

## Backwards Compatibility

What is frozen is the registry's shape, tables being created `IF NOT EXISTS` — in practice one
column, `projected`, which a catalog built without a term map lacks and a later run needing it is
refused for, by name. Pass every adapter the catalog uses on every index run (order does not
matter), or index into a new catalog.

## Rejected Ideas

**Deriving `kind` from BIDS's own `rules.files` taxonomy.** Its 179 leaves look like exactly this
classification, but 149 of the 169 extension-bearing rules list `.json` alongside a non-JSON
extension, so it cannot separate a data file from its sidecar — the one distinction `kind` exists to
draw. Upstream defines a sidecar as `extension == ".json"` plus a hand-written exception.

**`(dataset_id, root_uri, file_path)` as a composite key everywhere.** The same information spread
over three columns and 26 tables' worth of index, `root_uri` being a long absolute path repeated per
satellite row.

**A sequence-generated `file_id`.** It would depend on ingest order, so a re-index would insert
duplicates instead of matching.

**DuckDB's `hash()`, or Rust's `DefaultHasher`, as the id function.** Neither guarantees stability
across versions, and `file_id` is stored: a value that shifted on a version bump would orphan every
satellite row rather than fail loudly.

**Renaming `scans` to `file_registry` in place.** `scans.tsv` is a real BIDS file with a real table;
taking its name for the manifest would leave the BIDS concept homeless.

**Seeding `scans` with a stub row per data file so `sidecars`' foreign key has a target.** The
registry holds every file whether or not a `scans.tsv` mentions it, so the key points there instead;
the stubs would be rows claiming to describe acquisitions nothing describes — 80 all-NULL rows on
`ds001`, whose 80 data files ship with none.

**A per-run row count on the registry.** Under upsert semantics a count of rows contributed by one
run is not a property of the file; a row count is `SELECT count(*) … WHERE file_id = ?` against the
table the rows landed in.

**A second view for the data files.** `WHERE kind = 'data'` is short enough not to earn a view that
could drift from `all_files`' concept expressions.

## Open Issues

- **A narrower later run silently strips concept columns off `all_files`.** Either union the
  incoming concept set with the catalog's stamped `bidslake_schema`, or refuse the run the way a
  missing `projected` is refused; the first needs the stamp read back at `create_tables` time, which
  nothing does. `test_registry_shape.rs::narrowing_is_allowed` asserts only that no error is raised.
- **Extending the foreign key to the 22 per-row tables**, once a per-row check on the bulk
  `read_csv` path has been measured against the batched-ingest benchmark.
- **A collision would be silent**, `file_id` being written with replace-on-conflict: two files
  sharing one become one row rather than an error. The registry stores the identity triple beside
  the id, so detecting it is one join against the upsert stage — worth doing before a catalog past
  10⁸ files.
- **Nothing enforces that a file's subject is a subject the dataset has.** `all_files.sub` and `ses`
  are select items of a view rather than columns of a table, and a foreign key against a view is
  refused, so the tie to `participants` is asserted by test instead — `test_adapter_freesurfer.rs`
  checks that every registry `sub` resolves to a `participants` row. Materializing the concepts onto
  `file_registry` is what would make them keyable, at the price of the retroactive fix the view
  buys, plus reconciling `sub` (`01`) against `participants.participant_id` (`sub-01`) and accepting
  that a file whose subject is absent becomes an ingest error rather than a queryable fact.
- **A catalog cannot gain `projected` short of a rebuild.** The concepts widen retroactively because
  they ride the view; `projected` is a physical column, so a catalog created without a term map is
  refused the later run that needs one, by name, and the remedy is to name every adapter on every
  run or index afresh. Rebuilding in place — new table, copy, swap, most of the machinery being in
  `compact.rs` — is the fuller fix, and has to carry the four foreign keys aimed at `file_registry`
  across the swap.
