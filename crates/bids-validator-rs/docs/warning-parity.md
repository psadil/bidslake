# Parity with the reference validator

This document records how `bids-validator-rs` compares against the reference TypeScript validator,
[bids-standard/bids-validator](https://github.com/bids-standard/bids-validator) (tag
`3.0.0-alpha.4`). `bids-validator-rs` now bundles the BIDS schema **1.2.1** (BIDS 1.11.1, the
latest released version), vendored via the shared `bids-schema` crate.

> **Note:** the figures and per-schema claims in this document were measured when the crate
> bundled `@bids/schema` 1.2.4; after the move to the released 1.2.1 they are **pending
> re-verification** and may shift.

**Status (measured at schema 1.2.4): 59 of 107 `bids-examples` datasets match exactly. Every
difference in the remaining 48 is one of the four causes below** — there are no unexplained
discrepancies.

## How the diff is produced

Both validators run with `--json` over every `bids-examples` dataset, and the multiset of
`(code, subCode)` pairs is compared. The four codes in `tests/data/bids-examples-config.json`
(`EMPTY_FILE`, `NIFTI_HEADER_UNREADABLE`, `GZ_NOT_GZIPPED`, `NIFTI_TOO_SMALL`) are filtered from
**both** sides — they are artifacts of the placeholder files that `bids-examples` ships, not
validator behavior.

```bash
deno run -A jsr:@bids/validator@3.0.0-alpha.4 <dataset> --json          # reference (published upstream)
cargo run --release -- <dataset> --json                                 # this validator
```

This is not part of `cargo test`, because it would make `deno` a test dependency.
`tests/warning_parity.rs` asserts the JSON *shape* in pure Rust instead.

## Remaining differences

### Rust reports, reference does not

