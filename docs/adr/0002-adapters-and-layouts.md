# ADR 0002 — Adapters and layouts: reading and writing a producer's tree

```
ADR: 0002
Title: Adapters and layouts: reading and writing a producer's tree
Status: Provisional
Type: Design
Created: 14-Jul-2026
Requires: 0001
```

## Abstract

bidslake knows a data producer — FreeSurfer, FSL's FEAT/MELODIC/FIX, fMRIPrep — as a *named bundle*
of small, separately validated documents rather than as code. Three face the read direction and
are resolved by `--adapter <name>`: an overlay supplies vocabulary, a BEP-043 term map projects a
path onto BIDS concepts, and an ingestion fragment says what bidslake does with the file. The
fourth faces the write direction: a layout names the slots — *roles* — of a tree a pipeline is
about to produce, and is checked against the term map that reads that tree back.

## Motivation

A great deal of neuroimaging data lives in outputs that are standardized but not BIDS: FreeSurfer
`recon-all`, FSL FEAT directories, HCP trees, the `sourcedata/freesurfer` tree fMRIPrep itself
ships. Querying those beside BIDS data — one catalog, one set of concepts — is bidslake's core
advantage: `recon-all` becomes typed tables (`freesurfer_aseg`, `freesurfer_aparc`,
`freesurfer_measures`, `freesurfer_labels`) joined to BIDS data by `sub`/`ses`, with no per-dataset
code. Three separate things stand in the way.

**Vocabulary is not identity.** An overlay ([ADR 0001](0001-schema-augmentation-overlays.md))
extends the BIDS vocabulary: entities, suffixes, columns, table shapes. But every downstream
generator identifies a file by parsing BIDS entities out of its *filename*, and `stats/aseg.stats`
and `surf/lh.thickness` have no entities to parse. Their identity is their position in the tree
(`<subject>/stats/aseg.stats`). No amount of added vocabulary fixes a file whose name says nothing.

**BIDS has no opinion about databases.** Whether a file's contents are parsed into a table, whether
its row order is load-bearing, whether it should be skipped outright — BIDS answers none of it,
because BIDS has no database. Left to code, those answers are a scatter of predicates (a `.tsv`
gate, a `.bval`/`.bvec` branch, a `.tsv.gz` special case, an order-insensitivity list), every one of
which some producer eventually wants changed.

**Nothing names a position in a tree before that tree exists.** Many tools do not take a list of
files; they take a *directory*, organized a particular way, and read fixed positions inside it. FIX
is handed a FEAT directory, looks for `filtered_func_data.nii.gz`, `mask.nii.gz`,
`reg/highres.nii.gz` and `mc/prefiltered_func_data_mcf.par`, and writes its results back into the
same tree. Such trees are de-facto standards — nothing specifies them; the convention hardened
because a widely used tool reads and writes it — so they can be described but not looked up.
Adapters are for indexing, layouts are for writing de-facto standards. A term map answers what an
*existing* path denotes, which is no use to a pipeline naming a file it has not written yet, and
without a document for that every consumer hardcodes the convention and grows two dozen properties
that are only string joins.

## Rationale

### On pinning PCRE

BEP-043 floats two `Template` syntaxes — `{var}` interpolation and PCRE — and has not settled.
Named capture groups collapse combinatorics that `{var}` has to enumerate: the one template

```
(?:sub-)?(?P<subject>[0-9A-Za-z]+)(?:_ses-(?P<session>[0-9A-Za-z]+))?/stats/aseg\.stats
```

matches all three subject-directory conventions in the wild (`sub-01_ses-1`, `sub-01`, and bare
`01`/`bert`). The `regex` crate covers the subset this needs: named groups, optional groups,
character classes, no look-around or back-references.

### On deriving the projectable set

**Which columns get the fallback is derived, not declared.** A term map already states what it can
project — its literal `Entities`, its named capture groups, its `Concepts` — so
`TermMap::projectable_concepts` reads the set off the artifact rather than asking an author to
restate it in a second place. That is not tidiness: every registry column that falls back to a
projection pays a `COALESCE` on read ([ADR 0006](0006-file-registry.md)), and wrapping all 43
concept columns measured ~7.6% on the `SELECT *`-plus-filter shape `bidslake-py`'s `get()` issues,
against ~1.8% for the seven FreeSurfer can supply.

### On a tree that is mostly scratch

