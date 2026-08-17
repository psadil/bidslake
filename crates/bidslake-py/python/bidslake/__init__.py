"""bidslake — typed querying of BIDS-in-DuckDB datasets.

Open the catalog a `bidslake index` produced with `open`, and query it by BIDS concept:

    import bidslake
    lake = bidslake.open("study.duckdb")

    # The headline: an iterable of every resting-state fMRI file.
    for f in lake.get(task="rest", suffix="bold", extension=".nii.gz"):
        do_something(f.local_path)

    # Or work with whole tables as Polars.
    df = lake.scans.pl()

Anything `BidsLake.get` cannot express — a join, a disjunction, an aggregate — is an
ordinary SQLAlchemy statement over the generated models, compiled here and run by the same
engine:

    from sqlalchemy import select
    from bidslake.schema.models import AllFiles, Sidecars

    lake.sql(
        select(AllFiles.file_path, Sidecars.RepetitionTime)
        .join(Sidecars, Sidecars.file_id == AllFiles.file_id)
        .where(AllFiles.task == "rest", AllFiles.kind == "data")
    )

Beyond those two ways in: `sibling`, `sibling_path` and `unresolved` answer "which other
files belong with this one" for a whole study in one statement; `layout` names a pipeline's
*output* files, which no query can find because they do not exist yet; `Relation` is how one
dataset in the catalog relates to another; and `C` carries typed `pl.col` accessors per
table.
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
    """Open the bidslake DuckDB database at `path` (read-only by default).

    A dataset's `root_uri` is stored as the ingesting host saw it, so both rebasing
    arguments exist for querying a dataset that has moved since it was indexed.

    Args:
        path: The DuckDB catalog to open.
        read_only: Querying never mutates the catalog, and a read-only handle does not
            contend with a writer.
        base_dir: Rebases every dataset's stored `root_uri` under a new parent, keeping its
            directory name.
        root_override: Maps specific `dataset_id`s to explicit new roots. Wins over
            `base_dir`, per dataset.
    """
    return BidsLake(path, read_only=read_only, base_dir=base_dir, root_override=root_override)
