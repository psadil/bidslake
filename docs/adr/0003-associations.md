# ADR 0003 — Associations, within and across datasets

```
ADR: 0003
Title: Associations, within and across datasets
Status: Provisional
Type: Design
Created: 18-Jul-2026
Requires: 0001, 0002, 0006
```

## Abstract

One thing describes another: a `*_events.tsv` over the BOLD runs below it, a `dwi.bval` over the
images that inherit it, an MRIQC tree over acquisitions an fMRIPrep tree also holds. Both take one
shape — the **declaration** is stored, read out of the BIDS schema's `meta.associations` or out of
`dataset_description.json`, and the **correspondence** is resolved by a view at read time, never
inferred by comparing entity labels. Within a dataset the edges land in `file_associations` and the
payload keys on the *describing* file; across datasets they are the `dataset_relations` and
`dataset_link_targets` views over `dataset_links`/`dataset_identity`.

## Motivation

**Inheritance makes the described file the wrong key.** A per-row table keyed by the file its rows
are *about* cannot represent one describing file that covers many: `ds114` ships one root-level
`dwi.bval`/`dwi.bvec` pair covering 20 images, and no single image to key on — the name a writer
would derive, swapping `.bval` for `.nii.gz` on the stem, is `dwi.nii.gz`, nothing on disk. That
derivation also guesses at a relation the schema already states, and it is wrong for an
uncompressed `.nii` (`dwi_deriv`), for an `epi` fieldmap with gradients, and for every inherited
pair (`ds114`, `genetics_ukbb`) — each of which yields no `diffusion` rows at all under a derived
name. A `.bval` at the root with a `.bvec` beside the image cannot be represented at all: the two
derive different keys and neither entry holds both halves.

**A stored relation still has to reach consumers.** fMRIPrep's `*_desc-confounds_timeseries.tsv`
stores fine — one row per volume, keyed by the TSV — but base BIDS declares nothing about which
images it describes, so a caller matches `sub`/`ses`/`task`/`run` by hand. For an inherited
describing file that match is not merely unsound but impossible: a root-level `dwi.bval` has no
`sub` to join on.

**Across datasets, entity labels are not an identity.** A catalog routinely holds
`ds001761-fmriprep` and `ds001761-mriqc`, and "the MRIQC `fd_mean` for this fMRIPrep BOLD run"
otherwise needs a hand-written join on `sub`/`ses`/`task`/`run` across `dataset_id`. `sub-01` is
meaningful only relative to a source dataset, so joining two datasets nothing ties together
silently equates two possibly different subjects. The sound anchor BIDS offers is at dataset level:
`SourceDatasets`, which both ds001761 derivatives populate with the same source DOI, proves
co-derivation *even though the raw dataset is not in the catalog*; the file-level generalization
`Sources` is emitted by no producer available to us. Nor can the query name a `dataset_id` — an id
is free text chosen at index time, and a study processed one subject at a time has one dataset
*per subject* (`sub-01-freesurfer`, `sub-02-freesurfer`, …).

## Rationale

### On naming a link rather than a dataset id

Naming a link is what lets one query run against catalogs whose dataset ids differ — verified
against a two-dataset catalog where `../freesurfer` resolves to a dataset id of `fs-tree` and the
script names neither. Each shard's own `DatasetLinks` already says which tree is its own, so the
sharded study needs neither a hardcoded id nor a `link alias` command. The entity match the caller
then makes is well-defined because two datasets known to share a source share a subject/entity
namespace, and auditable because the user named the target and the query counts its matches: a hit
landing on more than one file reads as ambiguous rather than being silently taken. Naming on its
own yields no relation, though — pointing at a template, an atlas or a shared FreeSurfer tree is
not deriving from it, and two datasets referencing the same one are not co-derivatives.

### On keying a payload table on the describing file

It makes the foreign key satisfiable by construction: a `.bval` is always a `file_registry` row
under its own path, whereas an image synthesized from its stem often is not. It is also what every
other per-row table already does — `events`, `*_channels`, `*_electrodes`, `asl_context`, the
recordings, `fmriprep_confounds` — so the relation has one shape rather than two. The fan-out is
then single-copy: on `ds114`, 71 stored b-values from one file answer for 20 images, 1,420 view
rows; on `ds000117`, where every image has its own sibling pair, the view fans each out to exactly
one image for 11 × 65 = 715 rows, identical to an image-keyed table — the evidence the view is
right (`test_diffusion_inherited.rs`, `curated.rs`).

### On resolving structural edges over the registry path set