FSL's FEAT/MELODIC/FIX tree exercises a different part of the mechanism than FreeSurfer's. Its
files are identified purely by position (`reg/highres.nii.gz`,
`filtered_func_data.ica/melodic_mix`) inside a directory named after the BIDS stem of the run it
was built from, so the unit's entities come from the directory and each file's role from the
projection — and FSL's `<from>2<to>` transform naming maps directly onto the `from`/`to`/`mode`
entities BIDS derivatives already use, so the overlay layer needs to invent little. The tree is
mostly intermediates — a FIX run leaves roughly 230 of them in `fix/` alone — so the ingestion
fragment's main job is `ignore`, discriminating *within* a directory, since `mc/` and
`filtered_func_data.ica/` each hold one keeper amongst the noise. Measured on a real 27-unit tree:
863 files walked, 404 cataloged, 0 unmatched by the term map.

### On the schema deciding and the engines executing

The `.tsv` path is batched — files are grouped by header and ingested with one `read_csv` over many
— while adapter readers are per-file. Routing `.tsv` through a per-file reader, the naive reading of
"make it schema-driven", would un-batch the hot path, so the schema names a *reader* and the
existing engine does the work. That costs nothing on the hot path: schema-driven dispatch measures
3–8% *faster* than a hardcoded predicate chain (a `.tsv` gate plus a `.bval`/`.bvec` branch) on
the batched-ingest benchmark (ds001/002/108/114, criterion A/B), because the JSON short-circuit
also skips a per-file `find_datatype`. The per-file selector evaluation it adds is bounded from
both sides: data files never reach the dispatch, and selector expressions are parsed once and
cached in `bids_schema::expression`, worth 25–30% of validator runtime measured over ds007 and
7t_trt in July 2026.

### On invertibility

The collapsing that makes PCRE worth pinning is exactly what makes a term map non-invertible: there
is no single filename to render `(?:sub-)?(?P<subject>[0-9A-Za-z]+)` back into. Switching the read
syntax to `{var}` does not rescue the write direction, because **invertibility is a property of the
mapping, not of the syntax**: a mapping that recognizes a whole *class* of filenames (`mri/*.mgz`,
`label/*.annot`) has no concept to render *from*. Measured in July 2026 on a `recon-all` tree of 657
files, an 8-mapping PCRE FreeSurfer term map recognized all 657; a pure-`{var}` rewrite of the same
coverage needed 12 mappings, recognized 430, lost 227, and over-matched where alternation was doing
work, labelling `mri/T1.mgz` as `seg=T1`.

### On roles as destinations, not sources

A role's `Concepts` and `Entities` describe the file *once it sits at the role's path*, so they
cannot double as a catalog query for the file that will be copied or computed into that slot. The
bundled `feat` pair is the evidence:

- the term map's mapping for `reg/highres.nii.gz` declares `{datatype: anat, suffix: T1w}` and no
  entities at all. Adding `desc: preproc` to the `highres` role — the entity that would actually
  select fMRIPrep's T1w — makes `classify(render("highres"))` disagree with the declaration, and the
  layout fails to load;
- `filtered_func` declares `desc: filtered`, which is what FEAT's own output is, and is *filled*
  from fMRIPrep's `desc-preproc` BOLD. Both are correct; they describe different files;
- `highres` and `example_func` declare no `Entities`, so as filters they would match every `space-*`
  resampling of the same image — ambiguous on every unit.

This is a real constraint, not an oversight to be fixed by enriching the documents: a role and its
mapping gaining entities together moves the destination description, never supplies the source one.

### On filling a role

Filling a tree is mostly not placement. Measured on a MELODIC/FIX pipeline, of 15 roles filled
before FIX runs, 4 are a copy; the rest come from 11 external tool invocations —
`antsApplyTransforms`, `convert_xfm`, `convertwarp`, `CompositeTransformUtil`, `wb_command` — plus
in-Python derivation (a temporal mean, a trimmed motion matrix, ITK→FSL transform conversions). A
`(destination, source)` staging plan would therefore cover about a quarter of the work, and the
`wmparc` role being `reg/wmparc.nii.gz` while its FreeSurfer source is `wmparc.mgz` is the normal
case rather than an incompatibility to detect: the tool that fills it resamples and converts in one
step.

Placeholders bind at `under()` and not only per call because they are configuration for a whole run:
that is what lets `for role in layout.roles: at[role]` enumerate a tree at all.

