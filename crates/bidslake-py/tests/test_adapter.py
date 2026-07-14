"""Layout-adapter (FreeSurfer) behavior on the Python side.

Builds a tiny FreeSurfer `SUBJECTS_DIR` and indexes it with the bundled
`freesurfer` adapter, then checks that the adapter's tables are queryable at
runtime with no codegen step, that concept filters work, that values are typed, and
that adapter provenance is recoverable.
"""

# The embedded FreeSurfer `.stats` fixtures below reproduce real column-header and
# `# Measure` lines verbatim, some of which exceed the line-length limit.
# ruff: noqa: E501

from __future__ import annotations

import subprocess
from pathlib import Path

import bidslake
import pytest

ASEG_STATS = """\
# Measure BrainSeg, BrainSegVol, Brain Segmentation Volume, 1200000.000000, mm^3
# Measure EstimatedTotalIntraCranialVol, eTIV, Estimated Total Intracranial Volume, 1500000.000000, mm^3
# ColHeaders  Index SegId NVoxels Volume_mm3 StructName normMean normStdDev normMin normMax normRange
  1   4   5000   5100.0  Left-Lateral-Ventricle   35.0  10.0  10  90  80
  3  17   4200   4300.2  Left-Hippocampus         70.0   9.0  40 110  70
"""

APARC_STATS = """\
# Measure Cortex, MeanThickness, Mean Thickness, 2.5, mm
# ColHeaders StructName NumVert SurfArea GrayVol ThickAvg ThickStd MeanCurv GausCurv FoldInd CurvInd
bankssts         1000  700  2000  2.5  0.5  0.100  0.020  15  0.9
superiorfrontal  5000 3500 11000  2.8  0.6  0.090  0.020  30  2.5
"""


def _repo_root() -> Path:
    out = subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True)
    return Path(out.strip())


@pytest.fixture(scope="module")
def freesurfer_db(tmp_path_factory: pytest.TempPathFactory) -> str:
    binary = _repo_root() / "target" / "debug" / "bidslake"
    if not binary.exists():
        pytest.skip("build the debug binary first: cargo build -p bidslake")

    root = tmp_path_factory.mktemp("freesurfer")
    stats = root / "sub-01_ses-1" / "stats"
    surf = root / "sub-01_ses-1" / "surf"
    stats.mkdir(parents=True)
    surf.mkdir(parents=True)
    (stats / "aseg.stats").write_text(ASEG_STATS)
    (stats / "lh.aparc.stats").write_text(APARC_STATS)
    (surf / "lh.thickness").write_bytes(b"binary")

    db = tmp_path_factory.mktemp("db") / "fs.duckdb"
    subprocess.run(
        [
            str(binary),
            "index",
            "-i",
            str(root),
            "-o",
            str(db),
            "--dataset-id",
            "fsdemo",
            "--adapter",
            "freesurfer",
        ],
        check=True,
        capture_output=True,
    )
    return str(db)


def test_adapter_tables_are_queryable_at_runtime(freesurfer_db: str) -> None:
    with bidslake.open(freesurfer_db) as lake:
        assert "freesurfer_aparc" in lake.tables()
        df = lake.table("freesurfer_aparc").pl()
        assert df.height == 2


def test_adapter_values_are_typed(freesurfer_db: str) -> None:
    with bidslake.open(freesurfer_db) as lake:
        df = lake.table("freesurfer_aparc").pl()
        thick = df.filter(df["StructName"] == "bankssts")["ThickAvg"][0]
        assert thick == pytest.approx(2.5)


def test_adapter_concept_filter(freesurfer_db: str) -> None:
    with bidslake.open(freesurfer_db) as lake:
        rows = list(lake.get(table="freesurfer_aparc", hemi="lh", parc="aparc"))
        assert len(rows) == 2


def test_adapter_measures(freesurfer_db: str) -> None:
    with bidslake.open(freesurfer_db) as lake:
        etiv = lake.table("freesurfer_measures").pl()["eTIV"].drop_nulls().to_list()
        assert etiv == [pytest.approx(1_500_000.0)]


def test_adapter_provenance(freesurfer_db: str) -> None:
    with bidslake.open(freesurfer_db) as lake:
        assert [source for _idx, source, _sha in lake.term_maps] == ["freesurfer"]
