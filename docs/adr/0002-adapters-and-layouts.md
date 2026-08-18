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
entities BIDS derivatives already use, so the overlay layer invents no entity at all: it declares
those three, and every other concept this tree projects is one base BIDS already has. The tree is
mostly intermediates — a FIX run leaves roughly 230 of them in `fix/` alone — so the ingestion
fragment's main job is `ignore`, discriminating *within* a directory, since `mc/` and
`filtered_func_data.ica/` each hold one keeper amongst the noise. Measured on a real 27-unit tree:
863 files walked, 404 kept, 0 unmatched by the term map. The keeper in `mc/` is the motion trace,
and it is `read` rather than `catalog` — `matrix` turns it into `feat_motion` — so 27 of those 404
are rows rather than paths.

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

`matrix` is an engine for a different reason, since its files are single and small enough that
batching is beside the point. What a Rust reader would have to write is a delimiter split and a
type coercion, and DuckDB has both. The one thing it does *not* have is a whitespace-*run*
delimiter — `delim` is a fixed string, and these files separate their columns by runs — so the read
takes each line whole and `str_split_regex` does the split in SQL.

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

- the `highres` role and its mapping declare `desc: brain` — FEAT's copy is the brain-extracted
  structural. `desc: preproc` is the entity that would actually *select* fMRIPrep's T1w to fill it,
  and declaring that instead makes `classify(render("highres"))` disagree with the declaration, so
  the layout fails to load (`layout.rs`'s
  `a_declared_entity_the_projection_contradicts_is_rejected`);
- `filtered_func` declares `desc: filtered`, which is what FEAT's own output is, and is *filled*
  from fMRIPrep's `desc-preproc` BOLD. Both are correct; they describe different files;
- `example_func` declares `desc: exfunc`, the label the six transform roles already give this
  image's space. As a *filter* it selects nothing: the file that fills it is derived, not fetched —
  FEAT takes the halfway volume of the input run with `fslroi`.

This is a real constraint, not an oversight to be fixed by enriching the documents: a role and its
mapping gaining entities together moves the destination description, never supplies the source one.
Enriching them is worth doing on its own terms — every role now declares what it becomes, so a tree
this layout wrote is classified by what each file *is* rather than inheriting the unit directory's
`desc-preproc` — and it moved nothing about the source problem.

### On what a FEAT filename can become

The write direction can name more than the read direction can carry, and FIX is where the two come
apart. `fix4melview_{training}_thr{threshold}_{rater}.txt` carries three variables and BIDS has an
entity for none of them: none means "training set", none means "threshold", none means "rater". One
is worse than homeless. A BIDS label is alphanumeric, and `training` is a model *filename* — 7 of
the 9 models FSL ships spell theirs with an underscore (`HCP25_hp2000`, `WhII_MB6`,
`HCP_Style_Single_Multirun_Dedrift`) — so no entity, invented or otherwise, could hold it verbatim.

So the layout binds all three as `Template` placeholders, which is what a pipeline needs to *write*
the file, and the term map projects a `desc` of `auto` or `manual`, which is what a query needs to
*find* it: the automatic/hand-edited split is the one distinction here that is closed, two-valued,
and answerable without knowing a site's rater labels. The other two stay in the filename, and the
role `Description`s say so.

Binding `desc` to the training set instead, which is what the mapping did first, cost more than it
bought: the alphanumeric capture it needed matched none of those seven models, so the *common* FIX
output was the one that classified as nothing at all. Widening the token and not capturing it is what
fixed that. The price is a real one — two thresholds of one training set, or two raters of one unit,
now differ only by `file_path` — and it is the price of a concept space no wider than BIDS.

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

One overlay is not a producer's: `overlays/bep011.json` is applied to every load of the embedded
schema, the way `ingestion/base.json` is layered under every ingest, and is filtered out of the
adapter names a caller can select. It exists because a *shared* vocabulary has no owner among the
producers. fMRIPrep writes `_hemi-L_thickness.shape.gii` and FreeSurfer writes `surf/lh.thickness`,
and those are the same quantity; before it, neither name was vocabulary the schema could state, so
the same cortical thickness map answered to `suffix='thickness'` from one tool and only to a
`file_path LIKE` from the other. Put in either producer's overlay it would have to be duplicated
into both, and a caller indexing a `recon-all` tree would have to name a draft BEP to get thickness
back.