Sound because the resolver is pure path matching: it reads no file content and never touches
`absolute_path`, so a content-less `FileTree::from_paths` is enough. Safe because the registry
equals the walk — `test_file_registry.rs::every_walked_file_has_a_registry_row` asserts it on
`ds000117` — differing only where an ingestion `ignore` rule fired, and excluding such a file as a
target is the right reading of "neither read nor register it". Not an optimization either:
`diffusion` is a view over the edges, so a backend producing no edges would have no diffusion.

### On namespacing an overlay's association key

A generic `confounds` key would be declared by *both* pipeline overlays with different selectors
(`suffix == 'bold'` vs `suffix == 'dwi'`), and `overlay::merge_at` conflicts on differing values at
one JSON pointer — so `--adapter fmriprep --adapter qsiprep` would be a hard error. The same
collision runs one level down, which is why the two `rules.tabular_data` confounds rules carry a
`datatype` selector: `TabularRule::identity_key` drops the non-identity `DatasetType`, so without
it both collapse into one table and QSIPrep's rows land in `fmriprep_confounds`.

### On zipping the two gradient files at read time

The image is the only sound join key. A root `dwi.bval` and a per-image `sub-01_dwi.bvec` reach the
same `(file_id, volume_idx)` through their own edges, so a pair split across inheritance levels
resolves instead of annihilating — and one association per table is what keeps the generated view a
plain one-to-one join rather than an `any_value` pivot over a `GROUP BY`. `diffusion` is
hand-written because it joins *two* generated views: one step beyond what a per-table `describes`
block models, and a composition of the mechanism rather than an exception to it.

## Specification

### 1. A description is declared, never inferred from labels

bidslake resolves only relations something declared: `meta.associations` within a dataset,
`SourceDatasets`/`DatasetLinks` across datasets. Within a dataset the unit is the file; across
datasets it is the **dataset** — bidslake does not infer file-to-file correspondence by comparing
entities, and nothing it does on its own initiative consults `dataset_relations`. A query may name
a link and match entities itself; bidslake supplies the relation, the caller does the match.

### 2. A per-row table keys on the file whose rows it holds

> The payload rows live keyed by the **describing** file's `file_id` plus an ordinal; the edges
> live in `file_associations`. A read joins payload → edges → data file.

`file_associations` is `(source_file_id, target_file_id, target_file_path, association_type)`,
keyed on all but `target_file_id`, best-effort and free of foreign keys: `target_file_id` is
nullable because an `IntendedFor` may name a file the dataset does not ship, and the raw
`target_file_path` beside it keeps a dangling reference recorded.

Two tables key on the *described* file and are not exceptions. **`scans`** — a `scans.tsv` row
names its target outright in a `filename` column, a 1:1 lookup with no many-to-many to model, and
`participants`/`sessions` are the same shape. **`sidecars`** — its content is the nearest-wins
*merge* of several JSON documents (`SidecarIndex`), so there is no single describing file to key on.

### 3. Edges come from the schema, resolved over the registry path set

`resolve_structural_associations` walks a `FileTree::from_paths("", registered_paths())`, so it
runs identically on every backend and a *structural* association's `target_file_id` is never NULL.
For each data file it evaluates `meta.associations` through
`bids_schema::associations::resolve_associations` and writes a row per hit. Nothing derives a
target from a filename: `meta.associations.bval` selects `intersects([suffix], ['dwi','epi'])` and
`match(extension, '\.nii(\.gz)?$')`, declares no `target.suffix` — so `find_associated_file` falls
back to the source's own — and sets `inherit: true`, covering the inherited pair, the uncompressed
`.nii` and the `epi` fieldmap with no code per case. Where bidslake needs a relation base BIDS does
not carry, an overlay adds a `meta.associations` entry.

An `IntendedFor` target is normalized by `normalize_path`, which accepts a BIDS URI `bids::<path>`
(this dataset), a dataset-relative path and a legacy subject-relative path. A `bids:<name>:<path>`
naming *another* dataset is skipped, not fabricated into a path.

### 4. `describes` declares what the relation means, and generates the re-keying view

`meta.associations` says *that* a file describes another; it cannot say what the row ordinal means
or what to call the resulting view, because the BIDS metaschema pins its entries to
`additionalProperties: false`. That is bidslake's own statement about what it does with a file, so
it lives on `tables.<name>` in the ingestion schema:

```json
"bvals":  { "describes": { "association": "bval",  "axis": "volume", "view": "bval_volumes" } },
"events": { "ordered": false, "describes": { "association": "events" } }
```

