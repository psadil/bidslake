# ADR 0007 — A file describes data files: one shape for within-dataset associations

Status: accepted (2026-08-13)

Relates to: `file_associations` and the `bvals`/`bvecs`/`diffusion` DDL (`schema.rs`), the
`describes` block (`schema/ingestion.rs`, `data/ingestion-metaschema.json`), the view
generator (`schema/dynamic.rs`), `resolve_structural_associations` (`bids.rs`), and the
query surface (`layout.py`, `file.py`). Closes `TODO.md`'s inherited-gradient
item. Extends ADR 0006 §4's file-keyed rule with *which* file. Scoped **within** one
dataset; the cross-dataset case is ADR 0003's and stays there.

## Context

BIDS inheritance lets one file describe many. A `*_events.tsv` at the dataset root applies
to every matching run below it; so does a `dwi.bval`. `ds114` ships both.

The catalog already handled one of them correctly, and had for a long time. `events` rows
are stored **once**, keyed on the `*_events.tsv`'s own `file_id`, and reach all 20 of
ds114's BOLD runs through `file_associations` — 120 edges from 6 stored files. The edges
come from the schema: `resolve_structural_associations` evaluates `meta.associations` and
writes a row per hit.

`diffusion` did not. It was keyed `(image file_id, volume_idx)`, and it found the image by
swapping the extension on the gradient file's stem — `sub-01_dwi.bval` → `sub-01_dwi.nii.gz`
— which is right only for a gradient file sitting beside its image. Measured on the vendored
corpus, that produced **zero rows** for:

- `ds114` and `genetics_ukbb`, whose root-level `dwi.bval`/`dwi.bvec` synthesize a
  `dwi.nii.gz` that is nothing on disk (ADR 0006's foreign key turned this from a dangling
  reference into a skip, which is where `TODO.md`'s entry came from);
- `dwi_deriv`, whose image is an uncompressed `.nii` — nothing to do with inheritance, just
  a hardcoded `.nii.gz`;
- any `epi` fieldmap with gradients, which `meta.associations.bval` has always selected.

And a fourth case it could not represent at all: a `.bval` at the root with a `.bvec` beside
the image. The two hashed to different synthesized keys, neither entry had both halves, and
a both-present guard dropped **both** without a word.

Meanwhile the *other* half of the problem was the mirror image. fMRIPrep's
`*_desc-confounds_timeseries.tsv` is stored correctly — `fmriprep_confounds`, keyed on the
TSV, `row_idx` being the volume index — but had no edges at all, because nothing declared
the association. Consumers matched `sub`/`ses`/`task`/`run` by hand,
which is the same unsound entity match ADR 0003 abolished across the dataset boundary, and which is not merely unsound but *wrong* for an inherited describing file: a
root-level `dwi.bval` has no `sub` to join on.

So: one relation, already built, consumed in one place, missing in another, and reimplemented
badly in a third.

## Decisions

### 1. A per-row table keys on the file whose rows it holds

The invariant, stated once:

> **Within one dataset**, a file F describes data files D₁..Dₙ. The edges live in
> `file_associations`, derived from schema-declared `meta.associations`. The payload rows
> live keyed by the **describing** file's `file_id` plus an ordinal. A read joins payload →
> edges → data file.

`events`, `*_channels`, `*_electrodes`, `asl_context`, the recordings, and
`fmriprep_confounds` already satisfied it. `diffusion` was the sole exception and stops being
one. That is what makes this a correction rather than a redesign, and it is why the
`events` precedent leads this ADR rather than closing it.

Two tables key on the *described* file and are **not** exceptions:

- **`scans`** — a `scans.tsv` row names its target outright in a `filename` column. That is
  a 1:1 mapping resolved by lookup, not by inheritance; there is no many-to-many to model.
  `participants`/`sessions` are the same shape, keyed by an entity the file states.
- **`sidecars`** — its content is the nearest-wins **merge** of several JSON documents
  (`SidecarIndex`), so there is no single describing file to key on. Denormalizing is the
  correct shape there. Recorded explicitly because the invariant above would otherwise read
  as an indictment of it.

### 2. `bvals` + `bvecs` store; `diffusion` resolves

```sql
CREATE TABLE bvals (file_id HUGEINT, row_idx BIGINT, b DOUBLE, PRIMARY KEY (file_id, row_idx),
                    FOREIGN KEY (file_id) REFERENCES file_registry(file_id));
CREATE TABLE bvecs (file_id HUGEINT, row_idx BIGINT, x DOUBLE, y DOUBLE, z DOUBLE, …);
```

`file_id` is the gradient file's own, so the foreign key is **always** satisfiable — a
`.bval` is a registry row under its own path, whereas the synthesized image often was not.
That is the same constraint that made the bug loud in ADR 0006, now satisfied by
construction rather than by skipping.

**Two tables, not one with nullable columns.** A single `gradients` table would be half-NULL
on every row — a `.bval` row with no vector, a `.bvec` row with no scalar — with the
discriminator living implicitly in the edge type. Splitting means every row is fully
populated and a table's columns are exactly what its source file supplies. It also gives each
table a *single* association to name, which collapses the §4 view generator from a
multi-association pivot (`any_value` over a `GROUP BY`) to a plain one-to-one join.

`diffusion` is then a composition of the two generated views:

```sql
CREATE OR REPLACE VIEW diffusion AS
SELECT v.file_id, v.volume_idx, v.b AS bval, c.x AS bvec_x, c.y AS bvec_y, c.z AS bvec_z,
       v.source_file_id AS bval_file_id, c.source_file_id AS bvec_file_id
FROM bval_volumes v LEFT JOIN bvec_volumes c USING (file_id, volume_idx);
```

**The zip happens at read time, through the image**, which is the only sound join key — a
root `dwi.bval` and a per-image `sub-01_dwi.bvec` reach the same `(file_id, volume_idx)`
through their own edges. Pairing at write time is what the old code did by filename, and it
is not well-defined when the two halves sit at different inheritance levels.

`LEFT JOIN` from the b-values, so **they define the volume axis** — the rule the old writer
already applied when it NULL-filled a short `.bvec`, preserved so nothing changes for the
sibling case. A `.bvec` with no `.bval` yields no `diffusion` rows, also as before, but
unlike before its values are not lost: they are in `bvecs`, keyed by image in
`bvec_volumes`.

*Rejected:* **per-image duplication** — keep `diffusion` keyed by image and write the values
once per inheriting image. It leaves the catalog handling one BIDS rule two ways (`events`
shared, gradients copied), multiplies ds114's rows by 20, and — decisively — would make the
*writer* depend on the association edges, which the resolver only produces where it can see
the whole tree. The storage path stays backend-agnostic precisely because it does not.

*Rejected:* keying on the `.bval` and carrying a `bvec_file_id` column. One root `dwi.bval`
with twenty per-image `.bvec`s is legal BIDS, and `(bval_file_id, row_idx)` cannot hold
twenty different triplets.

### 3. Edges come from the schema, never from string surgery

Deleting the stem swap fixed the inherited case, the uncompressed `.nii`, and the `epi`
suffix in one move — because `meta.associations.bval` already selects
`intersects([suffix], ['dwi','epi'])` and `match(extension, '\.nii(\.gz)?$')`, and declares
**no** `target.suffix`, so `find_associated_file` falls back to the source's own. Every one
of those behaviours was in the schema all along; the reader was second-guessing it.

The general form: a companion file's relationship to its data file is the *schema's*
statement, not something to re-derive from filenames. Where bidslake needs a relation the
BIDS schema does not carry, an overlay adds a `meta.associations` entry (§5) rather than code
adding a rule.

### 4. `describes` — the ingestion schema declares what the relation means

`meta.associations` says *that* a file describes another. It cannot say what the row ordinal
means or what to call the resulting view, because the BIDS metaschema pins its entries to
`additionalProperties: false`. That belongs to bidslake anyway — it is "what bidslake does
with the file" (ADR 0002 §1) — so it lives on `tables.<name>` in the ingestion schema:

```json
"bvals":  { "describes": { "association": "bval",  "axis": "volume", "view": "bval_volumes" } },
"events": { "ordered": false, "describes": { "association": "events" } }
```

One association per table, because a table holds the rows of one *kind* of file; a payload
split across sibling files is split across tables too, and joining those is a view over views
(§2). `axis` and `view` are **optional**, which is what lets `events` be declared as the
instance it is: its rows correspond to a *time*, not to a position in the data file (hence
`ordered: false`), and both sides of the relation would want the name `events`, so there is
nothing to materialize. Recording the relation without a view is the point of the two
fields being optional.

The generated view is one template:

```sql
CREATE OR REPLACE VIEW <view> AS
SELECT fa.source_file_id AS file_id, t.row_idx AS <axis>_idx,
       t.* EXCLUDE (file_id, row_idx), t.file_id AS source_file_id
FROM <table> t JOIN file_associations fa ON fa.target_file_id = t.file_id
WHERE fa.association_type = '<association>';
```

`t.* EXCLUDE` on a qualified star means the generator **never needs the table's column
list**, which is what lets one code path serve a static table (`bvals`) and a
schema-generated one (`fmriprep_confounds`) alike.

Five instances at the time of writing — `bvals`, `bvecs`, `asl_context`, `events`,
`fmriprep_confounds` (plus `qsiprep_confounds`) — of which `asl_context` was a live gap:
an `*_aslcontext.tsv` is one row per volume of its ASL series, the association has always
been declared, and nothing linked the two, so "the volume types of this ASL image" needed an
entity guess. `*_channels` is a candidate and deliberately deferred, so that rewiring the
`motion_channels` column-name lookup (`bids.rs`) onto the view is a decision of its own.

Two failures are refused at load: an `axis` on an `ordered: false` table (there is no
`row_idx` to name), and two tables claiming one view name (`CREATE OR REPLACE VIEW` would let
one silently shadow the other — and fMRIPrep and QSIPrep both declare a confounds view, so
this is live). A **multi** association — one setting `target.entities`, so several targets
may resolve per source — is permitted and merely makes `(file_id, <axis>_idx)` non-unique,
with `source_file_id` saying which file each row came from.

### 5. Association keys are namespaced to their table

fMRIPrep's edge is `fmriprep_confounds`, not a generic `confounds`. Not a style choice: a
generic key would be declared by *both* pipeline overlays with **different** selectors
(`suffix == 'bold'` vs `suffix == 'dwi'`), and `overlay::merge_at` conflicts on differing
values at one JSON pointer — so `--adapter fmriprep --adapter qsiprep` would become a hard
error. Namespacing also makes edge-label → payload-table the identity function, which is what
lets the Python accessor default `table = association_type` with no lookup table.

The *view* is not namespaced (`timeseries`, `dwi_timeseries`): it is the name a user types.

While here, the two `rules.tabular_data` confounds rules gained a `datatype` selector
(`func`/`dwi`). They had byte-identical *identity* selectors, and `TabularRule::identity_key`
drops the non-identity `DatasetType` — so with both adapters applied they collapsed into one
table and QSIPrep's rows landed in `fmriprep_confounds` while `qsiprep_confounds` stayed
empty. Pre-existing, but adding the edges made it newly misleading: a join through
`qsiprep_confounds` would return nothing while correctly-labelled edges pointed elsewhere.

### 6. Structural associations resolve from the registry path set

`resolve_structural_associations` used to return early when the backend supplied no
in-memory `FileTree`, which is every S3 ingest — the limitation ADR 0003's Consequences
recorded. Now it builds one with `FileTree::from_paths` over `registered_paths()`, on both
backends.

Sound because the resolver is pure path matching: it reads no file content and never touches
`absolute_path`. Safe because the two sets are the same — `test_file_registry.rs::
every_walked_file_has_a_registry_row` already asserts the registry equals the walk on
ds000117 — differing only where an ingestion `ignore` rule fired, where excluding the file as
an association target is the correct reading of "neither read nor register it". The payoff is
that a *structural* association's `target_file_id` is never NULL. (`IntendedFor` can still
dangle by design; that is ADR 0006 §4.)