**It is a mirror, and mirroring is the whole discipline.** Its content is BEP-011 "Structural
derivatives" (bids-standard/bids-specification#518) transcribed from the branch, expanded out of
the `$ref`s a compiled render does not carry — 17 suffixes, three GIFTI/CIFTI extensions, and the
`rules.files.deriv.structural_mri` groups. Nothing in it is improved relative to upstream, because
the exit is a deletion: when `third_party/bids-schema` is bumped past BEP-011, an identical
upstream render makes the overlay inert and an edited one makes it an `OverlayError::Conflict`
naming the pointer that drifted. A paraphrase would turn that clean deletion into a merge.

This is also the answer to *what may an overlay declare* that the Rejected Ideas below leave
implicit. The bar is not the kind of term — it is whether the term is bidslake's invention. An
entity for FIX's training set is refused because BIDS has no such concept and is not getting one;
BEP-011's suffixes are adopted, unchanged, because BIDS is adopting them.

Declaring `rules.files` surfaced that the validator was choosing rules on incomplete evidence.
`SuffixRule::match_context` identifies a rule by suffix alone — faithfully, since the reference
validator does the same — and the downstream checks are what narrow it. Only the entity check
existed: `extensions` was parsed onto the rule struct and never read, and there was no
`EXTENSION_MISMATCH`. That is invisible while every suffix belongs to one rule, and wrong as soon
as one does not. BEP-011 gives `thickness` a `.shape.gii` rule requiring `hemisphere` and a
`.dscalar.nii` rule that does not; with nothing distinguishing them, both scored zero and a GIFTI
file was judged against whichever the walk reached first.

So the crate now follows the reference validator's shape. Identification stays loose — a suffix
rule is claimed by its suffix alone — and `hasMatch`'s two narrowing passes reduce what survives:
first to the rules whose `datatypes` include the directory the file sits in, then to those whose
entities and extension fit it. Both `extensionMismatch` and `datatypeMismatch` were added as
checks, so the file is finally held to what the surviving rules actually say.

**Each narrowing pass is discarded if it would eliminate everything**, and that guard is what makes
the scheme safe rather than clever. The compiled schema renders an unspecialized parent rule with
`datatypes: []` — matching nothing — beside the specializations carrying the real lists:
`electrodes` has six rules, two empty parents plus `[eeg, ieeg]`, `[meg]` and `[emg]`. Without the
guard a file whose datatype matches no rule loses every candidate; with it, narrowing that says
nothing is simply not applied. The same guard is what lets `bep011.json` stay a verbatim mirror:
BEP-011's rules inherit an entity list without `space`, `density` or `description`, so a real
`_space-fsLR_den-32k_thickness.dscalar.nii` fits no rule on entities, that pass empties, and the
file is judged on the rest rather than rejected over a gap in a draft.

Two schema spellings need honouring that a literal membership test misses: `.*`
(`objects.extensions.Any`, "any extension is allowed"), and the trailing slash on a pseudo-file
extension (`.ds/`, `.ome.zarr/`), which names a directory while the parsed extension of the path
has none. Missing the second reported every CTF and OME-Zarr recording in `bids-examples` as a
mismatch.

**One deliberate divergence remains, at the end.** Upstream checks every rule that survives
narrowing and reports all of them, on the stated principle that a wrongly-named file deserves as
much feedback as possible. This crate reports the quietest survivor instead. That principle is
right for a wrong name and wrong for a right one: `sub-01_acq-crosstalk_meg.fif` is claimed by both
`raw.meg.meg` and `raw.meg.crosstalk`, entities-and-extensions cannot separate them, and the first
requires a `task` a crosstalk file has no business carrying — so reporting both fails `ds000248`, a
canonical dataset. The integration tests here hold every vendored example to zero errors, a
stronger claim than upstream makes and worth more than matching its noise.

`bep011.json` is the first overlay to declare `rules.files`, which has a consequence worth stating:
`bids-validator-rs` reads `bids_schema::SCHEMA_JSON` directly and shares no schema loading with the
indexer, so the merge happens in `BidsSchema::bundled()` as well as `Schema::load_full`. Both, or
the rules have no reader. It is not carried in `Schema::overlays()`: that list is what
`bidslake_overlays` and `overlay_digest` record, and it answers what the *caller* augmented a
catalog with — an entry every catalog gets would make the answer never "nothing". The full merged
document is stamped as `effective_schema` regardless, so nothing about reproducibility rests on it.

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