## Specification

A producer is described by up to four documents, all under `crates/bids-schema/data/`:

| Artifact | Document | Answers | Metaschema |
|---|---|---|---|
| Overlay | `overlays/<name>.json` | What vocabulary and table shapes exist? | BIDS metaschema |
| Term map | `term-maps/<name>.json` | What concepts does this path denote? | `term-mapping-metaschema.json` |
| Ingestion fragment | `ingestion/<name>.json` | What does bidslake do with the file? | `ingestion-metaschema.json` |
| Layout | `layouts/<name>.json` | Where does a file this producer writes go? | `layout-metaschema.json` |

An overlay is ordinary BIDS and is all a BIDS-named derivative needs: fMRIPrep, MRIQC and QSIPrep
ship overlays and no term map. Overlay and term map are standards-track; the ingestion fragment and
the layout are deliberately bidslake's own.

### 1. An adapter is a named bundle of optional, single-purpose artifacts

`--adapter <name>` resolves whichever of the three read-side artifacts bidslake ships under that
name; a name resolves if at least one exists. `--overlay` takes file paths only, so a bundled
producer has exactly one way in. `<input>/.bidslake/overlay.json` and
`<input>/.bidslake/ingestion.json` are the hand-written counterpart, and the only route for a
producer bidslake does not bundle; the embedded ingestion fragment is applied last, so a dataset can
adjust the policy for its own contents.

### 2. A term map is an unmodified BEP-043 document

A term map carries `BIDSVersion` / `BIDSMapVersion` / `Mappings[]`, each mapping a `Template` plus
`Entities` / `Concepts` / `Metadata`. bidslake adds no keys of its own, so a bundled term map is a
valid BEP-043 document that could be contributed upstream. Templates are PCRE and are **anchored**
(`^(?:…)$`) against the dataset-relative path, so no mapping can read a leading `sourcedata` or
`derivatives` component as a subject label. Named capture groups bind BIDS entities, aliased from
BEP-043's long spellings to BIDS short keys (`subject`→`sub`, `session`→`ses`); literal `Entities`
override or add to them, aliased on the same terms; `Concepts` set `datatype` and `suffix`.
`extension` always comes from the filename. Mappings are matched as a `RegexSet` and the lowest
matching index wins, so specificity is expressed by ordering a narrow mapping before a catch-all.

The anchor has an operational consequence. A FreeSurfer tree nested at
`<fmriprep>/sourcedata/freesurfer/` is **not** picked up by indexing the fMRIPrep root: the ingest
walk reaches those files — it prunes no directory as opaque, unlike the validator — but the
`sourcedata/freesurfer/` prefix defeats the anchor. Such a tree is indexed as its own dataset under
its own `--dataset-id`, and both accumulate in one database.

`TermMap::classify(rel_path) -> Option<FileFacts>` is the analog of `read_entities` for a path with
no BIDS name. It is IO-free and shared, which is what lets the validator use it too (§7). What the
catalog stores of a projection, and how a registry column falls back to it, is
[ADR 0006](0006-file-registry.md).

### 3. The ingestion schema decides read, catalog, or ignore

Three dispositions, chosen by the first matching rule:

- **read** — parse the contents into a data table via the named reader;
- **catalog** — record the file, contents unread, left on disk. Compressed continuous recordings
  (`*_physio.tsv.gz`), too large to ingest row-per-sample, are `catalog` and not `ignore`;
- **ignore** — skip it entirely, and record nothing. The declarative `.bidsignore` override, and the
  one disposition that yields no registry row.

`crates/bids-schema/data/ingestion/base.json` is the BIDS default and is applied on every ingest; an
adapter contributes a fragment merged onto it. Rules select with the BIDS selector-expression
language over *projected* concepts, reusing the evaluator `Tabular::route` uses, so even bidslake's
private layer refers back to the BIDS schema rather than inventing a vocabulary. Per-table policy
lives beside the rules: `concepts` (which BIDS concepts a reader's table materializes as physical
columns), `ordered` (whether source row order is load-bearing), `undeclared` (see
[ADR 0004](0004-undeclared-column-policy.md)), and `describes` (which association fans a table's
rows out to the data files they are about).

Rules key on the *projected* `suffix` rather than on a computed extension, and that is load-bearing:
`filename_extension` uses BIDS first-dot semantics, so `lh.aparc.stats` has extension
`.aparc.stats`, not `.stats`.

