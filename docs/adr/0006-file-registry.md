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

`file_registry` is one row per file the walk sees, keyed by `file_id` — the first 128 bits of
`SHA-256(dataset_id ␟ root_uri ␟ file_path)`, stored as a `UUID`. Every file-keyed table keys on that id, four of them
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

The view spans every walked file because the concepts are functions of `file_path`, as meaningful
for a sidecar or a `*_events.tsv` as for the image beside it — and the per-row tables key on
*tabular* files, so a data-file-only view would leave "which subject is this row from?"
unanswerable for them.

### On the surrogate key

SHA-256 is fixed by specification, and `file_id` is a *stored* key that satellites point at;
deriving it from content rather than a sequence is what makes a re-index upsert onto the same rows,
and two machines agree on one tree.

`UUID` and not `HUGEINT`, though both are 128 bits, because the two cross the Arrow bridge
differently. DuckDB hands `HUGEINT` over as `Decimal128(38, 0)`, whose maximum is `10^38 - 1`
against `HUGEINT`'s `2^127 - 1`, about `1.7 × 10^38` — so two ids in five fall outside the type they
are handed over in and cannot rebuild a frame. That is what once sent this key down to 64 bits, and
the reasoning was one step short: it is a fact about `HUGEINT`'s *Arrow mapping*, not about width.
A `UUID` column exports as Arrow `Utf8` (`arrow_converter.cpp`, and duckdb-rs pins it in a test
against the same `query_arrow` call this crate makes), so it arrives as a `str` with no decimal in
sight, while remaining `PhysicalType::INT128` inside the engine. That second half is what makes it
the right 128-bit type rather than merely a working one: an id column wants integer join width,
integer hashing and the integer compression schemes, and a `VARCHAR` spelling of the same bits
forfeits all three. Measured on `ds000117`, `file_id` on `events` (33,236 rows) stores as **RLE** —
a scheme `VARCHAR` is not eligible for at all.

The write side pays something in principle and nothing that shows up in practice. duckdb-rs has no
UUID `Value`, so the id reaches the Appender as `Value::Text` and DuckDB casts it per value instead
of storing it through the typed vector path — `LogicalTypeId::UUID` has no arm in `appender.cpp`'s
switch. That cost is not measurable here: against the `ubigint` baseline, `ingest/ds108` — the
insert-heaviest dataset in the bench — comes in at 149.1–150.0 ms, **−1.5%** (p = 0.02), and `ds001`
shows no significant change. Doubling the key width is not plausibly a speedup, so read that as *no
regression* rather than a gain. The million-row tables would not pay it in any case: they go through
`read_csv`, where the id is one constant-folded literal per statement.

The derivation stamps RFC 9562 version 8, the version reserved for vendor-specific ids
(`uuid::Builder::from_custom_bytes`). Those 6 bits are the whole cost of the id being a *well-formed*
UUID rather than a raw digest prefix, and what they buy is that `uuid.UUID(...).version` reads `8`
instead of whichever nibble the hash happened to land on — for a truncated SHA-256 that answer is
arbitrary, and sometimes a confident, wrong `4` or `6`. The remaining 122 bits leave the birthday
bound at `n² / 2^123` — ≈ 9 × 10⁻²² at 10⁸ files, against 3 × 10⁻⁴ for the 64-bit key.

That last figure is what closes this ADR's longest-standing open issue rather than answering it.
A collision is still written with replace-on-conflict, so two files sharing an id would still become
one row silently — but at `9 × 10⁻²²` there is no scale at which to worry about it, where at 64 bits
`3 × 10⁻⁴` at 10⁸ files was a real number.