| Issue | subCode | Count | Datasets | Cause |
| --- | --- | ---: | ---: | --- |
| `B0_FIELD_SOURCE_RECOMMENDED` | `B0FieldSource` | 315 | 5 | [inert dataset context](#1-rules-gated-on-datasetdatatypes--datasetmodalities-are-inert-in-the-reference-validator) |
| `SIDECAR_KEY_RECOMMENDED` | `AnatomicalImage` | 96 | 2 | same |
| `SIDECAR_KEY_REQUIRED` | `NonlinearGradientCorrection` | 8 | 4 | same |
| `SIDECAR_KEY_RECOMMENDED` | `AnatomicalLandmarkCoordinates` | 6 | 2 | same |
| `MISSING_SESSION` | — | 21 | 6 | [message-only schema rule](#3-message-only-schema-rules-missing_session) |

### Reference reports, Rust does not

| Issue | subCode | Count | Datasets | Cause |
| --- | --- | ---: | ---: | --- |
| `SIDECAR_KEY_RECOMMENDED` | `AcquisitionDuration` | 2614 | 30 | [`deprecated` level regression](#2-the-deprecated-requirement-level-a-regression-in-the-vendored-300-alpha-line) |
| `SIDECAR_KEY_RECOMMENDED` | `ScanDate` | 10 | 6 | same |
| `TSV_COLUMN_TYPE_REDEFINED` | 7 columns | 197 | 6 | [not implemented](#4-not-implemented-tsv-value-signature-checks) |
| `TSV_PSEUDO_AGE_DEPRECATED` | — | 1 | 1 | same |

Cause 1 is deliberate and reported upstream ([bids-validator#433](https://github.com/bids-standard/bids-validator/issues/433)): we follow the schema, the reference validator does not. Cause 2 is a
pre-release regression already being fixed upstream ([bids-validator#436](https://github.com/bids-standard/bids-validator/pull/436)); we agree with stable `2.4.1`. Cause 3 is the reverse — we
over-report. Cause 4 is a genuine gap.

---

## The four causes

### 1. Rules gated on `dataset.datatypes` / `dataset.modalities` are inert in the reference validator

`BIDSContextDataset` declares both fields but nothing ever populates them:

```ts
// lib/bids-validator/src/schema/context.ts:56-57
this.datatypes = args.datatypes || []
this.modalities = args.modalities || []
```

The only production construction site passes neither:

```ts
// lib/bids-validator/src/validators/bids.ts:99
const dsContext = new BIDSContextDataset({ options, schema, tree: fileTree })
```

They are therefore always `[]`, `intersects(dataset.datatypes, …)` is always `false`, and every
selector gated on them silently never fires. Seven schema rules select on these arrays:

| Schema rule | Selector | Emits |
| --- | --- | --- |
| `rules.checks.dataset.SamplesTSVMissing` | `"micr" in dataset.modalities` | `SAMPLES_TSV_MISSING` (**error**) |
| `rules.sidecars.mri.PETMRISequenceSpecifics` | `intersects(dataset.modalities, ["pet"])` | `SIDECAR_KEY_REQUIRED` / `NonlinearGradientCorrection` (**error**) |
| `rules.sidecars.mri.MRIEchoPlanarImagingAndB0FieldSource` | `intersects(dataset.datatypes, ['fmap'])` | `B0_FIELD_SOURCE_RECOMMENDED` |
| `rules.sidecars.mrs.MRSConditionalAnatomicalImage` | `intersects(dataset.datatypes, ["anat"])` | `SIDECAR_KEY_RECOMMENDED` / `AnatomicalImage` |
| `rules.sidecars.mri.MRIAnatomicalLandmarks` | `intersects(dataset.datatypes, ["meg"])` | `SIDECAR_KEY_RECOMMENDED` / `AnatomicalLandmarkCoordinates` |
| `rules.sidecars.meg.MEGwithEEG` | `intersects(dataset.modalities, ["eeg"])` | nothing — all its fields are `optional` |
| `rules.json.meg.MEGCoordsystemAnatomicalMRI` | `intersects(dataset.datatypes, ["anat"])` | nothing — `IntendedFor` is `optional` |

The spec's `meta/context.yaml` marks both fields as required members of the dataset context, so the
reference validator is the one out of spec. We populate them (`src/context.rs`) and enforce the rules.

`SAMPLES_TSV_MISSING` is worth calling out: a `micr` dataset with no `samples.tsv` passes the
reference validator silently. It does not surface in the diff only because every `micr` dataset in
`bids-examples` ships one.

> **Naming.** `AnatomicalImage`, `AnatomicalLandmarkCoordinates` and `NonlinearGradientCorrection`
> are sidecar **field names**, surfaced as the `subCode` of a generic `SIDECAR_KEY_*` code.
> `PETMRISequenceSpecifics` is a schema **rule name**. Only `B0_FIELD_SOURCE_RECOMMENDED` is an
> issue code. `src/issues.rs` `issue_ignored` matches an ignore entry against the code *or* the
> rule name, which is why `tests/integration_test.rs` can suppress `PETMRISequenceSpecifics` by name.

Reported upstream as [bids-validator#433](https://github.com/bids-standard/bids-validator/issues/433).
If the reference validator populates these fields, it will match us and this row disappears.

### 2. The `deprecated` requirement level (a regression in the vendored 3.0.0-alpha line)

> **Version-specific, and already being fixed upstream.** It reproduces against the vendored
> `3.0.0-alpha.4` — the line we diff against — but **not** against stable `2.4.1`, which behaves the
> same as this validator (no warning). It is a regression on the 3.0 pre-release branch.
> [bids-validator#436](https://github.com/bids-standard/bids-validator/pull/436) fixes it. Confirmed
> by running both versions on `ds003` with the same schema: `2.4.1` emits 0, `3.0.0-alpha.4` emits 13.

Schema 1.2.4 has exactly two `deprecated` fields, both in object form:
`rules.sidecars.func.MRIFuncTimingParameters.AcquisitionDuration` and
`rules.sidecars.pet.PETTime.ScanDate`. (This definition is identical in schema 1.2.3, which the
published `2.4.1` resolves to by default — so the schema is *not* the variable here; the validator
code is.)

The shared root is that `getFieldSeverity` never maps `deprecated`, so it returns `undefined`:

```ts
// lib/bids-validator/src/schema/applyRules.ts:296-300
const levelToSeverity: Record<string, Severity> = {
  recommended: 'warning', required: 'error', optional: 'ignore', prohibited: 'ignore',
}
```

`src/data/metaschema.json` lists `deprecated` as a legal `requirement_level`
(`required`, `recommended`, `optional`, `deprecated`), so this omission is a latent gap in *both*
versions. Whether that `undefined` severity produces a warning depends on how the caller guards the
missing-field branch, and this is exactly what differs between the two versions:

```ts
// 2.4.1 — positive guard: emit only when severity is truthy and not 'ignore'.
// undefined is falsy → the deprecated field is correctly skipped.
if (severity && severity !== 'ignore') { …emit… }

// 3.0.0-alpha.4 (vendored) — negative guard: skip only when severity is *exactly* 'ignore',
// then fall through and emit. undefined !== 'ignore', so it does not skip → spurious RECOMMENDED.
if (severity && (severity === 'ignore')) { continue }
```

The guard was flipped by commit
[`b6635d3b`](https://github.com/bids-standard/bids-validator/commit/b6635d3b5acac6a00ca4ebc5cea180b46f28847b)
("Don't generate sidecarkey required errors for derivative datasets…", @rwblair, 2026-05-04), merged
via [bids-validator#399](https://github.com/bids-standard/bids-validator/pull/399). That PR restructured
the missing-field branch and inverted the guard as a side effect. Filed as
[bids-validator#434](https://github.com/bids-standard/bids-validator/issues/434); the fix is the open
PR [bids-validator#436](https://github.com/bids-standard/bids-validator/pull/436) ("fix: Fully invert
logical condition"), which restores correct behavior. (Note that #436 only fixes the regression; the
underlying question of how `deprecated` fields *should* be handled — `getFieldSeverity` never maps the
level — is deferred as a separate feature.) `level: deprecated` was introduced by
[bids-specification#1974](https://github.com/bids-standard/bids-specification/pull/1974).

We report only `required` and `recommended` missing fields (`src/rules/mod.rs`), so we already agree
with stable `2.4.1` and with the post-#436 behavior. Once we re-vendor a build that includes #436,
this row disappears.

### 3. Message-only schema rules (`MISSING_SESSION`)

`MISSING_SESSION` exists in the schema only as a message-only entry under `rules.errors` — a
`code`/`message`/`level` triple with **no selectors and no checks** — added by
[bids-specification#1146](https://github.com/bids-standard/bids-specification/pull/1146)
("Common error messages for rules that have no check in schema"). The reference validator's
`applyRules` only evaluates nodes carrying a `selectors` key, so it never fires.

We hand-implement the legacy heuristic (`src/rules/errors/dataset.rs`), which flags subjects whose
session layout differs from their peers. It over-reports on datasets with legitimately heterogeneous
layouts — a `sub-emptyroom` MEG recording alongside multi-session subjects — because it has no
carve-out. Compare `rules.checks.events.EventsMissing`, which *did* gain one
(`datatype != "meg" || entities.subject != "emptyroom" && entities.task != "noise"`) in response to
[bids-validator#12](https://github.com/bids-standard/bids-validator/issues/12).

Affected: `ds000117`, `ds000247`, `ds000248`, `eeg_ds003645s_hed_demo`, `eeg_rishikesh`,
`xeeg_hed_score`.

This is the one remaining difference where our behavior is arguably worse than the reference's.

### 4. Not implemented: TSV value-signature checks

`TSV_COLUMN_TYPE_REDEFINED` and `TSV_PSEUDO_AGE_DEPRECATED` both come from the reference validator's
TSV value-signature layer (`lib/bids-validator/src/schema/tables.ts`), which reconciles a sidecar's
column definition against the schema's and then type-checks every cell. We implement column presence
and additional-column rules only (`src/rules/tabular_data.rs`); there is no value-signature layer.

`TSV_COLUMN_TYPE_REDEFINED` is narrower than its name suggests: it fires only when a sidecar
redefinition *conflicts* with the schema's definition (incompatible base type, `Levels` not a subset,
`Units` mismatch, `Minimum`/`Maximum` outside the schema bound) and cannot be refined to a subset —
and only for columns defined JSON-schema-style rather than with a `definition` block. When it fires,
the reference validator discards the redefinition and keeps the schema's.

The surrounding design question — whether a sidecar column definition *replaces* or *patches* the
schema's — is open upstream:
[bids-validator#386](https://github.com/bids-standard/bids-validator/issues/386) and
[bids-specification#2045](https://github.com/bids-standard/bids-specification/issues/2045).
`TSV_PSEUDO_AGE_DEPRECATED` was split out by
[bids-validator#242](https://github.com/bids-standard/bids-validator/pull/242).

---

## The expression language

The evaluator is tested against the schema's **77 normative `meta.expression_tests` cases** —
`build.rs` generates one `#[test]` per case into `OUT_DIR/expression_tests.rs`, and
`tests/expression_conformance.rs` includes them. Because they come from the vendored schema, they
track whichever version the build pins. The same test also asserts that **every** `selectors`/`checks`
expression in the bundled schema evaluates without error (an unevaluable expression would otherwise
silently disable its rule). All pass.

Where the schema's *prose* (`src/schema/README.md`, "The special value `null`") disagrees with those
machine-readable cases — chiefly around `null` handling — we replicate the reference validator's
(JavaScript) behavior, since that is what the cases encode. The prose/test conflict is tracked in
[bids-specification#2149](https://github.com/bids-standard/bids-specification/issues/2149); it also
covers ordering comparisons with `null`, which neither source gets right (`null >= -60` is `true`, not
`false`, because JavaScript coerces `null` to `0`).

One schema expression cannot be evaluated by any conforming interpreter:
`rules.checks.anat.PDT2Echos` calls `len(...)`, a function that does not exist (it should be
`length`). Rather than add a `len` alias, it is allowlisted in
`every_schema_expression_evaluates` so the schema bug stays visible. Filed as
[bids-schema#13](https://github.com/bids-standard/bids-schema/issues/13).

---

## Issue-code provenance

Issue codes come from two independent registries, plus a synthesized tier. Knowing which is which
tells you what a schema-driven reimplementation must hand-write.

| Source | Count | Notes |
| --- | ---: | --- |
| Schema (`@bids/schema` 1.2.4) | 159 | 132 via `.issue.code` on `rules.checks`/`rules.sidecars` fields; 27 via `.code` on `rules.errors.*`. The two sets are disjoint. |
| `lib/bids-validator/src/issues/list.ts` | 52 | Hardcoded in the reference validator, exported as `nonSchemaIssues`. |
| In both | 10 | Deliberate duplication. |
| **Validator-invented** | **42** | `list.ts` only — no schema rule (e.g. `SIDECAR_FIELD_OVERRIDE`). Must be hand-written by any reimplementation. |

A third, synthesized tier never appears in either registry: `applyRules` builds
`SIDECAR_KEY_{REQUIRED,RECOMMENDED}` and `JSON_KEY_{REQUIRED,RECOMMENDED}` from the field's
requirement level. Most `subCode`s you see in the diff above belong to this tier.

That the code list moved between validator versions, and that downstream users depended on
`list.ts`, is [bids-validator#123](https://github.com/bids-standard/bids-validator/issues/123);
documenting the codes is [bids-validator#6](https://github.com/bids-standard/bids-validator/issues/6).

---

## Upstream status

Issues surfaced by this comparison, and where they stand:

| Finding | Status |
| --- | --- |
| `dataset.datatypes`/`dataset.modalities` never populated | Filed: [bids-validator#433](https://github.com/bids-standard/bids-validator/issues/433) (open) |
| `deprecated` fields flagged as missing (pre-release regression) | Filed: [bids-validator#434](https://github.com/bids-standard/bids-validator/issues/434); fix in progress: [bids-validator#436](https://github.com/bids-standard/bids-validator/pull/436) |
| Expression prose vs `meta.expression_tests` (incl. ordering-with-null) | Tracked: [bids-specification#2149](https://github.com/bids-standard/bids-specification/issues/2149) |
| `rules.checks.anat.PDT2Echos` calls undefined `len()` | Filed: [bids-schema#13](https://github.com/bids-standard/bids-schema/issues/13) (open) |
