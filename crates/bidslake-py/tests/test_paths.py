"""Path resolution and root rebasing (base_dir / root_override)."""

from __future__ import annotations

import bidslake
import polars as pl
import pytest


def test_default_resolves_to_existing_file(lake):
    f = next(lake.get(task="rest", suffix="bold", dataset_id="ds210"))

    assert f.local_path.exists()


def test_root_override_redirects_one_dataset(lake_db):
    lake = bidslake.open(str(lake_db), root_override={"ds210": "/relocated/ds210"})

    f = next(lake.get(suffix="bold", dataset_id="ds210"))

    assert f.uri == f"file:///relocated/ds210/{f.file_path}"


def test_base_dir_rebases_keeping_dataset_name(lake_db):
    lake = bidslake.open(str(lake_db), base_dir="/mnt/data")

    f = next(lake.get(suffix="bold", dataset_id="ds210"))

    assert f.uri == f"file:///mnt/data/ds210/{f.file_path}"


def test_root_override_wins_over_base_dir(lake_db):
    lake = bidslake.open(
        str(lake_db), base_dir="/mnt/data", root_override={"ds210": "s3://bucket/ds210"}
    )

    f = next(lake.get(suffix="bold", dataset_id="ds210"))

    assert f.uri == f"s3://bucket/ds210/{f.file_path}"


# -- resolving a row that is not a data file ---------------------------------
#
# `resolve` is the route to columns a catalog deliberately did not store: they stay in the
# file, which the registry indexes but does not read. Sorted rather than taken in catalog
# order so the two tests below are about the same file on every run.


@pytest.fixture(scope="module")
def resolved_tabular_path(lake):
    """The location of one ingested tabular file, resolved through the catalog."""
    rows = lake.all_files.pl().filter(
        (pl.col("kind") == "tabular") & (pl.col("status") == "ingested")
    )
    assert rows.height > 0, "fixture assumption: the catalog holds ingested tabular files"
    row = rows.sort("dataset_id", "file_path").row(0, named=True)
    return lake.resolve(row["dataset_id"], row["file_path"], row["root_uri"])


def test_resolve_reaches_a_real_file(resolved_tabular_path):
    assert resolved_tabular_path.exists(), f"{resolved_tabular_path} should be readable"


def test_a_resolved_path_opens_for_reading(resolved_tabular_path):
    """The point of resolving at all: the file can be read, not merely located.

    `.open()`, not `str(path)`: a UPath stringifies back to a URI.
    """
    df = pl.read_csv(resolved_tabular_path.open("rb"), separator="\t", null_values="n/a")

    assert df.height > 0


def test_resolve_honors_root_override(lake_db):
    lake = bidslake.open(str(lake_db), root_override={"ds210": "/relocated/ds210"})

    path = lake.resolve("ds210", "sub-01/anat/x.nii.gz")

    assert str(path).endswith("/relocated/ds210/sub-01/anat/x.nii.gz")