One association per table: a table holds the rows of one *kind* of file, and a payload split across
sibling files is split across tables too. `axis` and `view` are optional, which lets `events` be
declared as the instance it is — its rows correspond to a *time*, not a position, and both sides
would want the name `events`, so there is nothing to materialize.

```sql
CREATE OR REPLACE VIEW <view> AS
SELECT fa.source_file_id AS file_id, t.row_idx AS <axis>_idx,
       t.* EXCLUDE (file_id, row_idx), t.file_id AS source_file_id
FROM <table> t JOIN file_associations fa ON fa.target_file_id = t.file_id
WHERE fa.association_type = '<association>';
```

`row_idx` means what a table declares it means: the stored column records line order, `<axis>_idx`
on the view records what that order is an order *of*. The template above is the ordered case; an
`axis`-less declaration emits no ordinal column and `EXCLUDE (file_id)`. `t.* EXCLUDE` on a
qualified star means the generator never needs the table's column list, and a declaration on a
table the running schema does not create is skipped rather than fatal. Six declarations ship —
`bvals`, `bvecs`, `asl_context` and `events` in the base ingestion schema, `fmriprep_confounds` and
`qsiprep_confounds` in the pipeline fragments — of which `events` names no view, so five are
materialized: `bval_volumes`, `bvec_volumes`, `asl_volumes`, `timeseries`, `dwi_timeseries`.

The ordinal is declared, never checked against the data file: nothing verifies that a confounds
table has as many rows as its image has volumes, since bidslake reads no NIfTI headers —
`files/nifti.rs` lives in `bids-validator-rs`, which `bidslake` does not depend on. A BIDS dataset
is validated before indexing, so this bites non-BIDS and derivative trees.

Two failures are refused at load: an `axis` on an `ordered: false` table (there is no `row_idx` to
name), and two tables claiming one view name — `CREATE OR REPLACE VIEW` would let one silently
shadow the other, and both confounds fragments declare a view, so this is live. A **multi**
association — one setting `target.entities`, so several targets may resolve per source — merely
makes `(file_id, <axis>_idx)` non-unique, `source_file_id` saying which file each row came from.

### 5. An overlay's association key is namespaced to the table it feeds

fMRIPrep's edge is `fmriprep_confounds`, QSIPrep's `qsiprep_confounds`; neither is a generic
`confounds`. The *view* is not namespaced (`timeseries`, `dwi_timeseries`): that is the name a user
types. For a namespaced key the edge label names the payload table, so the Python accessor defaults
`table = association_type`; base BIDS keys keep the schema's own names (`bval` → `bvals`,
`aslcontext` → `asl_context`) and a caller passes `table=` for those.

### 6. `bvals` and `bvecs` store; `diffusion` composes them

`bvals` is `(file_id, row_idx, b)` and `bvecs` is `(file_id, row_idx, x, y, z)`, both keyed
`(file_id, row_idx)` with a real `FOREIGN KEY (file_id) REFERENCES file_registry(file_id)`.

```sql
CREATE OR REPLACE VIEW diffusion AS
SELECT v.file_id, v.volume_idx, v.b AS bval, c.x AS bvec_x, c.y AS bvec_y, c.z AS bvec_z,
       v.source_file_id AS bval_file_id, c.source_file_id AS bvec_file_id
FROM bval_volumes v LEFT JOIN bvec_volumes c USING (file_id, volume_idx);
```

`diffusion` is hand-written rather than generated. `LEFT JOIN` from the b-values, so they define
the volume axis and a short `.bvec` NULL-fills. A `.bvec` with no `.bval` yields no `diffusion`
rows, but its values are not lost: they are in `bvecs`, keyed by image in `bvec_volumes`. A
gradient file whose image is missing keeps its rows and describes nothing.

### 7. Across datasets, declarations are stored and the relation is a query-time view

Two tables, each keyed by a single `dataset_id`, so each ingest writes only its own rows.
**`dataset_links`** is what a dataset *declares* — one row per `SourceDatasets` entry
(`link_type='source'`), per `--source-dataset` flag (`'declared'`), per `DatasetLinks` mapping
(`'named'`), per `bidslake link alias` (`'alias'`) — carrying the verbatim `declared_ref` and the
canonicalized `identity`/`identity_kind`/`identity_base`. **`dataset_identity`** is what a dataset
*is*: its own `dataset:<id>`, its `DatasetDOI`, each of its `root_uri`s. Neither takes foreign
keys, so a declared source absent from the catalog — the usual case for a derivative — is kept.

The relation itself is the `dataset_relations` view:

- `shares_source` — two datasets declare the same source identity. Needs no `dataset_identity`
  row, which is why it resolves when the shared source is absent. Sharing **any** one identity
  suffices; `via_identity` records which one justified the edge.