Detecting one anyway was built and then removed, and the reason is worth recording so it is not
rebuilt. The argument for it was never the birthday bound; it was that a *derivation* bug — hashing
fewer than the three identity parts — collides systematically at a rate no bound describes. That
argument does not survive testing. Dropping `root_uri` from the hash fails
`file_id_is_stable_and_separator_safe` and the corpus proptest, at `cargo test`, unconditionally.
Hashing a different root than the row stores fails the `sidecars` foreign key on the first real
dataset, naming the id that has no registry row. Both are caught earlier, more precisely, and
without a per-upsert cost; a stage-versus-registry join would only have restated what the pinned
tests and the four foreign keys already guarantee.

Python sees **one** spelling of an id — the canonical string — and that is forced rather than
preferred. polars has no UUID dtype, so a frame column can only be `pl.String`; the one route to 16
raw bytes over Arrow is `arrow_lossless_conversion`, which is connection-wide and also retypes every
`BOOLEAN` column to an `arrow.bool8` extension. Since a frame cannot hand back anything else,
everything else matches it: `BidsFile.file_id` is a `str`, `bidslake.file_id(...)` returns a `str`,
and the generated models annotate `Mapped[str]`. A `uuid.UUID` is still accepted as a bind
parameter, for a caller who has built one.

Annotating the models `Mapped[uuid.UUID]` — describing the engine's type rather than the query
layer's — was tried and reverted, and the reason it is worth recording is that **nothing catches the
difference statically**. polars is not generic over its schema, so `df["file_id"][0]` is `Any` and a
type checker sees no conflict with a `uuid.UUID` on the other side; the mismatch surfaces only as a
comparison that is quietly always false. `COLUMNS` records the DuckDB type, which is where an
engine-level fact belongs. Note also that polars elides strings at 31 characters, so a 36-character
id needs `pl.Config.set_fmt_str_lengths(40)` to print in full.

The write path is where a key of any width goes wrong quietly, and the shape of the danger changed
with the type. A *number* in an id column can only be a stale 64-bit id or a mis-keyed row — no
`serde_json::Number` holds 128 bits — so `row_values` refuses one by name rather than coercing it.
A malformed *string* is passed through to DuckDB, whose appender casts it and throws
(`Could not convert string 'not-a-uuid' to INT128`). Both are loud. What must never happen is the
third outcome, a bad id becoming NULL or a different file's key, which is also why `UUID` is absent
from `needs_try_cast`: a `TRY_CAST` on a key would turn exactly that failure back into silence.

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
    file_id UUID PRIMARY KEY,
    dataset_id TEXT, root_uri TEXT, file_path TEXT, status TEXT,
    size_bytes UBIGINT, mtime_ns BIGINT   -- , projected JSON   (only with a term map)
);
```

Every file the walk sees, not every file that produced rows: a `*_physio.tsv.gz` left unread has a
row, carrying `status = 'on_disk'`. The one exclusion is what the walk never reached —
`.bidsignore`d files, and files an ingestion `ignore` disposition rejected. `size_bytes` and
`mtime_ns` are what the filesystem said, NULL under `--no-stat` and where a backend could not stat
it; `bidslake verify` is their consumer ([ADR 0007](0007-root-tenure.md)).

### 2. `status` records what bidslake did with the file

`status` ∈ `ingested`, `on_disk`, `skipped`, `failed` — the fate of a file bidslake *tried to
read*, NULL for one it never would have, an image having no reading to report.

What a file *is* stays a function of its path, spelled by the caller: `extension` for a format,
`datatype` for the files under a datatype directory. The walk's own classifier is
`is_primary_data` over `COMPANION_EXTENSIONS`, which encodes the companion rule: the four
companion extensions are claimed before the datatype is consulted, so a `.json` beside a
`.nii.gz` is a sidecar, not a second data file. (An earlier revision stored that classification
as a six-valued `kind` column; see Rejected Ideas.)

### 3. The concepts live on `all_files`, a view — once, not 26 times

```sql
CREATE OR REPLACE VIEW all_files AS
  SELECT *, <sub>, <ses>, <task>, …, <datatype>, <suffix>, <extension>, <modality>
  FROM file_registry;