This was not optional. Under §2 the gradient *payload* is backend-agnostic, but `diffusion`
is a view over the edges, so without parity an S3 catalog would have lost diffusion entirely.

## Consequences

- **The inherited case works, and cheaply.** ds114: 71 stored b-values answer for 20 images
  (1,420 view rows) from one file. `genetics_ukbb`, `dwi_deriv` and `epi` fieldmaps likewise
  go from zero rows to correct ones.
- **ds000117 is unchanged** — 715 rows over 11 images — because every image there has its own
  sibling pair, so the re-keying view fans each out to exactly one image. That the number did
  not move is the evidence the view is right, and `curated.rs` still asserts it.
- **`row_idx` means what a table declares it means.** The stored column records line order;
  `<axis>_idx` on the view records what that order is an order *of*.
- **Alignment is still only *declared*.** Nothing verifies that a confounds table has as many
  rows as its image has volumes: bidslake reads no NIfTI headers — `files/nifti.rs` lives in
  `bids-validator-rs`, which `bidslake` does not depend on. In practice this bites non-BIDS
  and derivative trees, since a BIDS dataset is validated before it is indexed; a ragged
  `.bval`/`.bvec` pair behaves exactly as it did before, with the b-values defining the axis.
- **A gradient file with no image is kept.** Its rows are in `bvals`/`bvecs` and it
  contributes nothing to `diffusion`. Previously the values were parsed and silently
  discarded, so a typo'd stem was indistinguishable from a file that never existed.
- **Every structural association now works on S3**, not just gradients — events, channels,
  coordsystem, physio. One pre-existing S3 gap survives and is *not* fixed here:
  `S3Client::walk` ignores `pseudo_exts`, so a `.ds`/`.ome.zarr` pseudo-file arrives as many
  object keys and cannot be an association source there.
- **A fourth DuckDB constraint for ADR 0006 §5's list**: a table cannot be replaced by a
  same-named view — `CREATE OR REPLACE VIEW` over a table errors, and
  `CREATE VIEW IF NOT EXISTS` silently no-ops. A shipped table's name is therefore effectively
  frozen, which is worth knowing before shipping a table you may later want to compute. (No
  migration was needed here: no catalogs required preserving.)
- **`COPY FROM DATABASE (SCHEMA)` emits views in dependency order.** `diffusion` selects from
  two generated views, making it the first object in the catalog to need this;
  `test_compact.rs` now pins it.