- `derived_from` / `source_of` — one dataset declares an identity that another *present* dataset
  *is*.

Depth-1 only: `UNION` dedups and `from <> to` drops self-links, so cycles cannot arise; a consumer
reads the view through `lake.related_datasets(id, relation=…)`, `Relation` naming the three kinds.

### 8. One identity normalization, and every declared reference goes through it

`links::canonicalize` maps any declared reference — a bare DOI, a `https://doi.org/…` URL, a
repository URL, a filesystem/S3 path, a `dataset:<id>` — to a stable `Identity` carrying a `value`,
a `kind` (`doi`/`url`/`file`/`dataset`/`opaque`) and a version-stripped `base`. DOIs are lowercased
once their resolver prefix is stripped, the Handle spec making a handle case-insensitive, which is
what makes MRIQC's bare `10.18112/…` and fMRIPrep's `https://doi.org/10.18112/…` collide. An
unrecognizable reference becomes `opaque:<verbatim>`. Versions are part of the identity — `…v2.0.1`
and `…v2.0.0` do not auto-link — and `identity_base` is stored only so `bidslake link list` can
warn about drift.

A `DatasetLinks` value is resolved against the dataset root by `links::canonicalize_relative_to`,
because BIDS writes those relative (`derivatives/fmriprep`, `../freesurfer`) far more often than
absolute; without it a relative value canonicalizes to `dataset:derivatives/fmriprep`, a `Dataset`
identity no `dataset_identity` row can ever equal, so the link is stored and never resolves.
Absolute forms pass through untouched.

### 9. A link is either provenance or naming, and a query names a link, never a relation

|  | from `dataset_description.json` *(cleared each index)* | user-asserted *(kept)* |
|---|---|---|
| **provenance** — "came from S" | `source` (`SourceDatasets`) | `declared` |
| **naming** — "here, N refers to L" | `named` (`DatasetLinks`) | `alias` |

`clear_derived_links` deletes exactly the left column on every re-index, so rows read out of the
description track the file and the user's survive.

Every arm of `dataset_relations` filters to the provenance types; **naming never produces a
relation**. Naming is resolved separately by **`dataset_link_targets`** (`link_name` →
`target_dataset_id`, NULL until the target is indexed), which is how a caller tells "you never
indexed that" from "you misspelled the name". Self-references are kept there, unlike in
`dataset_relations`: a dataset naming itself is the only way a scope-by-name can mean "my own
dataset".

**A name is resolved relative to the dataset that declared it**, which is what BIDS `DatasetLinks`
already means. `sibling(..., via="freesurfer")` joins `dataset_link_targets` on the anchor's own
`dataset_id`; a link declared in a neighbouring dataset is out of scope.

### 10. Links can be declared by hand, on an existing catalog

`--source-dataset <ref>` on `index` (repeatable) declares a source through the same `canonicalize`:
`--source-dataset ds001761-fmriprep` gives a `dataset:` identity and a `derived_from` edge,
`--source-dataset <doi>` a `shares_source` edge. `bidslake link add/rm` do the same post-hoc,
`link alias --as NAME --target REF` writes the naming counterpart, `link list` prints resolved
relations, dangling declarations and version drift, and `link init` creates both tables and both
views *and backfills declarations from the stored `dataset_description` rows*. Prefer a DOI where
one exists: `shares_source` keys on it, so the edge survives a rename, whereas `dataset_id` is free
text and `--source-dataset <id>` breaks with it.

## Backwards Compatibility

- **A catalog indexed before cross-dataset links needs no re-index**: `bidslake link init <db>`
  creates the tables and views and backfills them, and the Python reader raises with that
  instruction rather than a binder error when `dataset_relations` is absent. Adding a source or
  link target later needs none either — the edge appears on the next query.
- **`diffusion` is a view, and DuckDB cannot replace a table with a same-named view** — `CREATE OR
  REPLACE VIEW` over a table errors, `CREATE VIEW IF NOT EXISTS` silently no-ops. A catalog whose
  `diffusion` is a table must be rebuilt into a new file. Relatedly, `COPY FROM DATABASE (SCHEMA)`
  must emit views in dependency order, since `diffusion` selects from two generated views;
  `test_compact.rs` pins it.

## Rejected Ideas

**A stored `target_dataset_id` column.** A resolved target is a cache whose correctness depends on
what else is in the catalog: every NULL written while a source was absent is wrong the moment that
source is indexed, and only a re-index repairs it. The view makes order irrelevant instead —
`A`-then-`B` and `B`-then-`A` produce byte-identical catalogs
(`test_dataset_links.rs::ingest_order_does_not_matter`).

