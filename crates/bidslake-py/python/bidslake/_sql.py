"""Small shared SQL-building helpers.

The one place identifier quoting lives, so `layout`, `_lazy`, and the wide-view
builder all quote the same way. (BIDS TSV headers / metadata fields can be
reserved words or mixed-case, so every identifier is double-quoted.)
"""

from __future__ import annotations


def quote_ident(name: str) -> str:
    """Quote a SQL identifier for DuckDB (`"` doubled)."""
    return '"' + name.replace('"', '""') + '"'
