# ADR 0009 — Root tenure, and what the catalog is allowed to conclude

Status: accepted (2026-08-15)

Relates to: `dataset_roots` (ADR 0005 §1), `BidsParser::resolve_root` (`bids.rs`), `verify.rs`,
and the `Managed mode (design)` section of `crates/bidslake/README.md`, whose per-database mode
marker this replaces with a per-root one. Gives the phrase "a catalog is an index, never an
owner" — cited by `verify.rs` against ADR 0005, which never actually said it — a home, and a
condition under which it stops being true.

## Context

A pipeline was taught to index its own output and then ask the catalog which units were
finished. Pointed at an empty directory that had never been written to, it reported all six
steps complete and exited 0.

The immediate cause was a missing predicate: the query filtered on `dataset_id` and never on
`root_uri`, so it answered about the *indexed dataset* while the caller was asking about the
*directory being written to*. But adding the predicate would have fixed one caller and left the
next one to make the same mistake, so the prior question was asked first — is a catalog the
right thing to ask "is this work done" at all?

### What DuckLake settles

DuckLake separates catalog, storage and compute, and is explicit about which owns what
(*DuckLake: The Definitive Guide*, Ch. 3, p. 45): "The catalog provides consistency / The storage
layer provides data durability / The compute layer provides performance." Its catalog **is**
authoritative about which files are real. It earns that two ways:

- **The catalog is the commit point.** p. 47: "Data files are written first, before any catalog
  metadata is updated… Only after the data files are safely in place does it perform the metadata
  transaction that updates the snapshot table, making the new files visible and valid for
  readers." The [manifesto](https://ducklake.select/manifesto/) puts it as "the actual write to
  Parquet is not part of this sequence, it happens beforehand." A file on disk that is not in the
  catalog is not *stale* — it is *not a member*.
