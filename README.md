# bidslake

**A lakehouse for [BIDS](https://bids-specification.readthedocs.io/) datasets — DuckLake for
neuroimaging.**

bidslake walks a BIDS dataset and consolidates its metadata — JSON sidecars, `.tsv` tables and
filename entities — into a single [DuckDB](https://duckdb.org/) catalog, while the image files
stay on disk untouched. You then select, filter and edit with ordinary SQL, and hand the
resulting paths to whatever tool does the actual work.

> **Status: early and unstable.** Major architectural changes are expected, and the architecture
> decision records are marked `Provisional` accordingly.

## The workspace

| Crate | What it is |
| --- | --- |
| [`bidslake`](crates/bidslake) | The lakehouse: the ingest pipeline, the generated DuckDB schema, and the `bidslake` CLI. |
| [`bidslake-py`](crates/bidslake-py) | The Python query API — typed columns, SQLAlchemy composition, Polars results. |
| [`bids-core`](crates/bids-core) | Schema-agnostic BIDS primitives: the file tree, entities, datatypes, inheritance. |
| [`bids-schema`](crates/bids-schema) | BIDS semantics: the vendored schema and its expression language, plus overlays, term maps and layouts. |
| [`bidslake-synth`](crates/bidslake-synth) | Synthetic BIDS and derivative trees, built from the schemas — for benchmarks, and for authoring an adapter bundle. |
| [`bids-validator-rs`](crates/bids-validator-rs) | A pure-Rust BIDS validator, tracked for parity against the reference implementation. |
| [`hed-validator-rs`](crates/hed-validator-rs) | A HED validator, passing the upstream conformance suite. |

## Install

Requires a Rust toolchain. DuckDB is bundled, so there is no system library to install.

```bash
git clone --recurse-submodules git@github.com:psadil/bidslake.git
cd bidslake
cargo build --release
```

## Quickstart

The build above leaves the CLI at `target/release/bidslake`. Index a dataset:

```bash
./target/release/bidslake index --input path/to/bids/dataset --output dataset.duckdb
```

The catalog is an ordinary DuckDB file. Open it with anything that speaks DuckDB — the
[`duckdb` CLI](https://duckdb.org/docs/installation/), Python, R. (The DuckDB *engine* is
compiled into `bidslake`, so indexing needs no system library; the CLI is a separate download,
needed only if you want a SQL shell.)

```bash
duckdb dataset.duckdb
```

```sql
SELECT p.participant_id, p.age, f.file_path
FROM participants p
JOIN all_files f ON f.dataset_id = p.dataset_id AND f.kind = 'data'
                AND f.file_path LIKE p.participant_id || '/%'
WHERE p.age < 30 AND f.suffix = 'T1w';
```

## Tuning for a network filesystem

Indexing keeps a bounded number of filesystem operations in flight. The right number is a
property of the filesystem, not of the machine, and it is **not monotonic** — past the point a
server saturates, more concurrent requests queue and the ingest gets *slower*. On a busy shared
mount these usually want turning down.

Two dials, because the operations divide by which server answers them:

| Dial | Covers | Flag | Environment |
|---|---|---|---|
| Metadata | directory reads, file stats | `--metadata-concurrency N` | `BIDSLAKE_METADATA_CONCURRENCY` |
| Data | JSON sidecars, `.bval`/`.bvec`, adapter reads, TSV headers | `--read-concurrency N` | `BIDSLAKE_READ_CONCURRENCY` |

Both default to 16, which is what they were fixed at before they were configurable, so a run
that sets neither is unchanged. A flag beats the environment variable. The catalog is identical
at any width — these change how fast an ingest runs, never what it produces.

Tune by bisection against `BIDSLAKE_TIMING=1`, which prints a phase breakdown to stderr:
`walk` and `stat` respond to the metadata dial, `prefetch` to the data one.

```bash
BIDSLAKE_TIMING=1 ./target/release/bidslake index -i dataset -o out.duckdb \
    --metadata-concurrency 4 --read-concurrency 8
```

`BIDSLAKE_METADATA_CONCURRENCY` also tunes `bids-validator`, which walks the same trees through
the same code and has no flags of its own.

## Documentation

- **[The book](docs/introduction.md)** — orientation, vocabulary, the architecture decisions, and
  the roadmap. Build it with `mdbook serve`, or read the markdown directly in [`docs/`](docs/).
- **[Architecture decisions](docs/adr/index.md)** — why the system is shaped this way.
- **[Roadmap](docs/roadmap.md)** — managed mode, and what is not settled yet.
- **[Python API](crates/bidslake-py/README.md)** — opening a catalog, composing queries,
  resolving siblings, naming outputs through a layout.
- **API reference** — `cargo doc --open`. The `bidslake` crate page has runnable examples; its
  `schema` module is the table-by-table database reference.
- **[Contributing](CONTRIBUTING.md)** — how tests and docstrings are written here.
