"""Path resolution and root rebasing (base_dir / root_override)."""

from __future__ import annotations

import bidslake
import polars as pl


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


def test_resolve_opens_a_row_from_any_table(lake):
    """`resolve` reaches files that are not data files — the route to columns a
    catalog deliberately did not store (they stay in the file, indexed by the
    registry)."""
    row = (
        lake.all_files.pl()
        .filter((pl.col("kind") == "tabular") & (pl.col("status") == "ingested"))
        .row(0, named=True)
    )
    path = lake.resolve(row["dataset_id"], row["file_path"], row["root_uri"])
    assert path.exists(), f"{path} should be readable"
    # `.open()`, not `str(path)`: a UPath stringifies back to a URI.
    assert pl.read_csv(path.open("rb"), separator="\t", null_values="n/a").height >= 0


def test_resolve_honors_root_override(lake_db):
    lake = bidslake.open(str(lake_db), root_override={"ds210": "/relocated/ds210"})
    assert str(lake.resolve("ds210", "sub-01/anat/x.nii.gz")).endswith(
        "/relocated/ds210/sub-01/anat/x.nii.gz"
    )