- **The catalog owns the files.**
  [`ducklake_add_data_files`](https://ducklake.select/docs/stable/duckdb/metadata/adding_files)
  registers externally-written files without copying them, and "the ownership of the Parquet file
  is transferred to DuckLake, and as such, compaction operations… can cause the files to be
  deleted by DuckLake."

bidslake had neither. It indexes out of band, after the fact, over trees it did not write. So
the two error directions are not symmetric:

| | DuckLake | bidslake before this ADR |
|---|---|---|
| on disk, not in catalog | correctly invisible (an orphaned write) | a redundant re-run — costs time |
| in catalog, not on disk | impossible without corrupting the lake | **routine**: purged scratch, wrong destination, a deleted subject |

That makes the catalog **sound as a work-finder and unsound as a work-skipper** — which is
DuckLake's own pruning contract, stated on p. 55: "*File Pruning*: Query the catalog to decide
which files to examine. *Row Group Pruning*: Inspect those files to decide which pieces of the
file to scan." The catalog narrows; storage still confirms. The failing query used it in the
unsound direction.

### The part that is not DuckLake's problem

DuckLake never has to index a tree it did not write, so it needs no vocabulary for one. bidslake
does — an OpenNeuro dataset, a colleague's fMRIPrep output, a study tree assembled by hand. The
missing concept is therefore not "ownership" as a global property of the software, but **what was
promised about a particular root**, which differs from root to root inside one catalog. ADR 0005
already made roots plural and gave them a registry; this ADR gives that registry the column that
says what may be concluded from it.

## Decisions

### 1. Tenure is a property of a root, not of a database

```sql
ALTER TABLE dataset_roots ADD COLUMN tenure TEXT NOT NULL DEFAULT 'attached';
-- CHECK (tenure IN ('attached', 'managed'))
```

| Tenure | Who writes there | The catalog may conclude | Destructive verbs |
|---|---|---|---|
| **attached** (default) | somebody else | "this was here when I looked, and is still there unless `verify` says otherwise" | refused |
| **managed** (opt-in) | bidslake | "this is here" — the catalog is the commit point | allowed |

Per-root rather than per-database because ADR 0005 put many datasets and many roots in one
catalog, and an attached OpenNeuro dataset has to be able to sit beside a managed derivative.
The README's per-database marker cannot express that.

**`attached` is a permanent tier, not a waypoint.** It is how bidslake stays useful to somebody
who has data and no intention of handing control of it over, and most catalogs will never contain
anything else. Nothing in the read path depends on tenure; only conclusions do.

`attached` has no DuckLake analogue, and that absence is the whole explanation for why DuckLake's
catalog can always conclude and bidslake's cannot by default. `ducklake_add_data_files` is the
*transition* between the tiers — register files written by someone else, ownership moves — which
is exactly "the pipeline output has landed on shared storage; bring it under management."

### 2. Durability is asserted by the person indexing; bidslake does not guess at it

Indexing a root is a statement that its files will still be there. bidslake takes that
statement and does not audit it — in particular, **it does not refuse a root for looking
temporary.**

*Rejected:* refusing roots under `$TMPDIR`, `/tmp`, `/var/tmp`, `/var/folders`, or `/scratch`,
with an `--allow-transient` override. This was built and then removed, and the reason it had to
go is that it contradicts §1. Tenure exists precisely because authority over a root is
**declared** rather than inferred; deducing it from a path prefix is the same inference by
another route, and a worse one, since a prefix is a much weaker signal than a flag. A rule that
says "you may tell me what this root is" and then second-guesses the answer from its name is
incoherent.

It would also have been wrong constantly. `/scratch` is durable project space at many sites;
containers routinely mount real storage under `/tmp`; `$TMPDIR` is whatever a scheduler set it
to. There is no path prefix that reliably means ephemeral, so the list could only ever be a
heuristic imposed on people who know their own filesystem better than it does. The maintenance
was already visible before it shipped: `/var` is a symlink to `/private/var` on macOS, so every
prefix needed matching twice, and three separate test harnesses had to be handed an escape
hatch just to keep working.

And it bought nothing. The failure that prompted this ADR — a catalog reporting six finished
steps for a directory that had never been written to — is fixed by §3 scoping the query to the
root in hand. The refusal was belt-and-braces over the real fix, and pointed at the wrong thing:
the bug was a query that did not say which tree it meant, not a root in the wrong place.

What remains is honest about what it knows. An `attached` root's rows say what was true at
index time, `verify` is how that claim gets checked, and where a root lives is the indexer's
business.

### 3. The unsound direction is made unrepresentable, not merely discouraged

A completion-shaped query joins `dataset_roots` and is scoped by root, so a caller cannot forget
the scoping the way the original one did. This is the actual fix for the bug that prompted the
ADR; the predicate was never the point.

For an `attached` root the catalog's answer is a claim about a past observation, and turning it
into a decision to *skip* work needs confirmation — either a `stat` of the files in question, or
a `bidslake verify` that has passed since the last index. `verify` is what makes an attached
root's promise checkable, which is its reason to exist.

### 4. The boundary with the pipelines' own bookkeeping

A workflow engine's internal state is a different kind of file, and bidslake should stay out of
it. Dagster, nipype and Snakemake each keep a completion ledger and a working tree; those are
not degenerate catalogs, and indexing them would not be a favour. The line is genuinely blurry,
so it is argued here rather than asserted — and note that arguing it is *all* that happens.
§2 declines to enforce any of this, because the boundary is about what a file is *for*, which
is knowledge the person indexing has and bidslake does not.

**The criterion is not "is this file temporary" but "who is accountable if it disappears."** If
the answer is "the run that made it, which will simply remake it", the file belongs to the
engine — those systems have both a completion model *and* a recovery model, and a catalog row
adds only a second authority that can disagree with the first. If the answer is "nobody, and
somebody needs to notice", it belongs here.

**So the boundary is temporal rather than spatial.** A file becomes bidslake's concern when the
run that could have rebuilt it is over. That also explains why "ask the engine", "ask the
filesystem" and "ask the catalog" are not three implementations of one question: they have
different validity windows. The engine's ledger is authoritative *during* a run and meaningless
after it; the catalog is useful *between* runs and stale during one; the filesystem is correct in
both and expensive in both.

**What goes wrong if ephemeral files get in**, concretely:

- `verify` drowns. Thousands of `missing` lines for files that were *supposed* to be reaped, and
  the one product that genuinely vanished is somewhere in them. A check whose output is mostly
  expected failures is not a check.
- `file_registry` stops answering "what files are in this dataset", which ADR 0006 built it to
  answer.
- Under `managed` tenure, relocating or transcoding a file an engine is midway through writing is
  corruption. The tenure boundary and the ephemerality boundary therefore have to coincide.

**The mechanism already exists, and has already been used for exactly this.** The per-file line
is drawn by an adapter's ingestion fragment, not by a new concept. `data/ingestion/freesurfer.json`
opens with

```json
{ "selectors": ["match(path, \"/touch/\")"], "disposition": "ignore" }
```

and recon-all's `touch/` directory is precisely its own step-completion ledger — another engine's
tracking, correctly kept out, before this principle had a name. `data/ingestion/feat.json` opens
with three more of them, ignoring `fix/`, `mc/`, `filtered_func_data.ica/` and `pyfix.log` —
FIX's scratch and its logs. **Tenure draws the per-root line; `disposition: ignore` draws the
per-file one.**

Worth being exact about which layer does this, because the two are easy to confuse: `feat`'s
*term map* happily maps those same paths (`fix/[^/]+` carries `desc: fixscratch`, `pyfix.log`
carries `desc: log`). Naming a thing and admitting it to the catalog are separate decisions, and
it is the ingestion fragment that makes the second one.

**Where it stays blurry, the pipelines are already formalizing it.** Expensive intermediates are
the hard case: a FEAT tree's `filtered_func_data.nii.gz` is both a step input and a scientific
product, and people really do want to resume from one. The rule that indexing is a promise —
you may not both index a file and reap it as scratch — resolves it but leaves bidslake inventing
a criterion nobody else uses.

The better answer is that the line is being drawn externally, and bidslake should track it.
[fMRIPrep since 23.2.0](https://fmriprep.org/en/stable/usage.html) accepts precomputed derivatives
via `--derivatives`/`-d`, generalizing the older anatomy-only `--anat-derivatives`, so a user can
hand an artifact back instead of recomputing it. The qualifying condition is that those files
follow BIDS Derivatives conventions — entity-named, with a `dataset_description.json`. That is
already bidslake's indexing precondition, so the two criteria coincide without anyone having
coordinated them: **an intermediate fMRIPrep would accept back is one bidslake can index, and
anything in `work/` is neither.** Its `--level` (`minimal`/`resampling`/`full`) is the same
question tiered from the producing side.

This dissolves the hard case rather than adjudicating it. Resuming from an expensive intermediate
does not require indexing live working state; it requires *promoting* the intermediate to a
derivative, which is a deliberate act that then makes it indexable. bidslake therefore points at
the mechanism rather than maintaining a list — fMRIPrep calls derivatives reuse **experimental**
and does not enumerate the accepted set, and each adapter's ingestion fragment tracks whatever its
pipeline formalizes.

One consequence names itself: a FEAT output tree ships no `dataset_description.json`, which is
what stops a write→read round trip closing. That file is not paperwork. It is the same membership
token fMRIPrep demands of a precomputed derivative — how a pipeline says "this is a product now" —
and writing it is the natural moment to declare tenure.

### 5. What `managed` buys is named here and built later

Opting in has to be worth something, so the capabilities it unlocks are recorded now even though
none is implemented: `transcode` (already a stub in `main.rs`), relocation (move the files and
rewrite `root_uri`/`file_path` in one transaction), garbage collection, and eventually the
opaque assigned storage paths the README's managed-mode design describes. Relocation is the
first one worth building: it is the direct answer to "the output has moved from scratch to
shared storage", which today has no supported fix but a re-index.

## Consequences

- **`verify` gets a sharper job.** It stops being a general "is anything still true" scan and
  becomes an audit of a stated promise. An `attached` root that fails it has had its contract
  broken by somebody; a `managed` root that fails it means bidslake has a bug.
- **An existing catalog gains tenure without a re-index.** `CREATE TABLE IF NOT EXISTS` leaves
  an already-built `dataset_roots` two columns wide, so `create_tables` also issues an
  idempotent `ALTER TABLE … ADD COLUMN IF NOT EXISTS tenure TEXT DEFAULT 'attached'` — the same
  courtesy `link init` extends. The default is not merely convenient: a root registered before
  tenure existed promised its files were there and claimed nothing further, which is exactly
  what `attached` means.
- **A catalog answer is trustworthy but not yet citable.** Tenure establishes that a claim was
  made in good faith over a durable root; it does not date the claim. That is what snapshot ids
  are for (recorded in `TODO.md`) — "done as of snapshot 47" is checkable in a way "done" is not.
- **Indexing gains no new way to fail.** Tenure adds a column and a flag; it rejects nothing
  that was accepted before. That is deliberate (§2), and it is why no existing caller — the
  test suites, the spike harnesses, anyone's scripts — needed an escape hatch to keep working.
- **`(dataset_id, root_uri)` gains a third meaning.** It already identified where a file resolves
  from (ADR 0005) and keyed `file_id` (ADR 0006); it now also carries authority. That is a lot
  for one pair to mean, and is the reason tenure is a column on the existing registry rather than
  a new table — splitting it would create a second place to look for "is this root real."
- **Nothing in the read path changes.** `sibling()`, `LayoutAt`, discovery queries and the wide
  views are untouched. Only conclusions about completion are gated, and only writers assert
  tenure.
