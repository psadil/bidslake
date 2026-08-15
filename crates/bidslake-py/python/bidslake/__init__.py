"""bidslake — typed querying of BIDS-in-DuckDB datasets.

Open a database with :func:`open` and query it by BIDS concept::

    import bidslake
    lake = bidslake.open("study.duckdb")

    # The headline: an iterable of every resting-state fMRI file.
    for f in lake.get(task="rest", suffix="bold", extension=".nii.gz"):
        do_something(f.local_path)

    # Or work with whole tables as Polars.
    df = lake.scans.pl()

Anything :func:`~bidslake.BidsLake.get` cannot express — a join, a disjunction, an
aggregate — is an ordinary SQLAlchemy statement over the generated models, compiled
here and run by the same engine::

    from sqlalchemy import select
    from bidslake.schema.models import AllFiles, Sidecars

    lake.sql(
        select(AllFiles.file_path, Sidecars.RepetitionTime)
        .join(Sidecars, Sidecars.file_id == AllFiles.file_id)
        .where(AllFiles.task == "rest", AllFiles.kind == "data")
    )
"""

from __future__ import annotations

import os
from collections.abc import Mapping

from .file import BidsFile
from .layout import BidsLake, Table
from .layouts import Layout, LayoutAt, RoleState, layout
from .paths import RemotePathError, to_local_path, to_uri
from .query import Sibling, sibling, sibling_path, unresolved
from .relations import Relation
from .schema import C

# `_bidslake` (the compiled extension) is intentionally not re-exported here — it
# is a private implementation detail, imported and used by `layout`. It remains
# importable as `bidslake._bidslake` for anyone who needs it.
__all__ = [
    "BidsFile",
    "BidsLake",
    "C",
    "Layout",
    "LayoutAt",
    "Relation",
    "RemotePathError",
    "RoleState",
    "Sibling",
    "Table",
    "layout",
    "open",
    "sibling",
    "sibling_path",
    "to_local_path",
    "to_uri",
    "unresolved",
]


def open(
    path: str,
    *,
    read_only: bool = True,
    base_dir: str | os.PathLike[str] | None = None,
    root_override: Mapping[str, str | os.PathLike[str]] | None = None,
) -> BidsLake:
    """Open the bidslake DuckDB database at ``path`` (read-only by default).

    ``base_dir`` rebases every dataset's stored ``root_uri`` under a new parent
    (keeping its directory name), and ``root_override`` maps specific
    ``dataset_id``\\ s to explicit new roots — both for querying a dataset that
    has moved since it was indexed. ``root_override`` wins per dataset.
    """
    return BidsLake(path, read_only=read_only, base_dir=base_dir, root_override=root_override)