### 4. The schema decides; the existing engines execute

A rule names a disposition and a reader *name*; the engines are unchanged. `reader: "csv"` is the
batched tabular ingest, `reader: "diffusion"` the bval/bvec accumulator, `reader: "fs_stats"` and
`reader: "fs_ctab"` per-file `ContentReader`s. A multi-table reader's internal routing stays
reader-internal — `fs_stats` picks `freesurfer_aseg` or `freesurfer_aparc` by inspecting
`ColHeaders`, because choosing the table requires parsing the body.

Two structural guards bound the dispatch. Primary data files are recognized by structure (`kind_of`
over the parent datatype) and short-circuit *before* it, so imaging files are cataloged by structure
rather than by policy. And `datatype` is deliberately left unbound at the BIDS dispatch, so a
configured adapter's datatype-keyed rules cannot claim ordinary BIDS files.

### 5. Every artifact is validated on its own against a hand-written metaschema

The metaschemas are hand-written JSON Schema (draft 2020-12) checked with the `jsonschema` crate,
mirroring the overlay metaschema of [ADR 0001](0001-schema-augmentation-overlays.md). Each
document is validated on its own rather than after merging. BEP-043 has no official JSON Schema, so
`term-mapping-metaschema.json` is bidslake's.

### 6. A catalog records the artifacts that shaped it

Alongside `bidslake_overlays`, a catalog carries `bidslake_term_maps` and `bidslake_ingestion`:
every document that shaped the ingest travels with the data. A layout is not among them and does not
need to be — it contributes nothing to the DDL and is consulted before there is a catalog to stamp.

### 7. The validator treats a term-mapped file as expected

`ValidatorConfig.adapters` names bundled term maps; an otherwise-unmatched file that one of them
claims suppresses the `NotIncluded` issue rather than being reported as not part of BIDS.
Recognition walks each `/`-suffix of the path, so a nested derivative
(`derivatives/fmriprep/sourcedata/freesurfer/…`) still matches a subject-anchored template. This is
the one place the anchoring of §2 is deliberately relaxed.

### 8. A layout declares where a producer's tree puts each role

A layout belongs to the same named bundle as a producer's other three documents but is not one of
the three `--adapter` resolves. It names the term map that reads back what it writes, and is
reached by name from the query side: `bidslake.layout("feat")` loads, validates and renders with no
catalog open and no data on disk.

### 9. A role is a named slot, and rendering one is exact or refused

> A **role** is the stable name for a slot in a standardized directory — "the highres-to-standard
> affine" — independent of the filename convention that expresses it.

A role carries a `Template` (a POSIX path relative to a unit's output root, with `{name}`
placeholders), optional `Concepts` (`datatype`/`suffix`) and `Entities` that the term map must
project onto the rendered path, and a `Description` for humans. `feat` declares 23. Role names are
the consumer API — `out["highres2standard_mat"]` — and the unit of reuse: the same name means the
same slot across every tool, script and document that speaks about the tree.

Rendering is total or it fails: an unknown role, or a `{placeholder}` nothing has bound, yields no
path rather than a plausible one pointing at the wrong file — `Layout::render` returns `None`, and
the Python `path()` raises `KeyError`. Placeholders are bound per call
(`path("classification", training=…, threshold=…)`) or once alongside the root
(`layout("feat").under(root, training="UKBiobank", threshold="1")`), with a per-call keyword
winning over a bound one. A template, and again the interpolated result, must be relative and free
of any `..` segment: bindings are substituted verbatim, and a root may be an `s3://` URI where
joining is concatenation and `..` addresses an object rather than a parent.

### 10. `Examples` bind the two directions together at load time

Every layout declares `Examples`, and loading one renders **every role under every example** and
feeds each result back through the named term map — one render-and-classify per role per example,
paid at load time and never at index time. If `classify(render(role))` does not reproduce the
role's declared `Concepts` and `Entities`, the layout fails to load. An example that does not bind
a role's placeholders skips it, so another example must cover that role. A layout therefore cannot
name a term map that does not exist, and a producer with no term map cannot have a layout.

### 11. Filling a role is the caller's work; the layout names and observes

