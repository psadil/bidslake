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
        # The preprocessed BOLD is found by its (base) entities. `kind` because `get`
        # iterates the whole registry: the image and its sidecar share every entity, and
        # which of them you want is the caller's to say.
        files = list(lake.get(desc="preproc", suffix="bold", kind="data"))
        assert len(files) == 1
        assert len(list(lake.get(desc="preproc", suffix="bold"))) == 2, (
            "unfiltered, the sidecar is a row too"
        )
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


GOOD_QUERY = '''\
"""Queries over overlay-added vocabulary: must type-check."""

from sqlalchemy import select

from _bids_types import AllFiles, FmriprepConfounds, GetFilters

# `from`/`to` are fMRIPrep's entities, not BIDS'; against the bundled `GetFilters`
# neither key exists.
preproc: GetFilters = {"datatype": "func", "suffix": "bold", "desc": "preproc"}
xfm: GetFilters = {"suffix": "xfm", "from": "boldref", "to": "T1w"}

# The models the same catalog generated. `from` is a Python keyword, so its column
# is reachable as `from_`; every other entity keeps its own name.
Q = (
    select(AllFiles.file_path, AllFiles.to, FmriprepConfounds.trans_x)
    .join(FmriprepConfounds, FmriprepConfounds.file_id == AllFiles.file_id)
    .where(AllFiles.from_ == "boldref")
)
'''

BAD_QUERY = '''\
"""Queries the augmented vocabulary must still reject."""

from sqlalchemy import select

from _bids_types import AllFiles, FmriprepConfounds, GetFilters

bad_key: GetFilters = {"frm": "boldref"}
bad_suffix: GetFilters = {"suffix": "notarealsuffix"}
bad_datatype: GetFilters = {"datatype": "fnuc"}
bad_column = select(AllFiles.notanentity)
bad_overlay_column = select(FmriprepConfounds.notacolumn)
'''


def _ty(path: Path, search: Path) -> subprocess.CompletedProcess[str]:
    ty = Path(sys.executable).with_name("ty")
    if not ty.exists():
        pytest.skip("ty not installed in this environment")
    # `sys.prefix`, not `Path(sys.executable).resolve()`: resolving follows the venv's
    # symlink back to the base interpreter, whose site-packages has none of the
    # third-party imports the fixtures make (`sqlalchemy`), so every one of them would
    # be reported unresolved and the "good" fixture would fail for the wrong reason.
    return subprocess.run(
        [str(ty), "check", "--python", sys.prefix, "--extra-search-path", str(search), str(path)],
        capture_output=True,
        text=True,
        cwd=Path(__file__).resolve().parents[1],
    )


def test_stubgen_queries_check_against_the_augmented_vocabulary(
    augmented_db: str, tmp_path: Path
) -> None:
    """The generated filters and models accept overlay vocabulary — and only real ones.

    The bundled `GetFilters` and models are pinned to the BIDS schema this build ships,
    so `from`/`to`/`trans_x` are type errors there; the point of re-emitting them from a
    catalog is that they stop being errors *without* the vocabulary going untyped.
    """
    (tmp_path / "_bids_types.py").write_text(stubgen.generate(augmented_db))
    good = tmp_path / "good_query.py"
    good.write_text(GOOD_QUERY)
    bad = tmp_path / "bad_query.py"
    bad.write_text(BAD_QUERY)

    ok = _ty(good, tmp_path)
    assert ok.returncode == 0, ok.stdout + ok.stderr

    rejected = _ty(bad, tmp_path)
    out = rejected.stdout + rejected.stderr
    assert rejected.returncode != 0
    for expected in ("frm", "notarealsuffix", "fnuc", "notanentity", "notacolumn"):
        assert expected in out, f"{expected} was not flagged:\n{out}"
