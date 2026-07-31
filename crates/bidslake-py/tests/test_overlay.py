"""Schema-augmentation (overlay) behavior on the Python side.

Builds a tiny fMRIPrep-style derivative database with the bundled `fmriprep`
overlay, then checks that augmented columns are queryable at runtime with no extra
step, that the overlay provenance and effective schema are recoverable, and that the
opt-in stubgen types the augmented schema.
"""

from __future__ import annotations

import subprocess
import sys
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
        [str(binary), "index", "-i", str(root), "-o", str(db), "--adapter", "fmriprep"],
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


GOOD_BINDING = '''\
"""A binding over overlay-added vocabulary: must type-check."""

from _bids_types import Binding, FileInput

DENOISE = Binding(
    anchor={"datatype": "func", "suffix": "bold", "desc": "preproc"},
    key=("sub", "ses", "task", "run"),
    inputs={
        "xfm": FileInput(
            join=("sub", "ses"),
            where={"suffix": "xfm", "from": "boldref", "to": "T1w"},
        ),
    },
)
'''

BAD_BINDING = '''\
"""Bindings the augmented vocabulary must still reject."""

from _bids_types import Binding, FileInput

bad_key = FileInput(join=("sub",), where={"frm": "boldref"})
bad_suffix = FileInput(join=("sub",), where={"suffix": "notarealsuffix"})
bad_join = FileInput(join=("notanentity",), where={"from": "boldref"})
bad_anchor = Binding(anchor={"datatype": "fnuc"}, key=("sub",), inputs={})
'''


def _ty(path: Path, search: Path) -> subprocess.CompletedProcess[str]:
    ty = Path(sys.executable).with_name("ty")
    if not ty.exists():
        pytest.skip("ty not installed in this environment")
    venv = Path(sys.executable).resolve().parents[1]
    return subprocess.run(
        [str(ty), "check", "--python", str(venv), "--extra-search-path", str(search), str(path)],
        capture_output=True,
        text=True,
        cwd=Path(__file__).resolve().parents[1],
    )


def test_stubgen_bindings_check_against_the_augmented_vocabulary(
    augmented_db: str, tmp_path: Path
) -> None:
    """The generated `Binding`/`FileInput` accept overlay entities — and only real ones.

    The bundled dataclasses are pinned to the BIDS schema this build ships, so
    `from`/`xfm` are type errors there; the point of re-emitting them from a catalog
    is that they stop being errors *without* the vocabulary going untyped.
    """
    (tmp_path / "_bids_types.py").write_text(stubgen.generate(augmented_db))
    good = tmp_path / "good_binding.py"
    good.write_text(GOOD_BINDING)
    bad = tmp_path / "bad_binding.py"
    bad.write_text(BAD_BINDING)

    ok = _ty(good, tmp_path)
    assert ok.returncode == 0, ok.stdout + ok.stderr

    rejected = _ty(bad, tmp_path)
    out = rejected.stdout + rejected.stderr
    assert rejected.returncode != 0
    for expected in ("frm", "notarealsuffix", "notanentity", "fnuc"):
        assert expected in out, f"{expected} was not flagged:\n{out}"
