"""Lazy table access via the Polars IO source (projection + correct predicates)."""

from __future__ import annotations

import polars as pl
import pytest


def test_lazy_matches_eager_with_predicate(lake):
    # The source must apply predicates (Polars does not re-apply them), so lazy
    # and eager results must be identical. Both halves are the claim, so the pair of
    # calls is one Act.
    cols = ["dataset_id", "file_path", "task"]
    pred = (pl.col("task") == "rest") & (pl.col("suffix") == "bold")

    eager = lake.all_files.pl().filter(pred).select(cols).sort("file_path")
    lazy = lake.all_files.lazy().filter(pred).select(cols).sort("file_path").collect()

    assert eager.equals(lazy)


@pytest.fixture(scope="module")
def projected_sidecars(lake):
    """Two columns out of the widest table in the catalog."""
    return lake.sidecars.lazy().select("file_id", "RepetitionTime").collect()


def test_lazy_projection_selects_only_the_named_columns(projected_sidecars):
    assert projected_sidecars.columns == ["file_id", "RepetitionTime"]


def test_lazy_projection_keeps_every_row(projected_sidecars, lake):
    """Narrowing the columns must not narrow the rows."""
    assert projected_sidecars.height == lake.sidecars.pl().height


@pytest.fixture(scope="module")
def projected_files_view(lake):
    """The same, over the wide joined view rather than a stored table."""
    return (
        lake.files.lazy()
        .filter(pl.col("suffix") == "bold")
        .select("file_path", "sidecar__RepetitionTime")
        .collect()
    )


def test_lazy_over_files_view_projects(projected_files_view):
    assert projected_files_view.columns == ["file_path", "sidecar__RepetitionTime"]


def test_lazy_over_files_view_returns_rows(projected_files_view):
    assert projected_files_view.height > 0


def test_lazy_returns_lazyframe(lake):
    assert isinstance(lake.all_files.lazy(), pl.LazyFrame)