bidslake offers no staging plan. A binding renders paths, creates a role's parent directory on
request (`mkdir`, which is why `reg/` and `mc/` need no declarations of their own), and reports what
has arrived: `states`/`present`/`state` give each renderable role's existence with the size and
mtime the ingest records in `all_files`, and `digest` hashes those into a cache key for a
content-addressed workflow engine. A role whose placeholders nothing has bound is omitted from those
answers rather than reported absent, because unaddressable and missing are different answers. A
role's declared extension is what will be *written* there, not what to go looking for.

## Backwards Compatibility

A plain BIDS catalog is unaffected in every byte: with no term map configured the
projectable-concept set is empty and the emitted DDL is unchanged.

Adding an adapter to an existing catalog is the one migration, and the rule that makes run order
irrelevant is that **the adapter set describes the catalog, not the dataset being added**: pass
every adapter the catalog uses on every index run, or index into a fresh catalog. What a catalog's
first run freezes, and which later run is refused for it, is [ADR 0006](0006-file-registry.md).

## Rejected Ideas

**One bespoke DSL for the whole bundle.** `x_bidslake` keys, `action: read` and `x_bidslake_tables`
hung off the schema do work, but they put three independent concerns in one artifact, with no path
into any standard and a parser and validator of their own to carry.

**LinkML for the new metaschemas.** BIDS itself uses hand-written JSON Schema; LinkML in BIDS is a
single unmerged experimental PR.

**`--overlay <name>` for a bundled producer.** It would apply that producer's vocabulary while
silently omitting its ingestion policy. Names resolve through `--adapter` only, and passing a
bundled name to `--overlay` says so.

**An `identity` field on an ingestion rule** (`per_file`/`per_row`/`per_entity`). It conflates
three things that are already structural: the registry is always per-file, a `read` is always
per-row, and per-entity is a BIDS relational notion expressed by `index_columns`.

**A `sort_by` key.** Only `ordered` is needed.

**A second ingestion-policy key naming the projectable set.** It would be a synonym for the
per-table `concepts` key — both answer which concepts come from projection rather than the filename
— and the ingestion metaschema excludes the registry from `tables` outright. The set is read off
the term map instead, on the argument under *On deriving the projectable set*.

**A side table joined at query time**, holding what a term map projected. It splits "what is this
file?" across two places, so `get()` must either keep returning nothing for a projected concept or
silently learn to join, and derived columns like `modality` would need re-implementing in the view.
Projection and the concepts derived from it stay in one relation
([ADR 0006](0006-file-registry.md)).

**Rebuilding the registry in place instead of refusing a widening run.** It would lift the
constraint entirely, and nothing here settles against it: the registry's shape belongs to
[ADR 0006](0006-file-registry.md), which carries the rebuild as an open issue with the cost that
keeps it open. What this record settles is that configuring an adapter does not attempt it.

**`$schema` fields on the artifacts.** The URIs are unhosted, so they help no IDE, and the BIDS
metaschema's top-level `additionalProperties: false` forbids one on an overlay anyway.

**`{var}` templates instead of PCRE**, on the coverage numbers under *On invertibility*.

**One document holding both directions.** Co-locating the read and write descriptions of a tree
would prevent only *textual* drift. The `Examples` round trip prevents semantic drift: without it, a
file the pipeline writes that the term map cannot classify is produced and then silently ignored at
index time, and `reg/wmparc.nii.gz` in the bundled `feat` pair is that case.

**A `Render` field on the BEP-043 `Mapping`.** It would spend the term map's "no bidslake keys,
contributable upstream" property to solve a problem only half its mappings have, and it cannot
solve it for the other half at all.

**Roles as catalog selectors**, on the `feat` evidence under *On roles as destinations, not
sources*.

**A `(destination, source)` staging plan.** It is the wrong abstraction for filling a tree: 4 of 15
roles on a MELODIC/FIX pipeline are a copy.

## Open Issues

- BEP-043 has not settled between PCRE and `{var}`. If it settles on `{var}`, the bundled term maps
  need a rewrite, versioned by `BIDSMapVersion`; the engine boundary is small.
- Selecting the file that fills a role has no home. It cannot be the layout — a role describes its
  destination, not its source (*On roles as destinations, not sources*) — and no other document
  currently declares it.
- `feat` is the only layout. fMRIPrep, MRIQC and QSIPrep have read-side artifacts but no write
  direction, so code producing files in their conventions still hardcodes paths.
- A layout used to *produce* a dataset is not recorded as provenance anywhere.
