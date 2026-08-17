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
issue codes. This crate bundles `@bids/schema` **1.2.1** (BIDS 1.11.1) via the shared
`bids-schema` crate; a reference validator on a different schema version will differ for that
reason alone, so a parity run has to state both versions to mean anything.

Issues have been diffed across all 107 `bids-examples` datasets, and every difference has a known
cause — there are no unexplained discrepancies. They reduce to four: two where the reference
validator is the one that departs from the schema, one where we over-report, and one genuine gap.

**[docs/warning-parity.md](docs/warning-parity.md)** is the one place that tracks them: the
measured match rate and the versions it was measured at, the per-dataset breakdown, the mechanism
behind each cause with the source lines in both validators, and the upstream issue status.

The diff is not part of `cargo test`, since it would make `deno` a test dependency;
`tests/warning_parity.rs` asserts the JSON shape in pure Rust instead.

### Schema expression conformance

The BIDS schema ships 77 normative test cases for its expression language at
`meta.expression_tests`. These belong to the shared schema crate, not this one:
`crates/bids-schema/build.rs` generates one Rust test per case, so the suite tracks whichever
schema version is vendored, and `crates/bids-schema/tests/expression_conformance.rs`
additionally asserts that every `selectors` / `checks` expression in the bundled schema actually
*evaluates* — an expression the evaluator cannot handle would otherwise silently disable its
rule rather than failing loudly.
