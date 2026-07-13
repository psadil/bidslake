"""Schema-augmentation (overlay) behavior on the Python side.

Builds a tiny fMRIPrep-style derivative database with the bundled `fmriprep`
overlay, then checks that augmented columns are queryable at runtime with no extra
step, that the overlay provenance and effective schema are recoverable, and that the
opt-in stubgen types the augmented schema.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import bidslake
import pytest
from bidslake import stubgen


def _repo_root() -> Path:
    out = subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True)
    return Path(out.strip())


@pytest.fixture(scope="module")
def augmented_db(tmp_path_factory: pytest.TempPathFactory) -> str:
    binary = _repo_root() / "target" / "debug" / "bidslake"
    if not binary.exists():
        pytest.skip("build the debug binary first: cargo build -p bidslake")

    root = tmp_path_factory.mktemp("deriv")
    (root / "sub-01" / "func").mkdir(parents=True)
    (root / "dataset_description.json").write_text(
        '{"Name":"deriv","BIDSVersion":"1.11.1","DatasetType":"derivative"}'
    )
    func = root / "sub-01" / "func"
    (func / "sub-01_task-rest_desc-preproc_bold.nii.gz").write_bytes(b"")
    (func / "sub-01_task-rest_desc-preproc_bold.json").write_text('{"RepetitionTime":2.0}')
    (func / "sub-01_task-rest_desc-confounds_timeseries.tsv").write_text(
        "trans_x\ttrans_y\n0.10\t0.20\n0.11\t0.21\n0.12\t0.22\n"
    )

    db = tmp_path_factory.mktemp("db") / "aug.duckdb"
    subprocess.run(
        [str(binary), "index", "-i", str(root), "-o", str(db), "--overlay", "fmriprep"],
        check=True,
        capture_output=True,
    )
    return str(db)


def test_overlay_provenance_and_effective_schema(augmented_db: str) -> None:
    with bidslake.open(augmented_db) as lake:
        assert [source for _idx, source, _sha in lake.overlays] == ["fmriprep"]
        schema = lake.effective_schema()
        assert schema is not None
        assert "fmriprep" in schema["rules"]["tabular_data"]


def test_augmented_columns_are_queryable_at_runtime(augmented_db: str) -> None:
    with bidslake.open(augmented_db) as lake:
        # The preprocessed BOLD is found by its (base) entities.
        files = list(lake.get(desc="preproc", suffix="bold"))
        assert len(files) == 1
        # The overlay's confounds table exists with its typed columns, ordered.
        assert "trans_x" in lake.columns("fmriprep_confounds")
        rows = lake.sql("SELECT row_idx, trans_x FROM fmriprep_confounds ORDER BY row_idx")
        assert rows["row_idx"].to_list() == [0, 1, 2]
        assert rows["trans_x"].to_list() == [0.10, 0.11, 0.12]


def test_stubgen_types_the_augmented_schema(augmented_db: str) -> None:
    module = stubgen.generate(augmented_db)
    assert '"timeseries"' in module, "augmented Suffix should include timeseries"
    assert "class fmriprep_confounds" in module, "C should gain the augmented table"
    assert '"from"' in module, "augmented entity should reach GetFilters/Entity"