```

Each concept select item is a regex over `file_path`, wrapped as
`COALESCE(json_extract(projected, …), <regex>)` where a term map can supply the concept — so a
term-mapped file's `datatype` comes from the projection and a BIDS-named file's from its path. The
view covers every walked file; "data files only" is an `extension`/`datatype` predicate, spelled
by the caller. It is emitted `CREATE OR REPLACE`, so its concept set is whatever the current
run's schema yields, for rows already stored as much as for new ones; `projected`, a physical
column, is not.

### 4. `file_id` is a surrogate key over the identity triple

```
file_id = first 128 bits of SHA-256(dataset_id ␟ root_uri ␟ file_path),
          version/variant stamped RFC 9562 v8   as UUID
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
`sub` nor `suffix`. `get()` defaults to `all_files`, hence to every walked file.

## Backwards Compatibility

What is frozen is the registry's shape, tables being created `IF NOT EXISTS` — in practice one
column, `projected`, which a catalog built without a term map lacks, so a later run whose term map
would store a projection has nowhere to put it. `BidsDb::check_registry_shape` refuses that run and
names the missing column, rather than letting `IF NOT EXISTS` drop the difference in silence. Pass
every adapter the catalog uses on every index run (order does not matter), or index into a new
catalog.

## Rejected Ideas

**A stored `kind` classification column.** An earlier revision stored a six-valued classifier
(`data`/`sidecar`/`tabular`/`gradient`/`description`/`other`) on every registry row, as the query
idiom for "just the data files". Removed: nothing in the product read it, five of its six values
had at most one query site each (all redundant with `extension`), every known external query
already pinned `extension` beside it, and the vocabulary was bidslake's own invention where
everything else on the view is BIDS's. What the column recorded beyond a path function — the
promoted metadata-only records, and an adapter disposition — remains recoverable: a promoted
`.json` owns its `sidecars` row, and an adapter catalog's data files carry a projected
`datatype`. Its cousin was **deriving `kind` from BIDS's own `rules.files` taxonomy**, rejected
earlier still: 149 of the 169 extension-bearing rules list `.json` alongside a non-JSON
extension, so that taxonomy cannot separate a data file from its sidecar.

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

**A second view for the data files.** An `extension`/`datatype` predicate is short enough not to
earn a view that could drift from `all_files`' concept expressions.

## Open Issues

- **A narrower later run silently strips concept columns off `all_files`.** Either union the
  incoming concept set with the catalog's stamped `bidslake_schema`, or refuse the run the way a
  missing `projected` is refused; the first needs the stamp read back at `create_tables` time, which
  nothing does. `test_registry_shape.rs::narrowing_is_allowed` asserts only that no error is raised.
- **Extending the foreign key to the 22 per-row tables**, once a per-row check on the bulk
  `read_csv` path has been measured against the batched-ingest benchmark.
- **Nothing enforces that a file's subject is a subject the dataset has.** `all_files.sub` and `ses`
  are select items of a view rather than columns of a table, and a foreign key against a view is
  refused, so the tie to `participants` is asserted by test instead — `test_adapter_freesurfer.rs`
  checks that every registry `sub` resolves to a `participants` row. Materializing the concepts onto
  `file_registry` is what would make them keyable, at the price of the retroactive fix the view
  buys, plus reconciling `sub` (`01`) against `participants.participant_id` (`sub-01`) and accepting
  that a file whose subject is absent becomes an ingest error rather than a queryable fact.
- **A catalog cannot gain `projected` short of a rebuild.** The concepts widen retroactively because
  they ride the view; `projected` is a physical column, so the refusal under *Backwards
  Compatibility* is the whole of today's answer. Rebuilding in place — new table, copy, swap, most
  of the machinery being in `compact.rs` — is the fuller fix, and has to carry the four foreign
  keys aimed at `file_registry` across the swap.
