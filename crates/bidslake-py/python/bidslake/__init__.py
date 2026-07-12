"""bidslake — typed querying of BIDS-in-DuckDB datasets.

Open a database with :func:`open` and query it by BIDS concept::

    import bidslake
    lake = bidslake.open("study.duckdb")

    # The headline: an iterable of every resting-state fMRI file.
    for f in lake.get(task="rest", suffix="bold", extension=".nii.gz"):
        do_something(f.local_path)

    # Or work with whole tables as Polars.
    df = lake.scans.pl()
"""

from __future__ import annotations

import os
from collections.abc import Mapping

from . import _bidslake
from .file import BidsFile
from .layout import BidsLake, Table
from .paths import RemotePathError
from .schema import C

__all__ = ["BidsFile", "BidsLake", "C", "RemotePathError", "Table", "_bidslake", "open"]


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
