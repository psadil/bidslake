# ADR 0008 — Layouts: a directory as a tool interface

Status: accepted (2026-08-13)

Relates to: `crates/bids-schema/src/layout.rs`, `data/layout-metaschema.json`,
`data/layouts/feat.json`, and the query surface (`crates/bidslake-py/python/bidslake/layouts.py`).
Extracts and replaces [ADR 0002](0002-layout-adapters.md) §12, which introduced the artifact as a
subsection of the adapter ADR.

## Context

A great many analysis tools do not take a list of files. They take a **directory**, organized a
particular way, and read what they need from fixed positions inside it. FSL's MELODIC and FIX are
the clearest examples: FIX is handed a FEAT directory and looks for `filtered_func_data.nii.gz`,
`mask.nii.gz`, `reg/highres.nii.gz` and `mc/prefiltered_func_data_mcf.par` — and writes its results
back into that same tree. The directory *is* the interface.

These trees are **de-facto standards**. Nothing specifies them; the convention hardened because a
widely-used tool reads and writes it, and everything downstream had to agree. That is the character
BIDS has by design for raw data and that a FEAT directory has by accident — and it is the whole
reason bidslake needs a document for them. A tool whose interface is a collection of files is
standardized in practice whether or not anyone wrote it down, so it can be described; it just
cannot be looked up.

The artifact that describes one arrived in ADR 0002 §12, alongside the FEAT adapter. That was the
wrong home. An adapter answers *"how do I read someone else's tree into a catalog?"*; a layout
answers *"where does a role go in a tree I am about to write?"*. They are consulted at different
times by different code, and ADR 0002 itself already records the split — §9 states that a layout is
deliberately **not** stamped into a catalog, and §1 that it sits outside `--adapter` resolution and
is reached by name from the query side (`bidslake.layout("feat")`).

Meanwhile the central noun is undocumented. **Role** is a struct in `layout.rs:109` and a
`patternProperties` entry in `layout-metaschema.json`, and appears in no ADR at all — so the one
concept a consumer actually types (`out["highres2standard_mat"]`) has no design record.

This ADR gives the layout its own home, and states two properties that were implicit and have since
been mistaken for their opposites.

## Decisions

### 1. A layout describes a de-facto standard, and is not an adapter artifact

A layout is a first-class artifact in its own right: `data/layouts/<name>.json`, validated against
`layout-metaschema.json`. It belongs to the same *named bundle* as a producer's overlay, term map
and ingestion fragment, but it is not one of the three that `--adapter` resolves, because it is
consulted before there is a catalog to index.

The distinction that makes this more than filing: the other three artifacts describe **how bidslake
reads what already exists**. A layout describes **what a tool expects to exist**, which is a
statement about the tool, not about any dataset. A layout is meaningful with no catalog open and no
data on disk.

### 2. A Role is the stable name for a slot in the tree

> A **role** is the stable name for a slot in a standardized directory — "the
> highres-to-standard affine" — independent of the filename convention that expresses it.

A role carries a `Template` (a POSIX path relative to the unit's output root, with `{name}`
placeholders), optional `Concepts` (`datatype`/`suffix`) and `Entities` that the term map must
project onto the rendered path, and a `Description` for humans. `data/layouts/feat.json` declares 23.

Role names are what consumers type, and they are the unit of reuse: the same name means the same
slot across every tool, script and document that speaks about the tree. This is the reason the
layout exists — the alternative, recorded in the FSL consumer that motivated it, is "two dozen
properties that are only string joins."

### 3. The write direction is a separate artifact, because invertibility is per-mapping

*(Carried over from ADR 0002 §12; the reasoning is unchanged and the measurements stand.)*

A term map (BEP-043) pins PCRE for the read direction, because optional groups collapse
FreeSurfer's `sub-01_ses-1` / `sub-01` / bare `bert` forms into one mapping. That collapsing is
precisely what makes them non-invertible, so a term map cannot simply be run backwards to name a
file before it exists.

Swapping PCRE for BEP-043's other floated `{var}` syntax does not rescue it, because
**invertibility is a property of the mapping, not of the syntax**: a mapping that recognizes a whole
*class* of filenames (`mri/*.mgz`, `label/*.annot`) has no concept to render *from*. Measured on a
real `recon-all` tree of 657 files, the 8 PCRE mappings recognize all of them; a pure-`{var}`
rewrite needs 12 mappings, recognizes 430, and loses 227 — and over-matches where alternation was
doing work, labelling `mri/T1.mgz` as `seg=T1`.

So the read direction keeps PCRE and stays read-only, and the roles that *can* be written are
declared separately.

