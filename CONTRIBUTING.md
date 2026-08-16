# Contributing

Currently scoped to how tests are written. Everything else — build, layout, schema
decisions — lives in `docs/adr/` and the crate READMEs.

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
