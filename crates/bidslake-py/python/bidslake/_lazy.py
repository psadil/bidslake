"""Lazy table access: a Polars IO source backed by the Rust query bridge.

`Table.lazy()` returns a `pl.LazyFrame` whose source is registered here. When
Polars collects it, it hands the source the requested **columns** (projection)
and a **predicate**; we push the projection into the DuckDB `SELECT` (fetch only
those columns — the big win for wide tables like `sidecars`) and apply the
predicate ourselves.

Correctness note: Polars does **not** re-apply a predicate the source was given
(verified) — the source owns it. We apply it via Polars on the fetched frame
(`df.filter(predicate)`), which is always correct. That means the predicate is
*not* pushed into DuckDB (DuckDB returns all rows for the projected columns);
pushing predicates into SQL is a future optimization, but must never be the only
place filtering happens. When a predicate is present we fetch all columns (so it
can reference any of them), filter, then project.
"""

from __future__ import annotations

from collections.abc import Iterator
from typing import TYPE_CHECKING

import polars as pl
from polars.io.plugins import register_io_source

from ._sql import quote_ident

if TYPE_CHECKING:
    from .layout import BidsLake


def build_lazy(lake: BidsLake, base_sql: str) -> pl.LazyFrame:
    """A `LazyFrame` over `base_sql` (a table or the wide-view query)."""
    # Full schema up front (Polars needs it to plan); zero-row fetch is cheap.
    schema = lake._query(f"SELECT * FROM ({base_sql}) AS _t LIMIT 0", []).schema

    def source(
        with_columns: list[str] | None,
        predicate: pl.Expr | None,
        n_rows: int | None,
        batch_size: int | None,
    ) -> Iterator[pl.DataFrame]:
        # Projection pushdown only when there's no predicate to evaluate; with a
        # predicate we need every column it might reference, so fetch all.
        if with_columns is not None and predicate is None:
            select = ", ".join(quote_ident(c) for c in with_columns)
        else:
            select = "*"
        df = lake._query(f"SELECT {select} FROM ({base_sql}) AS _t", [])
        if predicate is not None:
            df = df.filter(predicate)
            if with_columns is not None:
                df = df.select(with_columns)
        if n_rows is not None:
            df = df.head(n_rows)
        yield df

    return register_io_source(source, schema=schema)


__all__ = ["build_lazy"]
