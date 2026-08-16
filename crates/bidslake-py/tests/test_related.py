"""Cross-dataset relations: `related_datasets` / `Relation` (docs/adr/0003).

Builds a two-dataset catalog whose datasets declare the *same* source DOI in different forms
(a bare DOI vs a `https://doi.org/…` URL), so they are co-derivatives (`shares_source`). The
shared source (the raw dataset) is deliberately absent — the relation still resolves.
"""

from __future__ import annotations

import json
from collections.abc import Callable
from pathlib import Path

import bidslake
import pytest
from bidslake import Relation

#: Each dataset names the same source, spelled the two ways BIDS allows.
SOURCE_DOI = {
    "fmriprep": "https://doi.org/10.18112/openneuro.ds001761.v2.0.1",
    "mriqc": "10.18112/openneuro.ds001761.v2.0.1",
}


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
def linked_lake(tmp_path_factory: pytest.TempPathFactory, bidslake_cli: Callable[..., None]):
    base = tmp_path_factory.mktemp("linked")
    db = base / "cat.duckdb"
    for name, doi in SOURCE_DOI.items():
        _write_dataset(base / name, name, doi)
        bidslake_cli("index", "--input", base / name, "--output", db, "--dataset-id", name)
    return bidslake.open(str(db))


def test_datasets_lists_both(linked_lake):
    assert set(linked_lake.datasets()["dataset_id"]) == set(SOURCE_DOI)


@pytest.mark.parametrize(("anchor", "expected"), [("fmriprep", ["mriqc"]), ("mriqc", ["fmriprep"])])
def test_shares_source_resolves_from_either_side(linked_lake, anchor, expected):
    """The relation is symmetric: the DOI spelling differs, the source does not."""
    related = linked_lake.related_datasets(anchor, relation=Relation.SHARES_SOURCE)

    assert related == expected


def test_unrelated_is_empty(linked_lake):
    assert linked_lake.related_datasets("does-not-exist") == []


@pytest.fixture(scope="module")
def fmriprep_t1w(linked_lake):
    """The fMRIPrep anatomical, which the file-level relation hangs off."""
    files = list(linked_lake.get(dataset_id="fmriprep", suffix="T1w", extension=".nii.gz"))
    assert files, "fixture assumption: the fMRIPrep T1w should be indexed"
    return files[0]


def test_bidsfile_related_datasets(fmriprep_t1w):
    # bidslake gives the dataset relation; the caller then matches files by entity.
    assert fmriprep_t1w.related_datasets(relation=Relation.SHARES_SOURCE) == ["mriqc"]


def test_relation_str_is_the_stored_value():
    assert str(Relation.SHARES_SOURCE) == "shares_source"


def test_relation_is_constructible_from_its_value():
    assert Relation("derived_from") is Relation.DERIVED_FROM
