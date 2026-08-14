"""Small shared SQL-building helpers.

The one place identifier quoting lives, so `layout`, `_lazy`, and the wide-view
builder all quote the same way. (BIDS TSV headers / metadata fields can be
reserved words or mixed-case, so every identifier is double-quoted.)

The registry's name and the SQLAlchemy compilation step live here for the same
reason: both are shared, and neither can import `layout`, which imports this.
"""

from __future__ import annotations

from typing import Any

from sqlalchemy.dialects import postgresql
from sqlalchemy.sql import ClauseElement
from sqlalchemy.sql.compiler import SQLCompiler

#: The dialect statements compile against. Built once: constructing one per call is pure
#: overhead, and it holds no connection state.
_DIALECT = postgresql.dialect(paramstyle="qmark")

#: The file registry with its BIDS-concept columns — the relation every file-keyed
#: table joins to reach `sub`/`ses`/`task`/... (docs/adr/0006).
ALL_FILES = "all_files"


def quote_ident(name: str) -> str:
    """Quote a SQL identifier for DuckDB (`"` doubled)."""
    return '"' + name.replace('"', '""') + '"'


def compile_statement(stmt: ClauseElement) -> tuple[str, list[Any]]:
    """A SQLAlchemy statement as ``(sql, positional_params)`` for the Rust executor.

    Compiled, never executed: bidslake's connection lives in Rust, and this exists so a
    statement can be *built* with SQLAlchemy and run by that engine. No `Engine`, no
    `Session`, no second database handle, and the result still arrives over the Arrow
    bridge as Polars.

    The dialect is PostgreSQL's with `paramstyle="qmark"`. DuckDB's SQL is close enough to
    PostgreSQL's for everything a query builds — quoting, `IS NULL`, `IS NOT DISTINCT
    FROM`, `LATERAL` — and `qmark` is what makes it emit the `?` placeholders DuckDB binds.
    Using the stock dialect is what keeps `duckdb-engine`, and with it a second embedded
    DuckDB, out of the dependency list.

    Compiled params come back as a *dict* under a positional paramstyle; `positiontup` is
    the order the `?`s expect, and pairing them any other way silently binds the wrong
    values to the wrong placeholders.
    """
    compiled = stmt.compile(dialect=_DIALECT)
    # `compile` is typed as returning the base `Compiled`; only the SQL compiler carries
    # `positiontup`. Narrowed rather than cast, so a statement that somehow compiled to
    # something else says so here instead of failing on a missing attribute.
    if not isinstance(compiled, SQLCompiler):
        msg = f"{type(stmt).__name__} did not compile to SQL (got {type(compiled).__name__})"
        raise TypeError(msg)
    # `positiontup` is None for a *named* paramstyle, where there are no positional
    # parameters to order; `_DIALECT` is qmark so it is always a list here, and `or ()`
    # keeps that from being an assumption.
    order = compiled.positiontup or ()
    return str(compiled), [compiled.params[k] for k in order]
