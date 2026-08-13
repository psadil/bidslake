"""Small shared SQL-building helpers.

The one place identifier quoting lives, so `layout`, `_lazy`, and the wide-view
builder all quote the same way. (BIDS TSV headers / metadata fields can be
reserved words or mixed-case, so every identifier is double-quoted.)

The two relation names below live here for the same reason, and because `binding`
needs them without importing `layout`, which imports it.
"""

from __future__ import annotations

#: The file registry with its BIDS-concept columns — the relation every file-keyed
#: table joins to reach `sub`/`ses`/`task`/... (docs/adr/0006).
ALL_FILES = "all_files"

#: The data-file subset of it. A `kind` filter rather than a database object, so it is
#: spelled out wherever it is queried and resolved to `all_files` wherever it is
#: described. The default relation for anything that means "the files in this catalog".
DATAFILES = "datafiles"


def quote_ident(name: str) -> str:
    """Quote a SQL identifier for DuckDB (`"` doubled)."""
    return '"' + name.replace('"', '""') + '"'
