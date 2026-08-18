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

from collections.abc import Callable

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


@pytest.fixture(scope="module")
def freesurfer_db(
    tmp_path_factory: pytest.TempPathFactory, bidslake_cli: Callable[..., None]
) -> str:
    root = tmp_path_factory.mktemp("freesurfer")
    stats = root / "sub-01_ses-1" / "stats"
    surf = root / "sub-01_ses-1" / "surf"
    mri = root / "sub-01_ses-1" / "mri"
    stats.mkdir(parents=True)
    surf.mkdir(parents=True)
    mri.mkdir(parents=True)
    (stats / "aseg.stats").write_text(ASEG_STATS)
    (stats / "lh.aparc.stats").write_text(APARC_STATS)
    (surf / "lh.thickness").write_bytes(b"binary")
    # Cataloged, not read: its concepts can only come from the term-map projection.
    (mri / "wmparc.mgz").write_bytes(b"MGZ")

    db = tmp_path_factory.mktemp("db") / "fs.duckdb"
    bidslake_cli("index", "-i", root, "-o", db, "--dataset-id", "fsdemo", "--adapter", "freesurfer")
    return str(db)


@pytest.fixture(scope="module")
def fs_lake(freesurfer_db: str):
    with bidslake.open(freesurfer_db) as lake:
        yield lake


@pytest.fixture(scope="module")
def aparc(fs_lake):
    return fs_lake.table("freesurfer_aparc").pl()


def test_adapter_tables_are_registered(fs_lake) -> None:
    assert "freesurfer_aparc" in fs_lake.tables()


def test_adapter_tables_hold_the_parsed_rows(aparc) -> None:
    assert aparc.height == 2


def test_adapter_values_are_typed(aparc) -> None:
    """A float column arrives as a float, not the text the `.stats` file holds."""
    thickness = aparc.filter(aparc["StructName"] == "bankssts")["ThickAvg"][0]

    assert thickness == pytest.approx(2.5)


def test_adapter_concept_filter(fs_lake) -> None:
    rows = list(fs_lake.get(table="freesurfer_aparc", hemi="L", parc="aparc"))

    assert len(rows) == 2


def test_adapter_measures(fs_lake) -> None:
    etiv = fs_lake.table("freesurfer_measures").pl()["eTIV"].drop_nulls().to_list()

    assert etiv == [pytest.approx(1_500_000.0)]


def test_adapter_provenance(fs_lake) -> None:
    assert [source for _idx, source, _sha in fs_lake.term_maps] == ["freesurfer"]


# -- a file whose only concepts come from the term map -----------------------


def test_a_term_mapped_file_is_reachable_by_projected_concept(fs_lake) -> None:
    """The query that replaces `file_path.endswith("mri/wmparc.mgz")`.

    `mri/wmparc.mgz` carries no BIDS entity in its name, so without the retained projection
    the only way to reach it is matching the path by hand.
    """
    hits = list(fs_lake.get(sub="01", ses="1", seg="wmparc", extension=".mgz"))

    assert len(hits) == 1


@pytest.fixture(scope="module")
def wmparc(fs_lake):
    return next(iter(fs_lake.get(seg="wmparc", extension=".mgz")))


def test_the_projected_concept_reaches_the_intended_file(wmparc) -> None:
    assert wmparc.file_path.endswith("mri/wmparc.mgz")


def test_the_term_map_supplies_datatype_and_suffix(wmparc) -> None:
    assert (wmparc.datatype, wmparc.suffix) == ("anat", "dseg")


def test_projection_is_not_exposed_as_an_entity(wmparc) -> None:
    """`projected` backs the concept columns; it is not itself a concept.

    It is a real column on adapter catalogs, so without an explicit exclusion it would
    surface as a JSON blob in `entities` — on adapter catalogs only, which is exactly the
    kind of difference that is painful to discover later.
    """
    assert "projected" not in wmparc.entities


def test_the_projected_concepts_appear_under_their_own_names(wmparc) -> None:
    supplied = {k: wmparc.entities.get(k) for k in ("seg", "sub")}

    assert supplied == {"seg": "wmparc", "sub": "01"}
