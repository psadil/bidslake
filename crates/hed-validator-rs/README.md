# HED Validator (Rust) — `hed-validator-rs`

A Rust implementation of the official [Hierarchical Event Descriptors (HED)](https://www.hedtags.org/) validator. It aims for behavioral parity with the reference [`hed-python`](https://github.com/hed-standard/hed-python) implementation on the areas the shared [`hed-tests`](https://github.com/hed-standard/hed-tests) conformance suite covers, with the speed and type safety of Rust.

## Current state

The validator passes the **complete `hed-tests` conformance suite** — every `string_tests`, `sidecar_tests`, `event_tests`, and `combo_tests` case across all 25 files in `validation_test_data/`, plus every `schema_tests` case across all 18 files in `schema_test_data/`, with **no skipped entries**.

Implemented capabilities:

- **HED string validation** — a hand-written recursive-descent parser that reports structural errors (`COMMA_MISSING`, `PARENTHESES_MISMATCH`) precisely, plus tag resolution against the schema tree, character/value/unit-class grammars, group/reserved-tag rules, duplicate detection, and full `Definition`/`Def`/`Def-expand` handling.
- **BIDS sidecar validation** — JSON sidecar structure, `{column}` splice references (existence, cycles, placement), and per-column HED validation with Value vs. Categorical placeholder rules.
- **Tabular (events) validation** — assembles each row's HED-bearing columns (Value-column `#` substitution, Categorical lookups, `{column}` splicing) and runs cross-row temporal (`Onset`/`Offset`/`Inset`/`Delay`) sequence checks.
- **Multi-schema / library loading** — merge-group specs like `["8.3.0", "sc:score_1.0.0"]`, namespace-prefixed tags (`sc:Sleep-modulator`), and `withStandard` partner resolution.
- **Schema-source validation** — a mediawiki (`.mediawiki`) schema parser and a post-load compliance checker (`SCHEMA_ATTRIBUTE_INVALID`, `SCHEMA_DEPRECATION_ERROR`, `SCHEMA_DUPLICATE_NODE`, etc.).

The default standard schema (HED 8.4.0) is embedded in the binary as JSON; other versions and library schemas are loaded from a local `hed-standard/hed-schemas` checkout (see below) or fetched and cached on demand.

### Out of scope (for now)

Subsystems of `hed-python` that the `hed-tests` suite does not exercise are not implemented: BIDS dataset walking, spreadsheet/Excel inputs, the HED search/query language, schema saving/format conversion, and full CLI parity.

## Usage

### Command line

Validate a single string against the embedded 8.4.0 schema:

```bash
cargo run -- --string "Event, Action/Think"
```

Validate strings from a file (one HED string per line):

```bash
cargo run -- --file strings.txt
```

Validate against a multi-schema merge group, using a local schemas checkout for the non-embedded versions:

```bash
cargo run -- \
  --schema-version "8.3.0,sc:score_1.0.0" \
  --schema-dir tests/hed-schemas \
  --string "sc:Sleep-modulator, Red"
```

`--schema-version` accepts a single version (`8.4.0`) or a comma-separated merge-group spec with optional namespace prefixes (`8.3.0,sc:score_1.0.0`).
`--schema-dir` is optional; without it, non-embedded schemas are fetched from GitHub and cached in the platform app-data directory.

### Library

```rust
use hed_validator_rs::parser::parse_hed_string;
use hed_validator_rs::schema::{Schema, SchemaCollection};
use hed_validator_rs::validator::{
    DefinitionMap, DefinitionSite, PlaceholderMode, ValidationContext, Validator,
};

let schemas = SchemaCollection::single(Schema::load_standard("8.4.0").unwrap());
let validator = Validator::new(&schemas);

let parsed = parse_hed_string("Event, Action/Think").unwrap();
let defs = DefinitionMap::new();
let ctx = ValidationContext::new(
    PlaceholderMode::Forbidden,
    DefinitionSite::PlainString,
    &defs,
);
let errors = validator.validate(&parsed, &ctx);
assert!(errors.is_empty());
```

## Building and testing

This repository uses git submodules for the conformance test data, the vendored schemas, and the `hed-python` reference. After cloning:

```bash
git submodule update --init --recursive
cargo test
```

The submodules (see `.gitmodules`) are all required for the test suite:

- `tests/hed-tests` — the official conformance test data (JSON fixtures).
- `tests/hed-schemas` — the official schema repository, used to load the non-embedded standard versions (8.0–8.3) and library schemas (`score`, `lang`, `testlib`) that the multi-schema fixtures reference.

The two integration harnesses are generic drivers over the JSON fixtures:

```bash
cargo test --test validation      # string / sidecar / event / combo conformance
cargo test --test schema_tests    # mediawiki schema-source conformance
cargo test --lib                  # in-crate unit tests (parser, wiki parser, loader)
```

`tests/validation.rs` loads each fixture entry's declared schema (a plain version or a merge-group array) and runs its `string_tests`, `sidecar_tests`, `event_tests`, and `combo_tests`; `tests/schema_tests.rs` runs each inline mediawiki schema through the parser and compliance checker. A `"fails"` case must produce an error whose code is in the entry's `error_code` ∪ `alt_codes`; a `"passes"` case must produce none. (One parity behavior worth knowing: if a validation entry's declared schema *fails to load*, that counts as the entry's
expected `"fails"` outcome and its sub-tests are skipped — matching `hed-python`'s spec harness, which some `SCHEMA_LOAD_FAILED` fixtures rely on.)

## Contributing

An architectural overview of the codebase lives in the crate-level documentation — run `cargo doc --open` to browse it, or read the module doc-comments starting from `src/lib.rs`. When changing a validation rule, cross-check the intended behavior against `hed-python`, and prefer extending the generic test harnesses over adding bespoke tests. Before submitting, run  `cargo test`, `cargo clippy --all-targets` (expected clean), and `cargo fmt`.