**Transitive relation chains.** Derivative-of-derivative closure has no caller, and depth-1 is what
makes cycles impossible by construction.

**Matching on the version-stripped identity.** Subjects are added and removed between OpenNeuro
versions, so linking `…v2.0.1` to `…v2.0.0` is a guess dressed as a proof. `identity_base` is kept
for a `link list` warning; `--source-dataset` forces the link where a user knows better.

**Requiring identical source *sets* for `shares_source`.** Set equality fails on real data, since
pipelines list templates, atlases and code URLs beside the real source. The any-identity rule's
honest gap — a dataset derived from A+B relates to one derived from A alone — is bounded by the
consumer's own suffix and entity match.

**Rejecting an unrecognizable reference.** An `Opaque` identity keeps the verbatim string, which
preserves the property that matters: a user who typed the same thing twice gets a match.

**Consuming `RawSources` instead of `Sources`.** `RawSources` is deprecated and its entries are
bare paths with no dataset qualifier, so a target cannot be resolved through `DatasetLinks`;
`Sources` carries BIDS URIs and is what an upstream request to MRIQC asks for.

**"Sibling" as a relation name.** In DataLad a sibling is a remote or clone of the *same* dataset.
These relate *different* datasets by shared provenance, so `shares_source` / `derived_from` /
`source_of` say what is meant and borrow no wrong intuition.

**Resolving a link name catalog-wide.** Unsound in a way a match count cannot catch: two studies
each name their own recon-all tree `fs`, and for a `sub-01` whose recon failed in study A the union
of every `fs` puts study B's tree in scope, where the entity join finds exactly one match — and one
hit is never ambiguous, so the unit silently resolves to another person's anatomical. Pinned on a
fixture catalog aliasing every dataset to every dataset by name, so the name is the only thing
selecting between them (`test_query.py::test_via_scopes_the_sibling_to_the_linked_dataset`).

**One `gradients` table with nullable columns.** Every row would be half-NULL — a `.bval` row with
no vector, a `.bvec` row with no scalar — with the discriminator implicit in the edge type.

**Per-image duplication of gradient rows.** Keying `diffusion` by image and writing the values once
per inheriting image leaves the catalog handling one BIDS rule two ways (`events` shared, gradients
copied), multiplies `ds114`'s rows by 20, and — decisively — makes the *writer* depend on the
association edges, which is what keeps the storage path backend-agnostic.

**Keying `diffusion` on the `.bval` with a `bvec_file_id` column.** One root `dwi.bval` with twenty
per-image `.bvec`s is legal BIDS, and `(bval_file_id, row_idx)` cannot hold twenty triplets.

**Deriving the described file from the describing file's name.** A companion file's relationship to
its data file is the schema's statement, not something to re-derive from filenames.

## Open Issues

- **File-level provenance (`Sources`).** The precise generalization is the BIDS `Sources` field —
  BIDS URIs (`bids:<name>:<path>`) naming exact input files, resolved through `DatasetLinks` —
  letting `file_associations` cross the dataset boundary: a `target_dataset_id`, a widened primary
  key, and corrected `dataset_id`/`root_uri` stamping in `_associated_for`. Deferred because no
  producer available to us emits it (MRIQC emits neither `Sources` nor the deprecated `RawSources`;
  fMRIPrep only `RawSources`), so it cannot be exercised. An upstream request is filed with MRIQC.
- **A `Name` collision merges two datasets.** `dataset_id` defaults to `dataset_description.json`'s
  `Name`, so two datasets sharing one and indexed without `--dataset-id` become a single dataset,
  their links and identities merged with them. Pre-existing, and not fixed here.
- **`*_channels.tsv` should declare `describes`.** The `channels` association exists and the table
  is ordered, so `{"association": "channels", "axis": "channel", "view": …}` would give it the same
  re-keying. Held back so that rewiring `channel_columns`'s positional `SELECT name FROM
  motion_channels … ORDER BY row_idx` — the one place row order is load-bearing inside bidslake —
  is a deliberate change. `*_electrodes`/`coordsystem` are the multi-association case.
- **Pseudo-files cannot be association sources on S3.** `S3Client::walk` ignores `pseudo_exts`, so
  a `.ds`/`.ome.zarr` pseudo-file arrives as many object keys rather than one file.
- **Per-row tables reference `file_registry` by convention.** `bvals`/`bvecs` declare a real
  foreign key; the other per-row tables and `file_associations` do not (see
  [ADR 0006](0006-file-registry.md)).
