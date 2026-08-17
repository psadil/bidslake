# Contributing

Scoped to getting a working environment, how tests are written, and how docstrings are written.
The design decisions behind any of it live in `docs/adr/`; how to *use* each crate is in its
README.

## Setup

Rust needs only the toolchain — DuckDB is bundled. The Python package builds under maturin rather
than cargo, so its compiled extension has to be installed into a venv before anything can import
it. From `crates/bidslake-py`, with [`uv`](https://docs.astral.sh/uv/):

```bash
uv venv --python 3.14           # Python 3.14 floor (t-strings, Unpack, `type`)
uv pip install maturin pytest hypothesis ty ruff
.venv/bin/maturin develop       # build + install the extension (editable)
```

That install list is what every command below needs — `hypothesis` is imported unconditionally
by `conftest.py`, so omitting it is a collection error rather than a quietly smaller suite. CI
installs the same set by name; if you add a tool here, add it to
[`.github/workflows/ci.yml`](.github/workflows/ci.yml) too.

## Tests

Structure every test as **Arrange–Act–Assert**
([Bill Wake, *3A*](https://xp123.com/3a-arrange-act-assert/)), or **Given–When–Then** if you
prefer that vocabulary — the same three phases. One assertion per test, per
[*Python Testing with pytest*](https://pythontest.com/pytest-book/).

Two things that follow from it and are easy to miss:

- **Act exactly once.** A test that calls the code under test twice is testing two things,
  whatever its assertion count. Split it, or parametrize it. Where the relationship *between*
  two calls is the behaviour — lazy agrees with eager, an id is stable across two reads — say
  so in the docstring.
- **Preconditions are Arrange.** A check that the fixture data is what the test assumes belongs
  in the fixture, where a failure errors rather than fails.

An assertion also has to be able to fail: no bare `is_err()` where several things are invalid
at once, no `all()` over a possibly-empty sequence, no `if df.is_empty(): continue`, no skip
guard that turns a missing fixture into a pass.

## Properties

Rules that hold over a whole input space — a parser and its printer agree, a merge is
order-independent, a function never panics — are written as property tests: `proptest` in Rust
(inline in `#[cfg(test)] mod tests`, never in `tests/`, where it cannot find its regression
directory), `hypothesis` in Python. Generators live in each crate's `strategy` module.

Three phases still. The generated input is Arrange, so it belongs in the strategy — and the
strategy returns the expected value alongside the input, by *rendering* a structure it already
holds. A strategy that leaves the body to work out what the answer should be has written the
code under test a second time, and will agree with it when it is wrong.

Prefer a total strategy to `prop_assume!`/`assume()`: a filter that rejects most of what it
sees spends the budget proving nothing, which is the empty-`all()` problem again.

A counterexample is pinned in source — a `#[case::…]` row or an `@example(...)` — not left in
`proptest-regressions/` or `.hypothesis/`. Those are gitignored seed caches, and a seed stops
reproducing the moment the strategy changes.

## Running them

Rust — default members only; the `bidslake-py` crate builds under maturin, not cargo:

```bash
cargo test
```

Python, from `crates/bidslake-py`. `maturin develop` must precede the first run and any change
to the Rust extension:

```bash
.venv/bin/maturin develop && .venv/bin/python -m pytest -q -rs
```

`-rs` lists skip reasons. The suite should report zero skips.

Both generators are configured per test and can be turned up from the shell without editing
anything — proptest reads its environment after applying a per-test `ProptestConfig`, so the
variable wins:

```bash
PROPTEST_CASES=10000 cargo test
PROPTEST_RNG_SEED=1234 cargo test
.venv/bin/python -m pytest --hypothesis-profile=thorough
```

The session fixture ingests three `bids-examples` datasets, which dominates the runtime. To
iterate against an already-built catalog:

```bash
BIDSLAKE_TEST_DB=/path/to/test.duckdb .venv/bin/python -m pytest -q
```

Lint and types (ruff runs as a `prek` hook, `ty` in both the hook and CI):

```bash
.venv/bin/ruff check . && .venv/bin/ruff format --check . && .venv/bin/ty check python/bidslake
```

## Regenerating the typed schema module

`crates/bidslake-py/python/bidslake/schema/_generated.py` and `schema/models.py` are committed,
and produced by a Rust bin that reuses the exact `bidslake` schema/DDL model — no part of it is
re-implemented in Python. From `crates/bidslake-py`:

```bash
# PYO3_PYTHON points cargo's link step at the venv interpreter.
PYO3_PYTHON=$PWD/.venv/bin/python cargo run -p bidslake-py --bin emit-types
```

Run it after anything that changes the emitted schema: a vendored BIDS schema bump, a bundled
overlay, a change to the DDL model itself.

## CI

`.github/workflows/ci.yml`, every job of which runs locally too:

- **rust** — `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- **python** — `pytest`, which includes `test_codegen.py` (the generated `COLUMNS` map equals the
  real database) and `test_typing.py` (asserts `ty` *rejects* a fixture of bad queries — the one
  typing check the `ty` hook cannot make); then `ty check python/bidslake`.
- **docs** — `cargo doc` with `RUSTDOCFLAGS="-D warnings"`, `cargo test --doc`, `mdbook build`, and
  a `lychee` link check over `docs/`, `README.md`, `CONTRIBUTING.md` and `crates/*/README.md` with
  the network on. The `prek` hook runs the same check offline, so a dead upstream URL fails in CI
  rather than blocking a local commit.
- **codegen-drift** — re-runs `emit-types` and `git diff --exit-code` on `schema/`; fails if the
  committed types drifted from the schema. This is the only check covering the value-set
  `Literal`s (Datatype/Suffix/Modality/…), which `test_codegen.py` — DB-introspected `COLUMNS`
  only — does not.

## Docstrings

Google style, and `convention = "google"` under `[tool.ruff.lint.pydocstyle]` means that
first `ruff check` is the enforcement — a convention nobody checks is a convention that
drifts one merge at a time. Parameters go in `Args:`, one entry each, described there and
nowhere else. No types in them: the signature is already annotated, and the second copy is
the one that goes stale.

Shape is the cheap part. The rule worth keeping is the one that predates the convention: a
docstring says *why*. It names the trade-off that was rejected and cites the ADR where
there is one — `query.sibling`'s `via` is a paragraph on why a hardcoded `dataset_id`
cannot do that job, not a paragraph on what a link name is. A docstring that restates the
function name is worse than none: it costs a read and returns nothing, and it will still be
sitting there long after the name has moved on without it.

Plain backticks. Sphinx roles (`` :func:`x` ``, `` :class:`~y.Z` ``) survive from a docs
build that no longer exists; nothing renders them now, so they only clutter the one place a
docstring is actually read, which is the source.

Generated docstrings are fixed at the generator — `crates/bidslake-py/src/bin/emit_types.rs`
for `schema/_generated.py` and `schema/models.py`, `python/bidslake/stubgen.py` for a
catalog's re-emitted vocabulary — and never in the output. Those two files are
ruff-excluded to make that hard to get wrong: an edit there survives until the next
regeneration and fails `codegen-drift` in the meantime.

Tests are exempt from D103, and from nothing else. A test's docstring is where the *why*
goes — the relationship between two calls, the failure the assertion rules out — and plenty
of tests have no such thing to say, their name having already said it. Requiring one
everywhere would have bought 141 docstrings restating 141 test names, which is the rule
above run backwards. The formatting rules still apply in `tests/`, so the docstrings that
are there stay well-formed, and so does the module docstring saying what the file covers.
