"""Lazy table access via the Polars IO source (projection + correct predicates)."""

from __future__ import annotations

import polars as pl


def test_lazy_matches_eager_with_predicate(lake):
    # The source must apply predicates (Polars does not re-apply them), so lazy
    # and eager results must be identical.
    cols = ["dataset_id", "file_path", "task"]
    pred = (pl.col("task") == "rest") & (pl.col("suffix") == "bold")
    eager = lake.scans.pl().filter(pred).select(cols).sort("file_path")
    lazy = lake.scans.lazy().filter(pred).select(cols).sort("file_path").collect()
    assert eager.equals(lazy)


def test_lazy_projection_on_wide_table(lake):
    df = lake.sidecars.lazy().select("dataset_id", "RepetitionTime").collect()
    assert df.columns == ["dataset_id", "RepetitionTime"] and df.height == lake.sidecars.pl().height


def test_lazy_over_files_view(lake):
    df = (
        lake.files.lazy()
        .filter(pl.col("suffix") == "bold")
        .select("file_path", "sidecar__RepetitionTime")
        .collect()
    )
    assert df.height > 0 and df.columns == ["file_path", "sidecar__RepetitionTime"]


def test_lazy_returns_lazyframe(lake):
    assert isinstance(lake.scans.lazy(), pl.LazyFrame)