A rule names a disposition and a reader *name*; the engines are unchanged. Three names are
engines, which hand the file to DuckDB: `csv` is the batched tabular ingest, `diffusion` the
bval/bvec accumulator, and `matrix` a headerless whitespace-delimited read. One is a per-file
`ContentReader`: `fs_stats`.

**A reader does not name its table.** The file's projected concepts route through
`rules.tabular_data` — the same routing the `.tsv` path does — and the reader is handed the answer,
along with that table's columns *in declared order*. For `matrix` that order is the whole format:
column *i* of the table is field *i* of the line, `initial_columns` is where the order is stated,
and `TRY_CAST` to each column's declared type is the typing. So FSL's `mcflirt` motion parameters
and FreeSurfer's `.ctab` colour tables — both headerless, one separated by two spaces and the other
column-aligned — are described in JSON alone, with no reader in Rust. Nothing is dropped for being
malformed: a short or unparseable line becomes a NULL row rather than no row, because on a
positional table the ordinal is the alignment to the file the rows describe, and a dropped line
shifts every later one silently.

A reader names a table only where the routing cannot reach one, which is a narrower case than it
first appears: not "the table depends on the contents" but "one file holds two payloads". A
`?h.aparc.stats` yields both per-structure rows — whose table its projected suffix decides — and
`# Measure` scalars, which belong in `freesurfer_measures`; no projection of one path yields two
tables, so `fs_stats` states that one name. It stays a reader for a second reason too: its column
names are in the file, on the `# ColHeaders` line, and matching them by name survives a FreeSurfer
release reordering a column where positions would quietly mis-assign.

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

**An overlay-declared entity for FIX's rater, training set, or threshold.** An overlay may extend the
BIDS vocabulary ([ADR 0001](0001-schema-augmentation-overlays.md)), and `rater` was declared that way
at first, so this is a removal and not a road not taken. An entity BIDS does not have widens a
catalog's concept space past BIDS and earns a per-read `COALESCE` on a column only that overlay's
schema emits — for a training set no label could hold anyway, and for two discriminators whose one
queryable distinction fits in `desc` (*On what a FEAT filename can become*). What the overlays do keep
declaring is suffixes, extensions and columns — vocabulary for a file's *shape*, each already current
in the derivative ecosystem (`xfm`, `boldref`, `mixing`, `timeseries`).

**`{name}` placeholders inside a role's `Entities`, interpolated from `Bindings` at load time.** Five
lines in `validate_round_trip`, and the round trip would then check `desc == {training}` under each
example. With `desc` carrying a literal there is nothing varying left for the two `classification`
roles to declare, so it would be a mechanism with no caller; the training-set capture that seemed to
need it is rejected above.

## Open Issues

- BEP-043 has not settled between PCRE and `{var}`. If it settles on `{var}`, the bundled term maps
  need a rewrite, versioned by `BIDSMapVersion`; the engine boundary is small.
- Selecting the file that fills a role has no home. It cannot be the layout — a role describes its
  destination, not its source (*On roles as destinations, not sources*) — and no other document
  currently declares it.
- `feat` and `freesurfer` are the only layouts. fMRIPrep, MRIQC and QSIPrep have read-side
  artifacts but no write direction, so code producing files in their conventions still hardcodes
  paths.
- **`invalidLocation` is the fourth `ruleCheck` and has no counterpart here**, so nothing asserts
  that a file's `sub-`/`ses-` entities agree with the directories it sits in. Same shape as the two
  that were added, and wants its own corpus measurement.
- **Reporting only the quietest surviving rule hides detail in the wrong-name case.** It is the
  right trade for the corpus, but a file that misses several rules by a little reports as missing
  one by a little. If `MISSING_REQUIRED_ENTITY` and `ENTITY_NOT_IN_RULE` are ever split out as
  distinct codes the way upstream has them, this is worth revisiting — reporting every survivor
  *except* where one fits cleanly would get both.
- A layout used to *produce* a dataset is not recorded as provenance anywhere.
