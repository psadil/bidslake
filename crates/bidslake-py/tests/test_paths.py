"""Path resolution and root rebasing (base_dir / root_override)."""

from __future__ import annotations

import bidslake


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