*Rejected:* replacing PCRE with `{var}`, on the numbers above. *Rejected:* a `Render` field on the
BEP-043 `Mapping`, which would spend the "no bidslake keys, contributable upstream" property of the
term map to solve a problem only half its mappings have.

### 4. `Examples` are what stop the two directions drifting

Every layout declares `Examples`, and loading one renders **every role under every example** and
feeds the result back through the named term map. If `classify(render(role))` does not reproduce the
role's declared `Concepts` and `Entities`, the layout fails to load
(`Layout::validate_round_trip`, `layout.rs:188`). An example that does not bind a role's
placeholders skips it, so another example must cover it.

The check is not decoration. It immediately caught `reg/wmparc.nii.gz` — a file the pipeline writes
that the `feat` term map had no mapping for, so those files were being produced and then silently
ignored at index time. Co-locating the two directions in one document would have prevented only
*textual* drift; this prevents semantic drift.

### 5. A role names a **destination**, not a source — so roles are not selectors

A role's `Concepts` and `Entities` describe the file **once it sits at the role's path**. They are
therefore not usable as a catalog query for the file that will be *copied or computed into* that
slot, and the round-trip check of §4 actively enforces this.

The evidence, on the bundled `feat` pair:

- The term map's mapping for `reg/highres.nii.gz` declares `{datatype: anat, suffix: T1w}` and
  **no entities at all**. Adding `desc: preproc` to the layout's `highres` role — the entity that
  would actually select fMRIPrep's T1w — makes `classify(render("highres"))` disagree with the
  declaration, and the layout **fails to load**.
- `filtered_func` declares `desc: filtered`, which is what FEAT's own output is. It is *filled* from
  fMRIPrep's `desc-preproc` BOLD. Both are correct; they describe different files.
- `highres` and `example_func` declare no `Entities`, so as filters they would match every
  `space-*` resampling of the same image — ambiguous on every unit.

This is a real constraint, not an oversight to be fixed by enriching the documents. Enriching them
*is* worthwhile — a role and its mapping gaining entities together makes the written tree better
classified when it is re-indexed — but it moves the destination description, never supplies the
source one. **Selecting the file that fills a role is a separate declaration and belongs in a
separate place.**

### 6. Filling a role is usually a computation, not a copy

A layout says where a role goes. It says nothing about how the file gets there, and the common case
is not placement.

Measured on the FSL consumer that drove this design (a MELODIC/FIX pipeline), of 15 roles filled
before FIX runs, **4** are a copy; the rest come from **11 external tool invocations** —
`antsApplyTransforms`, `convert_xfm`, `convertwarp`, `CompositeTransformUtil`, `wb_command` — plus
in-Python derivation (a temporal mean, a trimmed motion matrix, ITK→FSL transform conversions).

Two consequences follow. A "staging plan" of `(destination, source)` pairs is **not** the right
abstraction for filling a tree; it would cover about a quarter of the work, and bidslake does not
offer one. And a role's declared extension is **what will be written there**, not what to go looking
for: the `wmparc` role is `reg/wmparc.nii.gz` while its FreeSurfer source is `wmparc.mgz`, because
the tool that fills it resamples and converts in one step. That is not an incompatibility to be
detected; it is the normal case, and it is further evidence for §5.

## Consequences

- **A layout is usable with no catalog.** `bidslake.layout("feat")` loads, validates and renders
  without a database, which is what lets a pipeline name its outputs before it has written any.
- **Role names are the API.** `out["highres2standard_mat"]` is the whole consumer surface, plus
  `.mkdir(role)`, which creates a role's parent so `reg/` and `mc/` need no declarations of their
  own. A role with unbound `{placeholder}`s raises rather than rendering a plausible wrong path, so
  `layout["classification"]` is an error and `path("classification", training=…, threshold=…)` is
  the call.
- **A layout is not stamped into a catalog** (ADR 0002 §9), because it contributes nothing to the
  DDL and is consulted before a catalog exists. A catalog records how its files were *read*, not
  which layout named them. Recording a layout as provenance when one is used to *produce* a dataset
  remains open in `TODO.md`.
- **Adding a layout is authoring a document, not writing code.** `feat` is the first; fMRIPrep,
  MRIQC and QSIPrep have term maps or overlays but no write direction, so code producing files in
  their conventions still hardcodes paths.
- **The read direction must exist first.** A layout names its term map and is checked against it, so
  a producer with no term map cannot have a layout until one is written — which is the correct
  order, since a tree nothing can read back is a tree bidslake cannot index.
- **§5 bounds what a layout can be asked to do.** A design that tries to derive a unit's *inputs*
  from layout roles will produce ambiguous or empty matches on the majority of them, and cannot be
  rescued by adding entities, because the round-trip check refuses exactly those additions.
