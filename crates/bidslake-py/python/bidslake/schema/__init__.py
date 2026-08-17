"""Types generated from the vendored BIDS schema.

`_generated.py` (re-exported here) and its sibling `models.py` are emitted by
`cargo run -p bidslake-py --bin emit-types` and are never hand-edited; the `codegen-drift`
CI job fails when either has drifted from the schema.

`C` carries typed `pl.col` accessors per table, `GetFilters` types the keyword filters
`BidsLake.get` takes, `COLUMNS` maps each table to its columns and their DuckDB types, and
`Entity`, `Datatype`, `Suffix` and their neighbours are the literal unions the schema
defines. `SCHEMA_VERSION` names the schema version all of it came from.
"""

from ._generated import (
    COLUMNS,
    SCHEMA_VERSION,
    C,
    Datatype,
    Entity,
    GetFilters,
    Handedness,
    Modality,
    Sex,
    Suffix,
)

__all__ = [
    "COLUMNS",
    "SCHEMA_VERSION",
    "C",
    "Datatype",
    "Entity",
    "GetFilters",
    "Handedness",
    "Modality",
    "Sex",
    "Suffix",
]
