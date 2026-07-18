"""Cross-dataset relations: `related_datasets` / `Relation` (docs/adr/0003).

Builds a two-dataset catalog whose datasets declare the *same* source DOI in different forms
(a bare DOI vs a `https://doi.org/…` URL), so they are co-derivatives (`shares_source`). The
shared source (the raw dataset) is deliberately absent — the relation still resolves.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import bidslake
import pytest
from bidslake import Relation


def _repo_root() -> Path:
    out = subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True)
    return Path(out.strip())


def _write_dataset(root: Path, name: str, source_doi: str) -> None:
    (root / "sub-01" / "anat").mkdir(parents=True)
    (root / "dataset_description.json").write_text(
        json.dumps(
            {
                "Name": name,
                "BIDSVersion": "1.9.0",
                "DatasetType": "derivative",
                "SourceDatasets": [{"DOI": source_doi}],
            }
        )
    )
    (root / "sub-01" / "anat" / "sub-01_T1w.nii.gz").write_bytes(b"")


@pytest.fixture(scope="module")
def linked_lake(tmp_path_factory: pytest.TempPathFactory):
    repo = _repo_root()
    base = tmp_path_factory.mktemp("linked")
    trees = {
        "fmriprep": "https://doi.org/10.18112/openneuro.ds001761.v2.0.1",
        "mriqc": "10.18112/openneuro.ds001761.v2.0.1",
    }
    db = base / "cat.duckdb"
    for name, doi in trees.items():
        _write_dataset(base / name, name, doi)
        subprocess.run(
            [
                "cargo",
                "run",
                "-q",
                "--release",
                "-p",
                "bidslake",
                "--",
                "index",
                "--input",
                str(base / name),
                "--output",
                str(db),
                "--dataset-id",
                name,
            ],
            cwd=repo,
            check=True,
        )
    return bidslake.open(str(db))


def test_datasets_lists_both(linked_lake):
    assert set(linked_lake.datasets()["dataset_id"]) == {"fmriprep", "mriqc"}


def test_shares_source_both_directions(linked_lake):
    assert linked_lake.related_datasets("fmriprep", relation=Relation.SHARES_SOURCE) == ["mriqc"]
    assert linked_lake.related_datasets("mriqc", relation=Relation.SHARES_SOURCE) == ["fmriprep"]


def test_unrelated_is_empty(linked_lake):
    assert linked_lake.related_datasets("does-not-exist") == []


def test_bidsfile_related_datasets(linked_lake):
    files = list(linked_lake.get(dataset_id="fmriprep", suffix="T1w", extension=".nii.gz"))
    assert files, "expected the fMRIPrep T1w"
    # bidslake gives the dataset relation; the caller then matches files by entity.
    assert files[0].related_datasets(relation=Relation.SHARES_SOURCE) == ["mriqc"]


def test_relation_enum_str_value():
    assert str(Relation.SHARES_SOURCE) == "shares_source"
    assert Relation("derived_from") is Relation.DERIVED_FROM
