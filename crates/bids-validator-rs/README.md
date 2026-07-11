# bids-validator-rs

A pure-Rust BIDS (Brain Imaging Data Structure) validator.

> [!WARNING]
> **State of the project: UNSTABLE**. This project is currently in active development and is considered unstable. Much of this has been vibe-coded. I am slowly going through the code to verify implementation and overall structure. Please do not use!

## Overview

This project aims to provide a fast, safe, and efficient validator for BIDS datasets, leveraging the performance and safety guarantees of the Rust programming language. It is intended to eventually be an alternative to the official BIDS validator.

## Validation

Have at with one of the datasets from the [bids-examples](tests/data/bids-examples) (requires submodule initialization).

```{bash}
cargo run --release -- tests/data/bids-examples/ds002 --ignore-warnings --config tests/data/bids-examples-config.json 
```

## References

- **BIDS Standard**: [bids.neuroimaging.io](https://bids.neuroimaging.io/)
- **BIDS Specification**: [bids-specification.readthedocs.io](https://bids-specification.readthedocs.io/)
- **Official BIDS Validator (TypeScript/JavaScript)**: [github.com/bids-standard/bids-validator](https://github.com/bids-standard/bids-validator)

## Notes

### Parity with the reference (TypeScript) validator

The `--json` output is structurally identical to the reference
[`bids-validator`](https://github.com/bids-standard/bids-validator) — same `issues.issues` /
`codeMessages` shape, same `code` / `subCode` / `severity` / `issueMessage` / `rule` fields, same
issue codes. Both are pinned to `@bids/schema` 1.2.4, so any difference is an implementation
difference rather than a schema-version difference.

Issues were diffed across all 107 `bids-examples` datasets. **59 match exactly, and every difference
in the remaining 48 has a known cause** — there are no unexplained discrepancies. The four causes:

1. **Rules gated on `dataset.datatypes` / `dataset.modalities`.** The reference validator declares
   both fields but never populates them, so the seven schema rules selecting on them are inert
   there. We populate them and enforce the rules. Reported upstream:
   [bids-validator#433](https://github.com/bids-standard/bids-validator/issues/433).
2. **The `deprecated` requirement level.** The vendored `3.0.0-alpha.4` warns about the *absence* of
   a deprecated field (`AcquisitionDuration`, `ScanDate`); we follow the schema and stay silent. This
   is a regression on the 3.0 pre-release line — stable `2.4.1` agrees with us, and it is already
   being fixed by [bids-validator#436](https://github.com/bids-standard/bids-validator/pull/436).
3. **`MISSING_SESSION`.** A message-only schema rule the reference validator never fires. We
   hand-implement the legacy heuristic, and over-report on heterogeneous session layouts.
4. **TSV value-signature checks** (`TSV_COLUMN_TYPE_REDEFINED`, `TSV_PSEUDO_AGE_DEPRECATED`) are
   not implemented here.

Cause 1 is deliberate — we follow the schema and the reference validator does not. Cause 2 is a
pre-release regression being fixed upstream. Cause 3 is the reverse (we over-report). Cause 4 is a
genuine gap.

The diff is not part of `cargo test`, since it would make `deno` a test dependency;
`tests/warning_parity.rs` asserts the JSON shape in pure Rust instead. The mechanism behind each
difference, the exact source lines in both validators, and the upstream status are in
**[docs/warning-parity.md](docs/warning-parity.md)**.

### Schema expression conformance

The BIDS schema ships 77 normative test cases for its expression language at
`meta.expression_tests`. `build.rs` generates one Rust test per case, so the suite tracks whichever
schema version is vendored. `tests/expression_conformance.rs` additionally asserts that every
`selectors` / `checks` expression in the bundled schema actually *evaluates* — an expression the
evaluator cannot handle would otherwise silently disable its rule rather than failing loudly.
